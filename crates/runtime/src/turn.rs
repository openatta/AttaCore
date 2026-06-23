//! Turn loop — user message → streaming events → TurnOutcome.
//!
//! Core processing logic. Ported from attacode-engine/src/engine/turn/mod.rs
//! and adapted to use the agent's protocol-agnostic types.

use crate::agent::{Agent, EngineCommand, InputMessage};
use base::interface::event::AgentEvent;
use base::interface::memory::{DurableMemory, MemoryStore, MemoryType};
use base::interface::model::{
    MessageRole, ModelContentBlock, ModelMessage, ModelStream, ToolDef, Usage,
};
use base::interface::prompt::{assemble_prompt, PromptBlock};
use base::interface::scene::ScenePromptContext;
use base::tool::{ToolContext, ToolResultContent};
use std::borrow::Cow;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

/// Empty hashset fallback when frozen context is not yet available.
static EMPTY: LazyLock<HashSet<String>> = LazyLock::new(HashSet::new);
use tokio_util::sync::CancellationToken;

impl Agent {
    /// Process one input message — the core turn loop.
    /// Internal turn processing. Consumers should use [`Agent::run`] or [`Agent::run_turn`].
    pub(crate) async fn process_turn(
        &mut self,
        msg: InputMessage,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, TurnError> {
        match msg {
            InputMessage::User {
                content, turn_id, ..
            } => {
                self.current_turn_id = turn_id;
                // ── Slash command interception (TS parity: processSlashCommand) ──
                if let Some(sc) = crate::commands::parse_slash_command(&content) {
                    if let Some(cmd) = self.commands.resolve(&sc.name) {
                        match cmd {
                            crate::commands::Command::Prompt { entry } => {
                                // Expand skill body → replace content → continue to LLM
                                let expanded = crate::commands::handle_prompt_command(entry, &sc);
                                return self.run_user_turn(expanded, cancel).await;
                            }
                            crate::commands::Command::Local { .. } => {
                                // Handle well-known local commands directly
                                let result_text = match sc.name.as_str() {
                                    "help" => self.handle_help_command(),
                                    "skills" => self.handle_skills_command(),
                                    "clear" => {
                                        self.handle_clear_command();
                                        "Session cleared. All messages removed.".into()
                                    }
                                    "compact" => {
                                        let _ = self.compact_now().await;
                                        "Compaction triggered.".into()
                                    }
                                    "cost" => self.handle_cost_command(),
                                    _ => format!("Unknown local command: {}", sc.name),
                                };
                                let _ = self.event_tx.send(AgentEvent::TextDelta {
                                    text: result_text,
                                    turn_id: self.current_turn_id.clone(),
                                });
                                let _ = self.event_tx.send(AgentEvent::TurnComplete {
                                    stop_reason: "command_executed".into(),
                                    api_calls: 0,
                                    tool_calls: 0,
                                    usage: Usage::default(),
                                    turn_id: self.current_turn_id.clone(),
                                });
                                self.last_had_tool_uses = false;
                                return Ok(TurnOutcome {
                                    stop_reason: "command_executed".into(),
                                    api_calls: 0,
                                    tool_calls: 0,
                                    usage: Usage::default(),
                                });
                            }
                        }
                    }
                    // Unknown slash command — pass through to LLM as-is
                }

                // ── Token budget directive parsing (TS parity: outputTokenBudget) ──
                // Parse directives like "+500k", "spend 2M tokens", "use 1B tokens"
                // from the user message, set the budget on Agent state, and strip
                // the directive before passing content to the turn loop.
                let processed_content = if let Some(target) = parse_token_budget_directive(&content) {
                    self.output_token_target = Some(target);
                    self.accumulated_output_tokens = 0;
                    self.token_budget_continuation_count = 0;
                    tracing::info!(
                        target,
                        "Token budget directive parsed — set output target"
                    );
                    strip_token_budget_directive(&content)
                } else {
                    content
                };

                // Not a slash command → normal flow
                self.run_user_turn(processed_content, cancel).await
            }
            InputMessage::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                self.handle_tool_result(tool_use_id, content, is_error)
                    .await
            }
            InputMessage::PermissionResponse { decision, .. } => {
                // Track permission denials for telemetry (TS parity)
                if let crate::agent::PermissionDecision::Deny { .. } = decision {
                    self.permission_denial_count += 1;
                }
                Ok(TurnOutcome::default())
            }
            InputMessage::System { kind, content } => {
                match kind {
                    EngineCommand::Shutdown => Err(TurnError::Shutdown),
                    EngineCommand::CompactNow => {
                        // Trigger compaction
                        let _ = self.event_tx.send(AgentEvent::CompactAction {
                            strategy: "manual".into(),
                            messages_before: self.session.messages.len(),
                            messages_after: self.session.messages.len(),
                            turn_id: self.current_turn_id.clone(),
                            dropped_rounds: None,
                            dropped_messages: None,
                            estimated_tokens_saved: None,
                        });
                        Ok(TurnOutcome::default())
                    }
                    EngineCommand::SetSessionId => {
                        self.session.set_session_id(content);
                        let _ = self.event_tx.send(AgentEvent::SessionChanged {
                            session_id: self.session.session_id.clone(),
                        });
                        Ok(TurnOutcome::default())
                    }
                    _ => Ok(TurnOutcome::default()),
                }
            }
        }
    }

    /// Run a full turn from a user message.
    async fn run_user_turn(
        &mut self,
        content: String,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, TurnError> {
        let _timer = self.perf.start_timer("turn", "total");
        let mut api_calls: u32 = 0;
        let mut tool_calls: u32 = 0;
        let mut structured_output_calls: u32 = 0;
        const MAX_STRUCTURED_OUTPUT_RETRIES: u32 = 5;
        let mut max_tokens_recovery: u32 = 0;
        let mut effective_max_tokens = self.settings.model.max_tokens;
        let mut effective_model = self.settings.model.model_name.clone();
        let max_calls = self.settings.execution.max_api_calls_per_turn;
        let max_budget = self.settings.execution.max_budget_usd;
        let mut total_cost_usd: f64 = 0.0;
        let start = std::time::Instant::now();

        // P2: Check for externally modified skill files and reload them.
        // TS parity: file-watching integration in loadSkillsDir.ts.
        let changed = self.skills.check_for_changes();
        if changed > 0 {
            tracing::debug!(count = changed, "Skills reloaded from file changes");
        }

        // Compute frozen context lazily on first turn (TS parity: getSystemContext +
        // getUserContext in query.ts). Includes git status, branch, platform, etc.
        if self.frozen.is_none() {
            let cwd = self.settings.paths.local_data_dir.clone();
            self.frozen = Some(base::frozen::FrozenContext::collect(cwd).await);
        }

        // Inject CLAUDE.md as userContext — synthetic <system-reminder> user message
        // (TS parity: prependUserContext in query.ts). Injected once per session.
        if let Some(ref claude_md) = self.claude_md_content.clone() {
            if !self.claude_md_injected {
                let today = chrono_now();
                self.session.push_message(ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text {
                        text: format!(
                            "<system-reminder>\n\
                             As you answer the user's questions, you can use the following context:\n\
                             # claudeMd\n\
                             {claude_md}\n\n\
                             # currentDate\n\
                             Today's date is {today}.\n\n\
                             IMPORTANT: this context may or may not be relevant to your tasks. \
                             You should not respond to this context unless it is highly relevant \
                             to your task.\n\
                             </system-reminder>"
                        ),
                    }],
                });
                self.claude_md_injected = true;
            }
        }

