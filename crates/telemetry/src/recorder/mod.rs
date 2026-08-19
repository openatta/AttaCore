//! Recorder — records every LLM call and replays it deterministically.
//!
//! A `Model` decorator sitting on `Model::stream()`, the one place in the
//! engine that sees a complete request and a complete response at once: the
//! turn loop assembles the request but hands the stream away, and the stream
//! consumer sees the response but not what produced it.
//!
//! What it produces is diagnostic data, not session state. It takes no part in
//! resume, fork, or context reconstruction — deleting a recording leaves the
//! session working exactly as before. `crates/history` remains the sole
//! authority on what a conversation is.
//!
//! Replay matches by **position**, not by hashing the request: the k-th live
//! call replays the k-th recorded one. Hash-keyed lookup makes every
//! environment-dependent byte in a prompt a correctness problem, and when it
//! misses it can only report that it missed. Recording the whole request
//! instead means a mismatch can name the field that moved.
//!
//! **A recording holds everything the model saw, in the clear.** System
//! prompts, the whole conversation, tool arguments and results — verbatim, and
//! deliberately not passed through [`crate::RedactionPolicy`], which exists for
//! telemetry events and would defeat the point here: a recording is worth
//! keeping only if it says what was actually sent. So a recording carries
//! whatever secrets the prompt carried, and its directory deserves the same
//! handling as the transcript it came from. Recording is off unless configured.
//!
//! See `docs/recorder_design.md`.

pub mod blob;
pub mod format;
pub mod pack;
pub mod reader;
pub mod writer;

use async_trait::async_trait;
use base::interface::model::{
    Model, ModelError, ModelEvent, ModelMessage, ModelStream, StreamParams, ToolDef, Usage,
};
use base::interface::prompt::PromptBlock;
use base::interface::settings::{Divergence, RecorderConfig, RecorderMode};
use base::provider::ApiType;
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio_util::sync::CancellationToken;

use blob::BlobId;
use format::{
    now_ms, CallRecord, ChunkRecord, EndRecord, Header, Outcome, Record, RecordedError,
    RecordedParams, FORMAT_VERSION,
};
use pack::RunPacker;
use reader::Recording;
use writer::RecordingWriter;

/// Name used when a call carries no session identity and the config named no
/// recording — auxiliary calls made outside any session.
const UNATTRIBUTED: &str = "unattributed";

pub struct RecorderModel {
    inner: Arc<dyn Model>,
    config: Option<RecorderConfig>,
    engine_version: String,
    /// One writer per recording name. A sub-agent runs its own session through
    /// the same model, and its calls belong in their own recording rather than
    /// interleaved into the parent's — which would also break replay, whose
    /// positions are per session.
    writers: Mutex<HashMap<String, Arc<RecordingWriter>>>,
    replays: Mutex<HashMap<String, ReplayState>>,
}

struct ReplayState {
    recording: Recording,
    cursor: usize,
}

impl RecorderModel {
    pub fn new(
        inner: Arc<dyn Model>,
        config: Option<RecorderConfig>,
        engine_version: &str,
    ) -> Self {
        Self {
            inner,
            config: config.or_else(Self::env_config),
            engine_version: engine_version.to_string(),
            writers: Mutex::new(HashMap::new()),
            replays: Mutex::new(HashMap::new()),
        }
    }

    /// `ATTA_RECORD=<name>` / `ATTA_REPLAY=<name>` with `ATTA_RECORDINGS_DIR`
    /// for the root. Absent all of them, the decorator is a pass-through.
    fn env_config() -> Option<RecorderConfig> {
        let root = std::env::var("ATTA_RECORDINGS_DIR")
            .ok()
            .map(PathBuf::from)?;
        let on_divergence = Self::default_divergence();
        if let Ok(name) = std::env::var("ATTA_RECORD") {
            Some(RecorderConfig {
                mode: RecorderMode::Record,
                name: Some(name),
                root,
                on_divergence,
            })
        } else if let Ok(name) = std::env::var("ATTA_REPLAY") {
            Some(RecorderConfig {
                mode: RecorderMode::Replay,
                name: Some(name),
                root,
                on_divergence,
            })
        } else {
            None
        }
    }

    fn recording_name(&self, params: &StreamParams) -> String {
        if let Some(config) = &self.config {
            if let Some(name) = &config.name {
                return name.clone();
            }
        }
        params
            .origin
            .as_ref()
            .map(|o| o.session_id.clone())
            .unwrap_or_else(|| UNATTRIBUTED.to_string())
    }

