//! Streaming executor — consume model stream, execute tools concurrently.
//!
//! v2: Streaming tool execution (TS parity: `StreamingToolExecutor.ts`).
//! Concurrency-safe tools start executing during stream consumption — they run
//! while the model continues streaming text and more tool_use blocks. Sequential
//! (non-concurrency-safe) tools still wait until their batch completes.
//! Sibling abort on error (any tool error cancels all siblings in the same batch).

use crate::agent::EventSender;
use base::interface::event::AgentEvent;
use base::interface::model::{
    MessageRole, ModelContentBlock, ModelEvent, ModelMessage, ModelStream, Usage,
};
use std::collections::HashSet;
use std::future::Future;
use tokio_util::sync::CancellationToken;

/// Result of processing a single model stream.
#[derive(Debug, Clone)]
pub struct StreamResult {
    pub stop_reason: String,
    pub usage: Usage,
    pub has_tool_uses: bool,
    pub tool_calls: u32,
}

/// A tool invocation queued for execution.
#[derive(Debug)]
struct QueuedTool {
    #[allow(dead_code)]
    id: String,
    name: String,
    #[allow(dead_code)]
    input: serde_json::Value,
    #[allow(dead_code)]
    concurrency_safe: bool,
}

/// Process a model stream with streaming tool execution.
///
/// v2: Concurrency-safe tools are spawned into `FuturesUnordered` immediately
/// when the `ToolUse` event arrives, while the stream is still being consumed.
/// This is the TS `StreamingToolExecutor` pattern:
///   - ToolUse arrives → spawn execution into background batch
///   - Stream continues → more text/tool_use events arrive
///   - Stream ends → await remaining tool futures, then execute sequential tools
///
/// Non-concurrency-safe tools still wait: they execute after all prior
/// concurrent-safe tools in their batch have completed.
///
/// **Session message grouping**: every `ToolUse` block emitted since the last
/// flush is buffered (not pushed to the session immediately) until its whole
/// batch resolves, then flushed as exactly one assistant message (all the
/// `ToolUse` blocks) immediately followed by exactly one user message (all the
/// matching `ToolResult` blocks). The Anthropic-compatible API rejects a
/// request where an assistant `tool_use` isn't immediately followed by its
/// `tool_result` — pushing one session message per tool call (the original
/// approach) broke that invariant as soon as a single response mixed a
/// concurrency-safe tool with a non-concurrency-safe one, since the
/// non-concurrency-safe tool's `ToolUse` message landed *before* the
/// concurrency-safe tool's `ToolResult` message in session order. Concurrent
/// execution timing is unaffected — only when results get written to the
/// session changed.
pub async fn execute_stream<F, Fut, G>(
    stream: ModelStream,
    session: &mut session::session::SessionManager,
    event_tx: &EventSender,
    turn_id: String,
    execute_tool: F,
    is_concurrency_safe: G,
    cancel: CancellationToken,
) -> Result<StreamResult, crate::turn::TurnError>
where
    F: Fn(String, serde_json::Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(String, Option<Vec<serde_json::Value>>), String>> + Send,
    G: Fn(&str, &serde_json::Value) -> bool + Send + Sync,
{
    use futures::StreamExt;
    tokio::pin!(stream);
    let mut stop_reason = String::new();
    let mut usage = Usage::default();
    let mut has_tool_uses = false;
    let mut tool_calls: u32 = 0;
    let mut pending_text = String::new();
    let mut queued_tools: Vec<QueuedTool> = Vec::new();
    let mut tool_index: usize = 0;
    // P1: Dedup consecutive identical tool calls within a turn.
    // Key = "(tool_name, serialized_input)". TS parity: deduplicateToolCalls.
    let mut seen_tool_calls: HashSet<String> = HashSet::new();

    // Streaming execution: concurrent-safe tools run during stream consumption.
    // Each batch of consecutive concurrency-safe tools gets its own FuturesUnordered.
    // A non-concurrency-safe tool creates a barrier — all prior batches must drain first.
    let mut batch_abort = CancellationToken::new();
    let mut batch_futures = futures::stream::FuturesUnordered::new();

    // Not-yet-flushed tool_use/tool_result blocks for the current batch — see
    // the "Session message grouping" doc above.
    let mut pending_use_blocks: Vec<ModelContentBlock> = Vec::new();
    let mut pending_result_blocks: Vec<ModelContentBlock> = Vec::new();
    let mut pending_new_msgs: Vec<Vec<serde_json::Value>> = Vec::new();

    // Phase 1: Consume stream, spawn concurrent-safe tools immediately
    while let Some(event) = stream.next().await {
        if cancel.is_cancelled() {
            break;
        }
        match event.map_err(|e| crate::turn::TurnError::Model(e.to_string()))? {
            ModelEvent::TextDelta { text } => {
                pending_text.push_str(&text);
                let _ = event_tx.send(AgentEvent::TextDelta { text, turn_id: turn_id.clone() });
            }
            ModelEvent::ToolUse { id, name, input } => {
                has_tool_uses = true;
                // Flush pending text to session before tool block
                if !pending_text.is_empty() {
                    session.push_message(ModelMessage {
                        role: MessageRole::Assistant,
                        content: vec![ModelContentBlock::Text {
                            text: std::mem::take(&mut pending_text),
                        }],
                    });
                }
                // Buffer every ToolUse the model actually emitted — duplicate or
                // not — it gets flushed together with its ToolResult once the
                // batch resolves (see module-level doc comment).
                pending_use_blocks.push(ModelContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
                let _ = event_tx.send(AgentEvent::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    turn_id: turn_id.clone(),
                });

                // P1: Dedup consecutive identical tool calls within a turn.
                // TS parity: deduplicateToolCalls in query.ts.
                let dedup_key = format!("({name},{input})");
                if !seen_tool_calls.insert(dedup_key.clone()) {
                    // Duplicate tool call — skip execution, buffer a synthetic
                    // tool result (paired with the ToolUse block just buffered
                    // above) so the model doesn't get stuck waiting.
                    tracing::warn!(
                        tool = %name,
                        tool_use_id = %id,
                        "Skipping duplicate tool call"
                    );
                    buffer_result(
                        event_tx,
                        &turn_id,
                        &mut pending_result_blocks,
                        &id,
                        &name,
                        "[Duplicate tool call skipped — identical call was already made this turn.]",
                        false,
                    );
                    continue;
                }

                let safe = is_concurrency_safe(&name, &input);
                queued_tools.push(QueuedTool {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    concurrency_safe: safe,
                });

                if safe {
                    // Spawn immediately into the current concurrent batch
                    let exec = execute_tool(name.clone(), input);
                    let abort = batch_abort.clone();
                    let idx = tool_index;
                    batch_futures.push(async move {
                        let result = tokio::select! {
                            _ = abort.cancelled() => Err("cancelled by sibling error".to_string()),
                            r = exec => r,
                        };
                        (idx, id, result)
                    });
                } else {
                    // Non-concurrency-safe: drain current batch first (results
                    // buffered, not pushed to session yet), then execute alone,
                    // then flush the whole batch as one paired message pair.
                    if !batch_futures.is_empty() {
                        while let Some((idx, tid, result)) = batch_futures.next().await {
                            let is_err = result.is_err();
                            let (content, new_msgs) = match &result {
                                Ok((t, msgs)) => (t.clone(), msgs.clone()),
                                Err(e) => (e.clone(), None),
                            };
                            if is_err {
                                batch_abort.cancel();
                            }
                            let tname = queued_tools.get(idx).map(|t| t.name.clone()).unwrap_or_default();
                            buffer_result(event_tx, &turn_id, &mut pending_result_blocks, &tid, &tname, &content, is_err);
                            if let Some(msgs) = new_msgs {
                                pending_new_msgs.push(msgs);
                            }
                        }
                    }
                    // Execute the sequential tool
                    let result = execute_tool(name.clone(), input).await;
                    let is_err = result.is_err();
                    let (content, new_msgs) = match &result {
                        Ok((t, msgs)) => (t.clone(), msgs.clone()),
                        Err(e) => (e.clone(), None),
                    };
                    buffer_result(event_tx, &turn_id, &mut pending_result_blocks, &id, &name, &content, is_err);
                    if let Some(msgs) = new_msgs {
                        pending_new_msgs.push(msgs);
                    }

                    flush_tool_batch(
                        session,
                        event_tx,
                        &turn_id,
                        &mut pending_use_blocks,
                        &mut pending_result_blocks,
                        &mut pending_new_msgs,
                    );

                    // Start a new batch for subsequent concurrent-safe tools
                    batch_abort = CancellationToken::new();
                    batch_futures = futures::stream::FuturesUnordered::new();
                }

                tool_calls += 1;
                tool_index += 1;
            }
            ModelEvent::EndTurn {
                stop_reason: sr,
                usage: u,
            } => {
                stop_reason = sr;
                usage = u;
            }
            _ => {}
        }
    }

    // Flush remaining pending text
    if !pending_text.is_empty() {
        session.push_message(ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::Text {
                text: std::mem::take(&mut pending_text),
            }],
        });
    }

    // Drain any remaining in-flight concurrent tools, then flush the final batch.
    while let Some((idx, tid, result)) = batch_futures.next().await {
        let is_err = result.is_err();
        let (content, new_msgs) = match &result {
            Ok((t, msgs)) => (t.clone(), msgs.clone()),
            Err(e) => (e.clone(), None),
        };
        let tname = queued_tools.get(idx).map(|t| t.name.clone()).unwrap_or_default();
        buffer_result(event_tx, &turn_id, &mut pending_result_blocks, &tid, &tname, &content, is_err);
        if let Some(msgs) = new_msgs {
            pending_new_msgs.push(msgs);
        }
    }
    flush_tool_batch(
        session,
        event_tx,
        &turn_id,
        &mut pending_use_blocks,
        &mut pending_result_blocks,
        &mut pending_new_msgs,
    );

    Ok(StreamResult {
        stop_reason,
        usage,
        has_tool_uses,
        tool_calls,
    })
}