        // Inject system-reminder: git status + memory summary (TS parity: buildSystemReminder).
        // Called once per turn — git status may change between turns.
        if let Some(ref frozen) = self.frozen {
            let mut reminder = String::new();
            if let Some(ref git_status) = frozen.git_status {
                reminder.push_str(&format!(
                    "\n<system-reminder>\ngitStatus: {git_status}\n</system-reminder>"
                ));
            }
            if let Some(ref mem) = frozen.memory_index {
                if !mem.is_empty() {
                    reminder.push_str(&format!(
                        "\n<system-reminder>\n# Memory index\n{mem}\n</system-reminder>"
                    ));
                }
            }
            if !reminder.is_empty() {
                self.session.push_message(ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text { text: reminder }],
                });
            }
        }

        // Memory prefetch: fire LLM-based relevant memory selection as a background task.
        // TS parity: startRelevantMemoryPrefetch in query.ts (fires Sonnet call, collects after tools).
        let mut prefetch_handle: Option<tokio::task::JoinHandle<Vec<String>>> = {
            let store = self.memory_store.clone();
            let model = self.model.clone();
            let query = content.clone();
            let already_surfaced: HashSet<String> = self.frozen.as_ref()
                .map(|f| f.already_surfaced.clone())
                .unwrap_or_default();
            let recent_tools = self.tools.names();
            let model_name = self.settings.model.model_name.clone();
            Some(tokio::spawn(async move {
                base::interface::memory::select_memories_with_llm(
                    &store,
                    &query,
                    model.as_ref(),
                    5,
                    &already_surfaced,
                    &recent_tools,
                    &model_name,
                ).await
            }))
        };
        let prefetch_started_at: std::time::Instant = std::time::Instant::now();

        // Skill discovery prefetch: scan workspace for matching skills as a
        // background task (TS parity: startSkillDiscoveryPrefetch with
        // findWritePivot guard in query.ts:331-335). Discovery runs while
        // the model streams and tools execute; the result is consumed
        // post-tool-execution alongside the memory prefetch.
        let mut skill_prefetch: Option<
            tokio::task::JoinHandle<(Vec<skills::manager::SkillInfo>, Vec<String>)>,
        > = {
            let skills = self.skills.clone();
            let local_dir = self.settings.paths.local_data_dir.clone();
            let invoked_skills: Vec<String> = self.invoked_skills.clone();
            // findWritePivot guard: skip on non-write turns (TS parity).
            // Only scan when the previous turn produced tool calls that may
            // have written files that could reveal new skills.
            // findWritePivot guard: run on first turn (last_had_tool_uses
            // starts true) and after turns that had tool uses (TS parity).
            let should_run = self.last_had_tool_uses;
            if should_run {
                Some(tokio::spawn(async move {
                    let paths = vec![local_dir];
                    let discovered = skills.discover_for_paths(&paths);
                    let new_names: Vec<String> = discovered
                        .iter()
                        .filter(|s| !invoked_skills.contains(&s.name))
                        .map(|s| s.name.clone())
                        .collect();
                    (discovered, new_names)
                }))
            } else {
                None
            }
        };

        // Push user message (memory injection deferred until after tool execution).
        self.session.push_message(ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text { text: content }],
        });

        let mut had_tool_uses_this_turn = false;

        loop {
            if cancel.is_cancelled() {
                self.last_had_tool_uses = had_tool_uses_this_turn;
                return Ok(TurnOutcome {
                    stop_reason: "cancelled".into(),
                    api_calls,
                    tool_calls,
                    usage: Usage::default(),
                });
            }
            if api_calls >= max_calls {
                self.last_had_tool_uses = had_tool_uses_this_turn;
                return Ok(TurnOutcome {
                    stop_reason: "max_turns".into(),
                    api_calls,
                    tool_calls,
                    usage: Usage::default(),
                });
            }

            // 1. Compact if token budget exceeded
            self.compact_if_needed().await;

            // 2. Build prompt, tool defs, and clone messages for the model call
            let (prompt_blocks, tool_defs, messages) = self.build_prompt_for_turn();

            // 3. Call model
            api_calls += 1;
            let stream_result = self
                .model
                .stream(
                    prompt_blocks.clone(),
                    tool_defs.clone(),
                    messages.clone(),
                    base::interface::model::StreamParams {
                        model: effective_model.clone(),
                        max_tokens: effective_max_tokens,
                        thinking_mode: self.settings.model.thinking_mode.clone(),
                        fallback_model: self.settings.model.fallback_model.clone(),
                        cache_edits: self.cached_mc.consume_pending_edits(),
                    },
                    cancel.clone(),
                )
                .await;

            // 4. Handle fallback — Overloaded → switch to fallback model
            let stream = match stream_result {
                Ok(s) => s,
                Err(base::interface::model::ModelError::Overloaded) => {
                    self.handle_overloaded_recovery(
                        tool_defs,
                        messages,
                        effective_max_tokens,
                        &mut effective_model,
                        cancel.clone(),
                    )
                    .await?
                }
                // T3.1: PTL recovery — catch prompt-too-long before generic error
                Err(base::interface::model::ModelError::Internal(ref msg))
                    if msg.contains("prompt too long") || msg.contains("413") =>
                {
                    tracing::warn!("prompt too long, attempting recovery compaction");
                    let threshold = self.scene.token_budget().compact_threshold.max(50000);
                    let keep = self.scene.token_budget().compact_keep_recent.min(5);
                    let messages_before = self.session.messages.len();
                    if let Ok((compacted, _result)) = self
                        .compactor
                        .compact(messages, threshold, keep)
                        .await
                    {
                        if compacted.len() < messages_before {
                            self.session.messages = compacted;
                            let fb_ctx = build_prompt_context(&self.settings, &self.session, self.frozen.as_ref(), None, None);
                            let fb_prompt = assemble_prompt(
                                self.scene.as_ref(),
                                &self.settings,
                                &self.memory_store,
                                &fb_ctx,
                                None,
                                None,
                            );
                            match self
                                .model
                                .stream(
                                    fb_prompt,
                                    tool_defs.clone(),
                                    self.session.messages().to_vec(),
                                    base::interface::model::StreamParams {
                                        model: effective_model.clone(),
                                        max_tokens: effective_max_tokens,
                                        thinking_mode: self.settings.model.thinking_mode.clone(),
                                        fallback_model: self.settings.model.fallback_model.clone(),
                                        cache_edits: vec![], // already consumed above
                                    },
                                    cancel.clone(),
                                )
                                .await
                            {
                                Ok(s) => s,
                                Err(e2) => {
                                    return Err(TurnError::Model(format!(
                                        "failed to stream model response: {}",
                                        e2
                                    )))
                                }
                            }
                        } else {
                            return Err(TurnError::Model(
                                "prompt too long and compaction could not reduce message count"
                                    .to_string(),
                            ));
                        }
                    } else {
                        return Err(TurnError::Model(format!(
                            "prompt too long and compaction failed: {msg}"
                        )));
                    }
                }
                Err(e) => {
                    return Err(TurnError::Model(format!(
                        "failed to stream model response: {}",
                        e
                    )))
                }
            };

            // 5. Process streaming response — execute tools as they arrive
            let tools = Arc::clone(&self.tools);
            let tools_for_safety = Arc::clone(&self.tools);
            let cwd = self.settings.paths.local_data_dir.clone();
            let session_id = self.session.session_id.clone();
            let turn_no = self.session.turn_count;
            let th = self.telemetry_handle.clone();
            let tid = self.current_turn_id.clone();
            let cancel_for_exec = cancel.clone();
            let stream_result = crate::streaming::execute_stream(
                stream,
                &mut self.session,
                &self.event_tx,
                tid.clone(),
                move |name, input| {
                    let exec_ctx = ToolExecCtx {
                        tools: Arc::clone(&tools),
                        cwd: cwd.clone(),
                        session_id: session_id.clone(),
                        turn_no,
                        telemetry_handle: th.clone(),
                        turn_id: tid.clone(),
                        cancel: cancel_for_exec.clone(),
                    };
                    async move {
                        execute_tool_with_telemetry(&exec_ctx, &name, input).await
                    }
                },
                move |name: &str, input: &serde_json::Value| {
                    tools_for_safety
                        .get(name)
                        .map(|t| t.is_concurrency_safe(input))
                        .unwrap_or(false)
                },
                cancel.clone(),
            )
            .await?;

            let has_tool_uses = stream_result.has_tool_uses;
            had_tool_uses_this_turn = had_tool_uses_this_turn || has_tool_uses;
            tool_calls += stream_result.tool_calls;
            let stop_reason = stream_result.stop_reason;
            let usage = stream_result.usage;

            // ── Memory prefetch: collect results from background task ──
            // TS parity: collectRelevantMemoryPrefetch in query.ts:1599-1614.
            // Fired at turn start as a background Haiku call; collected here after
            // tool execution completes. 30-second timeout with graceful fallback.
            if let Some(prefetch) = prefetch_handle.take() {
                let turn_duration_so_far = start.elapsed();
                let prefetch_names = match tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    prefetch,
                )
                .await
                {
                    Ok(Ok(names)) => {
                        let lat = prefetch_started_at.elapsed();
                        let hidden = lat <= turn_duration_so_far;
                        tracing::debug!(
                            count = names.len(),
                            latency_ms = lat.as_millis(),
                            hidden = hidden,
                            "memory prefetch completed"
                        );
                        names
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "memory prefetch task failed");
                        Vec::new()
                    }
                    Err(_) => {
                        tracing::warn!("memory prefetch timed out after 30s");
                        Vec::new()
                    }
                };
                let relevant: Vec<base::interface::memory::DurableMemory> = {
                    let all = self.memory_store.load_all();
                    let surfaced: &std::collections::HashSet<String> =
                        self.frozen.as_ref().map(|f| &f.already_surfaced).unwrap_or(&EMPTY);
                    prefetch_names.iter()
                        .filter_map(|name| all.iter().find(|m| &m.name == name).cloned())
                        .filter(|m| !surfaced.contains(&m.name))
                        .collect()
                };
                if !relevant.is_empty() {
                    // P0-1: Mark injected memories as surfaced to avoid repeated injection.
                    // TS parity: alreadySurfaced set in memdir.ts.
                    // P2-1: Increment recall_count for surfaced memories (staleness scoring).
                    if let Some(ref mut frozen) = self.frozen {
                        for m in &relevant {
                            frozen.already_surfaced.insert(m.name.clone());
                        }
                    }
                    // P2-1: Increment recall counts on the persisted memory files.
                    // Fire-and-forget — failure is non-blocking.
                    {
                        let store = self.memory_store.clone();
                        let names: Vec<String> = relevant.iter().map(|m| m.name.clone()).collect();
                        tokio::spawn(async move {
                            let mut all = store.load_all();
                            for name in &names {
                                if let Some(mem) = all.iter_mut().find(|m| &m.name == name) {
                                    mem.recall_count += 1;
                                }
                            }
                            let _ = store.persist_batch(all);
                        });
                    }

                    // Only inject as context if there will be another LLM call (tools were executed).
                    if has_tool_uses {
                        let mut mem_text = String::from(
                            "<system-reminder>
Relevant memories for this query:
",
                        );
                        for m in relevant.iter().take(5) {
                            mem_text.push_str(&format!("- **{}**: {}
", m.name, m.description));
                        }
                        mem_text.push_str(
                            "
Use these memories to inform your response.
</system-reminder>",
                        );
                        self.session.push_message(ModelMessage {
                            role: MessageRole::User,
                            content: vec![ModelContentBlock::Text { text: mem_text }],
                        });
                    }
                }
            }

            // T0: Structured output retry limit (TS parity: QueryEngine.ts:1004-1048)
            let so_calls_this_turn = count_structured_output_calls(&self.session.messages);
            if so_calls_this_turn > structured_output_calls {
                structured_output_calls = so_calls_this_turn;
            }
            if structured_output_calls >= MAX_STRUCTURED_OUTPUT_RETRIES {
                tracing::warn!(structured_output_calls, "structured output retry limit exceeded");
                self.last_had_tool_uses = had_tool_uses_this_turn;
                return Ok(TurnOutcome {
                    stop_reason: "max_structured_output_retries".into(),
                    api_calls,
                    tool_calls,
                    usage: Usage::default(),
                });
            }

            // T3.2: USD budget tracking with continue mode (TS parity: checkTokenBudget
            // in query.ts:1308-1355). At 90% inject a warning; at 100% abort.
            {
                let input_cost = usage.input_tokens as f64 * 3.0 / 1_000_000.0;
                let output_cost = usage.output_tokens as f64 * 15.0 / 1_000_000.0;
                let call_cost = input_cost + output_cost;
                total_cost_usd += call_cost;
                if let Some(budget) = max_budget {
                    if total_cost_usd >= budget {
                        tracing::warn!(total_cost_usd, budget, "USD budget exceeded");
                        self.last_had_tool_uses = had_tool_uses_this_turn;
                        return Ok(TurnOutcome {
                            stop_reason: "budget_exceeded".into(),
                            api_calls, tool_calls, usage: Usage::default(),
                        });
                    }
                    if total_cost_usd >= budget * 0.9 {
                        // TS parity: checkTokenBudget "continue" mode in query.ts:1308.
                        // Inject a reminder so the model wraps up before hitting the hard cap.
                        tracing::warn!(total_cost_usd, budget, "approaching USD budget limit; injecting continue reminder");
                        self.session.push_message(ModelMessage {
                            role: MessageRole::User,
                            content: vec![ModelContentBlock::Text {
                                text: "<system-reminder>\nOutput token budget nearly exhausted. Keep your response concise and wrap up.\n</system-reminder>".into(),
                            }],
                        });
                    }
                }
            }

            // If tools were executed during streaming, continue to next API call.
            if has_tool_uses {
                // Collect async skill discovery prefetch (fired at turn start).
                // TS parity: collectSkillDiscoveryPrefetch in query.ts:1620-1628.
                if let Some(handle) = skill_prefetch.take() {
                    // Time-bounded wait: if discovery hasn't completed after
                    // tool execution, skip it this turn (it'll run next turn).
                    if let Ok(Ok((_discovered, new_names))) =
                        tokio::time::timeout(
                            std::time::Duration::from_millis(500),
                            handle,
                        )
                        .await
                    {
                        if !new_names.is_empty() {
                            let skills_text = format!(
                                "<system-reminder>\nSkills discovered in workspace: {}. Use /<skill-name> to invoke.\n</system-reminder>",
                                new_names.join(", ")
                            );
                            self.session.push_message(ModelMessage {
                                role: MessageRole::User,
                                content: vec![ModelContentBlock::Text { text: skills_text }],
                            });
                            for name in &new_names {
                                if !self.invoked_skills.contains(name) {
                                    self.invoked_skills.push(name.clone());
                                }
                            }
                        }
                    }
                }

                // P2: Activate conditional skills whose `paths` patterns match
                // files accessed by Read/Write/Edit tool operations this turn.
                // TS parity: conditional skills activation from skill frontmatter.
                {
                    let file_paths = Self::extract_tool_file_paths(
                        self.session.messages(),
                    );
                    if !file_paths.is_empty() {
                        let activated = self
                            .skills
                            .activate_conditional_skills_for_paths(&file_paths);
                        if !activated.is_empty() {
                            let names: Vec<&str> = activated
                                .iter()
                                .map(|s| s.name.as_str())
                                .collect();
                            let skills_text = format!(
                                "<system-reminder>\nConditional skills activated for \
                                 current context: {}. Use /<skill-name> to invoke.\n\
                                 </system-reminder>",
                                names.join(", "),
                            );
                            self.session.push_message(ModelMessage {
                                role: MessageRole::User,
                                content: vec![ModelContentBlock::Text {
                                    text: skills_text,
                                }],
                            });
                            for s in &activated {
                                if !self.invoked_skills.contains(&s.name) {
                                    self.invoked_skills.push(s.name.clone());
                                }
                            }
                        }
                    }
                }

                // Emit tool usage summary for SDK display.
                // TS parity: generateToolUseSummary in query.ts:1411-1482.
                // Text-based: extract tool names from recent session messages.
                let tool_names: Vec<String> = self
                    .session
                    .messages()
                    .iter()
                    .rev()
                    .take(50)
                    .filter_map(|m| {
                        m.content.iter().find_map(|b| {
                            if let ModelContentBlock::ToolUse { name, .. } = b {
                                Some(name.clone())
                            } else {
                                None
                            }
                        })
                    })
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let summary = if tool_names.is_empty() {
                    String::new()
                } else {
                    format!(
                        "Turn {} used tools: {}",
                        self.session.turn_count,
                        tool_names.join(", ")
                    )
                };
                if !summary.is_empty() {
                    let _ = self.event_tx.send(AgentEvent::TextDelta {
                        text: format!("\n[{summary}]\n"),
                        turn_id: self.current_turn_id.clone(),
                    });
                }
                // Refresh MCP tools between turns (TS parity: refreshTools in query.ts:1659)
                self.mcp.refresh_tools().await;
                continue;
            }

            // 6. max_output_tokens recovery: escalate and retry
            if self.handle_max_tokens_recovery(
                &stop_reason,
                &mut max_tokens_recovery,
                &mut effective_max_tokens,
            ) {
                continue;
            }

            // 7. Stop hooks + teammate lifecycle hooks
            // (TS parity: handleStopHooks in query.ts:1267).
            if self.hooks.has_hooks_for(hooks::HookEvent::Stop) {
                let stop_hook_input = hooks::HookInput {
                    hook_event_name: "Stop".into(),
                    session_id: self.session.session_id.to_string(),
                    cwd: self.settings.paths.local_data_dir.display().to_string(),
                    permission_mode: "default".into(),
                    tool_name: None, tool_input: None, tool_use_id: None,
                    tool_result: None, is_error: None, user_prompt: None,
                };
                let hook_result = self.hooks.run(hooks::HookEvent::Stop, &stop_hook_input).await;
                if hook_result.discontinued() {
                    tracing::info!("Stop hook discontinued the turn");
                    let tid = self.current_turn_id.clone();
                    let _ = self.telemetry_handle.record(
                        telemetry::TelemetryEvent::turn_complete(
                            &self.session.session_id, self.session.turn_count,
                            Some(tid.clone()),
                            telemetry::TurnCompletePayload {
                                turn_no: self.session.turn_count,
                                turn_id: Some(tid),
                                stop_reason: "stopped_by_hook".into(),
                                api_calls, tool_calls,
                                permission_denials: self.permission_denial_count,
                                last_tool_name: None, last_tool_was_error: false,
                                turn_duration_ms: start.elapsed().as_millis() as u64,
                            },
                        ));
                    self.last_had_tool_uses = had_tool_uses_this_turn;
                    return Ok(TurnOutcome {
                        stop_reason: "stopped_by_hook".into(),
                        api_calls, tool_calls, usage,
                    });
                }
                // Teammate lifecycle hooks (TS parity: TaskCompleted + TeammateIdle
                // in stopHooks.ts:335-453). Only run if agent is part of a team.
                if self.team_id.is_some() {
                    let _ = self.hooks.run(hooks::HookEvent::TaskCompleted, &stop_hook_input).await;
                    let _ = self.hooks.run(hooks::HookEvent::TeammateIdle, &stop_hook_input).await;
                }
            }

            // 8. Token budget continuation mode (TS parity: query/tokenBudget.ts).
            // Continue while accumulated output < 90% of target AND not diminishing
            // (≥3 continuations, both this & previous delta < 500). No hard cap.
            if let Some(target) = self.output_token_target {
                self.accumulated_output_tokens = self
                    .accumulated_output_tokens
                    .saturating_add(usage.output_tokens as u64);

                let this_delta = usage.output_tokens as u64;
                if should_continue_token_budget(
                    self.accumulated_output_tokens,
                    target,
                    self.token_budget_continuation_count,
                    this_delta,
                    self.last_delta_tokens,
                ) {
                    self.token_budget_continuation_count += 1;
                    self.last_delta_tokens = this_delta;
                    let remaining = target.saturating_sub(self.accumulated_output_tokens);
                    let nudge = format!(
                        "\
<system-reminder>
Continue working. Used {accumulated}/{target} output tokens ({remaining} remaining).
</system-reminder>",
                        accumulated = self.accumulated_output_tokens,
                        target = target,
                        remaining = remaining,
                    );
                    self.session.push_message(ModelMessage {
                        role: MessageRole::User,
                        content: vec![ModelContentBlock::Text { text: nudge }],
                    });
                    tracing::info!(
                        accumulated = self.accumulated_output_tokens,
                        target,
                        continuation = self.token_budget_continuation_count,
                        "Token budget continuation — injecting nudge"
                    );
                    continue;
                }

                // Budget met (≥90%) or diminishing returns — clear budget state.
                let threshold = (target as f64 * 0.9) as u64;
                let budget_met = self.accumulated_output_tokens >= threshold;
                tracing::info!(
                    accumulated = self.accumulated_output_tokens,
                    target,
                    budget_met,
                    "Token budget session complete"
                );
                self.output_token_target = None;
                self.accumulated_output_tokens = 0;
                self.token_budget_continuation_count = 0;
                self.last_delta_tokens = 0;
            }

            // No tools → turn complete
            self.session.increment_turn();
            let latency_ms = start.elapsed().as_millis() as f64;
            let tid = self.current_turn_id.clone();
            let _ = self
                .telemetry_handle
                .record(telemetry::TelemetryEvent::turn_complete(
                    &self.session.session_id,
                    turn_no,
                    Some(tid.clone()),
                    telemetry::TurnCompletePayload {
                        turn_no,
                        turn_id: Some(tid.clone()),
                        stop_reason: stop_reason.clone(),
                        api_calls,
                        tool_calls,
                        permission_denials: self.permission_denial_count,
                        last_tool_name: None,
                        last_tool_was_error: false,
                        turn_duration_ms: latency_ms as u64,
                    },
                ));
            let _ = self.event_tx.send(AgentEvent::TurnComplete {
                stop_reason: stop_reason.clone(),
                api_calls,
                tool_calls,
                usage: usage.clone(),
                turn_id: tid,
            });
            // P0-2: Auto-extract durable memories after turn completion.
            // TS parity: initExtractMemories() called via handleStopHooks in stopHooks.ts.
            // Only extract if the model produced a complete response (not cancelled/max_turns).
            {
                let session_messages = self.session.messages().to_vec();
                let store = self.memory_store.clone();
                let model = self.model.clone();
                tokio::spawn(async move {
                    extract_memories_after_turn(&store, &session_messages, model.as_ref()).await;
                });
            }

            // Feature #9: Check if session memory is stale and inject a system reminder
            // prompting the model to update its cross-session session notes.
            if let Some(ref sm) = self.session.session_memory {
                let current_turn = self.session.turn_count;
                if sm.is_stale(current_turn) {
                    tracing::debug!(
                        last_update_turn = sm.last_update_turn(),
                        current_turn,
                        "session memory is stale; injecting update reminder"
                    );
                    self.session.push_message(ModelMessage {
                        role: MessageRole::User,
                        content: vec![ModelContentBlock::Text {
                            text: "\
<system-reminder>
Your session notes (`session_memory.md`) have not been updated in several turns.
Consider reviewing and updating them with any persistent facts, user preferences,
or project context that should survive across sessions.
</system-reminder>"
                                .into(),
                        }],
                    });
                }
            }

            if let Err(e) = self.session.persist().await {
                tracing::warn!(error = %e, "failed to persist session");
            }
            self.last_had_tool_uses = had_tool_uses_this_turn;
            return Ok(TurnOutcome {
                stop_reason,
                api_calls,
                tool_calls,
                usage,
            });
        }
    }

    async fn handle_tool_result(
        &mut self,
        tool_use_id: String,
        content: String,
        is_error: bool,
    ) -> Result<TurnOutcome, TurnError> {
        // Inject tool result into session message history (TS parity).
        // The caller (Agent::run) will feed the next input; if that input is a
        // User message, run_user_turn will pick up from the updated session state.
        self.session.push_message(ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error: Some(is_error),
            }],
        });
        Ok(TurnOutcome {
            stop_reason: "tool_result_received".into(),
            api_calls: 0,
            tool_calls: 0,
            usage: Usage::default(),
        })
    }

    /// Extract file paths from Read/Write/Edit tool uses in the session messages.
    ///
    /// Scans recent messages for `ToolUse` blocks with these tool names and
    /// extracts the `file_path` field from their input JSON. The collected
    /// paths are passed to `activate_conditional_skills_for_paths` so that
    /// skills with matching `paths` patterns are injected into context.
    fn extract_tool_file_paths(messages: &[ModelMessage]) -> Vec<PathBuf> {
        // Only look at messages from the current turn (last half of messages).
        let cutoff = if messages.len() > 40 {
            messages.len() / 2
        } else {
            0
        };
        let tool_names = ["Read", "Write", "Edit"];
        let mut paths: Vec<PathBuf> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for msg in messages.iter().skip(cutoff) {
            for block in &msg.content {
                if let ModelContentBlock::ToolUse { name, input, .. } = block {
                    if !tool_names.contains(&name.as_str()) {
                        continue;
                    }
                    if let Some(fp) = input.get("file_path").and_then(|v| v.as_str()) {
                        let path = PathBuf::from(fp);
                        if seen.insert(path.clone()) {
                            paths.push(path);
                        }
                    }
                }
            }
        }
        paths
    }

    fn build_tool_defs(&self) -> Vec<base::interface::model::ToolDef> {
        use std::collections::BTreeMap;
        let allowed = self.scene.tools();
        let disallowed = self.scene.disallowed_tools();
        // Combine built-in + MCP tools with dedup (TS parity: assembleToolPool).
        // Built-in tools take priority on name conflicts.
        let mut pool: BTreeMap<String, base::interface::model::ToolDef> = BTreeMap::new();
        // MCP tools first
        for t in self.mcp.tool_adapters() {
            pool.insert(
                t.name().to_string(),
                base::interface::model::ToolDef {
                    name: t.name().to_string(),
                    description: t.description().to_string(),
                    input_schema: t.input_schema(),
                },
            );
        }
        // Built-in tools overwrite on name conflict
        for t in self.tools.list() {
            pool.insert(
                t.name().to_string(),
                base::interface::model::ToolDef {
                    name: t.name().to_string(),
                    description: t.description().to_string(),
                    input_schema: t.input_schema(),
                },
            );
        }
        pool.into_values()
            .filter(|t| {
                if allowed.is_empty() {
                    !disallowed.contains(&t.name)
                } else {
                    allowed.contains(&t.name)
                }
            })
            .collect()
    }

    /// Compact session messages if token budget is exceeded.
    async fn compact_if_needed(&mut self) {
        // P2-3: Time-based micro-compact — clear old tool results before budget check.
        // TS parity: timeBasedMC in microCompact.ts. Runs even if token budget not exceeded.
        {
            let config = self.time_based_mc_config.clone();
            if config.max_age.is_some() {
                let ages = self.session.message_ages();
                let mc_result = compaction::time_based_mc::apply_time_based_mc(
                    &mut self.session.messages,
                    &config,
                    &ages,
                );
                if mc_result.cleared > 0 {
                    tracing::info!(
                        cleared = mc_result.cleared,
                        skipped = mc_result.skipped,
                        "time-based micro-compact applied"
                    );
                }
            }
        }

        // Cached micro-compact: time-driven cache edit generation.
        // When enabled, clears old tool results and records tool_use_ids as
        // cache_edits for the next API request. Gated by the `cached_microcompact`
        // feature flag. TS parity: `cachedMicrocompactPath()` in microCompact.ts.
        if self.cached_mc.should_run() {
            let cleared = self.cached_mc.run(&mut self.session.messages);
            if cleared > 0 {
                tracing::info!(
                    cleared,
                    "cached micro-compact applied — pending cache_edits generated"
                );
            }
        }

        // T2.6: Enforce per-message tool result budget BEFORE compaction
        // (TS parity: applyToolResultBudget in query.ts runs before microcompact).
        let budget_modified =
            compaction::compact::enforce_tool_result_budget(&mut self.session.messages);
        if budget_modified > 0 {
            tracing::debug!(modified = budget_modified, "tool result budget enforced");
        }

        // Feature 2 (#29): Reactive compact — proactive compaction before budget exhausted.
        // Uses token usage velocity to predict when compaction is needed and triggers early.
        // v2: Circuit breaker — skips compaction if consecutive failures exceeded limit.
        if self.settings.feature_flags.reactive_compact {
            let context_limit = self.scene.token_budget().compact_threshold;
            if context_limit > 0 {
                let current = self.session.token_count();
                let velocity = compaction::reactive::estimate_token_velocity(&self.session.messages);
                // Check circuit breaker before attempting compaction
                if self.compaction_state.circuit_open {
                    tracing::warn!(
                        failures = self.compaction_state.consecutive_failures,
                        "compaction circuit breaker open — skipping reactive compact"
                    );
                } else if compaction::reactive::should_compact_with_state(
                    current, context_limit, velocity, &self.compaction_state,
                ) {
                    tracing::info!(
                        current_tokens = current,
                        context_limit = context_limit,
                        "reactive compact triggered — proactively clearing old tool results"
                    );
                    let keep = self.scene.token_budget().compact_keep_recent.max(5);
                    let (compacted, _strategy) = compaction::compact::DefaultCompactor
                        .micro_compact(self.session.messages().to_vec(), keep);
                    if compacted.len() < self.session.messages.len() {
                        self.session.messages = compacted;
                        self.session.message_timestamps.truncate(self.session.messages.len());
                        self.compaction_state.record_success();
                        // P1-6: Run post-compact cleanup callbacks (cache clearing, etc.)
                        // TS parity: postCompactCleanup.ts
                        compaction::cleanup::run_cleanup_callbacks();
                        tracing::info!("reactive micro-compact completed");
                    } else {
                        self.compaction_state.record_failure();
                        tracing::warn!("reactive micro-compact had no effect");
                    }
                }
            }
        }

        let threshold = self.scene.token_budget().compact_threshold;
        // P1-2: Compact warning — inject system-reminder when approaching threshold.
        // TS parity: compactWarningState.ts. Warns at 80% of compact threshold
        // so the user is not surprised by a sudden compaction boundary.
        if threshold > 0 {
            let current = self.session.token_count();
            let warn_at = (threshold as f64 * 0.8) as usize;
            if current > warn_at && current <= threshold && !self.compact_warning_issued {
                let warn_msg = format!(
                    "<system-reminder>\n\
                     ⚠️ Context is at {:.0}% of the token budget ({}/{} tokens). \
                     The conversation will be compacted soon to make room. \
                     If you have pending work, wrap it up or ask the user what to preserve.\n\
                     </system-reminder>",
                    (current as f64 / threshold as f64 * 100.0).min(99.0),
                    current,
                    threshold,
                );
                self.session.push_message(ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text { text: warn_msg }],
                });
                self.compact_warning_issued = true;
            }
        }
        if threshold > 0 && self.session.token_count() > threshold {
            // P1: Fire PreCompact hook (TS parity: executePreCompactHooks)
            if self.hooks.has_hooks_for(hooks::config::HookEvent::PreCompact) {
                let hook_result = self.hooks.run(
                    hooks::config::HookEvent::PreCompact,
                    &hooks::HookInput {
                        hook_event_name: "PreCompact".into(),
                        session_id: self.session.session_id.clone(),
                        cwd: self.settings.paths.local_data_dir.display().to_string(),
                        permission_mode: "default".into(),
                        tool_input: Some(serde_json::json!({
                            "messages_before": self.session.messages.len(),
                            "token_count": self.session.token_count(),
                            "threshold": threshold,
                        })),
                        tool_name: None, tool_use_id: None,
                        tool_result: None, is_error: None, user_prompt: None,
                    },
                ).await;
                // P0-3: Respect hook decisions — discontinue or block compaction.
                if hook_result.discontinued() {
                    tracing::info!("PreCompact hook discontinued — skipping compaction");
                    return;
                }
                if hook_result.blocked().is_some() {
                    tracing::info!("PreCompact hook blocked — skipping compaction");
                    return;
                }
            }

            let keep = self.scene.token_budget().compact_keep_recent;
            let messages_before = self.session.messages.len();
            match self
                .compactor
                .compact(self.session.messages().to_vec(), threshold, keep)
                .await
            {
                Ok((mut compacted, result)) => {
                    // T1.4: Post-compact recovery — re-inject critical context
                    let recent_files = compaction::compact::extract_recent_reads(&compacted);
                    let recovery_ctx = compaction::compact::PostCompactContext {
                        recent_files,
                        invoked_skills: self.invoked_skills.clone(),
                        in_plan_mode: self.in_plan_mode,
                        plan_content: self.plan_content.clone(),
                        activated_tools: Vec::new(),
                        running_tasks: self.running_task_summaries.clone(),
                    };
                    if recovery_ctx.recent_files.is_empty()
                        && recovery_ctx.invoked_skills.is_empty()
                        && !recovery_ctx.in_plan_mode
                    {
                        // Skip recovery if nothing to inject
                    } else {
                        let recovery_msgs =
                            compaction::compact::build_post_compact_recovery(&recovery_ctx);
                        compacted.splice(0..0, recovery_msgs);
                    }
                    let messages_after = compacted.len();
                    self.session.messages = compacted;
                    self.session.message_timestamps.truncate(messages_after);
                    let (dropped_rounds, dropped_messages, estimated_tokens_saved) =
                        if let Some(ref proj) = result.projection {
                            (Some(proj.dropped_rounds), Some(proj.dropped_messages), Some(proj.estimated_tokens_saved))
                        } else {
                            (None, None, None)
                        };
                    let _ = self.event_tx.send(AgentEvent::CompactAction {
                        strategy: format!("{:?}", result.strategy),
                        messages_before,
                        messages_after,
                        turn_id: self.current_turn_id.clone(),
                        dropped_rounds,
                        dropped_messages,
                        estimated_tokens_saved,
                    });
                    // P1-2: Reset compact warning after successful compaction —
                    // the warning can fire again if the budget is exhausted again later.
                    self.compact_warning_issued = false;

                    // P2-1: Compact analysis — log token composition after compaction.
                    // TS parity: contextAnalysis.ts → analyzeContext().
                    {
                        let analysis = compaction::compact::analyze_context(&self.session.messages);
                        tracing::info!(
                            strategy = ?result.strategy,
                            messages_before,
                            messages_after,
                            tokens_before = result.tokens_before,
                            tokens_after = result.tokens_after,
                            "compaction completed"
                        );
                        tracing::debug!(
                            "{}",
                            compaction::compact::format_context_analysis(&analysis)
                        );
                    }

                    // P1: Fire PostCompact hook (TS parity: executePostCompactHooks)
                    if self.hooks.has_hooks_for(hooks::config::HookEvent::PostCompact) {
                        let _ = self.hooks.run(
                            hooks::config::HookEvent::PostCompact,
                            &hooks::HookInput {
                                hook_event_name: "PostCompact".into(),
                                session_id: self.session.session_id.clone(),
                                cwd: self.settings.paths.local_data_dir.display().to_string(),
                                permission_mode: "default".into(),
                                tool_input: Some(serde_json::json!({
                                    "strategy": format!("{:?}", result.strategy),
                                    "messages_before": messages_before,
                                    "messages_after": messages_after,
                                    "tokens_before": result.tokens_before,
                                    "tokens_after": result.tokens_after,
                                })),
                                tool_name: None, tool_use_id: None,
                                tool_result: None, is_error: None, user_prompt: None,
                            },
                        ).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to compact messages, continuing with full context");
                }
            }
        }
    }

    /// Build MCP server instructions text for system prompt injection.
    /// TS parity: getMcpInstructions / getMcpInstructionsSection in prompts.ts.
    fn build_mcp_instructions(&self) -> String {
        let instructions = self.mcp.server_instructions();
        if instructions.is_empty() {
            return String::new();
        }
        let mut text = String::from(
            "# MCP Server Instructions\n\n\
             The following MCP servers have provided instructions for how to \
             use their tools and resources:\n\n",
        );
        for instr in &instructions {
            text.push_str(&format!("## {}\n{}\n\n", instr.name, instr.instructions));
        }
        text
    }

    /// Build skills text for system prompt injection.
    /// TS parity: skill listing in claude-code system prompt.
    fn build_skills_text(&self) -> String {
        let skills = self.skills.list();
        // Filter out skills with disable_model_invocation: true
        let llm_skills: Vec<_> = skills.iter()
            .filter(|s| !s.disable_model_invocation)
            .collect();
        if llm_skills.is_empty() {
            return String::new();
        }

        // P1-3: Budget-aware skill listing. TS parity: SKILL_BUDGET_CONTEXT_PERCENT = 1%.
        // Budget: 1% of context window tokens * 4 chars/token, fallback 8000 chars.
        let context_window = self.scene.token_budget().compact_threshold;
        let budget_chars = if context_window > 0 {
            ((context_window as f64 * 0.01) * 4.0) as usize
        } else {
            8000
        };
        const HEADER_CHARS: usize = 22;
        const PER_ENTRY_OVERHEAD: usize = 6;
        const MAX_DESC_CHARS: usize = 250;

        let available_budget = budget_chars.saturating_sub(HEADER_CHARS);
        let per_entry = (available_budget / llm_skills.len().max(1)).saturating_sub(PER_ENTRY_OVERHEAD);
        let desc_cap = per_entry.min(MAX_DESC_CHARS);

        let mut text = String::from("## Available Skills\n\n");
        if desc_cap < 20 {
            // Names-only fallback
            for s in &llm_skills {
                text.push_str(&format!("- **{}**\n", s.name));
            }
        } else {
            for s in &llm_skills {
                let desc = if s.description.len() > desc_cap {
                    format!("{}…", &s.description[..desc_cap])
                } else {
                    s.description.clone()
                };
                if let Some(when) = &s.argument_hint {
                    text.push_str(&format!("- **{}**: {} (args: {})\n", s.name, desc, when));
                } else {
                    text.push_str(&format!("- **{}**: {}\n", s.name, desc));
                }
            }
        }
        text
    }

    /// Build prompt blocks, tool definitions, and clone messages for the model call.
    fn build_prompt_for_turn(&self) -> (Vec<PromptBlock>, Vec<ToolDef>, Vec<ModelMessage>) {
        let mcp_instructions = self.build_mcp_instructions();
        let mcp_ref: Option<&str> = if mcp_instructions.is_empty() { None } else { Some(&mcp_instructions) };
        let skills_text = self.build_skills_text();
        let skills_ref: Option<&str> = if skills_text.is_empty() { None } else { Some(&skills_text) };
        // Build comma-separated tool names for dynamic session guidance
        let tool_names: String = self.tools.list().iter()
            .map(|t| t.name().to_string())
            .chain(self.mcp.tool_adapters().iter().map(|t| t.name().to_string()))
            .collect::<Vec<_>>()
            .join(",");
        let tools_ref: Option<Cow<'_, str>> = if tool_names.is_empty() { None } else { Some(Cow::Owned(tool_names)) };
        let mut ctx = build_prompt_context(
            &self.settings, &self.session, self.frozen.as_ref(), mcp_ref, skills_ref,
        );
        ctx.available_tools = tools_ref;
        let prompt_blocks = assemble_prompt(
            self.scene.as_ref(),
            &self.settings,
            &self.memory_store,
            &ctx,
            skills_ref,  // skills_text
            mcp_ref,     // mcp_instructions
        );
        let tool_defs = self.build_tool_defs();
        let messages = self.session.messages().to_vec();
        (prompt_blocks, tool_defs, messages)
    }

    /// Handle model Overloaded error by switching to fallback model and retrying.
    async fn handle_overloaded_recovery(
        &mut self,
        tool_defs: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        effective_max_tokens: u32,
        effective_model: &mut String,
        cancel: CancellationToken,
    ) -> Result<ModelStream, TurnError> {
        if let Some(ref fallback) = self.settings.model.fallback_model {
            tracing::warn!(
                model = %*effective_model,
                fallback = %fallback,
                "model overloaded, switching to fallback"
            );
            *effective_model = fallback.clone();
            let fb_ctx = build_prompt_context(&self.settings, &self.session, self.frozen.as_ref(), None, None);
            let fb_prompt = assemble_prompt(
                self.scene.as_ref(),
                &self.settings,
                &self.memory_store,
                &fb_ctx,
                None,
                None,
            );
            self.model
                .stream(
                    fb_prompt,
                    tool_defs,
                    messages,
                    base::interface::model::StreamParams {
                        model: fallback.clone(),
                        max_tokens: effective_max_tokens,
                        thinking_mode: self.settings.model.thinking_mode.clone(),
                        fallback_model: None,
                        cache_edits: vec![],
                    },
                    cancel,
                )
                .await
                .map_err(|e| TurnError::Model(format!("failed to stream model response: {}", e)))
        } else {
            Err(TurnError::Model(format!(
                "model overloaded and no fallback configured: {}",
                *effective_model
            )))
        }
    }

    /// Handle max_tokens stop reason by escalating the limit and injecting a continuation message.
    /// Returns true if recovery was triggered (caller should continue the loop).
    /// TS parity: max_tokens escalation path (query.ts:1188-1256):
    ///   1. First attempt: escalate to 64K (ESCALATED_MAX_TOKENS_OVERRIDE)
    ///   2. Subsequent: escalate to 8K + continuation message (up to 3 total retries)
    fn handle_max_tokens_recovery(
        &mut self,
        stop_reason: &str,
        max_tokens_recovery: &mut u32,
        effective_max_tokens: &mut u32,
    ) -> bool {
        const MAX_TOKENS_RECOVERY_LIMIT: u32 = 3;
        const ESCALATED_MAX_TOKENS: u32 = 8000;
        const ESCALATED_64K: u32 = 64000;

        if stop_reason != "max_tokens" || *max_tokens_recovery >= MAX_TOKENS_RECOVERY_LIMIT {
            return false;
        }
        *max_tokens_recovery += 1;
        // First attempt: try 64K override (TS parity: ESCALATED_MAX_TOKENS in query.ts:1199)
        if *max_tokens_recovery == 1 && *effective_max_tokens < ESCALATED_64K {
            *effective_max_tokens = ESCALATED_64K;
            tracing::info!("max_tokens escalated to 64K");
            return true;
        }
        if *effective_max_tokens < ESCALATED_MAX_TOKENS {
            *effective_max_tokens = ESCALATED_MAX_TOKENS;
        }
        self.session.push_message(ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: "Output token limit hit. Resume directly — no apology, no \
                       recap of what you were doing. Pick up mid-thought if that \
                       is where the cut happened. Break remaining work into \
                       smaller pieces."
                    .into(),
            }],
        });
        tracing::info!(
            recovery = *max_tokens_recovery,
            max_tokens = *effective_max_tokens,
            "max_tokens recovery triggered"
        );
        true
    }
}

