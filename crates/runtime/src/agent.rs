//! Engine struct and builder — the central AGENT orchestrator.

use base::interface::event::AgentEvent;
use base::interface::memory::MemoryStore;
use base::interface::model::Model;
use base::interface::permission::Permission;
use base::interface::scene::AgentScene;
use base::interface::settings::Settings;
use base::tool::InMemoryToolRegistry;
use compaction::cached::CachedMicroCompact;
use compaction::compact::{Compactor, DefaultCompactor};
use hooks::HookRunner;
use mcp::manager::McpManager;
use session::session::SessionManager;
use std::path::PathBuf;
use std::sync::Arc;
use telemetry::perf::PerfCollector;
use telemetry::{TelemetryHandle, TelemetryRecorder};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing;

// ── Channel types ──

#[derive(Debug)]
pub enum InputMessage {
    User {
        content: String,
        attachments: Vec<Attachment>,
        turn_id: String,
    },
    ToolResult {
        tool_use_id: String,
        name: String,
        content: String,
        is_error: bool,
    },
    PermissionResponse {
        prompt_id: String,
        decision: PermissionDecision,
    },
    System {
        kind: EngineCommand,
        content: String,
    },
}

#[derive(Debug, Clone)]
pub struct Attachment {
    pub path: String,
    pub content: Option<String>,
}

/// Where a `PermitAlways` decision's effect should be retained.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistScope {
    /// In-memory only, this session.
    Session,
    /// In-memory for this session AND written to `.atta/settings.local.json`
    /// so future sessions in this project also allow it.
    Local,
}

/// Wire shape: `{"type":"permit"}` / `{"type":"deny","reason":"..."}` /
/// `{"type":"permit_always","scope":"session"}` /
/// `{"type":"permit_always","scope":"local"}` — what
/// `session.respondToPrompt` (daemon RPC) parses out of its `decision`
/// param and what `InputMessage::PermissionResponse` carries.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PermissionDecision {
    Permit,
    Deny {
        reason: String,
    },
    /// Like `Permit`, but also persists an Allow rule beyond this single
    /// tool call — see `base::interface::permission::Permission::add_persistent_allow`.
    /// `scope: Session` keeps it in-memory only (rest of this session);
    /// `scope: Local` additionally writes it to `.atta/settings.local.json`
    /// so future sessions in this project inherit it too.
    PermitAlways {
        scope: PersistScope,
    },
}

#[derive(Debug, Clone)]
pub enum EngineCommand {
    SetSessionId,
    CompactNow,
    RefreshMcp,
    UpdateModel,
    Shutdown,
}

pub type InputSender = mpsc::UnboundedSender<InputMessage>;
pub type InputReceiver = mpsc::UnboundedReceiver<InputMessage>;
pub type EventSender = mpsc::UnboundedSender<AgentEvent>;
pub type EventReceiver = mpsc::UnboundedReceiver<AgentEvent>;

// ── Engine ──

pub struct Agent {
    pub(crate) scene: Arc<dyn AgentScene>,
    pub(crate) model: Arc<dyn Model>,
    pub(crate) tools: Arc<InMemoryToolRegistry>,
    pub(crate) settings: Arc<Settings>,
    /// Real `EngineConfig` derived from `settings` via `EngineConfig::from_settings`
    /// — the tool-dispatch closure (`ToolExecCtx.config`) clones this instead of
    /// the `defaults_for("unknown")` placeholder it used to hardcode, so every
    /// tool call sees the actual configured sandbox/permission/shell-execution
    /// settings, not silent defaults.
    pub(crate) config: Arc<base::context::EngineConfig>,
    pub(crate) permission: Arc<dyn Permission>,
    /// Permission requests currently awaiting a host response, keyed by
    /// `prompt_id`. `execute_tool_inner` registers a `oneshot` sender here
    /// when `Permission::check` returns `PermissionOutcome::Prompt`, then
    /// awaits the paired receiver (with a timeout); the
    /// `InputMessage::PermissionResponse` branch below looks the sender up
    /// by `prompt_id` and fires it. A `prompt_id` with no entry (already
    /// timed out, or a stale/duplicate response) is silently ignored.
    pub(crate) pending_permissions: Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, tokio::sync::oneshot::Sender<PermissionDecision>>,
        >,
    >,
    pub(crate) memory_store: Arc<MemoryStore>,
    pub(crate) session: SessionManager,
    pub(crate) perf: Arc<PerfCollector>,

    pub(crate) compactor: Arc<dyn Compactor>,
    pub(crate) hooks: Arc<HookRunner>,
    pub(crate) skills: std::sync::Arc<skills::manager::SkillManager>,
    pub(crate) commands: std::sync::Arc<crate::commands::CommandRegistry>,
    pub(crate) mcp: McpManager,

    pub(crate) telemetry_handle: TelemetryHandle,
    pub(crate) current_turn_id: String,
    /// Compaction circuit breaker state — tracks consecutive failures and
    /// prevents infinite compaction loops. TS parity: AutoCompactTrackingState.
    pub(crate) compaction_state: compaction::reactive::CompactionState,
    /// Session-start frozen environment snapshot. Computed lazily on first turn.
    /// TS parity: `getSystemContext()` + `getUserContext()` in context.ts.
    pub(crate) frozen: Option<base::frozen::FrozenContext>,
    /// Pre-read AGENTS.md/CLAUDE.md content for userContext injection (TS parity).
    pub(crate) claude_md_content: Option<String>,
    /// Whether CLAUDE.md has been injected as a synthetic user message this session.
    pub(crate) claude_md_injected: bool,
    /// Track invoked skill names during the turn (for post-compact recovery T1.4).
    pub(crate) invoked_skills: Vec<String>,
    /// Whether the previous turn had tool uses (findWritePivot guard for skill
    /// discovery prefetch). TS parity: findWritePivot in query.ts.
    pub(crate) last_had_tool_uses: bool,
    /// Whether the agent is currently in plan mode (for post-compact recovery T1.4).
    pub(crate) in_plan_mode: bool,
    /// Plan file content when in plan mode (for post-compact recovery T1.4).
    pub(crate) plan_content: Option<String>,
    /// Running background task summaries: (task_id, status). Populated by AgentTool.
    pub(crate) running_task_summaries: Vec<(String, String)>,
    /// Count of permission denials in the current session (TS parity).
    pub(crate) permission_denial_count: u32,
    /// Whether a compact warning has been issued this cycle. Reset after compaction
    /// so the warning can fire again if the token budget is exhausted again later.
    /// P1-2: TS parity — compactWarningState.ts.
    pub(crate) compact_warning_issued: bool,
    /// P2-3: Time-based micro-compact configuration. Controls when old tool
    /// results are cleared based on wall-clock age. Default: 15 minutes.
    /// TS parity: timeBasedMCConfig.ts.
    pub(crate) time_based_mc_config: compaction::time_based_mc::TimeBasedMcConfig,
    /// Cached micro-compact state: time-driven cache edit generation.
    /// When enabled, clears old tool results and records their tool_use_ids
    /// as `cache_edits` to send to the Anthropic API, avoiding cache invalidation.
    /// TS parity: `cachedMCState` in microCompact.ts.
    pub(crate) cached_mc: CachedMicroCompact,
    /// Team ID if this agent is a worker in a team (TS parity: teammate lifecycle hooks).
    pub(crate) team_id: Option<String>,
    /// Orphaned permission from a previous session (for resume recovery, TS parity).
    pub(crate) orphaned_permission: Option<crate::agent::PermissionDecision>,
    /// Whether we've already handled the orphaned permission this session.
    pub(crate) has_handled_orphaned_permission: bool,
    /// Message replay / acknowledgement state (TS parity).
    #[allow(dead_code)]
    pub(crate) messages_to_ack: Vec<String>,
    /// Output token budget target (e.g., 500k, 2M). `None` = no budget active.
    /// TS parity: `outputTokenTarget` in query.ts.
    pub(crate) output_token_target: Option<u64>,
    /// Accumulated output tokens for the current budget session.
    /// Reset each time a new budget target is set.
    pub(crate) accumulated_output_tokens: u64,
    /// How many continuation turns have been injected for the current budget.
    /// Guarded by diminishing-returns (TS parity: tokenBudget.ts), not a hard cap.
    pub(crate) token_budget_continuation_count: u32,
    /// Output-token delta from the previous continuation — for diminishing-returns
    /// detection (TS parity: tokenBudget.ts lastDeltaTokens).
    pub(crate) last_delta_tokens: u64,
    pub(crate) input_rx: InputReceiver,
    pub(crate) event_tx: EventSender,
    /// Skip startup warmup (for tests).
    pub(crate) skip_warmup: bool,
    /// Multi-provider per-task-type model routing — see `Builder::task_router`.
    /// Retained here (not just forwarded to `AgentTool`) so turn-processing
    /// code (`turn.rs`) can route its own background LLM calls — e.g. post-turn
    /// memory extraction's `task_models.memory` — through the same mechanism
    /// already proven for sub-agent spawns (`"subagent"` task type).
    pub(crate) task_router: Option<Arc<base::provider::TaskRouter>>,
}

