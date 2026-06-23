//! Compactor — context compression when token budget exceeded.
//! TS parity: 5 strategies in priority order with automatic fallback.
//! - Snip: drop oldest API rounds
//! - MicroCompact: clear old tool result content in-place
//! - CollapseContext: fold old rounds into summary, keep recent rounds
//! - FullCompact: LLM summary of all old messages
//! - SessionMemory: extract memories to persistent store during compact

use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage};
use async_trait::async_trait;
use std::sync::Arc;

use crate::grouping::{estimate_tokens, group_by_api_round, ApiRound};

// ── Strategy types ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactStrategy {
    /// Drop oldest rounds, keep recent N rounds (not messages).
    Snip,
    /// Clear tool results older than K rounds, replacing content with a placeholder.
    MicroCompact,
    /// Fold old rounds into a summary message, keep recent rounds intact.
    CollapseContext,
    /// LLM-driven full summary of the conversation.
    FullCompact,
    /// Extract durable memories during compaction (writes to MemoryStore).
    SessionMemory,
}

#[derive(Debug, Clone)]
pub struct CompactResult {
    pub strategy: CompactStrategy,
    pub messages_before: usize,
    pub messages_after: usize,
    pub tokens_before: usize,
    pub tokens_after: usize,
    /// Snip projection — metadata about dropped rounds when using Snip strategy.
    /// Emitted so the UI can show what was preserved vs dropped.
    pub projection: Option<SnipProjection>,
}

/// Metadata about rounds dropped by the Snip compaction strategy.
/// Preserved for history/UI purposes — lets consumers display
/// compaction impact without inspecting every message.
#[derive(Debug, Clone)]
pub struct SnipProjection {
    /// Number of API rounds that were entirely dropped.
    pub dropped_rounds: usize,
    /// Number of messages (across all dropped rounds) that were removed.
    pub dropped_messages: usize,
    /// Estimated tokens saved by dropping these rounds.
    pub estimated_tokens_saved: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum CompactError {
    #[error("all strategies exhausted")]
    Exhausted,
    #[error("{0}")]
    Internal(String),
}

// ── Compactor trait ──

#[async_trait]
pub trait Compactor: Send + Sync {
    /// Compact messages to fit within `max_tokens`, keeping at least `keep_rounds` recent rounds.
    /// Returns the compacted message list and metadata.
    async fn compact(
        &self,
        messages: Vec<ModelMessage>,
        max_tokens: usize,
        keep_rounds: usize,
    ) -> Result<(Vec<ModelMessage>, CompactResult), CompactError>;
}

// ── DefaultCompactor: Snip + MicroCompact + CollapseContext ──

pub struct DefaultCompactor;

impl DefaultCompactor {
    fn tokens(msgs: &[ModelMessage]) -> usize {
        estimate_tokens(msgs)
    }

    /// Snip: drop oldest API rounds until within token budget (keep at least 1 round).
    /// Returns (messages, strategy, dropped_rounds, dropped_messages, tokens_saved) so the
    /// caller can build a SnipProjection for telemetry/UI.
    fn snip(&self, messages: Vec<ModelMessage>, max_tokens: usize, keep_rounds: usize)
        -> (Vec<ModelMessage>, CompactStrategy, usize, usize, usize)
    {
        let rounds = group_by_api_round(&messages);
        let total_rounds = rounds.len();
        let total_messages = messages.len();
        let tokens_before = Self::tokens(&messages);
        let keep = keep_rounds.min(rounds.len()).max(1);
        let mut result: Vec<ModelMessage> = rounds[rounds.len() - keep..]
            .iter()
            .flat_map(|r| r.messages.clone())
            .collect();
        let mut tokens = Self::tokens(&result);
        // If still over budget, drop more rounds (but never below 1)
        let mut drop = rounds.len().saturating_sub(keep);
        while tokens > max_tokens && drop > 0 && rounds.len().saturating_sub(keep + drop) >= 1 {
            drop -= 1;
            let start = rounds.len().saturating_sub(keep + drop);
            result = rounds[start..]
                .iter()
                .flat_map(|r| r.messages.clone())
                .collect();
            tokens = Self::tokens(&result);
            if tokens <= max_tokens {
                break;
            }
        }
        let kept_rounds = keep + drop;
        let dropped_rounds = total_rounds.saturating_sub(kept_rounds);
        let dropped_messages = total_messages.saturating_sub(result.len());
        let tokens_saved = tokens_before.saturating_sub(tokens);
        (result, CompactStrategy::Snip, dropped_rounds, dropped_messages, tokens_saved)
    }

