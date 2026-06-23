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
                // P1: Dedup consecutive identical tool calls within a turn.
                // TS parity: deduplicateToolCalls in query.ts.
                let dedup_key = format!("({name},{input})");
                if !seen_tool_calls.insert(dedup_key.clone()) {
                    // Duplicate tool call — skip execution, inject a synthetic
                    // tool result so the model doesn't get stuck waiting.
                    tracing::warn!(
                        tool = %name,
                        tool_use_id = %id,
                        "Skipping duplicate tool call"
                    );
                    session.push_message(ModelMessage {
                        role: MessageRole::User,
                        content: vec![ModelContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: "[Duplicate tool call skipped — identical call was already made this turn.]".to_string(),
                            is_error: Some(false),
                        }],
                    });
                    let _ = event_tx.send(AgentEvent::ToolResult {
                        id: id.clone(),
                        name: name.clone(),
                        content: "[Duplicate tool call skipped]".to_string(),
                        is_error: Some(false),
                        turn_id: turn_id.clone(),
                    });
                    continue;
                }
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
                session.push_message(ModelMessage {
                    role: MessageRole::Assistant,
                    content: vec![ModelContentBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    }],
                });
                let _ = event_tx.send(AgentEvent::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    turn_id: turn_id.clone(),
                });

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
                    // Non-concurrency-safe: drain current batch first, then execute alone
                    if !batch_futures.is_empty() {
                        // Drain the current concurrent batch
                        while let Some((idx, tid, result)) = batch_futures.next().await {
                            let is_err = result.is_err();
                            let (content, new_msgs) = match &result {
                                Ok((t, msgs)) => (t.clone(), msgs.clone()),
                                Err(e) => (e.clone(), None),
                            };
                            if is_err {
                                batch_abort.cancel();
                            }
                            // Find tool name
                            let tname = queued_tools.get(idx).map(|t| t.name.clone()).unwrap_or_default();
                            push_result_to_session(session, event_tx, &turn_id, idx, &tid, &tname, &content, is_err);
                            // Inject new_messages after tool result
                            if let Some(msgs) = new_msgs {
                                inject_new_messages(session, event_tx, &turn_id, &msgs);
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
                    push_result_to_session(session, event_tx, &turn_id, tool_index, &id, &name, &content, is_err);
                    if let Some(msgs) = new_msgs {
                        inject_new_messages(session, event_tx, &turn_id, &msgs);
                    }
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

    // Drain any remaining in-flight concurrent tools
    while let Some((idx, tid, result)) = batch_futures.next().await {
        let is_err = result.is_err();
        let (content, new_msgs) = match &result {
            Ok((t, msgs)) => (t.clone(), msgs.clone()),
            Err(e) => (e.clone(), None),
        };
        let tname = queued_tools.get(idx).map(|t| t.name.clone()).unwrap_or_default();
        push_result_to_session(session, event_tx, &turn_id, idx, &tid, &tname, &content, is_err);
        if let Some(msgs) = new_msgs {
            inject_new_messages(session, event_tx, &turn_id, &msgs);
        }
    }

    Ok(StreamResult {
        stop_reason,
        usage,
        has_tool_uses,
        tool_calls,
    })
}

#[allow(clippy::too_many_arguments)]
fn push_result_to_session(
    session: &mut session::session::SessionManager,
    event_tx: &EventSender,
    turn_id: &str,
    _idx: usize,
    tool_use_id: &str,
    tool_name: &str,
    content: &str,
    is_error: bool,
) {
    session.push_message(ModelMessage {
        role: MessageRole::User,
        content: vec![ModelContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: content.to_string(),
            is_error: Some(is_error),
        }],
    });
    let _ = event_tx.send(AgentEvent::ToolResult {
        id: tool_use_id.to_string(),
        name: tool_name.to_string(),
        content: content.to_string(),
        is_error: Some(is_error),
        turn_id: turn_id.to_string(),
    });
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
