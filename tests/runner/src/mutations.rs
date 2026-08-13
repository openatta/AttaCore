//! Mid-test-case fixture mutations — `.mutations.json` sidecar file.
//!
//! Lets a `.test` case apply filesystem changes to the copied fixture
//! *between* specific turns (add/edit/delete a skill file, agent-type
//! definition, hook script, rule, ...) so the case can assert the *next*
//! turn actually sees the change — proving live reload (the Skills/
//! Agent-type `notify` watchers, or a daemon `config.reload`) end to end,
//! not just that the initial fixture snapshot works.
//!
//! Sidecar convention: `<dir>/<stem>.test` pairs with
//! `<dir>/<stem>.mutations.json` (same directory, same stem, swapped
//! extension). Absent sidecar = no mutations — every existing case that
//! doesn't need this is completely unaffected.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct MutationManifest {
    pub mutations: Vec<TurnMutation>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TurnMutation {
    /// Apply these ops after this turn index completes, before the next
    /// turn's message is sent (0-based, matches `script::Turn::index`).
    pub after_turn: usize,
    pub ops: Vec<MutationOp>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum MutationOp {
    /// Write `content` to `path` (relative to the fixture's copied workdir
    /// root), creating parent directories as needed. Overwrites if it
    /// already exists — the same primitive covers both "add a new file" and
    /// "edit an existing one," the two are indistinguishable to the
    /// filesystem and don't need separate op types.
    Write { path: String, content: String },
    /// Remove `path` (relative to the workdir root). A no-op if it doesn't
    /// exist, matching this being a deliberately idempotent test primitive
    /// rather than a strict filesystem operation.
    Delete { path: String },
}

/// Load the sidecar manifest for `case_path` (e.g.
/// `tests/cases/skills/002_reload_add.test` →
/// `tests/cases/skills/002_reload_add.mutations.json`). `Ok(None)` (not an
/// error) when no sidecar exists — true for the overwhelming majority of
/// cases, which aren't testing reload at all.
pub fn load_for_case(case_path: &Path) -> anyhow::Result<Option<MutationManifest>> {
    let sidecar = case_path.with_extension("mutations.json");
    if !sidecar.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&sidecar)?;
    let manifest: MutationManifest = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", sidecar.display()))?;
    Ok(Some(manifest))
}

/// Apply every op in `mutation` under `workdir` (the fixture's copied root).
pub fn apply(workdir: &Path, mutation: &TurnMutation) -> anyhow::Result<()> {
    for op in &mutation.ops {
        match op {
            MutationOp::Write { path, content } => {
                let full = workdir.join(path);
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&full, content)?;
                tracing::debug!(path = %full.display(), "mutation: wrote file");
            }
            MutationOp::Delete { path } => {
                let full = workdir.join(path);
                if full.exists() {
                    std::fs::remove_file(&full)?;
                    tracing::debug!(path = %full.display(), "mutation: deleted file");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_for_case_returns_none_when_no_sidecar_exists() {
        let dir = tempfile::tempdir().unwrap();
        let case_path = dir.path().join("some_case.test");
        std::fs::write(&case_path, "irrelevant").unwrap();
        assert!(load_for_case(&case_path).unwrap().is_none());
    }

    #[test]
    fn load_for_case_parses_a_real_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let case_path = dir.path().join("reload_add.test");
        std::fs::write(&case_path, "irrelevant").unwrap();
        std::fs::write(
            dir.path().join("reload_add.mutations.json"),
            serde_json::json!({
                "mutations": [
                    {
                        "after_turn": 0,
                        "ops": [
                            {"op": "write", "path": "a.md", "content": "hello"},
                            {"op": "delete", "path": "b.md"}
                        ]
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();

        let manifest = load_for_case(&case_path).unwrap().unwrap();
        assert_eq!(manifest.mutations.len(), 1);
        assert_eq!(manifest.mutations[0].after_turn, 0);
        assert_eq!(manifest.mutations[0].ops.len(), 2);
    }

    #[test]
    fn apply_write_creates_parent_dirs_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let mutation = TurnMutation {
            after_turn: 0,
            ops: vec![MutationOp::Write {
                path: "nested/dir/skill.md".into(),
                content: "v1".into(),
            }],
        };
        apply(dir.path(), &mutation).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("nested/dir/skill.md")).unwrap(),
            "v1"
        );

        let mutation2 = TurnMutation {
            after_turn: 1,
            ops: vec![MutationOp::Write {
                path: "nested/dir/skill.md".into(),
                content: "v2".into(),
            }],
        };
        apply(dir.path(), &mutation2).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("nested/dir/skill.md")).unwrap(),
            "v2"
        );
    }

    #[test]
    fn apply_delete_is_idempotent_on_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let mutation = TurnMutation {
            after_turn: 0,
            ops: vec![MutationOp::Delete {
                path: "does-not-exist.md".into(),
            }],
        };
        // Must not error even though the file was never created.
        apply(dir.path(), &mutation).unwrap();
    }

    #[test]
    fn apply_delete_removes_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gone.md"), "bye").unwrap();
        let mutation = TurnMutation {
            after_turn: 0,
            ops: vec![MutationOp::Delete {
                path: "gone.md".into(),
            }],
        };
        apply(dir.path(), &mutation).unwrap();
        assert!(!dir.path().join("gone.md").exists());
    }
}