    /// MicroCompact: replace old tool results with a placeholder (TS parity).
    pub fn micro_compact(&self, messages: Vec<ModelMessage>, keep_rounds: usize) -> (Vec<ModelMessage>, CompactStrategy) {
        let rounds = group_by_api_round(&messages);
        if rounds.len() <= keep_rounds {
            return (messages, CompactStrategy::MicroCompact);
        }
        let keep_start = rounds.len() - keep_rounds;
        let to_compact: Vec<&ApiRound> = rounds[..keep_start].iter().collect();
        let to_keep: Vec<&ApiRound> = rounds[keep_start..].iter().collect();

        let mut result: Vec<ModelMessage> = Vec::new();
        let mut last_tool_name: Option<String> = None;
        for round in &to_compact {
            for msg in &round.messages {
                // Track tool name from ToolUse blocks (for whitelist check)
                for block in &msg.content {
                    if let ModelContentBlock::ToolUse { name, .. } = block {
                        last_tool_name = Some(name.clone());
                    }
                }
                let mut msg = msg.clone();
                for block in &mut msg.content {
                    if matches!(block, ModelContentBlock::ToolResult { .. }) {
                        let is_compactable = last_tool_name
                            .as_deref()
                            .map(|n| COMPACTABLE_TOOLS.contains(&n))
                            .unwrap_or(false);
                        if is_compactable {
                            *block = ModelContentBlock::ToolResult {
                                tool_use_id: String::new(),
                                content: "[Old tool result content cleared]".to_string(),
                                is_error: Some(false),
                            };
                        }
                    }
                }
                result.push(msg);
            }
        }
        for round in &to_keep {
            result.extend(round.messages.clone());
        }
        (result, CompactStrategy::MicroCompact)
    }

    /// CollapseContext: fold old rounds into a single summary message (no LLM).
    /// Keeps the structure but truncates text to a fixed summary length.
    fn collapse_context(&self, messages: Vec<ModelMessage>, keep_rounds: usize) -> (Vec<ModelMessage>, CompactStrategy) {
        let rounds = group_by_api_round(&messages);
        if rounds.len() <= keep_rounds {
            return (messages, CompactStrategy::CollapseContext);
        }
        let keep_start = rounds.len() - keep_rounds;
        let to_collapse = &rounds[..keep_start];
        let to_keep = &rounds[keep_start..];

        // Build a synthetic summary of collapsed rounds
        let mut summary_parts: Vec<String> = Vec::new();
        for round in to_collapse {
            for msg in &round.messages {
                for block in &msg.content {
                    if let ModelContentBlock::Text { text } = block {
                        let truncated = if text.len() > 200 {
                            format!("{}...", &text[..200])
                        } else {
                            text.clone()
                        };
                        summary_parts.push(truncated);
                    } else if let ModelContentBlock::ToolUse { name, .. } = block {
                        summary_parts.push(format!("[tool: {name}]"));
                    }
                }
            }
        }
        let summary = summary_parts.join(" | ");

        let boundary = ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: "[Earlier conversation collapsed for context]".to_string(),
            }],
        };
        let summary_msg = ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: format!("[Summary of earlier conversation]: {summary}"),
            }],
        };

        let mut result = vec![boundary, summary_msg];
        for round in to_keep {
            result.extend(round.messages.clone());
        }
        (result, CompactStrategy::CollapseContext)
    }
}

#[async_trait]
impl Compactor for DefaultCompactor {
    async fn compact(
        &self,
        messages: Vec<ModelMessage>,
        max_tokens: usize,
        keep_rounds: usize,
    ) -> Result<(Vec<ModelMessage>, CompactResult), CompactError> {
        let tokens_before = Self::tokens(&messages);
        let count_before = messages.len();
        if tokens_before <= max_tokens {
            return Ok((
                messages,
                CompactResult {
                    strategy: CompactStrategy::Snip,
                    messages_before: count_before,
                    messages_after: count_before,
                    tokens_before,
                    tokens_after: tokens_before,
                    projection: None,
                },
            ));
        }

        // Strategy progression with fallback
        let (result, strategy, dropped_rounds, dropped_messages, tokens_saved) = {
            // 1. Try Snip (keep configured rounds)
            let (r, s, dr, dm, ts) = self.snip(messages.clone(), max_tokens, keep_rounds);
            if Self::tokens(&r) <= max_tokens {
                (r, s, dr, dm, ts)
            } else {
                // 2. Try MicroCompact (clear old tool results)
                let (r, s) = self.micro_compact(messages.clone(), keep_rounds);
                if Self::tokens(&r) <= max_tokens {
                    (r, s, 0, 0, 0)
                } else {
                    // 3. CollapseContext as fallback
                    let (r, s) = self.collapse_context(messages.clone(), keep_rounds);
                    (r, s, 0, 0, 0)
                }
            }
        };

        let tokens_after = Self::tokens(&result);
        let messages_after = result.len();
        let projection = if strategy == CompactStrategy::Snip && dropped_rounds > 0 {
            Some(SnipProjection {
                dropped_rounds,
                dropped_messages,
                estimated_tokens_saved: tokens_saved,
            })
        } else {
            None
        };
        Ok((
            result,
            CompactResult {
                strategy,
                messages_before: count_before,
                messages_after,
                tokens_before,
                tokens_after,
                projection,
            },
        ))
    }
}