impl Agent {
    /// Start the agent event loop. Runs until cancelled (caller calls `.cancel()` on the token)
    /// or the input channel closes. Does NOT consume self — the agent can be reused after stop.
    pub async fn run(&mut self, cancel: CancellationToken) {
        tracing::info!(scene = %self.scene.id(), "Engine started");

        // Emit SystemInit
        let tools: Vec<_> = self
            .tools
            .list()
            .iter()
            .map(|t| base::interface::event::ToolInfo {
                name: t.name().to_string(),
                description: t.description().to_string(),
            })
            .collect();
        let _ = self.event_tx.send(AgentEvent::SystemInit {
            scene: self.scene.id().to_string(),
            tools,
            mcp_servers: vec![],
        });

        // P1: Orphaned permission recovery (TS parity: resume from transcript).
        // If the previous session was interrupted while a permission prompt was
        // pending, re-inject the stored decision so the agent can continue.
        if let Some(decision) = self.orphaned_permission.take() {
            if !self.has_handled_orphaned_permission {
                tracing::info!(
                    ?decision,
                    "Recovering orphaned permission from previous session"
                );
                self.has_handled_orphaned_permission = true;
                let _ = self
                    .process_turn(
                        InputMessage::PermissionResponse {
                            prompt_id: "orphaned".into(),
                            decision,
                        },
                        cancel.clone(),
                    )
                    .await;
            }
        }
        self.has_handled_orphaned_permission = true;

        // P2: Startup warmup — pre-compute frozen context, re-scan skills, pre-connect API.
        // Runs after SystemInit but before the main input loop. Gate on skip_warmup for tests.
        if !self.skip_warmup {
            self.warmup().await;
        }

        loop {
            // P2: Between turns, check for wake signals and re-execute any
            // pending async rewake hooks (ones that returned `{rewake: true}`
            // and whose config has `async_rewake: true`).
            self.hooks.check_rewakes().await;

            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("Engine cancelled");
                    break;
                }
                msg = self.input_rx.recv() => {
                    match msg {
                        Some(input) => {
                            match self.process_turn(input, cancel.clone()).await {
                                Ok(_) => {}
                                Err(crate::turn::TurnError::Shutdown) => break,
                                Err(e) => {
                                    let _ = self.event_tx.send(AgentEvent::Error {
                                        code: "turn_error".into(),
                                        message: e.to_string(),
                                        turn_id: self.current_turn_id.clone(),
                                    });
                                }
                            }
                        }
                        None => {
                            tracing::info!("Input channel closed");
                            break;
                        }
                    }
                }
            }
        }
        tracing::info!("Engine stopped");
    }

    /// Warm up the agent by pre-computing the frozen environment snapshot,
    /// re-scanning skills directories, and pre-connecting to the API endpoint.
    /// All operations run in parallel via `tokio::join!` to minimize startup latency.
    async fn warmup(&mut self) {
        // NOTE: the actual project root, not `local_data_dir` itself (which
        // is `<project_root>/.atta` — see `PathSettings::project_root` docs).
        let cwd = self.settings.paths.project_root();
        let scope = self.settings.paths.scope.clone();
        // Skills: global default (flat, cross-scene) + scene-specific override
        // (same name wins — `SkillManager::load_dir_subdirs` overwrites by name, so
        // loading global first then scene lets the scene tier win).
        let global_skills_dir = self.settings.paths.global_data_dir.join("skills");
        let scene_skills_dir = self.settings.paths.user_data_dir.join("skills");
        // Project-level skills live in `.agents/skills/` (sibling of `.atta/`,
        // not inside it) — the external fact standard Codex also scans.
        let local_skills_dir = cwd.join(".agents").join("skills");
        let base_url = self.settings.model.base_url.clone();
        let skills = std::sync::Arc::clone(&self.skills);

        let (frozen, _skills_res, _) = tokio::join!(
            // 1. Pre-compute the frozen environment snapshot (git status, branch, platform, etc.)
            base::frozen::FrozenContext::collect(cwd.clone(), &scope),
            // 2. Re-scan skills directories for newly added skills.
            async move {
                let count0 =
                    skills.load_dir_subdirs(&global_skills_dir, skills::manager::SkillSource::User);
                let count1 =
                    skills.load_dir_subdirs(&scene_skills_dir, skills::manager::SkillSource::User);
                let count2 = skills
                    .load_dir_subdirs(&local_skills_dir, skills::manager::SkillSource::Project);
                (count0.ok(), count1.ok(), count2.ok())
            },
            // 3. Fire-and-forget pre-connect GET to the API base URL (warms TCP/TLS)
            async move {
                if !base_url.is_empty() {
                    if let Ok(client) = reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(5))
                        .build()
                    {
                        let _ = client.get(&base_url).send().await;
                    }
                }
            },
        );

        self.frozen = Some(frozen);
        tracing::debug!("Startup warmup complete");
    }

    /// Get current session summary. External interface (read-only).
    pub fn session_info(&self) -> session::session::SessionSummary {
        self.session.summary()
    }

    /// List persisted sessions. External interface (read-only, from HistoryStore).
    pub async fn list_sessions(
        &self,
    ) -> Result<Vec<session::session::SessionSummary>, session::session::SessionError> {
        self.session.list_sessions().await
    }

    /// Delete a persisted session from HistoryStore. External interface.
    pub async fn delete_session(&self, id: &str) -> Result<(), session::session::SessionError> {
        self.session.delete_session(id).await
    }

    /// Access the performance collector.
    pub fn perf(&self) -> &PerfCollector {
        &self.perf
    }

    /// Access the tool registry (read-only).
    pub fn tools(&self) -> &InMemoryToolRegistry {
        &self.tools
    }

    /// Access the engine settings (read-only).
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Switch the model at runtime (for `/model` slash command).
    pub fn set_model(&mut self, model_name: String) {
        Arc::make_mut(&mut self.settings).model.model_name = model_name;
    }

    /// Access telemetry recorder for external event recording.
    pub fn telemetry(&self) -> &dyn TelemetryRecorder {
        &self.telemetry_handle
    }

    /// VCR status — None if VCR is disabled.
    pub fn vcr(&self) -> Option<&base::interface::settings::VcrConfig> {
        self.settings.vcr.as_ref()
    }

    /// Access the permission handler (read-only).
    pub fn permission(&self) -> &dyn Permission {
        &*self.permission
    }

    /// Access the memory store (read-only).
    pub fn memory(&self) -> &MemoryStore {
        &self.memory_store
    }

    /// Access the skill manager for runtime loading/listing/reloading.
    pub fn skills(&self) -> &skills::manager::SkillManager {
        &self.skills
    }

    /// Get shared Arc to the skill manager (for tool registration).
    pub fn skills_arc(&self) -> std::sync::Arc<skills::manager::SkillManager> {
        self.skills.clone()
    }

    /// Access the hooks runner (read-only).
    pub fn hooks(&self) -> &HookRunner {
        &self.hooks
    }

    /// Access the MCP manager (read-only).
    pub fn mcp(&self) -> &McpManager {
        &self.mcp
    }

    /// Initialize MCP skills after the agent is constructed.
    ///
    /// For each connected MCP server, fetches the tool list and registers
    /// each tool as a user-invocable skill (name: `mcp__{server}__{tool}`).
    /// Must be called from an async context with a tokio runtime.
    pub async fn init_mcp_skills(&self) {
        for client in self.mcp.clients() {
            match client.list_tools().await {
                Ok(metas) => {
                    let tool_defs: Vec<base::interface::model::ToolDef> = metas
                        .into_iter()
                        .map(|m| base::interface::model::ToolDef {
                            name: m.name,
                            description: m.description.unwrap_or_default(),
                            input_schema: m.input_schema,
                        })
                        .collect();
                    if !tool_defs.is_empty() {
                        let count = self
                            .skills
                            .register_mcp_skills(client.server_name(), &tool_defs);
                        tracing::info!(
                            server = %client.server_name(),
                            count,
                            "MCP skills registered"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        server = %client.server_name(),
                        error = %e,
                        "Failed to list MCP tools for skill registration"
                    );
                }
            }
        }
    }

    /// Set plan mode state and optional plan content (for post-compact recovery).
    pub fn set_plan_mode(&mut self, active: bool, content: Option<String>) {
        self.in_plan_mode = active;
        self.plan_content = content;
    }

    /// Register a running background task summary (for post-compact recovery).
    pub fn register_running_task(&mut self, task_id: String, status: String) {
        self.running_task_summaries.push((task_id, status));
    }

    /// Clear completed/cancelled running task summaries.
    pub fn clear_running_tasks(&mut self) {
        self.running_task_summaries.clear();
    }

    /// Trigger manual compaction.
    pub async fn compact_now(&self) -> Result<(), EngineError> {
        if let Err(e) = self
            .compactor
            .compact(
                self.session.messages().to_vec(),
                self.scene.token_budget().compact_threshold,
                self.scene.token_budget().compact_keep_recent,
            )
            .await
        {
            tracing::warn!(error = %e, "failed to compact messages");
        }
        Ok(())
    }

    /// Convenience: run a single turn from a user message string.
    /// Sends events through the engine's event channel during execution.
    /// Returns `TurnOutcome` on completion.
    pub async fn run_turn(
        &mut self,
        content: String,
        turn_id: String,
        cancel: CancellationToken,
    ) -> Result<crate::turn::TurnOutcome, crate::turn::TurnError> {
        self.process_turn(
            InputMessage::User {
                content,
                attachments: vec![],
                turn_id,
            },
            cancel,
        )
        .await
    }

    /// Run hooks for a lifecycle event.
    /// Returns hook outputs that may block actions or inject text.
    pub async fn run_hooks(
        &self,
        event: hooks::HookEvent,
        input: &hooks::HookInput,
    ) -> hooks::runner::HookRunResult {
        self.hooks.run(event, input).await
    }

    // ── Slash command handlers ──

    /// `/help` — list all available slash commands.
    pub(crate) fn handle_help_command(&self) -> String {
        let commands = self.commands.list();
        if commands.is_empty() {
            return "No commands registered.".into();
        }
        let mut out = String::from("Available slash commands:\n\n");
        for (name, desc) in &commands {
            out.push_str(&format!("/{} — {}\n", name, desc));
        }
        out.push_str("\nUse /<name> [args] to invoke a command.");
        out
    }

    /// `/skills` — list all available skills.
    pub(crate) fn handle_skills_command(&self) -> String {
        let skills = self.skills.list();
        if skills.is_empty() {
            return "No skills loaded.".into();
        }
        let mut out = String::from("Available skills:\n\n");
        for s in &skills {
            let src = s.source.as_str();
            out.push_str(&format!("/{} — {} [{}]\n", s.name, s.description, src));
        }
        out
    }

    /// `/clear` — clear session message history.
    pub(crate) fn handle_clear_command(&mut self) {
        self.session.clear();
        self.claude_md_injected = false; // re-inject CLAUDE.md on next turn
        self.invoked_skills.clear();
        self.permission_denial_count = 0;
        self.output_token_target = None;
        self.accumulated_output_tokens = 0;
        self.token_budget_continuation_count = 0;
    }

    /// `/cost` — show estimated session API cost.
    pub(crate) fn handle_cost_command(&self) -> String {
        let messages = self.session.messages();
        let total_chars: usize = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .map(|b| match b {
                base::interface::model::ModelContentBlock::Text { text } => text.len(),
                _ => 100, // rough estimate for tool blocks
            })
            .sum();
        let est_tokens = total_chars / 4;
        let est_cost = est_tokens as f64 * 3.0 / 1_000_000.0; // input cost estimate
        format!(
            "Session: {} messages, ~{} tokens, est. cost ${:.4}",
            messages.len(),
            est_tokens,
            est_cost
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine shutdown requested")]
    Shutdown,
    #[error("{0}")]
    Internal(String),
}

// ── Builder ──

pub struct Builder {
    scene: Option<Arc<dyn AgentScene>>,
    model: Option<Arc<dyn Model>>,
    tools: Option<Arc<InMemoryToolRegistry>>,
    settings: Option<Arc<Settings>>,
    permission: Option<Arc<dyn Permission>>,
    memory_store: Option<Arc<MemoryStore>>,
    compactor: Option<Arc<dyn Compactor>>,
    hooks: Option<Arc<HookRunner>>,
    /// Backing store for session transcript persistence (JSONL append/resume).
    /// `None` = in-memory only, matching prior behavior — this is purely
    /// additive, existing callers that don't set it are unaffected.
    history_store: Option<Arc<dyn history::store::HistoryStore>>,
    mcp_servers: Option<Vec<String>>,
    mcp_manager_override: Option<McpManager>,
    telemetry_url: Option<String>,
    telemetry_handle_override: Option<TelemetryHandle>,
    instruction_file: Option<PathBuf>,
    session_id: Option<String>,
    skip_warmup: bool,
    /// Pre-built FrozenContext — skips lazy collection on first turn.
    /// When set, the Agent uses this snapshot instead of calling
    /// `FrozenContext::collect()`. Essential for deterministic VCR replay.
    frozen: Option<base::frozen::FrozenContext>,
    wake_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
    /// Plugins available to this session (built-ins + installed, discovered
    /// once per daemon instance — see `plugin::discover_plugins`). `None`
    /// (the default) means no plugins are wired in, matching prior
    /// behavior for non-daemon callers (tests, single-process CLI mode).
    plugins: Option<Arc<Vec<plugin::manifest::Plugin>>>,
    /// Pre-built command registry shared across many sessions instead of
    /// re-scanning skill dirs per session — see `Builder::commands_override`.
    commands_override: Option<Arc<crate::commands::CommandRegistry>>,
    /// Multi-provider per-task-type model routing — see `Builder::task_router`.
    task_router: Option<Arc<base::provider::TaskRouter>>,
}

/// Build a `HookRunner` from `settings.hooks_config` (raw JSON from
/// settings.json's `hooks` section) when the caller didn't inject one
/// explicitly via `Builder::hooks(...)`.
///
/// A malformed value degrades to no hooks + a `tracing::warn!` rather than
/// failing the build — same soft-degrade philosophy as the rest of
/// settings.json. Also wires the `.atta/hooks/` three-tier search path
/// (project > scene > global) so `Command` hooks can reference a bare
/// script name — see `hooks::HookRunner::with_hooks_search_dirs` docs.
///
/// `model`/`agent_tool` back the `"type": "prompt"` / `"type": "agent"`
/// hook executors (`crate::hook_executors`) — previously neither was ever
/// injected anywhere in the app, so those two hook types always fell
/// through to `HookOutcome::Skipped` with "no executor configured".
fn build_hook_runner(
    settings: &Settings,
    model: Arc<dyn Model>,
    agent_tool: Arc<crate::agent_tool::AgentTool>,
    plugins: &[plugin::manifest::Plugin],
) -> Arc<HookRunner> {
    let parsed: hooks::HooksSettings = match &settings.hooks_config {
        Some(v) => serde_json::from_value(v.clone()).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "settings.json `hooks` section failed to parse, ignoring");
            Default::default()
        }),
        None => Default::default(),
    };
    let hooks_search_dirs = vec![
        settings.paths.project_root().join(".atta").join("hooks"),
        settings.paths.user_data_dir.join("hooks"),
        settings.paths.global_data_dir.join("hooks"),
    ];
    let prompt_executor = Arc::new(crate::hook_executors::ModelPromptHookExecutor::new(
        model,
        settings.model.model_name.clone(),
        settings.model.max_tokens,
    ));
    let agent_spawner: Arc<dyn base::interface::agent_spawner::AgentSpawner> = Arc::new(
        crate::agent_spawner_impl::RuntimeAgentSpawner::new(agent_tool),
    );
    let agent_executor = Arc::new(crate::hook_executors::AgentSpawnerHookExecutor::new(
        agent_spawner,
        settings.paths.project_root(),
    ));
    let mut runner = HookRunner::new(parsed)
        .with_hooks_search_dirs(hooks_search_dirs)
        .with_prompt_executor(prompt_executor)
        .with_agent_executor(agent_executor);
    for plugin in plugins {
        if let Err(e) = plugin.install_hooks(&mut runner, &plugin.root) {
            tracing::warn!(
                plugin = %plugin.manifest.plugin.name,
                error = %e,
                "failed to install plugin hooks, skipping"
            );
        }
    }
    Arc::new(runner)
}

