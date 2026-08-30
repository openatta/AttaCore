//! A `Model` whose every answer is written down in the test, not recorded from
//! a provider.
//!
//! # Why not cassettes
//!
//! The recorder already gives deterministic replay, and reaching for it first
//! is the obvious move. It does not work as a *committed* regression net, for
//! two independent reasons:
//!
//! 1. `tests/fixtures/cassettes/` is gitignored on purpose. The rationale is
//!    in `.gitignore` itself: a recorded request bakes in the exact model
//!    name, prompt text and tool table, so a committed cassette goes stale the
//!    moment any of those change. Replay compares blob *ids* — content
//!    addresses — so "goes stale" means "every case fails".
//! 2. That staleness is not hypothetical for this refactor. The tickets this
//!    net exists to protect (prompt block naming, the registration contract,
//!    the assembly hook) all change prompt assembly deliberately. A cassette
//!    net would fail on every one of them for the intended reason, which
//!    trains people to regenerate the goldens without reading the diff —
//!    the failure mode that makes a regression net worse than none.
//!
//! So the model is scripted instead. The script is a few lines of Rust, it
//! commits, it costs nothing to run, and it is indifferent to prompt wording —
//! which is what lets it stay sensitive to the thing actually under test: the
//! turn loop's decisions.
//!
//! It also reaches cases a recording cannot. There is no way to ask a real
//! provider for a 529 on the third call, and the existing cassettes contain no
//! error outcomes at all, so overload/fallback, prompt-too-long recovery and
//! `max_tokens` continuation are only reproducible by scripting them.
//!
//! # What it does not cover
//!
//! Nothing here checks the *request*. Whether the assembled prompt is correct
//! is what the cassette machinery and `run_rerun.sh` are for; this asserts
//! what the engine does with a given sequence of answers.

use base::interface::model::{
    Model, ModelError, ModelEvent, ModelMessage, ModelStream, StreamParams, ToolDef, Usage,
};
use base::interface::prompt::PromptBlock;
use base::provider::ApiType;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// One scripted answer to one `Model::stream` call.
#[derive(Debug)]
pub enum Reply {
    /// Plain text, then `end_turn`.
    Text(&'static str),
    /// Text split across several deltas — token boundaries are behavior when
    /// what you are testing is a consumer of the stream.
    TextChunks(&'static [&'static str]),
    /// One tool call, then `tool_use`.
    Tool {
        id: &'static str,
        name: &'static str,
        input: Value,
    },
    /// Several tool calls in a single assistant message. Whether they run
    /// concurrently is the engine's decision (`Tool::is_concurrency_safe`),
    /// which is exactly what the trace records.
    Tools(Vec<(&'static str, &'static str, Value)>),
    /// Text, then a stop reason of the script's choosing — `max_tokens` is the
    /// interesting one, since the loop treats it as "keep going".
    TextWithStop {
        text: &'static str,
        stop_reason: &'static str,
    },
    /// The call fails before any event is produced.
    Fail(ModelError),
}

impl Reply {
    fn tool(id: &'static str, name: &'static str, input: Value) -> Vec<ModelEvent> {
        vec![
            ModelEvent::ToolUse {
                id: id.into(),
                name: name.into(),
                input,
            },
            ModelEvent::EndTurn {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            },
        ]
    }

    fn into_events(self) -> Result<Vec<ModelEvent>, ModelError> {
        Ok(match self {
            Reply::Text(t) => vec![
                ModelEvent::TextDelta { text: t.into() },
                ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Usage::default(),
                },
            ],
            Reply::TextChunks(parts) => {
                let mut v: Vec<ModelEvent> = parts
                    .iter()
                    .map(|p| ModelEvent::TextDelta {
                        text: (*p).to_string(),
                    })
                    .collect();
                v.push(ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Usage::default(),
                });
                v
            }
            Reply::Tool { id, name, input } => Self::tool(id, name, input),
            Reply::Tools(calls) => {
                let mut v: Vec<ModelEvent> = calls
                    .into_iter()
                    .map(|(id, name, input)| ModelEvent::ToolUse {
                        id: id.into(),
                        name: name.into(),
                        input,
                    })
                    .collect();
                v.push(ModelEvent::EndTurn {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                });
                v
            }
            Reply::TextWithStop { text, stop_reason } => vec![
                ModelEvent::TextDelta { text: text.into() },
                ModelEvent::EndTurn {
                    stop_reason: stop_reason.into(),
                    usage: Usage::default(),
                },
            ],
            Reply::Fail(e) => return Err(e),
        })
    }
}

/// Answers `Model::stream` from a fixed list, in order.
///
/// Running past the end is a panic rather than a default answer: a case that
/// makes more calls than its script accounts for has changed behavior, and
/// that is the whole thing being watched. Silently returning "" would let the
/// loop spin and the test still pass.
pub struct ScriptedModel {
    replies: Mutex<std::collections::VecDeque<Reply>>,
    /// Every `(model, max_tokens)` the engine asked for, in order. The
    /// fallback-model switch is visible only here — it changes the request,
    /// not the response.
    calls: Mutex<Vec<(String, u32)>>,
}

impl ScriptedModel {
    pub fn new(replies: Vec<Reply>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            calls: Mutex::new(Vec::new()),
        })
    }

    /// The `(model_name, max_tokens)` of each call the engine made.
    pub fn calls(&self) -> Vec<(String, u32)> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Scripted replies never consumed. A case that ends with leftovers took a
    /// shorter path than it was written for, which is as much a regression as
    /// taking a longer one.
    pub fn unconsumed(&self) -> usize {
        self.replies.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

#[async_trait::async_trait]
impl Model for ScriptedModel {
    fn api_type(&self) -> ApiType {
        ApiType::Anthropic
    }

    async fn stream(
        &self,
        _prompt_blocks: Vec<PromptBlock>,
        _tools: Vec<ToolDef>,
        _messages: Vec<ModelMessage>,
        params: StreamParams,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((params.model.clone(), params.max_tokens));

        let reply = self
            .replies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or_else(|| {
                panic!(
                    "the engine made more model calls than the script has replies \
                     (call #{}, model={}). The loop took a longer path than this case \
                     was written for.",
                    self.calls.lock().unwrap_or_else(|e| e.into_inner()).len(),
                    params.model
                )
            });

        let events = reply.into_events()?;
        Ok(Box::new(futures::stream::iter(
            events.into_iter().map(Ok).collect::<Vec<_>>(),
        )))
    }
}