// ── LlmCompactor: FullCompact + SessionMemory ──

pub struct LlmCompactor {
    model: Arc<dyn base::interface::model::Model>,
    /// Optional memory store for SessionMemory extraction during compaction.
    memory_store: Option<Arc<base::interface::memory::MemoryStore>>,
}

impl LlmCompactor {
    pub fn new(model: Arc<dyn base::interface::model::Model>) -> Self {
        Self { model, memory_store: None }
    }

    /// Create an LlmCompactor with a MemoryStore for SessionMemory extraction.
    pub fn with_memory_store(
        model: Arc<dyn base::interface::model::Model>,
        memory_store: Arc<base::interface::memory::MemoryStore>,
    ) -> Self {
        Self { model, memory_store: Some(memory_store) }
    }

    /// Set the memory store after construction.
    pub fn set_memory_store(&mut self, store: Arc<base::interface::memory::MemoryStore>) {
        self.memory_store = Some(store);
    }

    /// Build a Claude TS-aligned detailed compact prompt.
    fn compact_prompt(summary_text: &str) -> String {
        format!(
            "\
You have exactly ONE turn to produce a compact summary. You CANNOT make ANY \
tool calls — not Read, not Grep, not Glob, not Bash, not Write, not Edit, \
not NotebookEdit, not any other tool. You must work entirely from the \
conversation text provided below. If you try to call a tool you will waste \
your only turn for compaction — just produce the summary directly.\n\
\n\
Your task is to create a detailed summary of the conversation so far, paying \
close attention to the user's explicit requests and your previous actions. \
This summary should be thorough in capturing technical details, code patterns, \
and architectural decisions that would be essential for continuing development \
work without losing context.\n\
\n\
Before providing your final summary, wrap your analysis in <analysis> tags to \
organize your thoughts. In your analysis:\n\
1. Chronologically analyze each message and section of the conversation.\n\
   Identify: user's explicit requests, your approach, key decisions, specific \
   details (file names, code snippets, function signatures, file edits), errors \
   and how you fixed them, and specific user feedback.\n\
2. Double-check for technical accuracy and completeness.\n\
\n\
Your summary should include these sections:\n\
1. Primary Request and Intent\n\
2. Key Technical Concepts\n\
3. Files and Code Sections (with full code snippets where applicable)\n\
4. Errors and Fixes\n\
5. Problem Solving\n\
6. All User Messages (that are not tool results)\n\
7. Pending Tasks\n\
8. Current Work (describe precisely what was being worked on immediately \
   before this summary, paying special attention to the most recent messages)\n\
9. Optional Next Step (only if directly in line with the user's most recent \
   explicit requests)\n\
\n\
Here is the conversation to summarize:\n\
\n\
{summary_text}\n\
\n\
Respond with <analysis>...</analysis> followed by <summary>...</summary>. \
Do NOT call any tools."
        )
    }