impl Builder {
    pub fn new() -> Self {
        Self {
            scene: None,
            model: None,
            tools: None,
            settings: None,
            permission: None,
            memory_store: None,
            compactor: None,
            hooks: None,
            history_store: None,
            mcp_servers: None,
            mcp_manager_override: None,
            telemetry_url: None,
            instruction_file: None,
            session_id: None,
            telemetry_handle_override: None,
            skip_warmup: false,
            frozen: None,
            wake_rx: None,
            plugins: None,
            commands_override: None,
            task_router: None,
        }
    }

    pub fn scene(mut self, s: Arc<dyn AgentScene>) -> Self {
        self.scene = Some(s);
        self
    }
    pub fn model(mut self, m: Arc<dyn Model>) -> Self {
        self.model = Some(m);
        self
    }
    pub fn tools(mut self, t: Arc<InMemoryToolRegistry>) -> Self {
        self.tools = Some(t);
        self
    }
    pub fn settings(mut self, s: Arc<Settings>) -> Self {
        self.settings = Some(s);
        self
    }
    pub fn permission(mut self, p: Arc<dyn Permission>) -> Self {
        self.permission = Some(p);
        self
    }
    pub fn memory_store(mut self, m: Arc<MemoryStore>) -> Self {
        self.memory_store = Some(m);
        self
    }
    pub fn compactor(mut self, c: Arc<dyn Compactor>) -> Self {
        self.compactor = Some(c);
        self
    }
    pub fn hooks(mut self, h: Arc<HookRunner>) -> Self {
        self.hooks = Some(h);
        self
    }
    /// Inject a backing store for session transcript persistence. When set,
    /// the session's messages are incrementally appended to disk once per
    /// turn (see `session::session::SessionManager::persist`); `None` (the
    /// default) keeps sessions purely in-memory.
    pub fn history_store(mut self, store: Arc<dyn history::store::HistoryStore>) -> Self {
        self.history_store = Some(store);
        self
    }
    /// P2: Inject a wake channel receiver for async rewake support.
    /// When background work completes, something sends `()` on the
    /// associated sender; the hooks runner picks up the signal and
    /// re-executes any pending rewake hooks.
    pub fn wake_receiver(mut self, rx: tokio::sync::mpsc::UnboundedReceiver<()>) -> Self {
        self.wake_rx = Some(rx);
        self
    }
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }
    pub fn instruction(mut self, path: impl Into<PathBuf>) -> Self {
        self.instruction_file = Some(path.into());
        self
    }
    pub fn telemetry_url(mut self, url: Option<String>) -> Self {
        self.telemetry_url = url;
        self
    }
    /// Inject a pre-built telemetry handle (from e.g. CLI's `telemetry::spawn()`).
    /// Takes precedence over `telemetry_url`.
    pub fn telemetry_handle(mut self, h: TelemetryHandle) -> Self {
        self.telemetry_handle_override = Some(h);
        self
    }
    /// Inject a pre-built MCP manager with live connections. To share one
    /// centrally-connected set of MCP servers across many sessions without
    /// reconnecting per session, build this from the shared connection's
    /// `McpClientHandle`s via `McpManager::from_clients(...)` +
    /// `refresh_tools()` (see `daemon::SessionPool::create()`), rather than
    /// re-running `McpManager::connect_all(...)` for every session.
    pub fn mcp_manager(mut self, m: McpManager) -> Self {
        self.mcp_servers = None;
        self.mcp_manager_override = Some(m);
        self
    }
    pub fn mcp_servers(mut self, names: Vec<String>) -> Self {
        self.mcp_servers = Some(names);
        self
    }

    /// Pre-seed the FrozenContext snapshot (skips lazy collection on first turn).
    /// Essential for deterministic VCR replay across runs.
    pub fn frozen(mut self, ctx: base::frozen::FrozenContext) -> Self {
        self.frozen = Some(ctx);
        self
    }

    /// Disable startup warmup (for tests).
    pub fn skip_warmup(mut self, val: bool) -> Self {
        self.skip_warmup = val;
        self
    }

    /// Inject the daemon-discovered plugin list (see
    /// `plugin::discover_plugins`) so this session's hooks and agent types
    /// include plugin-declared ones. Defaults to none — non-daemon callers
    /// (tests, single-process CLI mode) are unaffected. Plugin-contributed
    /// slash commands are *not* installed from this list — see
    /// `commands_override` — since daemon builds one shared command
    /// registry up front instead of per session.
    pub fn plugins(mut self, p: Arc<Vec<plugin::manifest::Plugin>>) -> Self {
        self.plugins = Some(p);
        self
    }

    /// Inject a pre-built command registry (built once, shared across many
    /// sessions — see `daemon::SessionPool`) instead of scanning skill dirs
    /// fresh for every session. Falls back to the existing per-session
    /// `CommandRegistry::from_skill_manager` scan when not set.
    pub fn commands_override(mut self, c: Arc<crate::commands::CommandRegistry>) -> Self {
        self.commands_override = Some(c);
        self
    }

    /// Multi-provider per-task-type model routing (see
    /// `base::provider::TaskRouter`). When set, forwarded to the
    /// `AgentTool` this session's engine builds so its sub-agent spawn
    /// points (`run_sub`/`run_sub_inner`/resume) route through
    /// `TaskRouter::model_for("subagent")` instead of always inheriting
    /// this session's own `model`. Unset (the default) preserves prior
    /// behavior exactly — sub-agents always use the parent's model.
    pub fn task_router(mut self, r: Arc<base::provider::TaskRouter>) -> Self {
        self.task_router = Some(r);
        self
    }

    pub fn build(self) -> Result<(Agent, EventReceiver, InputSender), EngineError> {
        let scene = self
            .scene
            .ok_or_else(|| EngineError::Internal("scene required".into()))?;
        let model = self
            .model
            .ok_or_else(|| EngineError::Internal("model required".into()))?;
        let tools = self
            .tools
            .unwrap_or_else(|| Arc::new(InMemoryToolRegistry::new()));
        let settings = self
            .settings
            .ok_or_else(|| EngineError::Internal("settings required".into()))?;
        let permission = self.permission.unwrap_or_else(|| {
            struct AllowAll;
            #[async_trait::async_trait]
            impl Permission for AllowAll {
                async fn check(
                    &self,
                    _: &str,
                    _: &serde_json::Value,
                    _: &std::path::Path,
                    _: &str,
                ) -> base::interface::permission::PermissionOutcome {
                    base::interface::permission::PermissionOutcome::Permit
                }
            }
            Arc::new(AllowAll)
        });
        let instruction_file = self.instruction_file.or(settings.instruction_file.clone());
        // Pre-read AGENTS.md / CLAUDE.md content for userContext injection (TS parity).
        let claude_md_content = instruction_file.as_ref().and_then(|p| {
            match std::fs::read_to_string(p) {
                Ok(content) => {
                    if content.trim().is_empty() {
                        None
                    } else {
                        Some(content)
                    }
                }
                Err(e) => {
                    tracing::warn!(path = %p.display(), error = %e, "failed to read instruction file for CLAUDE.md context injection");
                    None
                }
            }
        });
        let memory_store = self.memory_store.unwrap_or_else(|| {
            let p = &settings.paths;
            // memory 不分 scene，只分全局/项目——见 base::paths 模块文档。
            Arc::new(MemoryStore::new(
                p.global_data_dir.join("memory"),
                p.local_data_dir.join("memory"),
            ))
        });
        // Session: `self.history_store` (set via `Builder::history_store(...)`)
        // makes this incrementally persisted to disk once per turn; `None`
        // (the default) keeps it in-memory only, matching prior behavior.
        let session = SessionManager::new(self.history_store.clone(), self.session_id, None);
        let compactor = self
            .compactor
            .unwrap_or_else(|| Arc::new(DefaultCompactor) as Arc<dyn Compactor>);
        // Plugins wired into this session — see `Builder::plugins` doc
        // comment. Cheap `Arc` clone; empty by default for non-daemon
        // callers.
        let plugins: Arc<Vec<plugin::manifest::Plugin>> = self.plugins.clone().unwrap_or_default();
        // Plugin-declared agent types, converted to the runtime's
        // `AgentTypeDefinition` (reads each plugin's `system_prompt_path`
        // relative to its own root — see `agent_tool::agent_def_to_type`).
        // A plugin whose prompt file can't be read (e.g. a built-in
        // plugin's synthetic `(builtin:...)` root) is skipped with a
        // warning rather than failing the whole build.
        let plugin_agent_types: Vec<crate::agent_tool::AgentTypeDefinition> = plugins
            .iter()
            .flat_map(|p| {
                p.manifest
                    .agents
                    .iter()
                    .filter_map(move |def| crate::agent_tool::agent_def_to_type(def, &p.root))
            })
            .collect();
        // AgentTool ("Agent") — lets the model spawn sub-agents. Previously
        // had zero production construction sites despite the CodingScene
        // system prompt instructing the model to use it (see
        // `render_session_guidance`'s `has_agent` check). Catalog is built
        // from the 6 built-in types + any custom `.atta/agents/*.md`
        // definitions, project > scene > global (same override order as
        // skills). `EngineConfig` isn't threaded through `Builder` today, so
        // this derives a minimal one from `settings.model` — just enough for
        // `AgentTool::sub_settings()`'s needs (model name/max_tokens/fallback).
        // Constructed here (before `hooks`, not at its registration point
        // further down) so its `AgentSpawner` wrapper can be handed to the
        // "agent"-type hook executor below — `tools.register(...)` for it
        // happens later, but `InMemoryToolRegistry` is a shared `Arc`, so
        // construction order doesn't affect which tools end up visible to
        // sub-agents (`resolve_tools()` reads the registry at call time).
        let agent_tool_arc = {
            let agent_engine_config = Arc::new({
                let mut c =
                    base::context::EngineConfig::defaults_for(settings.model.model_name.clone());
                c.max_tokens = settings.model.max_tokens;
                c.fallback_model = settings.model.fallback_model.clone();
                c
            });
            let project_agents_dir = settings.paths.project_root().join(".atta").join("agents");
            let scene_agents_dir = settings.paths.user_data_dir.join("agents");
            let global_agents_dir = settings.paths.global_data_dir.join("agents");
            let agent_dirs: [&std::path::Path; 3] =
                [&global_agents_dir, &scene_agents_dir, &project_agents_dir];
            let mut agent_tool = crate::agent_tool::AgentTool::with_parent_tools(
                model.clone(),
                agent_engine_config,
                tools.clone(),
                tools.clone(),
                &agent_dirs,
                &plugin_agent_types,
            )
            .with_settings(settings.clone());
            if let Some(router) = self.task_router.clone() {
                agent_tool = agent_tool.with_task_router(router);
            }
            Arc::new(agent_tool)
        };
        let hooks = self.hooks.unwrap_or_else(|| {
            build_hook_runner(&settings, model.clone(), agent_tool_arc.clone(), &plugins)
        });
        // P2: Wire the wake receiver into hooks for async rewake support.
        if let Some(rx) = self.wake_rx {
            hooks.set_wake_receiver(rx);
        }
        // Telemetry: use pre-built handle if injected, else noop (events silently dropped).
        let telemetry_handle = self.telemetry_handle_override.unwrap_or_else(|| {
            let (tx, _rx) = tokio::sync::mpsc::channel(1);
            TelemetryHandle::new(tx)
        });
        // MCP: use pre-built manager if injected, else empty (no servers).
        let mcp = self.mcp_manager_override.unwrap_or_else(McpManager::empty);
        // Hand `agent_tool_arc` a snapshot of the connected MCP tools now,
        // for `AgentTypeDefinition.mcp_servers` — see
        // `Inner::mcp_tool_adapters`'s doc comment for why a snapshot Vec,
        // not a live `McpManager` reference.
        agent_tool_arc.set_mcp_tool_adapters(mcp.tool_adapters().to_vec());
        // Skill auto-loading: scan ~/.atta/skills/ (global default), then
        // ~/.atta/scenes/<scope>/skills/ (scene override — same name wins,
        // `load_dir_subdirs` overwrites by name), then project/.agents/skills/
        // (project — external fact standard, also scanned by Codex).
        // `load_dir_subdirs` (not the flat-only `load_dir`) so both
        // `<name>.md` and the `<name>/SKILL.md` subdirectory convention (the
        // one Claude Code itself uses, and what `discover_for_paths`/
        // `reload_skill` already supported) are picked up at startup — a
        // project skill authored the SKILL.md way used to silently never
        // load until its first live-reload edit. Found via the new VCR test
        // suite: the fixture project's `code-review` skill (subdirectory
        // format) was completely absent from every recorded system prompt.
        let skill_mgr = skills::manager::SkillManager::new();
        let project_skills_dir = settings.paths.project_root().join(".agents").join("skills");
        let skill_load_results = [
            skill_mgr.load_dir_subdirs(
                &settings.paths.global_data_dir.join("skills"),
                skills::manager::SkillSource::User,
            ),
            skill_mgr.load_dir_subdirs(
                &settings.paths.user_data_dir.join("skills"),
                skills::manager::SkillSource::User,
            ),
            skill_mgr.load_dir_subdirs(&project_skills_dir, skills::manager::SkillSource::Project),
        ];
        let loaded_count: usize = skill_load_results
            .iter()
            .filter_map(|r| r.as_ref().ok())
            .sum();
        // Register built-in (bundled) skills after disk skills.
        // Disk-loaded skills with the same name take priority — bundled is fallback.
        for bundled in skills::bundled::bundled_skills() {
            skill_mgr.register_bundled(bundled);
        }
        let total_skills = skill_mgr.list().len();
        tracing::info!(loaded_count, total_skills, "skills loaded (incl. bundled)");

        // Live skill reload: `SkillManager::enable_watching` + `check_for_changes`
        // (the latter already called once per turn — `turn.rs`'s
        // `run_user_turn`) were both fully implemented but `enable_watching`
        // had no caller anywhere, so the watcher never actually started —
        // every session was silently stuck on the one-time `warmup()` rescan
        // for the rest of its life. `notify` setup can fail (inotify/kqueue
        // limits, permissions); that's degraded-but-safe (skills still work,
        // edits just need a session rebuild to show up, same as before this
        // fix), so a warning, not a hard error.
        if let Err(e) = skill_mgr.enable_watching(&[
            settings.paths.global_data_dir.join("skills"),
            settings.paths.user_data_dir.join("skills"),
            project_skills_dir.clone(),
        ]) {
            tracing::warn!(error = %e, "failed to enable skill file watching; live skill reload disabled for this session");
        }

        // Build command registry from skill manager + built-in local commands
        // — unless the caller already built one to share across many
        // sessions (see `Builder::commands_override`), which also carries
        // any plugin-contributed slash commands (daemon builds that catalog
        // once at startup, not per session).
        let skill_mgr_arc = std::sync::Arc::new(skill_mgr);
        // `agent_tool_arc` was built before skills finished loading (see the
        // comment at its own construction site) — hand it the manager now,
        // via interior mutability, so `AgentTypeDefinition.skills` (preload)
        // can resolve when a subagent is spawned later in this session.
        agent_tool_arc.set_skill_manager(skill_mgr_arc.clone());
        let command_registry = self.commands_override.clone().unwrap_or_else(|| {
            std::sync::Arc::new(crate::commands::CommandRegistry::from_skill_manager(
                &skill_mgr_arc,
            ))
        });
        // Register SkillTool using the populated skill manager. Also gives
        // it a real `AgentSpawner` — previously `register_skill_tool` never
        // received one at all, so a `context: fork` skill's spawner branch
        // (`SkillTool::call`) was unreachable in every real session; it
        // silently fell through to running inline instead of forking, no
        // matter what the skill's frontmatter said.
        let skill_tool_spawner: Arc<dyn base::interface::agent_spawner::AgentSpawner> = Arc::new(
            crate::agent_spawner_impl::RuntimeAgentSpawner::new(agent_tool_arc.clone()),
        );
        tools::register_skill_tool(
            &tools,
            Arc::clone(&skill_mgr_arc),
            Some(skill_tool_spawner),
            permission.clone(),
        );
        // Register TaskStopTool — stop running background tasks by ID
        tools.register(std::sync::Arc::new(tools::task_stop::TaskStopTool));
        // Register TaskOutputTool — retrieve output from running/completed tasks
        tools.register(std::sync::Arc::new(tools::task_output::TaskOutputTool));
        // Register ImportTool — backs the /import slash command (cross-tool
        // config import from Claude Code/Codex/Cursor). See
        // docs/design/2026-08-03-agents-config-migration.md §3.8.
        tools.register(std::sync::Arc::new(tools::import_tool::ImportTool));
        // Register AgentTool ("Agent") into the tool registry the model calls
        // (`agent_tool_arc` itself was constructed earlier, before `hooks`,
        // so its `AgentSpawner` wrapper could be handed to the "agent"-type
        // hook executor — see the `hooks` construction above).
        tools.register(agent_tool_arc);
        // Register MCP resource tools if clients are available
        if !mcp.clients().is_empty() {
            tools.register(std::sync::Arc::new(mcp::tools::ListMcpResourcesTool::new(
                mcp.clients().to_vec(),
            )));
            tools.register(std::sync::Arc::new(mcp::tools::ReadMcpResourceTool::new(
                mcp.clients().to_vec(),
            )));
            tools.register(std::sync::Arc::new(mcp::tools::DispatchMcpTool::new(
                mcp.clients().to_vec(),
            )));
            // Register each individual MCP tool adapter (mcp__<server>__<tool>)
            // so the model can actually call the names `build_tool_defs()`
            // (turn.rs) already advertises to it — previously these were
            // only ever advertised, never executable: `execute_tool_inner`
            // looks up this same `tools` registry, which had no per-tool
            // adapters in it, only the 3 generic ones above. A caveat this
            // doesn't address: this is a build-time snapshot, so a tool that
            // only appears via a later `refresh_tools()` call (e.g. a
            // reconnect that surfaces a genuinely new tool, not just the
            // same set again) won't be registered here — closing that gap
            // needs dispatch-time fallback to `self.mcp`, a bigger change.
            for adapter in mcp.tool_adapters() {
                tools.register(adapter.clone());
            }
        }

        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Capture feature flags before settings is moved into the Agent struct
        let cached_mc_enabled = settings.feature_flags.cached_microcompact;
        let config = Arc::new(base::context::EngineConfig::from_settings(&settings));

        Ok((
            Agent {
                scene,
                model,
                tools,
                settings,
                config,
                permission,
                pending_permissions: Arc::new(std::sync::Mutex::new(
                    std::collections::HashMap::new(),
                )),
                memory_store,
                session,
                perf: Arc::new(PerfCollector::new()),
                compactor,
                hooks,
                mcp,
                skills: skill_mgr_arc,
                commands: command_registry,
                telemetry_handle,
                current_turn_id: String::new(),
                frozen: self.frozen, // pre-seeded or lazily computed on first turn
                claude_md_content,
                claude_md_injected: false,
                invoked_skills: Vec::new(),
                last_had_tool_uses: true, // true → first turn scans for skills (TS parity)
                in_plan_mode: false,
                plan_content: None,
                running_task_summaries: Vec::new(),
                permission_denial_count: 0,
                compact_warning_issued: false,
                time_based_mc_config: compaction::time_based_mc::TimeBasedMcConfig::default(),
                cached_mc: CachedMicroCompact::new(compaction::cached::CachedMcConfig {
                    enabled: cached_mc_enabled,
                    ..Default::default()
                }),
                compaction_state: compaction::reactive::CompactionState::default(),
                team_id: None,
                orphaned_permission: None,
                has_handled_orphaned_permission: false,
                messages_to_ack: Vec::new(),
                output_token_target: None,
                accumulated_output_tokens: 0,
                token_budget_continuation_count: 0,
                last_delta_tokens: 0,
                input_rx,
                event_tx,
                skip_warmup: self.skip_warmup,
                task_router: self.task_router,
            },
            event_rx,
            input_tx,
        ))
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_settings() -> base::interface::settings::Settings {
        use base::interface::settings::*;
        Settings {
            model: ModelSettings {
                api_type: base::provider::ApiType::Anthropic,
                base_url: String::new(),
                auth_token: String::new(),
                model_name: "test".into(),
                max_tokens: 2000,
                thinking_mode: ThinkingMode::Auto,
                fallback_model: None,
            },
            paths: PathSettings {
                user_data_dir: "/tmp".into(),
                global_data_dir: "/tmp".into(),
                local_data_dir: "/tmp".into(),
                scope: "code".into(),
            },
            execution: ExecutionSettings::default(),
            compaction: CompactionConfig::default(),
            sandbox: SandboxConfig::default(),
            instruction_file: None,
            prompt_append: None,
            prompt_override: None,
            vcr: None,
            telemetry_url: None,
            session_dir: None,
            memory_enabled: true,
            disable_skill_shell_execution: false,
            permission_mode: PermissionMode::default(),
            permission_rules: Vec::new(),
            hooks_config: None,
            mcp_servers: Default::default(),
            providers: Default::default(),
            default_provider: None,
            task_models: Default::default(),
            language: None,
            feature_flags: Default::default(),
        }
    }

    #[test]
    fn builder_requires_scene() {
        assert!(Builder::new().build().is_err());
    }

    struct DummyModel;
    #[async_trait::async_trait]
    impl Model for DummyModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<base::interface::model::ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            unimplemented!("not exercised by these tests")
        }
    }

    fn dummy_agent_tool() -> Arc<crate::agent_tool::AgentTool> {
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(base::context::EngineConfig::defaults_for("test-model"));
        let tools = Arc::new(InMemoryToolRegistry::new());
        Arc::new(crate::agent_tool::AgentTool::new(model, config, tools))
    }

    #[tokio::test]
    async fn build_registers_mcp_tool_adapters_into_the_executable_tool_registry() {
        // Regression test: `build_tool_defs()` (turn.rs) advertises every
        // `mcp.tool_adapters()` entry to the model as a callable tool, but
        // `Builder::build()` previously only registered the 3 generic
        // List/Read/Dispatch MCP tools into `self.tools` — never the
        // individual per-tool adapters. The model would see
        // `mcp__test-server__do-thing` in its tool list and calling it would
        // always fail with "Tool not found" (`turn.rs::execute_tool_inner`
        // looks up `ctx.tools`, a clone of this same registry).
        let settings = Arc::new(test_settings());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);

        let mock_client = Arc::new(mcp::client::MockMcpClient::new(
            "test-server",
            vec![mcp::client::McpToolMeta {
                name: "do-thing".into(),
                description: Some("does a thing".into()),
                input_schema: serde_json::json!({"type": "object"}),
            }],
        ));
        let mut mcp_manager = McpManager::from_clients(vec![mock_client]);
        mcp_manager.refresh_tools().await;
        assert_eq!(
            mcp_manager.tool_adapters().len(),
            1,
            "sanity check: mock client should have produced one adapter"
        );

        let (agent, _event_rx, _input_tx) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(settings)
            .mcp_manager(mcp_manager)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        assert!(
            agent.tools.get("mcp__test-server__do-thing").is_some(),
            "MCP tool adapter should be registered in the executable tool registry, found: {:?}",
            agent.tools.names()
        );
    }

    /// Test tool: always succeeds, records nothing — just needs to exist so
    /// the mock model's `ToolUse` call has something real to dispatch to.
    struct ProbeTool;
    #[async_trait::async_trait]
    impl base::tool::Tool for ProbeTool {
        fn name(&self) -> &str {
            "Probe"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: base::tool::ToolContext,
            _progress: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, base::error::ToolError> {
            Ok(base::tool::ToolResult::text("probe-ok"))
        }
    }

    /// Mock model: 1st call emits a `Probe` tool call, every later call just
    /// ends the turn normally. Used to prove the discontinue path actually
    /// stops the outer loop — if it didn't, the agent would call the model
    /// a 2nd time asking what to do with the tool result.
    struct ToolThenStopModel {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl Model for ToolThenStopModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<base::interface::model::ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            let call_no = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events: Vec<
                Result<base::interface::model::ModelEvent, base::interface::model::ModelError>,
            > = if call_no == 0 {
                vec![
                    Ok(base::interface::model::ModelEvent::ToolUse {
                        id: "toolu_1".into(),
                        name: "Probe".into(),
                        input: serde_json::json!({}),
                    }),
                    Ok(base::interface::model::ModelEvent::EndTurn {
                        stop_reason: "tool_use".into(),
                        usage: Default::default(),
                    }),
                ]
            } else {
                vec![
                    Ok(base::interface::model::ModelEvent::TextDelta {
                        text: "done".into(),
                    }),
                    Ok(base::interface::model::ModelEvent::EndTurn {
                        stop_reason: "end_turn".into(),
                        usage: Default::default(),
                    }),
                ]
            };
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn post_tool_use_hook_discontinue_ends_the_turn_early() {
        // End-to-end regression test for the PostToolUse -> discontinue
        // wiring: a PostToolUse hook returning `continue: false` must stop
        // the turn right after the tool that triggered it, instead of
        // looping back to the model for another round. Proven two ways:
        // stop_reason == "stopped_by_hook", and the model is called exactly
        // once (a working-but-broken discontinue would still call it a 2nd
        // time to hand back the tool result).
        let mut settings = test_settings();
        settings.hooks_config = Some(serde_json::json!({
            "PostToolUse": [
                { "type": "command", "command": "echo '{\"continue\":false}'" }
            ]
        }));
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
            calls: call_count.clone(),
        });
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);
        let tools = Arc::new(InMemoryToolRegistry::new());
        tools.register(Arc::new(ProbeTool));

        let (mut agent, _event_rx, _input_tx) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(Arc::new(settings))
            .tools(tools)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let outcome = agent
            .process_turn(
                InputMessage::User {
                    content: "please use the probe tool".into(),
                    attachments: vec![],
                    turn_id: "t1".into(),
                },
                CancellationToken::new(),
            )
            .await
            .expect("process_turn should succeed");

        assert_eq!(outcome.stop_reason, "stopped_by_hook");
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "model should only have been called once — the turn must not have looped back for a 2nd round"
        );
    }

    #[test]
    fn build_hook_runner_defaults_to_empty_without_hooks_config() {
        let settings = test_settings();
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let runner = build_hook_runner(&settings, model, dummy_agent_tool(), &[]);
        assert!(runner.is_empty());
    }

    #[test]
    fn build_hook_runner_parses_hooks_config_into_real_hooks() {
        let mut settings = test_settings();
        settings.hooks_config = Some(serde_json::json!({
            "PreToolUse": [
                { "type": "command", "command": "echo hi" }
            ]
        }));
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let runner = build_hook_runner(&settings, model, dummy_agent_tool(), &[]);
        assert!(!runner.is_empty());
        assert!(runner.has_hooks_for(hooks::HookEvent::PreToolUse));
    }

    #[test]
    fn build_hook_runner_degrades_to_empty_on_malformed_hooks_config() {
        let mut settings = test_settings();
        // Wrong shape: hooks value must be a map of event -> Vec<HookConfig>.
        settings.hooks_config = Some(serde_json::json!("not-a-hooks-map"));
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let runner = build_hook_runner(&settings, model, dummy_agent_tool(), &[]);
        assert!(runner.is_empty());
    }

    #[test]
    fn build_hook_runner_installs_plugin_hooks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("plugin.toml"),
            r#"