/// TS parity: `query/tokenBudget.ts::checkTokenBudget` — pure continuation
/// decision. Continue while accumulated output < 90% of target AND not
/// diminishing. Diminishing: ≥3 continuations with both this and the previous
/// delta under 500 tokens. No hard cap (ref has none; was capped at 10).
fn should_continue_token_budget(
    accumulated: u64,
    target: u64,
    count: u32,
    this_delta: u64,
    last_delta: u64,
) -> bool {
    let threshold = (target as f64 * 0.9) as u64;
    let is_diminishing = count >= 3 && this_delta < 500 && last_delta < 500;
    !is_diminishing && accumulated < threshold
}

/// Context bundle for tool execution — owned to survive async closures.
#[derive(Clone)]
pub(crate) struct ToolExecCtx {
    pub tools: Arc<base::tool::InMemoryToolRegistry>,
    pub cwd: std::path::PathBuf,
    pub session_id: String,
    pub turn_no: u32,
    pub telemetry_handle: telemetry::TelemetryHandle,
    pub turn_id: String,
    pub cancel: tokio_util::sync::CancellationToken,
}

/// Execute a single tool and record telemetry. Free function for streaming executor.
pub(crate) async fn execute_tool_with_telemetry(
    ctx: &ToolExecCtx,
    name: &str,
    input: serde_json::Value,
) -> Result<(String, Option<Vec<serde_json::Value>>), String> {
    execute_tool_inner(ctx, name, input).await
}