    async fn full_compact(
        &self,
        messages: Vec<ModelMessage>,
        _max_tokens: usize,
        keep_rounds: usize,
    ) -> Result<(Vec<ModelMessage>, CompactStrategy), CompactError> {
        let rounds = group_by_api_round(&messages);
        if rounds.len() <= keep_rounds {
            let _tokens = DefaultCompactor::tokens(&messages);
            return Ok((messages, CompactStrategy::FullCompact));
        }
        let keep_start = rounds.len() - keep_rounds;
        let mut to_summarize: Vec<ModelMessage> = rounds[..keep_start]
            .iter()
            .flat_map(|r| r.messages.clone())
            .collect();
        // Strip images before LLM compaction (TS parity: stripImagesFromMessages)
        strip_images_from_messages(&mut to_summarize);
        let to_keep: Vec<ModelMessage> = rounds[keep_start..]
            .iter()
            .flat_map(|r| r.messages.clone())
            .collect();

        let summary_text: String = to_summarize
            .iter()
            .map(|m| {
                let role_label = match m.role {
                    MessageRole::User => "User",
                    MessageRole::Assistant => "Assistant",
                    MessageRole::System => "System",
                };
                let text = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ModelContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("[{role_label}]: {text}")
            })
            .collect::<Vec<_>>()
            .join("\n");

        let prompt = Self::compact_prompt(&summary_text);
        let prompt_blocks = vec![base::interface::prompt::PromptBlock {
            role: base::interface::prompt::BlockRole::System,
            content: prompt,
            cache_strategy: None,
        }];

        match self
            .model
            .stream(
                prompt_blocks,
                vec![],
                vec![],
                base::interface::model::StreamParams {
                    model: String::new(),
                    max_tokens: 2048,
                    thinking_mode: base::interface::settings::ThinkingMode::Off,
                    fallback_model: None,
                    cache_edits: vec![],
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await
        {
            Ok(mut stream) => {
                use futures::StreamExt;
                let mut raw = String::new();
                while let Some(event) = stream.next().await {
                    if let Ok(base::interface::model::ModelEvent::TextDelta { text }) = event {
                        raw.push_str(&text);
                    }
                }
                // Extract just the <summary> portion, falling back to full text
                let summary = extract_summary_tag(&raw).unwrap_or(&raw);

                let boundary = ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text {
                        text: "[Conversation compacted — earlier context summarized below]"
                            .to_string(),
                    }],
                };
                let summary_msg = ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text {
                        text: format!("[Previous conversation summary]:\n{summary}"),
                    }],
                };

                let mut result = vec![boundary, summary_msg];
                result.extend(to_keep);
                Ok((result, CompactStrategy::FullCompact))
            }
            Err(_e) => {
                // PTL retry: if compact itself fails, fall back to Snip with fewer rounds.
                // TS parity: compactConversation retries up to 3 times with
                // truncateHeadForPTLRetry before giving up.
                let compactor = DefaultCompactor;
                let target_tokens = DefaultCompactor::tokens(&to_keep) + 5000;
                let mut current_keep = keep_rounds;
                let mut last_result = None;
                for retry in 0..=2 {
                    // On each retry, keep fewer rounds and snip more aggressively
                    let effective_keep = if retry > 0 {
                        current_keep = current_keep.saturating_sub(1).max(1);
                        current_keep
                    } else {
                        keep_rounds
                    };
                    let (result, _strategy, _dropped, _msgs, _saved) =
                        compactor.snip(messages.clone(), target_tokens, effective_keep);
                    if !result.is_empty() {
                        last_result = Some(result);
                        break;
                    }
                }
                let result = last_result.unwrap_or_else(|| {
                    // Ultimate fallback: keep only the last 2 messages
                    let n = messages.len();
                    messages[n.saturating_sub(2)..].to_vec()
                });
                Ok((result, CompactStrategy::FullCompact))
            }
        }
    }
}

fn extract_summary_tag(raw: &str) -> Option<&str> {
    if let Some(start) = raw.find("<summary>") {
        let after_start = &raw[start + 9..];
        if let Some(end) = after_start.find("</summary>") {
            return Some(after_start[..end].trim());
        }
    }
    // No tags found, return the full text trimmed
    let trimmed = raw.trim();
    if trimmed.is_empty() { None } else { Some(trimmed) }
}