    fn writer_for(
        &self,
        config: &RecorderConfig,
        name: &str,
        params: &StreamParams,
    ) -> Arc<RecordingWriter> {
        let mut writers = self.writers.lock().unwrap_or_else(|e| e.into_inner());
        writers
            .entry(name.to_string())
            .or_insert_with(|| {
                Arc::new(RecordingWriter::create(
                    &config.root,
                    Header {
                        version: FORMAT_VERSION,
                        name: name.to_string(),
                        session_id: params
                            .origin
                            .as_ref()
                            .map(|o| o.session_id.clone())
                            .unwrap_or_default(),
                        parent: None,
                        created_at: now_ms(),
                        engine_version: self.engine_version.clone(),
                    },
                ))
            })
            .clone()
    }
}

fn flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v != "0" && !v.is_empty())
}

/// The environment facts that decide the default divergence policy. Split from
/// the lookups so the policy is a pure function that can be tested without
/// mutating process-global environment variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DivergenceInputs {
    forced_strict: bool,
    forced_warn: bool,
    in_test: bool,
    in_ci: bool,
}

fn resolve_divergence(inputs: DivergenceInputs) -> Divergence {
    if inputs.forced_strict {
        return Divergence::Strict;
    }
    if inputs.forced_warn {
        return Divergence::Warn;
    }
    // Strict wherever a divergence means a broken test; lenient only for a
    // human driving a replay by hand, so a brand-new case still runs before
    // anyone has recorded it.
    if inputs.in_test || inputs.in_ci {
        Divergence::Strict
    } else {
        Divergence::Warn
    }
}

impl RecorderModel {
    /// The divergence policy a harness should ask for rather than re-derive.
    ///
    /// Public because a harness that builds an explicit [`RecorderConfig`]
    /// never reaches [`RecorderModel::env_config`], and re-deriving this is
    /// how the equivalent flag once shipped as a silent no-op.
    ///
    /// Overrides, highest precedence first: `ATTA_REPLAY_STRICT=1`, then
    /// `ATTA_REPLAY_WARN=1`.
    pub fn default_divergence() -> Divergence {
        resolve_divergence(DivergenceInputs {
            forced_strict: flag("ATTA_REPLAY_STRICT"),
            forced_warn: flag("ATTA_REPLAY_WARN"),
            in_test: std::env::var("CARGO_TEST_RUNNER").is_ok()
                || std::env::var("ATTA_RECORDER_AUTO_DETECT").is_ok(),
            in_ci: flag("CI"),
        })
    }
}

/// The parts of a request that identify it, as blob ids. Computed for both a
/// live request and a recorded one so replay can compare them without loading
/// content that matches.
struct RequestShape {
    params: RecordedParams,
    system: Vec<BlobId>,
    tools: BlobId,
    messages: Vec<BlobId>,
}

impl RequestShape {
    fn of(
        prompt_blocks: &[PromptBlock],
        tools: &[ToolDef],
        messages: &[ModelMessage],
        params: &StreamParams,
    ) -> Self {
        Self {
            params: RecordedParams {
                model: params.model.clone(),
                max_tokens: params.max_tokens,
                thinking_mode: params.thinking_mode.clone(),
                fallback_model: params.fallback_model.clone(),
                cache_edits: params.cache_edits.clone(),
            },
            system: prompt_blocks.iter().map(BlobId::of_value).collect(),
            tools: BlobId::of_value(&tools),
            messages: messages.iter().map(BlobId::of_value).collect(),
        }
    }
}

/// Field-level differences between what was recorded and what the live run
/// produced. Empty means the two agree.
fn diverges(recorded: &CallRecord, live: &RequestShape) -> Vec<String> {
    let mut out = Vec::new();
    if recorded.params != live.params {
        out.push(format!(
            "params: recorded {:?}, live {:?}",
            recorded.params, live.params
        ));
    }
    if recorded.system.len() != live.system.len() {
        out.push(format!(
            "system: recorded {} blocks, live {}",
            recorded.system.len(),
            live.system.len()
        ));
    } else {
        for (i, (a, b)) in recorded.system.iter().zip(&live.system).enumerate() {
            if a != b {
                out.push(format!("system[{i}]: recorded {a}, live {b}"));
            }
        }
    }
    if recorded.tools != live.tools {
        out.push(format!(
            "tools: recorded {}, live {}",
            recorded.tools, live.tools
        ));
    }
    if recorded.messages.len() != live.messages.len() {
        out.push(format!(
            "messages: recorded {} messages, live {}",
            recorded.messages.len(),
            live.messages.len()
        ));
    } else {
        for (i, (a, b)) in recorded.messages.iter().zip(&live.messages).enumerate() {
            if a != b {
                out.push(format!("messages[{i}]: recorded {a}, live {b}"));
            }
        }
    }
    out
}