async fn execute_tool_inner(
    ctx: &ToolExecCtx,
    name: &str,
    input: serde_json::Value,
) -> Result<(String, Option<Vec<serde_json::Value>>), String> {
    let tool = ctx.tools
        .get(name)
        .ok_or_else(|| format!("Tool not found: {name}"))?;
    let tool_ctx = ToolContext {
        cwd: ctx.cwd.clone(),
        session_id: ctx.session_id.clone(),
        turn_no: ctx.turn_no,
        sandbox: Default::default(),
        cancel: ctx.cancel.clone(),
        additional_writable_dirs: vec![],
        snapshot_file: None,
        effects: None,
        running_tasks: None,
        dangerously_disable_sandbox: true,
        max_file_read_bytes: 0,
        permission_mode: base::tool::PermissionMode::default(),
        config: Arc::new(base::context::EngineConfig::defaults_for("unknown")),
        session: Arc::new(base::context::SessionState::new(ctx.cwd.clone())),
        tool_use_id: String::new(),
        agent: None,
        parent_messages: None,
        agent_depth: 0,
        events_tx: None,
    };
    let tool_start = std::time::Instant::now();
    let result = tool.call(input, tool_ctx, base::tool::ProgressSender::noop("")).await;
    let latency_ms = tool_start.elapsed().as_millis() as f64;
    let is_error = result.is_err();
    let _ = ctx.telemetry_handle
        .record(telemetry::TelemetryEvent::tool_execution(
            &ctx.session_id,
            ctx.turn_no,
            Some(ctx.turn_id.clone()),
            telemetry::ToolExecutionPayload {
                tool_name: name.to_string(),
                tool_use_id: String::new(),
                outcome: if is_error { telemetry::ToolOutcome::Failed } else { telemetry::ToolOutcome::Succeeded },
                is_error,
                error_message: None,
                latency_ms: latency_ms as u64,
                input_json_size: 0,
                result_content_size: 0,
                user_approved: true,
            },
        ));
    match result {
        Ok(r) => {
            let text = match r.content {
                ToolResultContent::Text(t) => t,
                ToolResultContent::Blocks(b) => format!("{:?}", b),
            };
            Ok((text, r.new_messages))
        }
        Err(e) => Err(e.to_string()),
    }
}