impl LlmCompactor {
    /// Extract durable memories from the conversation during compaction.
    /// TS parity: SessionMemory compact strategy — writes extracted memories
    /// to the MemoryStore for cross-session persistence.
    pub async fn session_memory_extract(&self, messages: &[ModelMessage]) -> usize {
        let Some(ref store) = self.memory_store else {
            return 0;
        };

        // Build a lightweight prompt asking the model to extract memories
        let messages_text: String = messages
            .iter()
            .rev()
            .take(20) // Only analyze recent messages
            .filter_map(|m| {
                let texts: Vec<&str> = m
                    .content
                    .iter()
                    .filter_map(|b| {
                        if let ModelContentBlock::Text { text } = b {
                            Some(text.as_str())
                        } else {
                            None
                        }
                    })
                    .collect();
                if texts.is_empty() {
                    None
                } else {
                    Some(texts.join("\n"))
                }
            })
            .collect::<Vec<_>>()
            .join("\n---\n");

        if messages_text.is_empty() {
            return 0;
        }

        let prompt = "\
Extract any durable memories from this conversation excerpt. A durable \
memory is a fact about the user, project, or workflow that should persist \
across sessions. Only extract memories that are NOT derivable from the \
current codebase or git history.\n\n\
For each memory, return a JSON object with:\n\
- topic: short kebab-case slug\n\
- summary: 1-3 sentence description\n\
- memory_type: one of [user, feedback, project, reference]\n\
- confidence: 0.0-1.0\n\n\
Return only a JSON array of memories. If nothing is worth saving, return []."
            .to_string();

        // Use the model to extract memories (lightweight Haiku call)
        let result = {
            use base::interface::settings::ThinkingMode;
            let request_messages = vec![ModelMessage {
                role: MessageRole::User,
                content: vec![
                    ModelContentBlock::Text { text: prompt },
                    ModelContentBlock::Text { text: messages_text },
                ],
            }];
            let params = base::interface::model::StreamParams {
                model: "claude-haiku-4-5-20251001".into(),
                max_tokens: 2000,
                thinking_mode: ThinkingMode::Off,
                fallback_model: None,
                cache_edits: vec![],
            };
            let mut full_text = String::new();
            let stream_result = self.model.stream(
                vec![],
                vec![],
                request_messages,
                params,
                tokio_util::sync::CancellationToken::new(),
            ).await;

            let Ok(mut stream) = stream_result else {
                return 0;
            };

            use futures::StreamExt;
            while let Some(Ok(event)) = stream.next().await {
                if let base::interface::model::ModelEvent::TextDelta { text } = event {
                    full_text.push_str(&text);
                }
            }
            full_text
        };

        // Parse extracted memories
        if let Ok(memories) = serde_json::from_str::<Vec<serde_json::Value>>(&result) {
            let mut saved = 0usize;
            for mem in &memories {
                let topic = mem["topic"].as_str().unwrap_or("");
                let summary = mem["summary"].as_str().unwrap_or("");
                let memory_type = mem["memory_type"].as_str().unwrap_or("user");
                let confidence = mem["confidence"].as_f64().unwrap_or(0.5);

                if topic.is_empty() || summary.is_empty() || confidence < 0.3 {
                    continue;
                }

                // ISO 8601 timestamp
                let timestamp = "2026-06-13T00:00:00Z".to_string();

                let durable = base::interface::memory::DurableMemory {
                    name: topic.to_string(),
                    description: summary.to_string(),
                    memory_type: match memory_type {
                        "feedback" => base::interface::memory::MemoryType::Feedback,
                        "project" => base::interface::memory::MemoryType::Project,
                        "reference" => base::interface::memory::MemoryType::Reference,
                        _ => base::interface::memory::MemoryType::User,
                    },
                    content: summary.to_string(),
                    source_session_id: String::new(),
                    confidence,
                    last_seen: timestamp,
                    recall_count: 0,
                };
                if store.persist_batch(vec![durable]).is_ok() {
                    saved += 1;
                }
            }
            saved
        } else {
            0
        }
    }
}

#[async_trait]
impl Compactor for LlmCompactor {
    async fn compact(
        &self,
        messages: Vec<ModelMessage>,
        max_tokens: usize,
        keep_rounds: usize,
    ) -> Result<(Vec<ModelMessage>, CompactResult), CompactError> {
        let tokens_before = DefaultCompactor::tokens(&messages);
        let count_before = messages.len();
        if tokens_before <= max_tokens || messages.len() <= keep_rounds {
            return Ok((
                messages,
                CompactResult {
                    strategy: CompactStrategy::FullCompact,
                    messages_before: count_before,
                    messages_after: count_before,
                    tokens_before,
                    tokens_after: tokens_before,
                    projection: None,
                },
            ));
        }

        let (result, strategy) = self.full_compact(messages, max_tokens, keep_rounds).await?;
        let tokens_after = DefaultCompactor::tokens(&result);
        let messages_after = result.len();
        Ok((
            result,
            CompactResult {
                strategy,
                messages_before: count_before,
                messages_after,
                tokens_before,
                tokens_after,
                projection: None,
            },
        ))
    }
}

// ── Post-compact recovery (T1.4) ──

/// Context that can be recovered after compaction.
#[derive(Debug, Clone, Default)]
pub struct PostCompactContext {
    /// Recently read files (paths and their content)
    pub recent_files: Vec<(String, String)>,
    /// Skill names that were invoked before compaction
    pub invoked_skills: Vec<String>,
    /// Whether plan mode was active
    pub in_plan_mode: bool,
    /// Plan file content (if plan mode was active)
    pub plan_content: Option<String>,
    /// Recently activated deferred tools
    pub activated_tools: Vec<String>,
    /// Background task statuses: (task_id, status_description)
    pub running_tasks: Vec<(String, String)>,
}

/// Maximum number of recently read files to re-inject after compaction.
const MAX_RECOVER_FILES: usize = 5;
/// Maximum characters per recovered file.
const MAX_CHARS_PER_FILE: usize = 5_000;
/// Maximum total characters for recovered skills.
const MAX_CHARS_SKILLS: usize = 25_000;