#[async_trait]
impl Model for RecorderModel {
    fn api_type(&self) -> ApiType {
        self.inner.api_type()
    }

    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        params: StreamParams,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let Some(config) = self.config.clone() else {
            return self
                .inner
                .stream(prompt_blocks, tools, messages, params, cancel)
                .await;
        };
        let name = self.recording_name(&params);

        match config.mode {
            RecorderMode::Replay => {
                let shape = RequestShape::of(&prompt_blocks, &tools, &messages, &params);
                self.replay(&config, &name, shape)
            }
            RecorderMode::Record => {
                let writer = self.writer_for(&config, &name, &params);
                self.record(writer, prompt_blocks, tools, messages, params, cancel)
                    .await
            }
        }
    }
}

impl RecorderModel {
    async fn record(
        &self,
        writer: Arc<RecordingWriter>,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        params: StreamParams,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let call_seq = writer.next_seq();
        let started = std::time::Instant::now();

        // Written before the stream opens, and flushed: a process killed
        // mid-response still leaves behind the request that was in flight.
        if let Err(e) = self.write_call(
            &writer,
            call_seq,
            &prompt_blocks,
            &tools,
            &messages,
            &params,
        ) {
            tracing::warn!(error = %e, "recorder: failed to write call record, continuing unrecorded");
        }

        let result = self
            .inner
            .stream(prompt_blocks, tools, messages, params, cancel)
            .await;

        match result {
            Ok(inner) => Ok(Box::new(RecordingStream {
                inner,
                writer,
                packer: RunPacker::new(),
                call_seq,
                started,
                stop_reason: String::new(),
                usage: Usage::default(),
                finished: false,
            })),
            Err(e) => {
                write_end(
                    &writer,
                    call_seq,
                    Outcome::Error {
                        error: RecordedError::from(&e),
                    },
                    None,
                    None,
                    started,
                );
                Err(e)
            }
        }
    }

    fn write_call(
        &self,
        writer: &RecordingWriter,
        call_seq: u64,
        prompt_blocks: &[PromptBlock],
        tools: &[ToolDef],
        messages: &[ModelMessage],
        params: &StreamParams,
    ) -> std::io::Result<()> {
        let blobs = writer.blobs();
        let system = prompt_blocks
            .iter()
            .map(|b| blobs.put(b))
            .collect::<Result<Vec<_>, _>>()?;
        let tools_id = blobs.put(&tools)?;
        let message_ids = messages
            .iter()
            .map(|m| blobs.put(m))
            .collect::<Result<Vec<_>, _>>()?;

        let (turn, step) = params
            .origin
            .as_ref()
            .map(|o| (o.turn, o.step))
            .unwrap_or((0, 0));

        writer.append(&Record::Call(CallRecord {
            seq: call_seq,
            ts: now_ms(),
            turn,
            step,
            provider: match self.inner.api_type() {
                ApiType::Anthropic => "anthropic".into(),
                ApiType::OpenAICompatible => "openai_compatible".into(),
            },
            api_type: self.inner.api_type(),
            params: RecordedParams {
                model: params.model.clone(),
                max_tokens: params.max_tokens,
                thinking_mode: params.thinking_mode.clone(),
                fallback_model: params.fallback_model.clone(),
                cache_edits: params.cache_edits.clone(),
            },
            system,
            tools: tools_id,
            messages: message_ids,
        }))?;
        writer.flush()
    }