[plugin]
name = "test-hook-plugin"
version = "1.0.0"

[hooks]
pre_tool_use = "hooks/pre.sh"
"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("hooks")).unwrap();
        std::fs::write(dir.path().join("hooks/pre.sh"), "echo plugin-hook").unwrap();
        let plugin =
            plugin::manifest::Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();

        let settings = test_settings();
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let runner = build_hook_runner(&settings, model, dummy_agent_tool(), &[plugin]);
        assert!(runner.has_hooks_for(hooks::HookEvent::PreToolUse));
    }

    #[test]
    fn channel_types_construct() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let _sender: EventSender = tx;
    }

    #[test]
    fn permission_decision_wire_format_round_trips() {
        let permit = PermissionDecision::Permit;
        let s = serde_json::to_value(&permit).unwrap();
        assert_eq!(s, serde_json::json!({"type": "permit"}));
        assert!(matches!(
            serde_json::from_value::<PermissionDecision>(s).unwrap(),
            PermissionDecision::Permit
        ));

        let deny = PermissionDecision::Deny {
            reason: "no".into(),
        };
        let s = serde_json::to_value(&deny).unwrap();
        assert_eq!(s, serde_json::json!({"type": "deny", "reason": "no"}));
        match serde_json::from_value::<PermissionDecision>(s).unwrap() {
            PermissionDecision::Deny { reason } => assert_eq!(reason, "no"),
            other => panic!("expected Deny, got {other:?}"),
        }

        let permit_always_session = PermissionDecision::PermitAlways {
            scope: PersistScope::Session,
        };
        let s = serde_json::to_value(&permit_always_session).unwrap();
        assert_eq!(
            s,
            serde_json::json!({"type": "permit_always", "scope": "session"})
        );
        match serde_json::from_value::<PermissionDecision>(s).unwrap() {
            PermissionDecision::PermitAlways {
                scope: PersistScope::Session,
            } => {}
            other => panic!("expected PermitAlways{{scope: Session}}, got {other:?}"),
        }

        let permit_always_local = PermissionDecision::PermitAlways {
            scope: PersistScope::Local,
        };
        let s = serde_json::to_value(&permit_always_local).unwrap();
        assert_eq!(
            s,
            serde_json::json!({"type": "permit_always", "scope": "local"})
        );
        match serde_json::from_value::<PermissionDecision>(s).unwrap() {
            PermissionDecision::PermitAlways {
                scope: PersistScope::Local,
            } => {}
            other => panic!("expected PermitAlways{{scope: Local}}, got {other:?}"),
        }

        // Also verify direct JSON string parsing of the exact wire shapes
        // named in the RPC docs, not just round-tripping our own output.
        let from_str: PermissionDecision =
            serde_json::from_str(r#"{"type":"permit_always","scope":"local"}"#).unwrap();
        assert!(matches!(
            from_str,
            PermissionDecision::PermitAlways {
                scope: PersistScope::Local
            }
        ));
    }
}