/// Build post-compact recovery messages.
///
/// After compaction removes old messages, critical context (recently read files,
/// invoked skills, plan mode status) needs to be re-injected so the model doesn't
/// lose its bearings. This mirrors Claude Code's `createPostCompactAttachments`.
pub fn build_post_compact_recovery(
    ctx: &PostCompactContext,
) -> Vec<ModelMessage> {
    let mut recovery = Vec::new();

    // 1. Recently read files
    if !ctx.recent_files.is_empty() {
        let files: Vec<_> = ctx.recent_files.iter()
            .take(MAX_RECOVER_FILES)
            .map(|(path, content)| {
                let truncated = if content.len() > MAX_CHARS_PER_FILE {
                    format!("{}...\n[content truncated]", &content[..MAX_CHARS_PER_FILE])
                } else {
                    content.clone()
                };
                format!("## {path}\n\n{truncated}")
            })
            .collect();
        if !files.is_empty() {
            let body = format!(
                "The following files were read before context compaction and may still be relevant:\n\n{}",
                files.join("\n\n---\n\n")
            );
            recovery.push(ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text {
                    text: format!("<system-reminder>\n{body}\n</system-reminder>"),
                }],
            });
        }
    }

    // 2. Invoked skills
    if !ctx.invoked_skills.is_empty() {
        let skills_text = ctx.invoked_skills.join(", ");
        let mut body = format!(
            "The following skills were invoked before compaction and their instructions may still apply: {skills_text}"
        );
        if body.len() > MAX_CHARS_SKILLS {
            body.truncate(MAX_CHARS_SKILLS);
            body.push_str("...");
        }
        recovery.push(ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: format!("<system-reminder>\n{body}\n</system-reminder>"),
            }],
        });
    }

    // 3. Plan mode — inject plan content if available
    if ctx.in_plan_mode {
        if let Some(ref plan) = ctx.plan_content {
            let truncated = if plan.len() > MAX_CHARS_PER_FILE {
                format!("{}...\n[plan content truncated]", &plan[..MAX_CHARS_PER_FILE])
            } else {
                plan.clone()
            };
            recovery.push(ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text {
                    text: format!("<system-reminder>\nPlan mode is still active. The current plan:\n\n{truncated}\n</system-reminder>"),
                }],
            });
        } else {
            recovery.push(ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text {
                    text: "<system-reminder>\nPlan mode is still active. Continue operating in plan mode.\n</system-reminder>".to_string(),
                }],
            });
        }
    }

    // 4. Background running tasks
    if !ctx.running_tasks.is_empty() {
        let tasks_lines: Vec<_> = ctx.running_tasks.iter()
            .map(|(id, status)| format!("- task:{id} — {status}"))
            .collect();
        let body = format!(
            "The following background tasks are still running:\n\n{}",
            tasks_lines.join("\n")
        );
        recovery.push(ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: format!("<system-reminder>\n{body}\n</system-reminder>"),
            }],
        });
    }

    // 5. Activated deferred tools
    if !ctx.activated_tools.is_empty() {
        let tools_text = ctx.activated_tools.join(", ");
        recovery.push(ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: format!("<system-reminder>\nThe following tools were activated and are now available: {tools_text}\n</system-reminder>"),
            }],
        });
    }

    recovery
}

/// Extract recently read file paths from a set of messages.
/// Returns (path, content) pairs for FileRead tool results found in the messages.
pub fn extract_recent_reads(messages: &[ModelMessage]) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for msg in messages.iter().rev() {
        for block in &msg.content {
            if let ModelContentBlock::ToolResult { content, is_error, .. } = block {
                // Look for Read tool results — heuristic: content starts with file path pattern
                // and is preceded by a ToolUse that has a Read-ish name
                if is_error != &Some(true) && !content.is_empty() {
                    // Extract first line as potential file path
                    if let Some(first_line) = content.lines().next() {
                        if first_line.starts_with('/') || first_line.starts_with("./") {
                            let path = first_line.to_string();
                            if seen.insert(path.clone()) {
                                files.push((path, content.clone()));
                            }
                        }
                    }
                }
            }
        }
    }
    files
}

/// Enforce per-message tool result budget, truncating oversized tool results.
/// TS parity: claude-code's `applyToolResultBudget()` in query.ts.
///
/// Runs BEFORE microcompact for clean composition:
/// - 50KB per individual tool result (truncate with preserved header)
/// - 500KB total across all tool results (clear oldest first)
///
/// Returns the number of tool results that were modified.
/// Replace image blocks with `[image]` text markers before LLM compaction.
/// TS parity: `stripImagesFromMessages()` in compact.ts:145-200.
/// Note: AttaCore's `ModelContentBlock` currently has no `Image` variant — this
/// function is a forward-compat stub. When image support is added, it will
/// strip them before compaction.
pub fn strip_images_from_messages(messages: &mut [ModelMessage]) {
    // Walk through and strip any image-like content.
    // Currently a no-op — ModelContentBlock has no Image variant.
    // Forward-compat: when Text blocks contain base64 image data URIs,
    // they can be detected and replaced here.
    let _ = messages;
}

