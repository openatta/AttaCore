//! Engine struct and builder — the central AGENT orchestrator.

use base::interface::event::AgentEvent;
use base::interface::memory::MemoryStore;
use base::interface::model::Model;
use base::interface::permission::Permission;
use base::interface::scene::AgentScene;
use base::interface::settings::Settings;
use compaction::cached::CachedMicroCompact;
use compaction::compact::{Compactor, DefaultCompactor};
use hooks::HookRunner;
use mcp::manager::McpManager;
use telemetry::perf::PerfCollector;
use session::session::SessionManager;
use telemetry::{TelemetryHandle, TelemetryRecorder};
use base::tool::InMemoryToolRegistry;
use std::path::PathBuf;
use std::sync::Arc;
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

#[derive(Debug, Clone)]
pub enum PermissionDecision {
    Permit,
    Deny { reason: String },
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
    pub(crate) permission: Arc<dyn Permission>,
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
    /// Pre-read CLAUDE.md/ATTA.md content for userContext injection (TS parity).
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
                let _ = self.process_turn(
                    InputMessage::PermissionResponse {
                        prompt_id: "orphaned".into(),
                        decision,
                    },
                    cancel.clone(),
                ).await;
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
        let cwd = self.settings.paths.local_data_dir.clone();
        let user_skills_dir = self.settings.paths.user_data_dir.join("skills");
        let local_skills_dir = self.settings.paths.local_data_dir.join("skills");
        let base_url = self.settings.model.base_url.clone();
        let skills = std::sync::Arc::clone(&self.skills);