/// Buffer a tool result (not yet pushed to the session — see
/// `flush_tool_batch`) and emit the live `AgentEvent::ToolResult` for UI
/// feedback immediately (that's independent of session message structure).
fn buffer_result(
    event_tx: &EventSender,
    turn_id: &str,
    pending_result_blocks: &mut Vec<ModelContentBlock>,
    tool_use_id: &str,
    tool_name: &str,
    content: &str,
    is_error: bool,
) {
    pending_result_blocks.push(ModelContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: content.to_string(),
        is_error: Some(is_error),
    });
    let _ = event_tx.send(AgentEvent::ToolResult {
        id: tool_use_id.to_string(),
        name: tool_name.to_string(),
        content: content.to_string(),
        is_error: Some(is_error),
        turn_id: turn_id.to_string(),
    });
}

/// Flush the current batch: one assistant message with every buffered
/// `ToolUse` block, immediately followed by one user message with every
/// buffered `ToolResult` block (same ids, order doesn't need to match — the
/// API only requires the *set* to line up), then any deferred `new_messages`
/// in resolution order. No-op if nothing is pending.
fn flush_tool_batch(
    session: &mut session::session::SessionManager,
    event_tx: &EventSender,
    turn_id: &str,
    pending_use_blocks: &mut Vec<ModelContentBlock>,
    pending_result_blocks: &mut Vec<ModelContentBlock>,
    pending_new_msgs: &mut Vec<Vec<serde_json::Value>>,
) {
    if pending_use_blocks.is_empty() {
        return;
    }
    session.push_message(ModelMessage {
        role: MessageRole::Assistant,
        content: std::mem::take(pending_use_blocks),
    });
    session.push_message(ModelMessage {
        role: MessageRole::User,
        content: std::mem::take(pending_result_blocks),
    });
    for msgs in pending_new_msgs.drain(..) {
        inject_new_messages(session, event_tx, turn_id, &msgs);
    }
}