    fn replay(
        &self,
        config: &RecorderConfig,
        name: &str,
        shape: RequestShape,
    ) -> Result<ModelStream, ModelError> {
        let mut replays = self.replays.lock().unwrap_or_else(|e| e.into_inner());
        let state = match replays.get_mut(name) {
            Some(state) => state,
            None => {
                let recording = reader::load(&config.root.join(name)).map_err(|e| {
                    ModelError::Internal(format!("recorder: cannot replay {name:?}: {e}"))
                })?;
                if recording.damaged > 0 {
                    tracing::warn!(
                        name,
                        damaged = recording.damaged,
                        "recorder: replaying a recording with unreadable lines"
                    );
                }
                replays.entry(name.to_string()).or_insert(ReplayState {
                    recording,
                    cursor: 0,
                })
            }
        };

        let Some(call) = state.recording.calls.get(state.cursor) else {
            return Err(ModelError::Internal(format!(
                "recorder: recording {:?} has {} calls, live run asked for #{}",
                name,
                state.recording.calls.len(),
                state.cursor + 1
            )));
        };
        state.cursor += 1;

        let differences = diverges(&call.request, &shape);
        if !differences.is_empty() {
            let report = format!(
                "recorder: call #{} diverges from recording {:?}:\n  {}",
                state.cursor,
                name,
                differences.join("\n  ")
            );
            match config.on_divergence {
                Divergence::Strict => return Err(ModelError::Internal(report)),
                Divergence::Warn => tracing::warn!("{report}"),
            }
        }

        if let Some(end) = &call.end {
            if let Outcome::Error { error } = &end.outcome {
                return Err(error.clone().into_model_error());
            }
        }

        let events: Vec<Result<ModelEvent, ModelError>> =
            call.response.iter().cloned().map(Ok).collect();
        Ok(Box::new(futures::stream::iter(events)))
    }
}

fn write_end(
    writer: &RecordingWriter,
    call_seq: u64,
    outcome: Outcome,
    stop_reason: Option<String>,
    usage: Option<Usage>,
    started: std::time::Instant,
) {
    let record = Record::End(EndRecord {
        seq: writer.next_seq(),
        ts: now_ms(),
        call: call_seq,
        outcome,
        stop_reason,
        usage,
        duration_ms: started.elapsed().as_millis() as u64,
    });
    if let Err(e) = writer.append(&record).and_then(|()| writer.flush()) {
        tracing::warn!(error = %e, "recorder: failed to write end record");
    }
}

/// Forwards the model's stream downstream while writing every event.
///
/// Recording happens on the way through rather than after collection: buffering
/// the whole response before returning it would make the recorder visible in
/// the agent's own latency, and would lose everything if the process died
/// mid-stream.
struct RecordingStream {
    inner: ModelStream,
    writer: Arc<RecordingWriter>,
    packer: RunPacker,
    call_seq: u64,
    started: std::time::Instant,
    stop_reason: String,
    usage: Usage,
    finished: bool,
}

impl RecordingStream {
    fn write(&mut self, records: Vec<Record>) {
        for record in &records {
            if let Err(e) = self.writer.append(record) {
                tracing::warn!(error = %e, "recorder: failed to write chunk");
                return;
            }
        }
    }

    fn finish(&mut self, outcome: Outcome) {
        if self.finished {
            return;
        }
        self.finished = true;
        let tail = self.packer.flush();
        self.write(tail);
        let stop_reason = (!self.stop_reason.is_empty()).then(|| self.stop_reason.clone());
        write_end(
            &self.writer,
            self.call_seq,
            outcome,
            stop_reason,
            Some(self.usage.clone()),
            self.started,
        );
    }
}