/// Tool names that MicroCompact may clear. TS parity: COMPACTABLE_TOOLS in microCompact.ts.
const COMPACTABLE_TOOLS: &[&str] = &[
    "Read", "Bash", "Grep", "Glob", "WebSearch", "WebFetch", "Edit", "Write",
];

// ── P2-1: Compact Analysis (TS parity: contextAnalysis.ts) ──

/// Token composition breakdown for a set of messages.
/// Useful for debugging compaction behavior and understanding where token budget is spent.
#[derive(Debug, Clone, Default)]
pub struct ContextAnalysis {
    pub total_messages: usize,
    pub total_estimated_tokens: usize,
    /// Tokens from user text messages (excluding tool results).
    pub user_text_tokens: usize,
    /// Tokens from assistant text messages.
    pub assistant_text_tokens: usize,
    /// Tokens from tool result blocks.
    pub tool_result_tokens: usize,
    /// Tokens from tool use blocks.
    pub tool_use_tokens: usize,
    /// Tokens from system messages.
    pub system_tokens: usize,
    /// Per-tool token breakdown: tool_name → (call_count, total_tokens).
    pub tool_usage: std::collections::HashMap<String, (usize, usize)>,
    /// Number of compressed/cleared tool results.
    pub cleared_results: usize,
    /// Percentage of token budget consumed by tool results.
    pub tool_result_pct: f64,
    /// Percentage of token budget consumed by user+assistant text.
    pub conversation_pct: f64,
}

/// Analyze token composition of a message list.
/// TS parity: `analyzeContext()` in contextAnalysis.ts.
pub fn analyze_context(messages: &[ModelMessage]) -> ContextAnalysis {
    let mut analysis = ContextAnalysis {
        total_messages: messages.len(),
        ..ContextAnalysis::default()
    };

    // Rough token estimation: 1 token ≈ 4 chars
    let estimate = |s: &str| s.len() / 4;

    for msg in messages {
        for block in &msg.content {
            match block {
                ModelContentBlock::Text { text } => {
                    let tokens = estimate(text);
                    analysis.total_estimated_tokens += tokens;
                    match msg.role {
                        MessageRole::User => analysis.user_text_tokens += tokens,
                        MessageRole::Assistant => analysis.assistant_text_tokens += tokens,
                        _ => analysis.system_tokens += tokens,
                    }
                }
                ModelContentBlock::ToolResult { content, .. } => {
                    let tokens = estimate(content);
                    analysis.total_estimated_tokens += tokens;
                    analysis.tool_result_tokens += tokens;
                    // Check if cleared
                    if content == "[Old tool result content cleared]" {
                        analysis.cleared_results += 1;
                    }
                }
                ModelContentBlock::ToolUse { name, input, .. } => {
                    let tokens = estimate(name) + serde_json::to_string(input)
                        .map(|s| estimate(&s))
                        .unwrap_or(0);
                    analysis.total_estimated_tokens += tokens;
                    analysis.tool_use_tokens += tokens;
                    let entry = analysis.tool_usage.entry(name.clone()).or_insert((0, 0));
                    entry.0 += 1;
                    entry.1 += tokens;
                }
            }
        }
    }

    // Compute percentages
    if analysis.total_estimated_tokens > 0 {
        let total = analysis.total_estimated_tokens as f64;
        analysis.tool_result_pct = (analysis.tool_result_tokens as f64 / total) * 100.0;
        let conversation = analysis.user_text_tokens + analysis.assistant_text_tokens;
        analysis.conversation_pct = (conversation as f64 / total) * 100.0;
    }

    analysis
}

/// Format a compact analysis as a human-readable summary string.
pub fn format_context_analysis(analysis: &ContextAnalysis) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Context: {} msgs, ~{} tokens\n",
        analysis.total_messages, analysis.total_estimated_tokens
    ));
    s.push_str(&format!(
        "  user text: {:.0}%  assistant: {:.0}%  tool results: {:.0}%  tool uses: {:.0}%\n",
        (analysis.user_text_tokens as f64 / analysis.total_estimated_tokens.max(1) as f64) * 100.0,
        (analysis.assistant_text_tokens as f64 / analysis.total_estimated_tokens.max(1) as f64) * 100.0,
        analysis.tool_result_pct,
        (analysis.tool_use_tokens as f64 / analysis.total_estimated_tokens.max(1) as f64) * 100.0,
    ));

    if !analysis.tool_usage.is_empty() {
        let mut tools: Vec<_> = analysis.tool_usage.iter().collect();
        tools.sort_by_key(|(_, (_calls, tokens))| std::cmp::Reverse(*tokens));
        s.push_str("  top tools by tokens:\n");
        for (name, (calls, tokens)) in tools.iter().take(5) {
            s.push_str(&format!("    {}: {} calls, ~{} tokens\n", name, calls, tokens));
        }
    }

    if analysis.cleared_results > 0 {
        s.push_str(&format!(
            "  {} tool result(s) already cleared\n",
            analysis.cleared_results
        ));
    }

    s
}