/// Inject new messages into the session after a tool result.
/// TS parity: Some tools (e.g. SkillTool) return new_messages that should be
/// injected as user messages into the conversation. The model sees these as new
/// instructions rather than as tool output.
fn inject_new_messages(
    session: &mut session::session::SessionManager,
    event_tx: &EventSender,
    turn_id: &str,
    messages: &[serde_json::Value],
) {
    for msg in messages {
        let role_str = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        let content_str = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let role = match role_str {
            "assistant" => MessageRole::Assistant,
            _ => MessageRole::User,
        };
        session.push_message(ModelMessage {
            role,
            content: vec![ModelContentBlock::Text {
                text: content_str.to_string(),
            }],
        });
        let _ = event_tx.send(AgentEvent::TextDelta {
            text: format!("[injected: {content_str}]"),
            turn_id: turn_id.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::{ModelError, ModelEvent, Usage};

    /// Regression for the bug that produced a real API 400 during a live
    /// recording run: `unexpected tool_use_id found in tool_result blocks —
    /// Each tool_result block must have a corresponding tool_use block in the
    /// previous message`. Root cause: when the model emits the same
    /// (name, input) tool call twice in one response, the duplicate-skip
    /// branch pushed a synthetic `ToolResult` for the duplicate's
    /// `tool_use_id` but never pushed a matching `ToolUse` block (that push
    /// happened *after* the dedup check, which `continue`d past it) — so the
    /// session ended up with a `tool_result` whose `tool_use_id` had no
    /// preceding `tool_use`, which every real provider rejects on the next
    /// request. Every `ToolUse` the model emits — duplicate or not — must
    /// now be recorded before any matching `ToolResult`.
    #[tokio::test]
    async fn duplicate_tool_call_still_gets_a_paired_tool_use_block() {
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::ToolUse {
                id: "call_1".into(),
                name: "Write".into(),
                input: serde_json::json!({"path": "a.txt"}),
            }),
            Ok(ModelEvent::ToolUse {
                id: "call_2".into(),
                name: "Write".into(),
                input: serde_json::json!({"path": "a.txt"}), // identical → duplicate of call_1
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            }),
        ];
        let stream: base::interface::model::ModelStream =
            Box::new(futures::stream::iter(events));

        let mut session = session::session::SessionManager::in_memory(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let result = execute_stream(
            stream,
            &mut session,
            &tx,
            "test-turn".into(),
            |_name, _input| async move { Ok(("ok".to_string(), None)) },
            |_name, _input| true,
            CancellationToken::new(),
        )
        .await
        .expect("execute_stream should succeed");

        assert!(result.has_tool_uses);
        // Only the real (non-duplicate) execution counts toward tool_calls —
        // the duplicate is skipped before that counter increments. That's
        // fine; the point of this test is the message-pairing invariant below.
        assert_eq!(result.tool_calls, 1);

        assert_adjacent_tool_pairing(session.messages());

        let mut seen_tool_use_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for msg in session.messages() {
            for block in &msg.content {
                if let ModelContentBlock::ToolUse { id, .. } = block {
                    seen_tool_use_ids.insert(id.clone());
                }
            }
        }
        assert!(seen_tool_use_ids.contains("call_1"));
        assert!(seen_tool_use_ids.contains("call_2"));
    }

    /// Regression for the real API 400 hit during a live recording run:
    /// `tool_use ids were found without tool_result blocks immediately
    /// after`. One response emits a concurrency-safe tool (Glob) then a
    /// non-concurrency-safe one (Bash) — the mix that forces the "drain
    /// batch then execute sequential" path. Before the grouping fix, each
    /// tool_use/tool_result pair was its own session message, so the second
    /// tool's ToolUse message landed before the first tool's ToolResult
    /// message — exactly what the API rejects.
    #[tokio::test]
    async fn mixed_concurrent_and_sequential_tool_calls_keep_strict_adjacency() {
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::ToolUse {
                id: "glob_1".into(),
                name: "Glob".into(),
                input: serde_json::json!({"pattern": "*.c"}),
            }),
            Ok(ModelEvent::ToolUse {
                id: "bash_1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "make build"}),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            }),
        ];
        let stream: base::interface::model::ModelStream =
            Box::new(futures::stream::iter(events));

        let mut session = session::session::SessionManager::in_memory(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        execute_stream(
            stream,
            &mut session,
            &tx,
            "test-turn".into(),
            |_name, _input| async move { Ok(("ok".to_string(), None)) },
            |name, _input| name == "Glob", // Glob concurrency-safe, Bash is not
            CancellationToken::new(),
        )
        .await
        .expect("execute_stream should succeed");

        assert_adjacent_tool_pairing(session.messages());
        // Exactly one assistant/user pair, both tool_use ids grouped together.
        assert_eq!(session.messages().len(), 2);
    }

    /// All-concurrency-safe batch (no sequential barrier at all): both tools
    /// only get flushed at the final drain, after the stream ends.
    #[tokio::test]
    async fn all_concurrent_batch_flushes_together_at_stream_end() {
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::ToolUse {
                id: "glob_1".into(),
                name: "Glob".into(),
                input: serde_json::json!({"pattern": "*.c"}),
            }),
            Ok(ModelEvent::ToolUse {
                id: "grep_1".into(),
                name: "Grep".into(),
                input: serde_json::json!({"pattern": "main"}),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            }),
        ];
        let stream: base::interface::model::ModelStream =
            Box::new(futures::stream::iter(events));

        let mut session = session::session::SessionManager::in_memory(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        execute_stream(
            stream,
            &mut session,
            &tx,
            "test-turn".into(),
            |_name, _input| async move { Ok(("ok".to_string(), None)) },
            |_name, _input| true,
            CancellationToken::new(),
        )
        .await
        .expect("execute_stream should succeed");

        assert_adjacent_tool_pairing(session.messages());
        assert_eq!(session.messages().len(), 2);
    }

    /// Two separate sequential tools back-to-back: two independent
    /// assistant/user pairs, each internally consistent.
    #[tokio::test]
    async fn two_sequential_tool_calls_produce_two_paired_batches() {
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::ToolUse {
                id: "bash_1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "make build"}),
            }),
            Ok(ModelEvent::ToolUse {
                id: "bash_2".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "make run"}),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            }),
        ];
        let stream: base::interface::model::ModelStream =
            Box::new(futures::stream::iter(events));

        let mut session = session::session::SessionManager::in_memory(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        execute_stream(
            stream,
            &mut session,
            &tx,
            "test-turn".into(),
            |_name, _input| async move { Ok(("ok".to_string(), None)) },
            |_name, _input| false,
            CancellationToken::new(),
        )
        .await
        .expect("execute_stream should succeed");

        assert_adjacent_tool_pairing(session.messages());
        assert_eq!(session.messages().len(), 4);
    }

    /// Every assistant message with ToolUse block(s) must be immediately
    /// followed by a user message whose ToolResult ids are exactly that set
    /// — the real constraint the Anthropic-compatible API enforces.
    fn assert_adjacent_tool_pairing(messages: &[ModelMessage]) {
        for (i, msg) in messages.iter().enumerate() {
            if msg.role != MessageRole::Assistant {
                continue;
            }
            let use_ids: std::collections::HashSet<String> = msg
                .content
                .iter()
                .filter_map(|b| match b {
                    ModelContentBlock::ToolUse { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect();
            if use_ids.is_empty() {
                continue;
            }
            let next = messages.get(i + 1).unwrap_or_else(|| {
                panic!("assistant message {i} has ToolUse {use_ids:?} but there is no next message at all: {messages:#?}")
            });
            let result_ids: std::collections::HashSet<String> = next
                .content
                .iter()
                .filter_map(|b| match b {
                    ModelContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                use_ids, result_ids,
                "message {i} (assistant) has ToolUse {use_ids:?} but the immediately following \
                 message {} has ToolResult {result_ids:?} — not an exact match: {messages:#?}",
                i + 1
            );
        }
    }
}