impl futures::Stream for RecordingStream {
    type Item = Result<ModelEvent, ModelError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(event))) => {
                if let ModelEvent::EndTurn { stop_reason, usage } = &event {
                    this.stop_reason = stop_reason.clone();
                    this.usage = usage.clone();
                }
                let records = this.packer.push(ChunkRecord {
                    seq: this.writer.next_seq(),
                    ts: now_ms(),
                    call: this.call_seq,
                    chunk: event.clone(),
                });
                this.write(records);
                Poll::Ready(Some(Ok(event)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.finish(Outcome::Error {
                    error: RecordedError::from(&e),
                });
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.finish(Outcome::Ok);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A stream dropped before it ended was cancelled — the turn was interrupted,
/// or a caller stopped reading. Closing the call here is what keeps every
/// `call` record paired with an `end`.
impl Drop for RecordingStream {
    fn drop(&mut self) {
        self.finish(Outcome::Cancelled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::CallOrigin;
    use base::interface::prompt::BlockRole;
    use base::interface::settings::ThinkingMode;
    use futures::StreamExt;

    /// Replays a fixed script, or fails to open when `open_error` is set.
    struct ScriptedModel {
        events: Vec<ModelEvent>,
        open_error: bool,
    }

    impl ScriptedModel {
        fn ok(events: Vec<ModelEvent>) -> Arc<dyn Model> {
            Arc::new(Self {
                events,
                open_error: false,
            })
        }

        fn overloaded() -> Arc<dyn Model> {
            Arc::new(Self {
                events: vec![],
                open_error: true,
            })
        }
    }

    #[async_trait]
    impl Model for ScriptedModel {
        fn api_type(&self) -> ApiType {
            ApiType::Anthropic
        }
        async fn stream(
            &self,
            _: Vec<PromptBlock>,
            _: Vec<ToolDef>,
            _: Vec<ModelMessage>,
            _: StreamParams,
            _: CancellationToken,
        ) -> Result<ModelStream, ModelError> {
            if self.open_error {
                return Err(ModelError::Overloaded);
            }
            let events: Vec<Result<ModelEvent, ModelError>> =
                self.events.iter().cloned().map(Ok).collect();
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    fn script() -> Vec<ModelEvent> {
        vec![
            ModelEvent::TextDelta { text: "Hel".into() },
            ModelEvent::TextDelta { text: "lo".into() },
            ModelEvent::TextDelta { text: "!".into() },
            ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Usage {
                    input_tokens: 11,
                    output_tokens: 3,
                },
            },
        ]
    }

    fn config(
        mode: RecorderMode,
        root: &std::path::Path,
        on_divergence: Divergence,
    ) -> RecorderConfig {
        RecorderConfig {
            mode,
            name: Some("run".into()),
            root: root.to_path_buf(),
            on_divergence,
        }
    }

    fn prompt(text: &str) -> Vec<PromptBlock> {
        vec![PromptBlock {
            role: BlockRole::System,
            content: text.into(),
            cache_strategy: None,
        }]
    }

    fn tools() -> Vec<ToolDef> {
        vec![ToolDef {
            name: "Bash".into(),
            description: "run".into(),
            input_schema: serde_json::json!({}),
        }]
    }

    fn params(session: &str, step: u32) -> StreamParams {
        StreamParams {
            model: "test-model".into(),
            max_tokens: 100,
            thinking_mode: ThinkingMode::Off,
            fallback_model: None,
            cache_edits: vec![],
            origin: Some(CallOrigin {
                session_id: session.into(),
                turn: 0,
                step,
            }),
        }
    }

    async fn drain(stream: ModelStream) -> Vec<ModelEvent> {
        stream.filter_map(|e| async { e.ok() }).collect().await
    }

    #[tokio::test]
    async fn unconfigured_recorder_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(ScriptedModel::ok(script()), None, "test");
        let stream = recorder
            .stream(
                prompt("sys"),
                tools(),
                vec![],
                params("S1", 0),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(drain(stream).await.len(), 4);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn record_then_replay_reproduces_the_event_stream() {
        let root = tempfile::tempdir().unwrap();

        let recorder = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(config(
                RecorderMode::Record,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        let recorded = drain(
            recorder
                .stream(
                    prompt("sys"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;
        drop(recorder);

        // The replaying model must never reach the inner one.
        let replayer = RecorderModel::new(
            ScriptedModel::ok(vec![]),
            Some(config(
                RecorderMode::Replay,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        let replayed = drain(
            replayer
                .stream(
                    prompt("sys"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;

        assert_eq!(format!("{replayed:?}"), format!("{recorded:?}"));
        assert_eq!(replayed.len(), 4);
    }

    #[tokio::test]
    async fn token_boundaries_survive_a_record_replay_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(config(
                RecorderMode::Record,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        drain(
            recorder
                .stream(
                    prompt("sys"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;
        drop(recorder);

        let recording = reader::load(&root.path().join("run")).unwrap();
        let texts: Vec<&str> = recording.calls[0]
            .response
            .iter()
            .filter_map(|e| match e {
                ModelEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Hel", "lo", "!"], "deltas must not be merged");
    }

    #[tokio::test]
    async fn a_recorded_failure_replays_as_a_failure() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::overloaded(),
            Some(config(
                RecorderMode::Record,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        let err = recorder
            .stream(
                prompt("sys"),
                tools(),
                vec![],
                params("S1", 0),
                CancellationToken::new(),
            )
            .await
            .err()
            .expect("expected an error");
        assert!(matches!(err, ModelError::Overloaded));
        drop(recorder);

        let replayer = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(config(
                RecorderMode::Replay,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        let replayed = replayer
            .stream(
                prompt("sys"),
                tools(),
                vec![],
                params("S1", 0),
                CancellationToken::new(),
            )
            .await;
        assert!(
            matches!(replayed, Err(ModelError::Overloaded)),
            "a recorded overload must replay as an overload, not as a success"
        );
    }

    #[tokio::test]
    async fn divergence_names_the_field_that_moved() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(config(
                RecorderMode::Record,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        drain(
            recorder
                .stream(
                    prompt("original system prompt"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;
        drop(recorder);

        let replayer = RecorderModel::new(
            ScriptedModel::ok(vec![]),
            Some(config(
                RecorderMode::Replay,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        let err = replayer
            .stream(
                prompt("a different system prompt"),
                tools(),
                vec![],
                params("S1", 0),
                CancellationToken::new(),
            )
            .await
            .err()
            .expect("expected an error");
        let message = err.to_string();
        assert!(
            message.contains("system[0]"),
            "the report must point at the field, got: {message}"
        );
    }

    #[tokio::test]
    async fn warn_mode_plays_the_recording_despite_divergence() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(config(
                RecorderMode::Record,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        drain(
            recorder
                .stream(
                    prompt("original"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;
        drop(recorder);

        let replayer = RecorderModel::new(
            ScriptedModel::ok(vec![]),
            Some(config(RecorderMode::Replay, root.path(), Divergence::Warn)),
            "test",
        );
        let replayed = drain(
            replayer
                .stream(
                    prompt("changed"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(replayed.len(), 4);
    }

    #[tokio::test]
    async fn a_stream_dropped_mid_response_records_a_cancelled_outcome() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(config(
                RecorderMode::Record,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        {
            let mut stream = recorder
                .stream(
                    prompt("sys"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            let _first = stream.next().await;
        }
        drop(recorder);

        let recording = reader::load(&root.path().join("run")).unwrap();
        let end = recording.calls[0]
            .end
            .as_ref()
            .expect("call must be closed");
        assert_eq!(end.outcome, Outcome::Cancelled);
    }

    #[tokio::test]
    async fn the_request_is_on_disk_before_the_response_arrives() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(config(
                RecorderMode::Record,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        let _stream = recorder
            .stream(
                prompt("sys"),
                tools(),
                vec![],
                params("S1", 0),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let content = std::fs::read_to_string(root.path().join("run").join("calls.jsonl")).unwrap();
        assert!(
            content.contains(r#""type":"call""#),
            "the request must be durable before the stream is consumed"
        );
    }

    #[tokio::test]
    async fn unchanged_request_parts_are_stored_once() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(config(
                RecorderMode::Record,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        for step in 0..3 {
            drain(
                recorder
                    .stream(
                        prompt("stable system prompt"),
                        tools(),
                        vec![],
                        params("S1", step),
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap(),
            )
            .await;
        }
        drop(recorder);

        let recording = reader::load(&root.path().join("run")).unwrap();
        assert_eq!(recording.calls.len(), 3);
        // One blob for the single system block, one for the tool table.
        let blobs = std::fs::read_dir(root.path().join("run").join("blobs"))
            .unwrap()
            .count();
        assert_eq!(blobs, 2, "identical request parts must dedup across calls");
    }

    #[tokio::test]
    async fn each_session_records_into_its_own_directory() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(RecorderConfig {
                mode: RecorderMode::Record,
                name: None,
                root: root.path().to_path_buf(),
                on_divergence: Divergence::Strict,
            }),
            "test",
        );
        for session in ["parent", "subagent"] {
            drain(
                recorder
                    .stream(
                        prompt("sys"),
                        tools(),
                        vec![],
                        params(session, 0),
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap(),
            )
            .await;
        }
        drop(recorder);

        assert!(root.path().join("parent").join("calls.jsonl").exists());
        assert!(root.path().join("subagent").join("calls.jsonl").exists());
        assert_eq!(
            reader::load(&root.path().join("parent"))
                .unwrap()
                .calls
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn replaying_past_the_end_of_a_recording_says_so() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(config(
                RecorderMode::Record,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        drain(
            recorder
                .stream(
                    prompt("sys"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;
        drop(recorder);

        let replayer = RecorderModel::new(
            ScriptedModel::ok(vec![]),
            Some(config(
                RecorderMode::Replay,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        for _ in 0..1 {
            drain(
                replayer
                    .stream(
                        prompt("sys"),
                        tools(),
                        vec![],
                        params("S1", 0),
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap(),
            )
            .await;
        }
        let err = replayer
            .stream(
                prompt("sys"),
                tools(),
                vec![],
                params("S1", 1),
                CancellationToken::new(),
            )
            .await
            .err()
            .expect("expected an error");
        assert!(err.to_string().contains("asked for #2"), "{err}");
    }
}