fn build_prompt_context<'a>(
    settings: &'a base::interface::settings::Settings,
    _session: &'a session::session::SessionManager,
    frozen: Option<&'a base::frozen::FrozenContext>,
    mcp_instructions: Option<&'a str>,
    skills_text: Option<&'a str>,
) -> ScenePromptContext<'a> {
    let (is_git, git_branch, is_worktree, git_status, memory_index, output_style) =
        if let Some(f) = frozen {
            (
                f.is_git,
                f.git_branch.clone(),
                f.is_worktree,
                f.git_status.clone(),
                f.memory_index.clone(),
                f.output_style.as_ref().map(|os| os.content.clone()),
            )
        } else {
            (false, None, false, None, None, None)
        };
    ScenePromptContext {
        cwd: Cow::Owned(settings.paths.local_data_dir.display().to_string()),
        os: Cow::Borrowed(std::env::consts::OS),
        shell: Cow::Borrowed("/bin/bash"),
        home_dir: Cow::Owned(std::env::var("HOME").unwrap_or_else(|_| "/home/user".into())),
        date: Cow::Owned(chrono_now()),
        model_name: Cow::Owned(settings.model.model_name.clone()),
        skills_text: skills_text.map(|s| Cow::Owned(s.to_string())),
        mcp_instructions: mcp_instructions.map(|s| Cow::Owned(s.to_string())),
        session_memory: memory_index.map(Cow::Owned),
        is_git,
        git_branch: git_branch.map(Cow::Owned),
        git_status: git_status.map(Cow::Owned),
        is_worktree,
        language: settings.language.clone().map(Cow::Owned),
        scratchpad_dir: settings.paths.local_data_dir.join(".atta/scratchpad").to_str().map(|s| Cow::Owned(s.to_string())),
        output_style_content: output_style.map(Cow::Owned),
        available_tools: None, // populated by caller if needed
    }
}