pub fn enforce_tool_result_budget(messages: &mut [ModelMessage]) -> usize {
    const MAX_PER_RESULT: usize = 50_000;
    const MAX_TOTAL: usize = 500_000;

    let mut modified = 0;
    let mut total_size: usize = 0;

    // Pass 1: truncate oversized individual results
    for msg in messages.iter_mut() {
        for block in &mut msg.content {
            if let ModelContentBlock::ToolResult { content, .. } = block {
                if content.len() > MAX_PER_RESULT {
                    let truncated = format!(
                        "[Tool result truncated: {} bytes > {} max; first 1000 chars preserved]\n{}...",
                        content.len(),
                        MAX_PER_RESULT,
                        &content[..1000.min(content.len())]
                    );
                    *content = truncated;
                    modified += 1;
                }
                total_size += content.len();
            }
        }
    }

    // Pass 2: if total still exceeds budget, clear oldest tool results
    if total_size > MAX_TOTAL {
        for msg in messages.iter_mut() {
            for block in &mut msg.content {
                if let ModelContentBlock::ToolResult { content, .. } = block {
                    if total_size <= MAX_TOTAL {
                        // Within budget now — stop clearing
                        break;
                    }
                    total_size = total_size.saturating_sub(content.len());
                    *content = "[Old tool result content cleared]".to_string();
                    modified += 1;
                }
            }
            if total_size <= MAX_TOTAL {
                break;
            }
        }
    }

    modified
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_user(s: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text { text: s.to_string() }],
        }
    }

    fn tool_result(id: &str, content: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: content.to_string(),
                is_error: Some(false),
            }],
        }
    }

    fn assistant_text(s: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::Text { text: s.to_string() }],
        }
    }

    fn tool_use(id: &str, name: &str, input: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: serde_json::from_str(input).unwrap_or_default(),
            }],
        }
    }

    fn build_long_conversation(rounds: usize) -> Vec<ModelMessage> {
        let mut msgs = Vec::new();
        for i in 0..rounds {
            msgs.push(text_user(&format!("msg {i}")));
            msgs.push(assistant_text(&format!("response {i}")));
        }
        msgs
    }

    #[tokio::test]
    async fn snip_keeps_recent_rounds() {
        let msgs = build_long_conversation(10);
        let compactor = DefaultCompactor;
        // Use tight budget to force snip: each round ~3 tokens
        let (result, _) = compactor.compact(msgs.clone(), 20, 3).await.unwrap();
        // Should keep ~3 rounds * 2 messages = 6 messages (if fits under 20 tokens)
        assert!(!result.is_empty() && result.len() <= 8, "expected <=8 and non-empty, got {}", result.len());
    }

    #[tokio::test]
    async fn micro_compact_replaces_old_results() {
        // Test that micro_compact clears old tool results for compactable tools.
        // Only results from whitelisted tools (Read, Bash, Grep, etc.) are cleared.
        let long = "VERY LONG TOOL RESULT ".repeat(100);
        let msgs = vec![
            text_user("read a"),
            tool_use("t1", "Read", "{}"),   // whitelisted → will be cleared
            tool_result("t1", &long),
            assistant_text("done a"),
            text_user("grep b"),
            tool_use("t2", "Grep", "{}"),   // whitelisted → will be cleared
            tool_result("t2", &long),
            assistant_text("done b"),
            text_user("edit c"),
            tool_use("t3", "Edit", "{}"),   // whitelisted but in recent round → kept
            assistant_text("done c"),
        ];
        let compactor = DefaultCompactor;
        let (result, _) = compactor.micro_compact(msgs, 1);
        // Old rounds (a, b) should have their tool results cleared.
        // Recent round (c) should be intact.
        let cleared_count = result.iter().filter(|m| {
            m.content.iter().any(|b| matches!(b,
                ModelContentBlock::ToolResult { content, .. } if content == "[Old tool result content cleared]"
            ))
        }).count();
        assert_eq!(cleared_count, 2, "Expected exactly 2 cleared tool results, got {cleared_count}");
    }

    #[test]
    fn extract_summary_from_tags() {
        let raw = "<analysis>blah</analysis>\n<summary>\nThis is the summary.\n</summary>";
        let extracted = extract_summary_tag(raw);
        assert_eq!(extracted, Some("This is the summary."));
    }

    #[test]
    fn extract_summary_no_tags_returns_trimmed() {
        assert_eq!(extract_summary_tag("  plain text  "), Some("plain text"));
    }
}
