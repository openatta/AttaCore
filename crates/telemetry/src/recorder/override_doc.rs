//! `replay.override.json` — hand edits layered over a recording.
//!
//! Debugging a turn often means asking "what if the model had said something
//! else here". Editing `calls.jsonl` answers that but destroys the recording,
//! and editing a blob is worse: blobs are content-addressed and deduplicated,
//! so changing one silently changes every call that referenced it.
//!
//! An override is a separate file. The recording stays exactly as recorded,
//! the edit is a reviewable artifact of its own, and deleting it restores the
//! original behavior. It is read only when present, so a recording without one
//! costs nothing.
//!
//! Indexes address the recording's own order — the position of a call in
//! `calls.jsonl`, which is what someone reading the file counts. Notably *not*
//! the per-session slice replay walks: an override is an edit to a file, and
//! it should not change meaning because replay grouped the file differently.
//!
//! Overrides replace responses, not requests. Changing what went *out* and
//! seeing what comes back needs a real provider, which is a different mode.

use base::interface::model::ModelEvent;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const OVERRIDE_FILE: &str = "replay.override.json";

#[derive(Debug, thiserror::Error)]
pub enum OverrideError {
    #[error("{OVERRIDE_FILE} is not valid JSON: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("io error reading {OVERRIDE_FILE}: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "{OVERRIDE_FILE} patches call #{at}, but the recording has {len} call(s) \
         — it was probably written against an earlier recording"
    )]
    OutOfRange { at: usize, len: usize },
    #[error("{OVERRIDE_FILE} patches call #{at} more than once")]
    DuplicatePatch { at: usize },
}

/// A replacement response for one call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverrideEntry {
    /// The events replay should emit instead of the recorded ones.
    pub response: Vec<ModelEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverridePatch {
    /// 0-based position in `calls.jsonl`.
    pub at: usize,
    #[serde(flatten)]
    pub entry: OverrideEntry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OverrideDoc {
    /// Replace every call's response, in recording order.
    Whole(Vec<OverrideEntry>),
    /// Keep the recording and swap the named positions.
    Patches { patches: Vec<OverridePatch> },
}

/// Load `dir`'s override, if it has one.
///
/// A missing file is `Ok(None)` — the ordinary case. A file that is present but
/// wrong fails loud: someone wrote it on purpose, and silently ignoring it
/// would send them hunting through replay output for an edit that never
/// applied.
pub fn load(dir: &Path) -> Result<Option<OverrideDoc>, OverrideError> {
    let path = dir.join(OVERRIDE_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(Some(serde_json::from_str(&text)?))
}

/// Apply `doc` to `responses`, indexed by position in the recording.
pub fn apply(doc: &OverrideDoc, responses: &mut [Vec<ModelEvent>]) -> Result<(), OverrideError> {
    match doc {
        OverrideDoc::Whole(entries) => {
            if entries.len() > responses.len() {
                return Err(OverrideError::OutOfRange {
                    at: entries.len() - 1,
                    len: responses.len(),
                });
            }
            for (slot, entry) in responses.iter_mut().zip(entries) {
                *slot = entry.response.clone();
            }
        }
        OverrideDoc::Patches { patches } => {
            let mut seen: Vec<usize> = Vec::with_capacity(patches.len());
            for patch in patches {
                if patch.at >= responses.len() {
                    return Err(OverrideError::OutOfRange {
                        at: patch.at,
                        len: responses.len(),
                    });
                }
                if seen.contains(&patch.at) {
                    return Err(OverrideError::DuplicatePatch { at: patch.at });
                }
                seen.push(patch.at);
                responses[patch.at] = patch.entry.response.clone();
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(t: &str) -> ModelEvent {
        ModelEvent::TextDelta { text: t.into() }
    }

    fn responses() -> Vec<Vec<ModelEvent>> {
        vec![vec![text("a")], vec![text("b")], vec![text("c")]]
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn a_patch_swaps_only_the_position_it_names() {
        let doc: OverrideDoc = serde_json::from_str(
            r#"{"patches":[{"at":1,"response":[{"kind":"text_delta","text":"Z"}]}]}"#,
        )
        .unwrap();
        let mut out = responses();
        apply(&doc, &mut out).unwrap();
        assert_eq!(format!("{:?}", out[0]), format!("{:?}", vec![text("a")]));
        assert_eq!(format!("{:?}", out[1]), format!("{:?}", vec![text("Z")]));
        assert_eq!(format!("{:?}", out[2]), format!("{:?}", vec![text("c")]));
    }

    #[test]
    fn a_bare_array_replaces_from_the_start() {
        let doc: OverrideDoc =
            serde_json::from_str(r#"[{"response":[{"kind":"text_delta","text":"Z"}]}]"#).unwrap();
        let mut out = responses();
        apply(&doc, &mut out).unwrap();
        assert_eq!(format!("{:?}", out[0]), format!("{:?}", vec![text("Z")]));
        assert_eq!(format!("{:?}", out[1]), format!("{:?}", vec![text("b")]));
    }

    /// Re-recording renumbers the calls, so an index that used to be valid can
    /// quietly start naming a different call. Refusing beats applying it.
    #[test]
    fn an_index_past_the_end_fails_loud() {
        let doc = OverrideDoc::Patches {
            patches: vec![OverridePatch {
                at: 7,
                entry: OverrideEntry { response: vec![] },
            }],
        };
        let mut out = responses();
        assert!(matches!(
            apply(&doc, &mut out),
            Err(OverrideError::OutOfRange { at: 7, len: 3 })
        ));
    }

    #[test]
    fn two_patches_for_one_call_fail_rather_than_pick_one() {
        let doc = OverrideDoc::Patches {
            patches: vec![
                OverridePatch {
                    at: 0,
                    entry: OverrideEntry { response: vec![] },
                },
                OverridePatch {
                    at: 0,
                    entry: OverrideEntry {
                        response: vec![text("z")],
                    },
                },
            ],
        };
        let mut out = responses();
        assert!(matches!(
            apply(&doc, &mut out),
            Err(OverrideError::DuplicatePatch { at: 0 })
        ));
    }
}