fn chrono_now() -> String {
    use time::OffsetDateTime;
    let now = OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    )
}

/// Count StructuredOutput tool uses in the current session messages.
/// TS parity: `countToolCalls(this.mutableMessages, SYNTHETIC_OUTPUT_TOOL_NAME)`.
fn count_structured_output_calls(messages: &[base::interface::model::ModelMessage]) -> u32 {
    messages
        .iter()
        .filter(|m| {
            m.role == base::interface::model::MessageRole::Assistant
                && m.content.iter().any(|b| {
                    matches!(b, base::interface::model::ModelContentBlock::ToolUse { name, .. } if name == "StructuredOutput")
                })
        })
        .count() as u32
}

/// Parse a token budget directive from user input.
///
/// Supports:
/// - Shorthand: `+500k`, `+2M`, `+1B` (at start of message, after optional whitespace)
/// - Natural language: `spend 2M tokens`, `use 1B tokens`, `set 500k output tokens`
///   (case-insensitive, anywhere in message)
///
/// Returns the token target as raw token count, or `None` if no directive found.
fn parse_token_budget_directive(input: &str) -> Option<u64> {
    let trimmed = input.trim();

    // 1. Shorthand: +500k, +2M, +1B at message start
    if let Some(after_plus) = trimmed.strip_prefix('+') {
        if let Some(tokens) = parse_suffixed_number(after_plus) {
            if tokens > 0 && tokens <= 2_000_000_000 {
                return Some(tokens);
            }
        }
    }

    // 2. Natural language patterns: look for "spend 2M tokens", "use 500k output tokens", etc.
    let lower = trimmed.to_lowercase();
    let actions = ["spend ", "use ", "set ", "budget "];
    for action in &actions {
        if let Some(idx) = lower.find(action) {
            let after_action = &lower[idx + action.len()..];
            if let Some(tokens) = extract_suffixed_number_from_start(after_action) {
                if tokens > 0 && tokens <= 2_000_000_000 {
                    return Some(tokens);
                }
            }
        }
    }

    // 3. Pattern: <number><suffix> token(s) at end of message (implicit)
    if let Some(idx) = lower.rfind(" token") {
        let before = &lower[..idx].trim();
        if let Some(tokens) = extract_suffixed_number_from_end(before) {
            if tokens > 0 && tokens <= 2_000_000_000 {
                return Some(tokens);
            }
        }
    }

    None
}

