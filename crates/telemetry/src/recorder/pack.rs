//! Lossless run-length packing for streamed delta chunks.
//!
//! Providers emit token-sized deltas, so a recording that stores one line per
//! chunk spends most of its bytes on JSON envelopes rather than content. This
//! module folds a run of consecutive same-kind deltas belonging to one call
//! into a single row and expands rows back into the exact original chunks.
//!
//! Two properties the encoder is built around:
//!
//! - **Members are never joined.** Token boundaries are evidence — where the
//!   model paused, which span came back from cache. Keeping each fragment a
//!   separate array element is also what makes packing correct without tracking
//!   content-block indices: even a run that spans a block boundary expands to
//!   the identical chunk sequence, because there is no per-chunk index to
//!   restore.
//! - **Anything unrecognized passes through verbatim.** An event the encoder
//!   cannot classify costs compression, never data.

use base::interface::model::ModelEvent;

use super::format::{ChunkRecord, Record, TextRun, ToolArgsRun};

/// Below this a row's envelope rivals the lines it replaces. A format constant,
/// not a tunable: both layouts expand identically, so changing it never
/// invalidates a stored recording.
const MIN_RUN: usize = 3;

/// Runs split here instead of growing without bound. The packer holds pending
/// members in memory until a run ends, so an uncapped run would mean a whole
/// content block buffered — and lost if the process died mid-stream, which is
/// exactly when the recording matters most.
const MAX_RUN: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunKind {
    Text,
    Thinking,
    ToolArgs,
}

fn classify(chunk: &ChunkRecord) -> Option<RunKind> {
    match &chunk.chunk {
        ModelEvent::TextDelta { .. } => Some(RunKind::Text),
        ModelEvent::ThinkingDelta { .. } => Some(RunKind::Thinking),
        ModelEvent::ToolArgsDelta { .. } => Some(RunKind::ToolArgs),
        _ => None,
    }
}

fn tool_args_id(chunk: &ChunkRecord) -> Option<&str> {
    match &chunk.chunk {
        ModelEvent::ToolArgsDelta { id, .. } => Some(id),
        _ => None,
    }
}

fn payload(chunk: &ChunkRecord) -> String {
    match &chunk.chunk {
        ModelEvent::TextDelta { text } | ModelEvent::ThinkingDelta { text } => text.clone(),
        ModelEvent::ToolArgsDelta { partial_json, .. } => partial_json.clone(),
        _ => String::new(),
    }
}

/// Whether `next` extends a run ending at `prev`.
fn continues(prev: &ChunkRecord, next: &ChunkRecord, kind: RunKind) -> bool {
    if next.call != prev.call || next.seq != prev.seq + 1 {
        return false;
    }
    if kind == RunKind::ToolArgs && tool_args_id(prev) != tool_args_id(next) {
        return false;
    }
    true
}

fn build_row(kind: RunKind, run: &[ChunkRecord]) -> Record {
    let first = &run[0];
    let dt: Vec<i64> = run
        .windows(2)
        .map(|w| w[1].ts as i64 - w[0].ts as i64)
        .collect();
    let members: Vec<String> = run.iter().map(payload).collect();
    match kind {
        RunKind::ToolArgs => Record::ToolArgsChunks(ToolArgsRun {
            seq0: first.seq,
            ts0: first.ts,
            call: first.call,
            id: tool_args_id(first).unwrap_or_default().to_string(),
            args: members,
            dt,
        }),
        RunKind::Text | RunKind::Thinking => {
            let run = TextRun {
                seq0: first.seq,
                ts0: first.ts,
                call: first.call,
                texts: members,
                dt,
            };
            if kind == RunKind::Text {
                Record::TextChunks(run)
            } else {
                Record::ThinkingChunks(run)
            }
        }
    }
}

/// Incremental packer: feed chunks in arrival order, write out what it returns.
///
/// Holding a partial run is what buys the compression, so the packer trades a
/// bounded write delay (at most [`MAX_RUN`] members) for it. [`RunPacker::flush`]
/// must be called at the end of a call's stream — otherwise its tail sits in
/// the buffer and never reaches the file.
#[derive(Debug, Default)]
pub struct RunPacker {
    pending: Vec<ChunkRecord>,
    kind: Option<RunKind>,
}

impl RunPacker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: ChunkRecord) -> Vec<Record> {
        let Some(kind) = classify(&chunk) else {
            let mut out = self.flush();
            out.push(Record::Chunk(chunk));
            return out;
        };

        let extends = self.kind == Some(kind)
            && self
                .pending
                .last()
                .is_some_and(|prev| continues(prev, &chunk, kind));

        if !extends {
            let out = self.flush();
            self.kind = Some(kind);
            self.pending.push(chunk);
            return out;
        }

        self.pending.push(chunk);
        if self.pending.len() >= MAX_RUN {
            return self.flush();
        }
        Vec::new()
    }

    pub fn flush(&mut self) -> Vec<Record> {
        let kind = self.kind.take();
        let run = std::mem::take(&mut self.pending);
        match kind {
            Some(kind) if run.len() >= MIN_RUN => vec![build_row(kind, &run)],
            _ => run.into_iter().map(Record::Chunk).collect(),
        }
    }
}