        let (frozen, _skills_res, _) = tokio::join!(
            // 1. Pre-compute the frozen environment snapshot (git status, branch, platform, etc.)
            base::frozen::FrozenContext::collect(cwd),
            // 2. Re-scan skills directories for newly added skills
            async move {
                let count1 = skills.load_dir(&user_skills_dir, skills::manager::SkillSource::User);
                let count2 = skills.load_dir(&local_skills_dir, skills::manager::SkillSource::Project);
                (count1.ok(), count2.ok())
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
    ) -> Result<Vec<session::session::SessionSummary>, session::session::SessionError>
    {
        self.session.list_sessions().await
    }

    /// Delete a persisted session from HistoryStore. External interface.
    pub async fn delete_session(
        &self,
        id: &str,
    ) -> Result<(), session::session::SessionError> {
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
    pub async fn run_hooks(&self, event: hooks::HookEvent, input: &hooks::HookInput) -> hooks::runner::HookRunResult {
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
            mcp_servers: None,
            mcp_manager_override: None,
            telemetry_url: None,
            instruction_file: None,
            session_id: None,
            telemetry_handle_override: None,
            skip_warmup: false,
            frozen: None,
            wake_rx: None,
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
    /// Inject a pre-built MCP manager with live connections.
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
        // Pre-read CLAUDE.md / ATTA.md content for userContext injection (TS parity).
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
            Arc::new(MemoryStore::new(
                p.user_data_dir.join("memory"),
                p.local_data_dir.join("memory"),
            ))
        });
        // Session: create in-memory manager. Persistence is handled externally
        // via history::HistoryStore injected by CLI.
        let session = SessionManager::in_memory(self.session_id);
        let compactor = self
            .compactor
            .unwrap_or_else(|| Arc::new(DefaultCompactor) as Arc<dyn Compactor>);
        let hooks = self.hooks.unwrap_or_else(|| Arc::new(HookRunner::noop()));
        // P2: Wire the wake receiver into hooks for async rewake support.
        if let Some(rx) = self.wake_rx {
            hooks.set_wake_receiver(rx);
        }
        // Telemetry: use pre-built handle if injected, else noop (events silently dropped).
        let telemetry_handle = self
            .telemetry_handle_override
            .unwrap_or_else(|| {
                let (tx, _rx) = tokio::sync::mpsc::channel(1);
                TelemetryHandle::new(tx)
            });
        // MCP: use pre-built manager if injected, else empty (no servers).
        let mcp = self.mcp_manager_override.unwrap_or_else(McpManager::empty);
        // Skill auto-loading: scan ~/.atta/code/skills/ and project/.atta/code/skills/
        let skill_mgr = skills::manager::SkillManager::new();
        let skill_load_results = [
            skill_mgr.load_dir(
                &settings.paths.user_data_dir.join("skills"),
                skills::manager::SkillSource::User,
            ),
            skill_mgr.load_dir(
                &settings.paths.local_data_dir.join("skills"),
                skills::manager::SkillSource::Project,
            ),
        ];
        let loaded_count: usize = skill_load_results.iter().filter_map(|r| r.as_ref().ok()).sum();
        // Register built-in (bundled) skills after disk skills.
        // Disk-loaded skills with the same name take priority — bundled is fallback.
        for bundled in skills::bundled::bundled_skills() {
            skill_mgr.register_bundled(bundled);
        }
        let total_skills = skill_mgr.list().len();
        tracing::info!(loaded_count, total_skills, "skills loaded (incl. bundled)");

        // Build command registry from skill manager + built-in local commands
        let skill_mgr_arc = std::sync::Arc::new(skill_mgr);
        let command_registry = std::sync::Arc::new(
            crate::commands::CommandRegistry::from_skill_manager(&skill_mgr_arc)
        );
        // Register SkillTool using the populated skill manager
        tools::register_skill_tool(&tools, Arc::clone(&skill_mgr_arc), scene.default_skills());
        // Register TaskStopTool — stop running background tasks by ID
        tools.register(std::sync::Arc::new(tools::task_stop::TaskStopTool));
        // Register TaskOutputTool — retrieve output from running/completed tasks
        tools.register(std::sync::Arc::new(tools::task_output::TaskOutputTool));
        // Register MCP resource tools if clients are available
        if !mcp.clients().is_empty() {
            tools.register(std::sync::Arc::new(mcp::tools::ListMcpResourcesTool::new(mcp.clients().to_vec())));
            tools.register(std::sync::Arc::new(mcp::tools::ReadMcpResourceTool::new(mcp.clients().to_vec())));
            tools.register(std::sync::Arc::new(mcp::tools::DispatchMcpTool::new(mcp.clients().to_vec())));
        }

        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Capture feature flags before settings is moved into the Agent struct
        let cached_mc_enabled = settings.feature_flags.cached_microcompact;

        Ok((
            Agent {
                scene,
                model,
                tools,
                settings,
                permission,
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
                cached_mc: CachedMicroCompact::new(
                    compaction::cached::CachedMcConfig {
                        enabled: cached_mc_enabled,
                        ..Default::default()
                    }
                ),
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

    #[allow(dead_code)]
    fn test_settings() -> base::interface::settings::Settings {
        use base::interface::settings::*;
        Settings {
            model: ModelSettings {
                api_type: base::provider::ApiType::Anthropic,
                base_url: String::new(), auth_token: String::new(),
                model_name: "test".into(), max_tokens: 2000,
                thinking_mode: ThinkingMode::Auto, fallback_model: None,
            },
            paths: PathSettings { user_data_dir: "/tmp".into(), local_data_dir: "/tmp".into() },
            execution: ExecutionSettings::default(),
            compaction: CompactionConfig::default(),
            sandbox: SandboxConfig::default(),
            instruction_file: None, prompt_append: None, prompt_override: None,
            vcr: None, telemetry_url: None, session_dir: None, memory_enabled: true,
            permission_mode: PermissionMode::default(),
            permission_rules: Vec::new(),
            hooks_config: None,
            mcp_servers: Vec::new(),
            language: None,
            feature_flags: Default::default(),
        }
    }

    #[test]
    fn builder_requires_scene() {
        assert!(Builder::new().build().is_err());
    }

    #[test]
    fn channel_types_construct() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let _sender: EventSender = tx;
    }
}