/// Parse a suffixed number like "500k" or "2M" or "1B" or "1000".
fn parse_suffixed_number(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_str, rest) = split_numeric_prefix(s)?;
    let rest = rest.trim();

    // Check for suffix character
    let multiplier = match rest.chars().next() {
        Some('k' | 'K') => 1_000u64,
        Some('m' | 'M') => 1_000_000u64,
        Some('b' | 'B') => 1_000_000_000u64,
        _ => {
            // No suffix, just the number
            let n: u64 = num_str.parse().ok()?;
            return Some(n);
        }
    };

    let n: u64 = num_str.parse().ok()?;
    n.checked_mul(multiplier)
}

/// Split off the numeric prefix from a string, returning (num_str, rest).
fn split_numeric_prefix(s: &str) -> Option<(&str, &str)> {
    let s = s.trim();
    let end = s
        .chars()
        .position(|c| !c.is_ascii_digit())
        .unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    Some((&s[..end], &s[end..]))
}

/// Extract a suffixed number from the start of a string (for natural language matching).
fn extract_suffixed_number_from_start(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num_str, rest) = split_numeric_prefix(s)?;
    let rest = rest.trim();

    // Check for suffix character
    let mut multiplier = 1u64;
    if let Some(c) = rest.chars().next() {
        multiplier = match c {
            'k' | 'K' => 1_000u64,
            'm' | 'M' => 1_000_000u64,
            'b' | 'B' => 1_000_000_000u64,
            _ => 1u64,
        };
    }

    let n: u64 = num_str.parse().ok()?;
    n.checked_mul(multiplier)
}

/// Extract a suffixed number from the end of a string.
/// E.g., "spend 500k" -> Some(500_000)
fn extract_suffixed_number_from_end(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();

    // Check for suffix at the end
    let mut end = len;
    let mut multiplier = 1u64;
    if end > 0 {
        match chars[end - 1] {
            'k' | 'K' => { multiplier = 1_000; end -= 1; }
            'm' | 'M' => { multiplier = 1_000_000; end -= 1; }
            'b' | 'B' => { multiplier = 1_000_000_000; end -= 1; }
            _ => {}
        }
    }

    if end == 0 || !chars[end - 1].is_ascii_digit() {
        return None;
    }

    // Find start of digits
    let mut start = end;
    while start > 0 && chars[start - 1].is_ascii_digit() {
        start -= 1;
    }

    if start == end {
        return None;
    }

    let num_str: String = chars[start..end].iter().collect();
    let n: u64 = num_str.parse().ok()?;
    n.checked_mul(multiplier)
}

/// Strip the token budget directive from user input.
/// Handles shorthand (`+500k task`) and natural language directives.
fn strip_token_budget_directive(input: &str) -> String {
    let trimmed = input.trim();

    // 1. Shorthand at start: "+500k", "+2M", "+1B"
    if let Some(rest) = trimmed.strip_prefix('+') {
        if parse_suffixed_number(rest).is_some() {
            let (_, after_num) = split_numeric_prefix(rest).unwrap_or(("", rest));
            let after_suffix = after_num
                .chars()
                .next()
                .and_then(|c| {
                    if matches!(c, 'k' | 'K' | 'm' | 'M' | 'b' | 'B') {
                        Some(&after_num[1..])
                    } else {
                        None
                    }
                })
                .unwrap_or(after_num);
            return after_suffix.trim().to_string();
        }
    }

    // 2. Natural language directives
    let lower = trimmed.to_lowercase();
    let actions = ["spend ", "use ", "set ", "budget "];
    for action in &actions {
        if let Some(idx) = lower.find(action) {
            // Found a potential action word — check if followed by a number
            let after_action = &trimmed[idx + action.len()..];
            let (_num_str, after_num) = match split_numeric_prefix(after_action) {
                Some(pair) => pair,
                None => continue,
            };

            // Check for optional suffix
            let after_suffix = if let Some(c) = after_num.chars().next() {
                if matches!(c, 'k' | 'K' | 'm' | 'M' | 'b' | 'B') {
                    &after_num[1..]
                } else {
                    after_num
                }
            } else {
                after_num
            };

            // Skip optional " output" and " tokens"
            let after_skip = after_suffix
                .strip_prefix(" output ")
                .or_else(|| after_suffix.strip_prefix(" output"))
                .or_else(|| after_suffix.strip_prefix(" "))
                .unwrap_or(after_suffix);
            let after_skip = after_skip
                .strip_prefix("tokens")
                .or_else(|| after_skip.strip_prefix("token"))
                .unwrap_or(after_skip);

            // Combine prefix + remaining text
            let prefix = &trimmed[..idx].trim();
            let remaining = after_skip.trim();
            let result = if prefix.is_empty() && remaining.is_empty() {
                String::new()
            } else if prefix.is_empty() {
                remaining.to_string()
            } else if remaining.is_empty() {
                prefix.to_string()
            } else {
                format!("{} {}", prefix, remaining)
            };
            return result.trim().to_string();
        }
    }

    // No directive found — return original
    input.to_string()
}

#[derive(Debug, Clone, Default)]
pub struct TurnOutcome {
    pub stop_reason: String,
    pub api_calls: u32,
    pub tool_calls: u32,
    pub usage: Usage,
}