/// Pack a complete batch. Equivalent to feeding every member to a
/// [`RunPacker`] and flushing.
pub fn pack(chunks: Vec<ChunkRecord>) -> Vec<Record> {
    let mut packer = RunPacker::new();
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend(packer.push(chunk));
    }
    out.extend(packer.flush());
    out
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PackError {
    #[error("{tag} row: dt has {dt} gaps for {members} members (expected {expected})")]
    DtArity {
        tag: &'static str,
        dt: usize,
        members: usize,
        expected: usize,
    },
    #[error("{tag} row: empty member list")]
    Empty { tag: &'static str },
    #[error("{tag} row: member timestamp left the representable range")]
    TimeRange { tag: &'static str },
}

fn expand_members(
    tag: &'static str,
    members: &[String],
    dt: &[i64],
    seq0: u64,
    ts0: u64,
    call: u64,
    make: impl Fn(&str) -> ModelEvent,
) -> Result<Vec<ChunkRecord>, PackError> {
    if members.is_empty() {
        return Err(PackError::Empty { tag });
    }
    if dt.len() != members.len() - 1 {
        return Err(PackError::DtArity {
            tag,
            dt: dt.len(),
            members: members.len(),
            expected: members.len() - 1,
        });
    }
    let mut out = Vec::with_capacity(members.len());
    let mut ts = ts0 as i64;
    for (i, member) in members.iter().enumerate() {
        if i > 0 {
            ts += dt[i - 1];
        }
        if ts < 0 {
            return Err(PackError::TimeRange { tag });
        }
        out.push(ChunkRecord {
            seq: seq0 + i as u64,
            ts: ts as u64,
            call,
            chunk: make(member),
        });
    }
    Ok(out)
}

pub fn expand_text_run(run: &TextRun, thinking: bool) -> Result<Vec<ChunkRecord>, PackError> {
    let tag = if thinking {
        "thinking_chunks"
    } else {
        "text_chunks"
    };
    expand_members(tag, &run.texts, &run.dt, run.seq0, run.ts0, run.call, |t| {
        if thinking {
            ModelEvent::ThinkingDelta { text: t.into() }
        } else {
            ModelEvent::TextDelta { text: t.into() }
        }
    })
}

pub fn expand_tool_args_run(run: &ToolArgsRun) -> Result<Vec<ChunkRecord>, PackError> {
    expand_members(
        "tool_args_chunks",
        &run.args,
        &run.dt,
        run.seq0,
        run.ts0,
        run.call,
        |a| ModelEvent::ToolArgsDelta {
            id: run.id.clone(),
            partial_json: a.into(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::Usage;

    fn text(seq: u64, ts: u64, s: &str) -> ChunkRecord {
        ChunkRecord {
            seq,
            ts,
            call: 0,
            chunk: ModelEvent::TextDelta { text: s.into() },
        }
    }

    fn args(seq: u64, ts: u64, id: &str, s: &str) -> ChunkRecord {
        ChunkRecord {
            seq,
            ts,
            call: 0,
            chunk: ModelEvent::ToolArgsDelta {
                id: id.into(),
                partial_json: s.into(),
            },
        }
    }

    fn expand_all(records: Vec<Record>) -> Vec<ChunkRecord> {
        let mut out = Vec::new();
        for record in records {
            match record {
                Record::Chunk(c) => out.push(c),
                Record::TextChunks(r) => out.extend(expand_text_run(&r, false).unwrap()),
                Record::ThinkingChunks(r) => out.extend(expand_text_run(&r, true).unwrap()),
                Record::ToolArgsChunks(r) => out.extend(expand_tool_args_run(&r).unwrap()),
                other => panic!("unexpected record: {other:?}"),
            }
        }
        out
    }

    fn assert_round_trip(chunks: Vec<ChunkRecord>) {
        let expected = format!("{chunks:?}");
        let packed = pack(chunks);
        assert_eq!(format!("{:?}", expand_all(packed)), expected);
    }

    #[test]
    fn a_run_at_the_minimum_packs_into_one_row() {
        let chunks = vec![text(0, 100, "a"), text(1, 112, "b"), text(2, 120, "c")];
        let packed = pack(chunks.clone());
        assert_eq!(packed.len(), 1);
        assert!(matches!(packed[0], Record::TextChunks(_)));
        assert_round_trip(chunks);
    }

    #[test]
    fn a_run_below_the_minimum_stays_one_line_each() {
        let chunks = vec![text(0, 100, "a"), text(1, 112, "b")];
        let packed = pack(chunks.clone());
        assert_eq!(packed.len(), 2);
        assert!(packed.iter().all(|r| matches!(r, Record::Chunk(_))));
        assert_round_trip(chunks);
    }

    #[test]
    fn token_boundaries_survive_packing() {
        let chunks = vec![
            text(0, 100, "Hel"),
            text(1, 112, "lo"),
            text(2, 120, " wor"),
            text(3, 135, "ld"),
        ];
        let packed = pack(chunks);
        let Record::TextChunks(run) = &packed[0] else {
            panic!("expected a packed run")
        };
        assert_eq!(run.texts, vec!["Hel", "lo", " wor", "ld"]);
        assert_eq!(run.dt, vec![12, 8, 15]);
    }

    #[test]
    fn a_non_delta_event_breaks_the_run() {
        let chunks = vec![
            text(0, 100, "a"),
            text(1, 110, "b"),
            text(2, 120, "c"),
            ChunkRecord {
                seq: 3,
                ts: 130,
                call: 0,
                chunk: ModelEvent::ContentBlockStop { index: 0 },
            },
            text(4, 140, "d"),
            text(5, 150, "e"),
            text(6, 160, "f"),
        ];
        let packed = pack(chunks.clone());
        assert_eq!(packed.len(), 3);
        assert_round_trip(chunks);
    }

    #[test]
    fn a_sequence_gap_breaks_the_run() {
        let chunks = vec![text(0, 100, "a"), text(1, 110, "b"), text(9, 120, "c")];
        assert_round_trip(chunks);
    }

    #[test]
    fn a_different_call_breaks_the_run() {
        let mut chunks = vec![text(0, 100, "a"), text(1, 110, "b"), text(2, 120, "c")];
        chunks[2].call = 1;
        assert_round_trip(chunks);
    }

    #[test]
    fn tool_args_with_a_different_id_never_share_a_run() {
        let chunks = vec![
            args(0, 100, "t1", "{\"a\":"),
            args(1, 110, "t1", "1}"),
            args(2, 120, "t2", "{\"b\":"),
            args(3, 130, "t2", "2}"),
        ];
        let packed = pack(chunks.clone());
        assert!(packed
            .iter()
            .all(|r| matches!(r, Record::Chunk(_) | Record::ToolArgsChunks(_))));
        assert_round_trip(chunks);
    }

    #[test]
    fn tool_args_run_round_trips_with_its_id() {
        let chunks = vec![
            args(0, 100, "t1", "{\"path\":"),
            args(1, 110, "t1", "\"/tmp"),
            args(2, 120, "t1", "/x\"}"),
        ];
        let packed = pack(chunks.clone());
        assert_eq!(packed.len(), 1);
        assert_round_trip(chunks);
    }

    #[test]
    fn a_run_longer_than_the_cap_splits_and_still_round_trips() {
        let chunks: Vec<_> = (0..MAX_RUN as u64 * 2 + 5)
            .map(|i| text(i, 100 + i * 3, &format!("t{i}")))
            .collect();
        let packed = pack(chunks.clone());
        assert!(packed.len() > 1);
        assert_round_trip(chunks);
    }

    #[test]
    fn a_backwards_clock_step_round_trips() {
        let chunks = vec![text(0, 200, "a"), text(1, 180, "b"), text(2, 210, "c")];
        let packed = pack(chunks.clone());
        let Record::TextChunks(run) = &packed[0] else {
            panic!("expected a packed run")
        };
        assert_eq!(run.dt, vec![-20, 30]);
        assert_round_trip(chunks);
    }

    #[test]
    fn mixed_event_kinds_round_trip_in_order() {
        let chunks = vec![
            ChunkRecord {
                seq: 0,
                ts: 100,
                call: 0,
                chunk: ModelEvent::ThinkingDelta { text: "hm".into() },
            },
            ChunkRecord {
                seq: 1,
                ts: 110,
                call: 0,
                chunk: ModelEvent::ThinkingDelta { text: "mm".into() },
            },
            ChunkRecord {
                seq: 2,
                ts: 120,
                call: 0,
                chunk: ModelEvent::ThinkingDelta { text: "!".into() },
            },
            ChunkRecord {
                seq: 3,
                ts: 130,
                call: 0,
                chunk: ModelEvent::ThinkingSignature {
                    signature: "s".into(),
                },
            },
            text(4, 140, "a"),
            text(5, 150, "b"),
            text(6, 160, "c"),
            ChunkRecord {
                seq: 7,
                ts: 170,
                call: 0,
                chunk: ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Usage {
                        input_tokens: 1,
                        output_tokens: 2,
                    },
                },
            },
        ];
        assert_round_trip(chunks);
    }

    #[test]
    fn a_malformed_row_fails_loudly_instead_of_dropping_the_run() {
        let run = TextRun {
            seq0: 0,
            ts0: 100,
            call: 0,
            texts: vec!["a".into(), "b".into(), "c".into()],
            dt: vec![10],
        };
        assert_eq!(
            expand_text_run(&run, false).unwrap_err(),
            PackError::DtArity {
                tag: "text_chunks",
                dt: 1,
                members: 3,
                expected: 2,
            }
        );
    }

    #[test]
    fn an_empty_row_fails_loudly() {
        let run = TextRun {
            seq0: 0,
            ts0: 100,
            call: 0,
            texts: vec![],
            dt: vec![],
        };
        assert_eq!(
            expand_text_run(&run, false).unwrap_err(),
            PackError::Empty { tag: "text_chunks" }
        );
    }
}