#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error("model error: {0}")]
    Model(String),
    #[error("shutdown")]
    Shutdown,
    #[error("internal: {0}")]
    Internal(String),
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use base::interface::settings::ThinkingMode;

    #[test]
    fn turn_outcome_default() {
        let outcome = TurnOutcome::default();
        assert_eq!(outcome.api_calls, 0);
    }

    // ── Token budget continuation decision tests (TS parity: tokenBudget.ts) ──

    #[test]
    fn token_budget_continuation_stops_at_90pct() {
        // 89% of target → continue
        assert!(should_continue_token_budget(89_000, 100_000, 0, 1_000, 0));
        // 90% of target → stop
        assert!(!should_continue_token_budget(90_000, 100_000, 0, 1_000, 0));
        // High continuation count alone does NOT stop (no hard cap; was 10).
        assert!(should_continue_token_budget(10_000, 100_000, 20, 1_000, 1_000));
    }

    #[test]
    fn token_budget_diminishing_returns_stops() {
        // ≥3 continuations, both deltas <500 → stop even below 90%
        assert!(!should_continue_token_budget(10_000, 100_000, 3, 400, 400));
        // ≥3 but large delta → continue
        assert!(should_continue_token_budget(10_000, 100_000, 3, 1_000, 1_000));
        // <3 continuations, small deltas → still continue
        assert!(should_continue_token_budget(10_000, 100_000, 2, 400, 400));
    }

    // ── Token budget directive parsing tests ──

    #[test]
    fn parse_shorthand_500k() {
        assert_eq!(parse_token_budget_directive("+500k do this task"), Some(500_000));
    }

    #[test]
    fn parse_shorthand_2m() {
        assert_eq!(parse_token_budget_directive("+2M"), Some(2_000_000));
    }

    #[test]
    fn parse_shorthand_1b() {
        assert_eq!(parse_token_budget_directive("+1B"), Some(1_000_000_000));
    }

    #[test]
    fn parse_spend_natural_language() {
        assert_eq!(
            parse_token_budget_directive("spend 2M tokens on refactoring"),
            Some(2_000_000)
        );
    }

    #[test]
    fn parse_use_natural_language() {
        assert_eq!(
            parse_token_budget_directive("use 500k output tokens"),
            Some(500_000)
        );
    }

    #[test]
    fn parse_set_natural_language() {
        assert_eq!(
            parse_token_budget_directive("set 1B output tokens"),
            Some(1_000_000_000)
        );
    }

    #[test]
    fn parse_budget_natural_language() {
        assert_eq!(
            parse_token_budget_directive("budget 100k tokens for testing"),
            Some(100_000)
        );
    }

    #[test]
    fn parse_no_directive() {
        assert_eq!(parse_token_budget_directive("hello world"), None);
    }

    #[test]
    fn parse_empty_string() {
        assert_eq!(parse_token_budget_directive(""), None);
    }

    #[test]
    fn strip_shorthand_500k() {
        assert_eq!(
            strip_token_budget_directive("+500k do this task"),
            "do this task"
        );
    }

    #[test]
    fn strip_spend_natural_language() {
        assert_eq!(
            strip_token_budget_directive("spend 2M tokens refactor this code"),
            "refactor this code"
        );
    }

    #[test]
    fn strip_no_directive() {
        assert_eq!(
            strip_token_budget_directive("hello world"),
            "hello world"
        );
    }

    #[test]
    fn parse_suffixed_number_k() {
        assert_eq!(parse_suffixed_number("500k"), Some(500_000));
    }

    #[test]
    fn parse_suffixed_number_m() {
        assert_eq!(parse_suffixed_number("2M"), Some(2_000_000));
    }

    #[test]
    fn parse_suffixed_number_plain() {
        assert_eq!(parse_suffixed_number("100"), Some(100));
    }

    #[test]
    fn parse_suffixed_number_invalid() {
        assert_eq!(parse_suffixed_number("abc"), None);
    }

    #[test]
    fn test_token_budget_exceeds_limit() {
        // Above 2B should be rejected
        assert_eq!(parse_token_budget_directive("+999999999999999999"), None);
    }

    #[test]
    fn prompt_context_includes_os() {
        let settings = base::interface::settings::Settings {
            model: base::interface::settings::ModelSettings {
                api_type: base::provider::ApiType::Anthropic,
                base_url: "https://api.example.com".into(),
                auth_token: "test".into(),
                model_name: "test-model".into(),
                max_tokens: 4096,
                thinking_mode: ThinkingMode::Auto,
                fallback_model: None,
            },
            paths: base::interface::settings::PathSettings {
                user_data_dir: "/tmp/user".into(),
                local_data_dir: "/tmp/local".into(),
            },
            execution: Default::default(),
            compaction: Default::default(),
            sandbox: Default::default(),
            instruction_file: None,
            prompt_append: None,
            prompt_override: None,
            vcr: None,
            telemetry_url: None,
            memory_enabled: true,
            permission_mode: base::interface::settings::PermissionMode::Default,
            permission_rules: Vec::new(),
            hooks_config: None,
            mcp_servers: Vec::new(),
            language: None,
        feature_flags: Default::default(),
            session_dir: None,
        };
        let session = session::session::SessionManager::in_memory(None);
        let ctx = build_prompt_context(&settings, &session, None, None, None);
        assert_eq!(ctx.os, std::env::consts::OS);
    }
}

// ── P0-2: Post-turn memory extraction (TS parity: initExtractMemories) ──

/// Extract durable memories from recent conversation messages using a lightweight
/// Haiku call, then persist them to the MemoryStore. Runs asynchronously after
/// each complete turn — failures are silently logged, never block the user.
///
/// TS parity: `initExtractMemories()` in extractMemories.ts, called via
/// handleStopHooks in stopHooks.ts after each complete query loop.
pub(crate) async fn extract_memories_after_turn(
    store: &MemoryStore,
    messages: &[ModelMessage],
    model: &dyn base::interface::model::Model,
) {
    // Only extract if there are messages worth analyzing
    if messages.len() < 2 {
        return;
    }

    // Build a lightweight prompt asking the model to extract memories
    let recent: Vec<&ModelMessage> = messages.iter().rev().take(20).collect();
    let messages_text: String = recent
        .iter()
        .rev()
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
        return;
    }

    let prompt = "\
Extract any durable memories from this conversation excerpt. A durable \
memory is a fact about the user, project, or workflow that should persist \
across sessions. Only extract memories that are NOT derivable from the \
current codebase or git history.\n\n\
For each memory, return a JSON object with:\n\
- name: short kebab-case slug\n\
- description: 1-line summary used to decide relevance during recall\n\
- content: the fact; for feedback/project, follow with **Why:** and **How to apply:** lines\n\
- type: one of [user, feedback, project, reference]\n\
- confidence: 0.0-1.0 (default 0.8)\n\n\
Return only a JSON array of memories. If nothing is worth saving, return [].";

    use base::interface::settings::ThinkingMode;
    let request_messages = vec![
        ModelMessage {
            role: MessageRole::User,
            content: vec![
                ModelContentBlock::Text { text: prompt.to_string() },
                ModelContentBlock::Text { text: messages_text },
            ],
        },
    ];
    let params = base::interface::model::StreamParams {
        model: "claude-haiku-4-5-20251001".into(),
        max_tokens: 2000,
        thinking_mode: ThinkingMode::Off,
        fallback_model: None,
        cache_edits: vec![],
    };
    let mut full_text = String::new();
    let stream_result = model
        .stream(
            vec![],
            vec![],
            request_messages,
            params,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

    let Ok(mut stream) = stream_result else {
        return;
    };

    use futures::StreamExt;
    while let Some(Ok(event)) = stream.next().await {
        if let base::interface::model::ModelEvent::TextDelta { text } = event {
            full_text.push_str(&text);
        }
    }

    // Parse extracted memories
    let memories: Vec<DurableMemory> =
        if let Ok(parsed) = serde_json::from_str::<Vec<serde_json::Value>>(&full_text) {
            parsed
                .iter()
                .filter_map(|mem| {
                    let name = mem["name"].as_str()?.to_string();
                    let description = mem["description"].as_str().unwrap_or("").to_string();
                    let content = mem["content"].as_str().unwrap_or("").to_string();
                    let memory_type = match mem["type"].as_str().unwrap_or("user") {
                        "feedback" => MemoryType::Feedback,
                        "project" => MemoryType::Project,
                        "reference" => MemoryType::Reference,
                        _ => MemoryType::User,
                    };
                    let confidence = mem["confidence"].as_f64().unwrap_or(0.8);
                    if name.is_empty() || confidence < 0.3 {
                        return None;
                    }
                    let timestamp = time::OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Iso8601::DEFAULT)
                        .unwrap_or_else(|_| "2026-01-01T00:00:00Z".to_string());
                    Some(DurableMemory {
                        name,
                        description,
                        memory_type,
                        content,
                        source_session_id: String::new(),
                        confidence,
                        last_seen: timestamp,
                        recall_count: 0,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };

    if !memories.is_empty() {
        if let Err(e) = store.persist_batch(memories) {
            tracing::debug!(error = %e, "auto memory extraction: persist failed");
        }
    }
}
