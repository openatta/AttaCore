//! SessionPool —— daemon 多 session 实例管理器。
//!
//! 每个 session 对应一个独立的 Agent 实例（后台 run loop + 独立 event channel）。
//! 支持：
//! - 按 session_id 查找/创建
//! - 容量上限 + LRU 驱逐
//! - 空闲超时回收
//! - session.list 合并活跃 + 历史

use crate::rpc::{codes, RpcResponse, SessionOptions, StreamFrame};
use base::id::Id;
use base::interface::event::AgentEvent;
use base::interface::memory::MemoryStore;
use base::interface::permission::Permission;
use base::interface::scene::AgentScene;
use base::interface::settings::Settings;
use base::interface::settings::{VcrConfig, VcrMode};
use mcp::manager::McpManager;
use model::adapter::AnthropicModel;
use model::client::AnthropicClient;
use runtime::agent::{Builder, EventReceiver, InputMessage, InputSender};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use telemetry::file_recorder::FileRecorder;
use telemetry::vcr::VcrModel;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::RwLock as AsyncRwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Outcome of comparing a session's recorded `Meta.scene` (if any) against
/// every scene this daemon currently serves (`SessionPool::active_scenes`) —
/// see docs/session_and_scene_invariants.md §1.
/// Resume/fork/close/delete don't yet take a per-request `scene` param
/// (every caller implicitly means "whichever active scene this session was
/// actually recorded under"), so `Mismatch`'s reported `requested_scene`
/// is always this pool's *default* scene — that only reflects what a
/// mismatch response echoes back, not what counts as a match.
pub enum SceneCheck {
    /// No `Meta` line, or one with no `scene` field (pre-v2 file, or a
    /// session with no history at all) — nothing to compare against.
    /// Callers proceed and report `scene_inferred: true`.
    Inferred,
    /// Recorded scene matches — proceed normally.
    Matches,
    /// Recorded scene differs — callers should reject with
    /// `SCENE_MISMATCH` before touching anything.
    Mismatch(String),
}

/// `SessionPool::resume_session`'s failure type — a plain `RpcError` for
/// most cases, plus a `SidechainTerminal` variant carrying the extra
/// `parent_session_id`/`final_state` fields `server.rs` needs to build the
/// `SIDECHAIN_TERMINAL` error's structured `data` (an `RpcError` is just
/// `(code, message)`, with no room for that). Both the terminal-state check
/// and the actual resume now happen inside `resume_session`'s own
/// `with_session_lock` critical section — see that method's doc comment —
/// so this needed to travel back out through `resume_session`'s return type
/// rather than being decided by a separate, unlocked check in `server.rs`
/// before it was ever called.
pub enum ResumeError {
    SidechainTerminal {
        session_id: String,
        parent_session_id: Option<String>,
        final_state: history::entry::SessionEndState,
    },
    Rpc(RpcError),
}

/// `session.create`'s `project_root` disambiguated three ways — JSON only
/// gives "key omitted" vs. "key present" for free; `null` vs. a string
/// needs an explicit check the wire layer (`server.rs`) does before this
/// ever reaches `SessionPool`.
/// What a per-session settings entry is keyed by: the project it belongs to
/// (`None` = the global no-project tier) and the scene it runs under.
///
/// Both halves matter. The scene contributes a real settings layer
/// (`~/.atta/scenes/<scene>/settings.json`, plus the skills, agents and
/// plugins beside it), so one project under two scenes is two different
/// resolutions, not one.
type ProjectSceneKey = (Option<PathBuf>, String);

pub enum ProjectSelector {
    /// `project_root` omitted — this pool's default project (`self.cwd`),
    /// unchanged pre-P3 behavior.
    Default,
    /// `project_root: null` — a genuine no-project session (global tier).
    NoProject,
    /// `project_root: "<path>"` — must exist; `session.create` rejects it
    /// otherwise with `PROJECT_NOT_FOUND` (unlike resume, which tolerates a
    /// moved/missing project — see `settings_for_project`'s doc comment).
    Path(PathBuf),
}

/// `(code, message)` for a failed pool operation, ready to hand to
/// `RpcResponse::err`. The older pool methods return a bare `String` and let
/// `server.rs` pick one fixed code per method; the session read/branch
/// surface (`session.history`/`.fork`/`.resume`) genuinely needs three
/// different codes from the *same* method — a bad id is `INVALID_PARAMS`, an
/// unknown one is `SESSION_NOT_FOUND`, an unreadable log is
/// `INTERNAL_ERROR` — so those carry the code with the message instead of
/// inventing a new response envelope for it.
pub type RpcError = (i32, String);

/// Global → scene → project `agents/*.md` directories — same three tiers,
/// same order, as `Builder::build()`'s own (now `SessionPool`-shared) copy of
/// this computation; see `SessionPool::agent_type_catalog`.
fn agent_type_dirs(settings: &Settings) -> [std::path::PathBuf; 3] {
    [
        settings.paths.global_data_dir.join("agents"),
        settings.paths.user_data_dir.join("agents"),
        settings.paths.project_root().join(".atta").join("agents"),
    ]
}

/// Build the command catalog shared by every session: the 5 built-in local
/// commands plus a live view of `skill_catalog` (via
/// `CommandRegistry::from_skill_manager`). The skill half tracks the catalog,
/// so a skill file added or deleted after daemon startup is reflected here —
/// and in `commands.list` — without a rebuild.
fn build_shared_commands(
    skill_catalog: Arc<skills::manager::SkillManager>,
) -> Arc<runtime::commands::CommandRegistry> {
    Arc::new(runtime::commands::CommandRegistry::from_skill_manager(
        skill_catalog,
    ))
}

/// The first `LogEntry::Meta` line in `entries`, if any — every session
/// history has at most one (see `create()`), always first when present, but
/// a linear `find_map` rather than `entries.first()` since pre-v2 files can
/// have none at all. Shared by the handful of pool methods that only need
/// one or two `Meta` fields (`session_kind_parent_and_resumable`,
/// `session_kind_and_scene`, `recorded_project`, `check_scene`,
/// `sidechain_terminal_state`) so they don't each repeat the same scan
/// shape.
fn find_meta(entries: &[history::entry::EnvelopedEntry]) -> Option<&history::entry::LogEntry> {
    entries
        .iter()
        .map(|env| &env.entry)
        .find(|entry| matches!(entry, history::entry::LogEntry::Meta { .. }))
}

// ── LiveSession ─────────────────────────────────────────────────────────

struct LiveSession {
    input_tx: InputSender,
    event_rx: Arc<AsyncMutex<Option<EventReceiver>>>,
    cancel: CancellationToken,
    /// Session name（CHAT 场景首轮后由 LLM 生成；CODING 为 None）。
    name: Option<String>,
    created_at: Instant,
    last_active: Instant,
    /// 用于首轮命名判断。
    is_first_turn: bool,
    /// 这个 session 的权限询问出口：turn 期间指向那条正在流式返回的连接。
    /// 见 `crate::permission_prompt`——权限提问由 daemon 层自己发帧、自己等
    /// 回应，不走引擎那条（目前无法闭环的）`AgentEvent::PermissionPrompt`
    /// 通道。
    /// 这个 session 上"已经问出去、还没被回应"的权限询问。
    /// `session.respondToPrompt` 直接在这里兑现决定。
    /// `SessionPool.config_generation` 的值，建这个 Agent 时记录的快照。
    /// `run_turn` 每次分派给一个已存在的 session 前，会拿这个值跟池子当前的
    /// 代数比——落后就说明期间有过 `config.setProvider`/`config.reload`，
    /// 在真正发这条消息之前原地重建（见 `run_turn` 里的重建逻辑）。用代数
    /// 而不是"reload 时主动去改每个 session"，是因为改动只需要在两处发生：
    /// reload 时 `fetch_add(1)`，用到时惰性比较一次，不用遍历/标记所有
    /// session。
    config_generation: u64,
    /// The turn currently running on this session, if any.
    ///
    /// A session runs one turn at a time — `event_rx` is taken exclusively
    /// for the duration, so the second caller could never have been served
    /// anyway. This records *which* turn holds it, so the refusal can name it
    /// (`SESSION_BUSY.data.current_turn_id`) and `session.get` can report
    /// `turn_state` instead of the caller having to infer it.
    current_turn: Option<String>,
}

// ── SessionPool ─────────────────────────────────────────────────────────

pub struct SessionPool {
    sessions: AsyncMutex<HashMap<String, LiveSession>>,
    /// Per-`session_id` mutexes serializing a session's own disk-mutating
    /// critical sections against each other — see `with_session_lock`.
    /// Entries are dropped again once nothing references them, so this
    /// stays proportional to concurrently-contended ids, not every id this
    /// pool has ever touched.
    session_locks: AsyncMutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    cap: usize,
    idle_timeout: Duration,
    /// 共享的 LLM client（用于 session 命名等）。
    _client: Arc<dyn AnthropicClient>,
    model: Arc<dyn base::interface::model::Model>,
    /// Hot-swappable so `config.setProvider` can take effect for sessions
    /// created *after* the call without a daemon restart. Sessions already
    /// running keep whatever `Arc<Settings>` snapshot they were built with —
    /// only new `create()` calls observe the update.
    settings: AsyncRwLock<Arc<Settings>>,
    /// This daemon instance's *default* scene (resolved + validated from
    /// `--scene` at startup — see `main.rs::resolve_scene`). Every session
    /// created without an explicit `scene` — which today means every
    /// session, since this predates `session.create`'s `scene` param —
    /// uses this one. Left completely alone by P4: every existing call site
    /// that reads `self.scene` keeps meaning exactly what it always has.
    scene: Arc<dyn AgentScene>,
    /// P4: every scene this daemon instance currently serves, keyed by id —
    /// always at least `{self.scene.id(): self.scene}`, plus whatever
    /// `--scenes` added at startup or `scene.activate` added at runtime.
    /// `session.create`'s optional `scene` param resolves against this, not
    /// `self.scene` — the pool's *default* scene concept and *which scenes
    /// are currently servable* are related but distinct (the default is
    /// always servable; not everything servable is the default).
    active_scenes: AsyncRwLock<HashMap<String, Arc<dyn AgentScene>>>,
    /// Every scene this binary knows how to construct, active or not — what
    /// `scene.list` enumerates and `scene.activate` resolves new entries
    /// against. Immutable after construction (registering a scene is a
    /// compile-time fact); only *activation* is runtime state.
    /// Every scene this binary can serve. Swapped wholesale when the plugin
    /// set changes, since a plugin may add or withdraw a scene; a session
    /// already running holds its own `Arc<dyn AgentScene>` and is unaffected.
    scene_registry: std::sync::RwLock<Arc<scene::scene::SceneRegistry>>,
    /// The **opt-out** permission instance: an allow-everything
    /// `Permission` handed to a session only when its effective permission
    /// mode is `bypassPermissions` (see `resolve_session_permission`).
    ///
    /// This used to be the pool's unconditional default — every daemon
    /// session ran allow-all unless it explicitly asked otherwise. That
    /// trust boundary moved deliberately (see `daemon/src/main.rs`'s
    /// `AllowAllPermission` doc comment and `docs/daemon_rpc_protocol.md`): the
    /// default is now a real `RuleSetPermission` in `ask` mode, and this
    /// `Arc` is what a host that genuinely sandboxes the daemon itself
    /// opts back into.
    permission: Arc<dyn Permission>,
    memory_store: Arc<MemoryStore>,
    /// Built-in tool registry (Bash/Read/Write/Edit/Grep/Glob/...), shared
    /// (cheap `Arc` clone) by every session `Builder`. Built once here via
    /// `tools::register_builtin_tools` — before this field existed,
    /// `Builder::tools()` was never called anywhere in this file, so every
    /// daemon session ran with only the handful of tools `Builder::build()`
    /// registers internally (Skill/TaskStop/TaskOutput/Import/Agent/MCP) and
    /// nothing else — no Bash, no file edits, no web fetch. See
    /// `tools::register_builtin_tools`'s doc comment for the full story and
    /// what's deliberately excluded.
    tools: Arc<base::tool::InMemoryToolRegistry>,
    cwd: PathBuf,
    history_store: Option<Arc<dyn history::store::HistoryStore>>,
    paths: Arc<dyn crate::config::DaemonPaths>,
    /// Centrally-connected MCP servers, partitioned by project — keyed the
    /// same way `settings_for_project`/`ProjectSelector` are (`None` = the
    /// no-project tier, `Some(root)` = that project). The pool's default
    /// project (this daemon's `--cwd`) connects eagerly at startup (see
    /// `connect_mcp_servers_in_background`); every other project connects
    /// lazily on its first session (see `mcp_for_project`). Each new session
    /// still gets its own owned `McpManager` built from the cached entry's
    /// `McpClientHandle`s (cheap `Arc` clones, no new connection) — see
    /// `create()`.
    /// Keyed by `(project, scene)` for the same reason `projects` is: a
    /// scene tier can declare its own `mcp_servers`, so two scenes on one
    /// project are not interchangeable.
    mcp_by_project: AsyncMutex<HashMap<ProjectSceneKey, Arc<McpManager>>>,
    /// Daemon-level async notifications (MCP connect outcomes, import
    /// auto-detection, future error events) — see `daemon.subscribeEvents`.
    /// `send()` returning `Err` just means "no subscribers right now", not
    /// a real error, so callers of `emit_event` ignore it.
    events_tx: tokio::sync::broadcast::Sender<serde_json::Value>,
    /// The plugin subsystem — see `crate::plugins`. Everything plugins
    /// contribute arrives through it, and in a build without the `plugins`
    /// feature it is a stub that contributes nothing.
    plugins: crate::plugins::PluginSubsystem,
    /// Command catalog shared by every session — skill-derived + built-in
    /// local commands, rebuilt by
    /// `refresh_plugins()` instead of re-scanning skill dirs per session
    /// (see `build_shared_commands`). Backs the `commands.list` RPC
    /// directly and is injected into each new session via
    /// `Builder::commands_override`.
    commands: AsyncRwLock<Arc<runtime::commands::CommandRegistry>>,
    /// Multi-provider per-task-type model routing (see
    /// `base::provider::TaskRouter` / `daemon::model_router`), built at
    /// startup from `settings.providers`/`task_models` when configured and
    /// rebuilt by `apply_reloaded_settings()` on every successful
    /// `config.setProvider`/`config.reload`. `None` when the user hasn't
    /// configured `providers` — every session created by this pool then
    /// behaves exactly as before multi-provider routing existed (all
    /// sub-agent spawns inherit `self.model`). Hot-swappable like
    /// `settings`/`mcp`/`plugins`/`commands` — only sessions created *after*
    /// a swap observe it directly; already-running sessions catch up lazily
    /// via `config_generation` (see `LiveSession::config_generation`).
    task_router: AsyncRwLock<Option<Arc<base::provider::TaskRouter>>>,
    /// One shared, live-reloaded agent-type catalog (+ its single file
    /// watcher thread) for every session this pool creates — see
    /// `runtime::agent_tool::SharedAgentTypeCatalog`'s doc comment. Before
    /// this existed, `Builder::build()` had each session start its own
    /// dedicated watcher on the exact same three directories: N sessions
    /// meant N redundant `notify` background threads. Refreshed by
    /// `refresh_plugins()` so a plugin-driven agent-type change reaches
    /// already-running sessions too, not just new ones — an improvement
    /// over the old per-session catalog, which never updated for plugin
    /// changes at all once a session was built.
    agent_type_catalog: runtime::agent_tool::SharedAgentTypeCatalog,
    /// One shared, live-reloaded `SkillManager` for every session this pool
    /// creates — built once at pool construction (`build_default_skill_
    /// manager`) instead of each session re-scanning the same three skill
    /// directories in `Builder::build()` (see `Builder::skill_catalog`).
    /// Because every session shares this exact instance, a skill file
    /// change reloads once and is immediately visible to every session,
    /// including ones already running — stronger than the old per-session
    /// `SkillManager` + shared-watcher setup, where each session's own copy
    /// only caught up on its own next `check_for_changes()` poll.
    skill_catalog: Arc<skills::manager::SkillManager>,
    /// Merged settings for every `(project_root, scene)` pair this pool has
    /// built a session for. `project_root` is understood exactly as
    /// `session.create`'s param is: `None` = the global no-project tier,
    /// `Some(root)` = that project's `settings.json` over the global/scene
    /// tiers.
    ///
    /// Scene is part of the key because the scene tier is a real settings
    /// layer (`~/.atta/scenes/<scene>/settings.json`, and the skills, agents
    /// and plugins beside it). Keying by project alone meant every session
    /// got whichever scene happened to be built first, so a `chat` session
    /// on a daemon started as `--scene coding` silently read coding's tier.
    ///
    /// The pool's default `(cwd, startup scene)` pair is deliberately **not**
    /// cached here — it stays on the hot-swappable `self.settings` so
    /// `config.setProvider`/`config.reload` keep working for it. Entries here
    /// are built once and never invalidated (no `config.reload` targets a
    /// non-default project yet).
    projects: AsyncMutex<HashMap<ProjectSceneKey, Arc<Settings>>>,
    /// Bumped by `apply_reloaded_settings()` on every successful reload.
    /// Compared against each `LiveSession::config_generation` at dispatch
    /// time (`run_turn`) to decide whether an already-running session needs
    /// to be recreated before serving its next turn. `Ordering::Relaxed`
    /// throughout — this is a "did *a* reload happen since I was built"
    /// signal, not something anything is synchronized against; the actual
    /// new config is read fresh (under its own lock) whenever a session
    /// does recreate, so a relaxed load can't hand out stale settings, only
    /// a stale *generation number* (worst case: one extra turn served on
    /// old config before the recreate check catches up, never fewer).
    config_generation: std::sync::atomic::AtomicU64,
    /// How long a `kind:"prompt"` permission ask may sit unanswered before
    /// the daemon answers it with a `Deny` on the client's behalf.
    /// `Duration::ZERO` disables the timer entirely (wait forever — the
    /// engine's own semantics, appropriate for a host that really does keep
    /// a human in the loop indefinitely).
    ///
    /// Defaults to [`DEFAULT_PERMISSION_PROMPT_TIMEOUT`]; `daemon/src/main.rs`
    /// overrides it from `--permission-prompt-timeout`. See
    /// `LiveSession.pending_prompts` for why this lives here rather than in
    /// the engine.
    permission_prompt_timeout: Duration,
    /// Pool-level telemetry sink — for events that happen *before* any
    /// per-session `Agent`/handle exists (`session.resume` against a
    /// session that's already live, a terminal sidechain, or an unknown
    /// id), which have nowhere else to record to. Every per-session event
    /// still goes through that session's own handle (see `create()`); this
    /// one is only for the handful of pool-level exits that never reach
    /// `create()` at all. Writes to `<local_data_dir>/telemetry/pool.jsonl`,
    /// gated by the same `settings.telemetry_enabled` flag — noop if
    /// disabled or the path can't be resolved/opened.
    pool_telemetry: telemetry::TelemetryHandle,
}

/// Default ceiling on an unanswered `kind:"prompt"` permission ask.
///
/// Five minutes: long enough that a human being asked "allow Bash?" in an
/// IDE has time to read it and switch windows, short enough that a client
/// which simply doesn't implement `session.respondToPrompt` fails visibly
/// (one denied tool call, turn continues) instead of hanging its RPC
/// forever. Fail *closed* — the timeout denies, it never auto-approves.
pub const DEFAULT_PERMISSION_PROMPT_TIMEOUT: Duration = Duration::from_secs(300);

/// session.list 返回的单条记录。
#[derive(serde::Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub name: Option<String>,
    pub preview: Option<String>,
    pub message_count: u32,
    pub created_at: String,
    pub last_active: String,
    pub status: SessionStatus,
    pub session_kind: history::entry::SessionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// `false` only for a sidechain that already ran its one-shot task to
    /// conclusion — `session.resume` against it returns `SIDECHAIN_TERMINAL`.
    pub resumable: bool,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Inactive,
}

/// Outcome of `SessionPool::apply_reloaded_settings()` — how a
/// `config.setProvider`/`config.reload` call's routing re-resolution went.
///
/// Two genuinely different checks, kept in separate fields on purpose:
/// `warnings`/`error` are `base::provider::resolve_task_models`'s own
/// soft-fallback outcome — pure config-shape validation (does every
/// `task_models` entry resolve to a provider that exists, etc.), unaware of
/// `api_type` or whether a real client could actually be built. A config can
/// resolve cleanly (`error: None`) yet still fail to produce a live
/// `TaskRouter` (`router_rebuilt: false`, `router_error: Some(..)`) — e.g.
/// `api_type: openai_compatible`, which the schema accepts but has no
/// runtime implementation. Collapsing these into one `ok` flag would be a
/// breaking change to what `routing.ok` has always meant to callers (see
/// `set_provider_writes_project_settings_and_doctor_sees_it`'s test, which
/// deliberately uses `openai_compatible` and asserts `routing.ok == true`).
#[derive(Debug, Default)]
struct ReloadReport {
    warnings: Vec<String>,
    error: Option<String>,
    router_rebuilt: bool,
    router_error: Option<String>,
    /// MCP servers from the freshly-reloaded config that are now actually
    /// connected — see `SessionPool::reconcile_mcp_servers`.
    mcp_connected: Vec<String>,
    /// MCP servers from the freshly-reloaded config that failed to connect
    /// (or had an invalid config) — specific reasons are in the daemon log
    /// and the `mcp_connect_failed` async notification, same contract as
    /// startup connection failures.
    mcp_failed: Vec<String>,
}

impl ReloadReport {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "ok": self.error.is_none(),
            "warnings": self.warnings,
            "error": self.error,
            "router_rebuilt": self.router_rebuilt,
            "router_error": self.router_error,
            "mcp_connected": self.mcp_connected,
            "mcp_failed": self.mcp_failed,
        })
    }
}

impl SessionPool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cap: usize,
        idle_timeout_secs: u64,
        client: Arc<dyn AnthropicClient>,
        settings: Arc<Settings>,
        scene: Arc<dyn AgentScene>,
        permission: Arc<dyn Permission>,
        memory_store: Arc<MemoryStore>,
        cwd: PathBuf,
        history_store: Option<Arc<dyn history::store::HistoryStore>>,
        paths: Arc<dyn crate::config::DaemonPaths>,
        task_router: Option<Arc<base::provider::TaskRouter>>,
    ) -> Self {
        // The main conversation's model.
        //
        // `task_models.main` had no production reader anywhere in the
        // workspace: multi-provider routing reached sub-agents, compaction
        // and memory extraction, while the conversation itself stayed pinned
        // to the single `ANTHROPIC_API_KEY`/`ANTHROPIC_BASE_URL` client built
        // in `main.rs`. So "configure a provider" worked for every task
        // except the one users actually meant. Consult the router first; with
        // no `providers` block configured (`task_router == None`) this is
        // exactly the previous behavior.
        let model: Arc<dyn base::interface::model::Model> = match task_router.as_ref() {
            Some(router) => router.model_for("main"),
            None => Arc::new(AnthropicModel::new(client.clone())),
        };
        let (events_tx, _) = tokio::sync::broadcast::channel(256);

        let plugins = crate::plugins::PluginSubsystem::new(
            paths.clone(),
            cwd.clone(),
            settings.plugins.clone(),
        );
        let plugin_agent_types = plugins.agent_types();
        // Built once for every session this pool ever creates — see the
        // `skill_catalog` field doc comment. `runtime::agent::Builder`'s own
        // fallback (used by tests / library embedding without a daemon)
        // does the equivalent scan per-session; a daemon shares one.
        let skill_catalog = Arc::new(runtime::agent::build_default_skill_manager(&settings));
        let commands = build_shared_commands(skill_catalog.clone());

        let agent_dirs = agent_type_dirs(&settings);
        let agent_dirs_ref: [&std::path::Path; 3] =
            [&agent_dirs[0], &agent_dirs[1], &agent_dirs[2]];
        let agent_type_catalog = runtime::agent_tool::SharedAgentTypeCatalog::build(
            &agent_dirs_ref,
            &plugin_agent_types,
        );

        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools::register_builtin_tools(&tools);

        let mut scene_registry = scene::scene::SceneRegistry::new();
        scene_registry.register_builtin();
        let scene_registry = std::sync::RwLock::new(Arc::new(scene_registry));
        let mut active_scenes = HashMap::new();
        active_scenes.insert(scene.id().to_string(), scene.clone());

        let pool_telemetry = build_pool_telemetry(&settings);

        Self {
            sessions: AsyncMutex::new(HashMap::new()),
            session_locks: AsyncMutex::new(HashMap::new()),
            cap,
            idle_timeout: Duration::from_secs(idle_timeout_secs),
            _client: client,
            model,
            settings: AsyncRwLock::new(settings),
            scene,
            active_scenes: AsyncRwLock::new(active_scenes),
            scene_registry,
            permission,
            memory_store,
            tools,
            cwd,
            history_store,
            paths,
            mcp_by_project: AsyncMutex::new(HashMap::new()),
            events_tx,
            plugins,
            commands: AsyncRwLock::new(commands),
            task_router: AsyncRwLock::new(task_router),
            agent_type_catalog,
            skill_catalog,
            projects: AsyncMutex::new(HashMap::new()),
            config_generation: std::sync::atomic::AtomicU64::new(0),
            permission_prompt_timeout: DEFAULT_PERMISSION_PROMPT_TIMEOUT,
            pool_telemetry,
        }
    }

    /// Override how long an unanswered permission prompt may block a turn
    /// before the daemon denies it on the client's behalf. `Duration::ZERO`
    /// means "wait forever" (the engine's own behavior).
    ///
    /// Builder-style rather than another `new()` parameter: `new()` already
    /// takes eleven arguments, and every existing caller (tests included)
    /// wants the default.
    pub fn with_permission_prompt_timeout(mut self, timeout: Duration) -> Self {
        self.permission_prompt_timeout = timeout;
        self
    }

    /// Subscribe to daemon-level async notifications (MCP connect
    /// outcomes, import auto-detection, ...) — see `daemon.subscribeEvents`.
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<serde_json::Value> {
        self.events_tx.subscribe()
    }

    /// Publish a daemon-level notification to every current subscriber. A
    /// `send` error just means nobody's listening right now — not a real
    /// failure, so this is intentionally infallible from the caller's side.
    pub fn emit_event(&self, event: serde_json::Value) {
        let _ = self.events_tx.send(event);
    }

    /// The project directory this pool's sessions run in.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Full slash command catalog — built-in locals + skill-derived
    /// (see `build_shared_commands`) — for the
    /// `commands.list` RPC. Executing one is not a separate RPC: send
    /// `/name args` as the `message` of `session.run_turn`, which already
    /// intercepts and runs it (see `runtime::turn::process_turn`).
    pub async fn list_commands(&self) -> Vec<runtime::commands::CommandInfo> {
        self.commands.read().await.list_detailed()
    }

    /// MCP server configs declared by *active* installed plugins. Callers
    /// merge this into `settings.mcp_servers` before the single startup
    /// `connect_mcp_servers_in_background` call, so plugin-declared servers
    /// get the exact same centrally-connected/shared-across-sessions
    /// treatment as user-configured ones.
    pub async fn plugin_mcp_servers(&self) -> HashMap<String, serde_json::Value> {
        self.plugins.mcp_servers()
    }

    fn scene_registry(&self) -> Arc<scene::scene::SceneRegistry> {
        self.scene_registry
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Compile and interrogate installed plugin components, then take up
    /// whatever scenes they own.
    ///
    /// **Every embedder that constructs a `SessionPool` must call this
    /// before serving.** It cannot happen in `new`, which is synchronous
    /// while compiling a component is not; and until it has run, plugins
    /// contribute their manifests but none of their tools or scenes, so a
    /// session created in that window is silently missing them with nothing
    /// to indicate why.
    pub async fn load_plugin_components(&self) {
        self.plugins.load_components().await;
        self.adopt_plugin_scenes().await;
    }

    /// Rebuild the scene registry from the built-ins plus whatever the
    /// current plugin set owns, and make those scenes servable.
    ///
    /// Plugin scenes are activated rather than merely registered: installing
    /// and enabling a plugin that owns a scene *is* the user's consent to
    /// serve it, and requiring a second `scene.activate` would be a step
    /// with no safety it buys — entering the scene is still an explicit
    /// `session.create`.
    ///
    /// Withdrawing one removes it from both maps, which stops new sessions
    /// from entering it. Sessions already inside keep the `Arc` they were
    /// built with and run to completion — the alternative would be pulling a
    /// session's system prompt out from under it mid-turn.
    async fn adopt_plugin_scenes(&self) {
        let plugin_scenes = self.plugins.scenes();

        let mut registry = scene::scene::SceneRegistry::new();
        registry.register_builtin();
        for s in &plugin_scenes {
            registry.register(s.clone());
        }
        let plugin_ids: std::collections::HashSet<String> =
            plugin_scenes.iter().map(|s| s.id().to_string()).collect();
        *self
            .scene_registry
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Arc::new(registry);

        let mut active = self.active_scenes.write().await;
        active.retain(|id, _| !id.starts_with("plugin:") || plugin_ids.contains(id));
        for s in plugin_scenes {
            active.insert(s.id().to_string(), s);
        }
    }

    /// Whether this binary has the plugin subsystem, and whether it is on —
    /// see `crate::plugins::PluginStatus`. `daemon.doctor` reports it, and
    /// the `plugin.*` RPCs refuse with `PLUGINS_DISABLED` when it is not
    /// `Enabled`.
    pub fn plugin_status(&self) -> crate::plugins::PluginStatus {
        self.plugins.status()
    }

    /// Every installed plugin with its current enable state — for the
    /// `plugin.list` RPC. Includes disabled ones, since a management UI needs
    /// to show and re-enable them.
    pub async fn list_plugins(&self) -> Vec<serde_json::Value> {
        self.plugins.list()
    }

    /// Install a plugin from an explicit source. `scope` picks which tier's
    /// cache it lands in ("global", the default a caller should use unless it
    /// specifically wants a scene-only install, or "scene"). Refreshes the
    /// active set on success so sessions created after this call see it —
    /// already-running sessions are unaffected, same as
    /// `config.setProvider`/`mcp.addServer`.
    pub async fn install_plugin(
        &self,
        name: &str,
        version: &str,
        download_url: &str,
        checksum: Option<&str>,
        scope: &str,
    ) -> Result<serde_json::Value, String> {
        let out = self
            .plugins
            .install(name, version, download_url, checksum, scope)
            .await?;
        self.refresh_after_plugin_change().await;
        Ok(out)
    }

    /// Uninstall a plugin (all versions, or a specific one) from `scope`'s
    /// tier — see `install_plugin` for the refresh semantics.
    pub async fn uninstall_plugin(
        &self,
        name: &str,
        version: Option<&str>,
        scope: &str,
    ) -> Result<serde_json::Value, String> {
        let out = self.plugins.uninstall(name, version, scope).await?;
        self.refresh_after_plugin_change().await;
        Ok(out)
    }

    /// Enable or disable a plugin by name in `scope`'s tier — see
    /// `install_plugin` for the refresh semantics.
    pub async fn set_plugin_enabled(
        &self,
        name: &str,
        enabled: bool,
        scope: &str,
    ) -> Result<serde_json::Value, String> {
        let out = self.plugins.set_enabled(name, enabled, scope).await?;
        self.refresh_after_plugin_change().await;
        Ok(out)
    }

    /// Re-merge whatever the plugin set now contributes into the pool-wide
    /// catalogs. The subsystem has already re-read itself by this point; what
    /// is left is the state this pool derives from it. Skill directories are
    /// deliberately not re-scanned: `self.skill_catalog` is the one
    /// persistent, live-reloaded manager every session shares.
    async fn refresh_after_plugin_change(&self) {
        self.adopt_plugin_scenes().await;
        let settings = self.settings.read().await.clone();
        let agent_dirs = agent_type_dirs(&settings);
        let agent_dirs_ref: [&std::path::Path; 3] =
            [&agent_dirs[0], &agent_dirs[1], &agent_dirs[2]];
        self.agent_type_catalog
            .refresh(&agent_dirs_ref, &self.plugins.agent_types());
        *self.commands.write().await = build_shared_commands(self.skill_catalog.clone());
    }

    /// Current MCP connection status for this pool's *default* project —
    /// see `mcp::manager::McpManager::server_statuses`. A pure cache read:
    /// unlike `mcp_for_project`, this never triggers a connect, so an
    /// unconnected (or not-yet-connected) project just reports no servers
    /// rather than paying for a connect attempt on a status query.
    pub async fn mcp_status(&self) -> Vec<serde_json::Value> {
        let startup_scene = self.settings.read().await.paths.scope.clone();
        self.mcp_by_project
            .lock()
            .await
            .get(&(Some(self.cwd.clone()), startup_scene))
            .cloned()
            .unwrap_or_else(|| Arc::new(McpManager::empty()))
            .server_statuses()
            .into_iter()
            .map(|s| serde_json::json!({"name": s.name, "transport": s.transport, "tool_count": s.tool_count}))
            .collect()
    }

    /// Connect the MCP servers configured in `settings.mcp_servers`
    /// (`HashMap<String, serde_json::Value>`, one entry per server) in the
    /// background — never blocks the caller. Each entry is parsed into an
    /// `mcp::config::McpServerConfig`; entries that fail to parse, or
    /// servers `McpManager::connect_all` can't connect to, are skipped with
    /// a `tracing::warn!` (already logged inside `connect_all` itself for
    /// the connect-failure case) *and* a `mcp_connect_failed`
    /// `daemon.event` notification — see `crate::rpc::StreamFrame::daemon_event`.
    /// Successfully connected servers get a `mcp_connected` notification.
    /// Once connection attempts finish, the pool's default-project cache
    /// entry (`mcp_by_project[Some(cwd)]`) is replaced — new sessions
    /// created for the default project after that point see the connected
    /// servers; sessions already running, and every other project's cache
    /// entry, are unaffected.
    pub fn connect_mcp_servers_in_background(
        self: &Arc<Self>,
        servers: HashMap<String, serde_json::Value>,
    ) {
        if servers.is_empty() {
            return;
        }
        let pool = self.clone();
        tokio::spawn(async move {
            let mut parsed = HashMap::new();
            for (name, v) in servers {
                match serde_json::from_value::<mcp::config::McpServerConfig>(v) {
                    Ok(cfg) => {
                        parsed.insert(name, cfg);
                    }
                    Err(e) => {
                        tracing::warn!(server = %name, error = %e, "invalid mcp_servers config, skipping");
                        pool.emit_event(serde_json::json!({
                            "kind": "mcp_connect_failed",
                            "server": name,
                            "error": format!("invalid config: {e}"),
                        }));
                    }
                }
            }
            if parsed.is_empty() {
                return;
            }
            let requested: std::collections::HashSet<String> = parsed.keys().cloned().collect();
            let mut manager = McpManager::connect_all(parsed).await;
            // A server may have announced a tool change while this project's
            // manager was being built or reused. Acting on it here is what
            // keeps the announcement from being one more thing nobody reads.
            manager.refresh_tools_if_announced().await;
            let statuses = manager.server_statuses();
            let connected: std::collections::HashSet<String> =
                statuses.iter().map(|s| s.name.clone()).collect();
            for status in &statuses {
                pool.emit_event(serde_json::json!({
                    "kind": "mcp_connected",
                    "server": status.name,
                    "transport": status.transport,
                    "tool_count": status.tool_count,
                }));
            }
            for name in requested.difference(&connected) {
                // `connect_all` already logged the specific reason via its own
                // `tracing::warn!` — not re-derived here, see its doc comment.
                pool.emit_event(serde_json::json!({
                    "kind": "mcp_connect_failed",
                    "server": name,
                    "error": "connect failed — see daemon log for the specific reason",
                }));
            }
            let startup_scene = pool.settings.read().await.paths.scope.clone();
            pool.mcp_by_project
                .lock()
                .await
                .insert((Some(pool.cwd.clone()), startup_scene), Arc::new(manager));
        });
    }

    /// Reconnect MCP servers to match `mcp_servers` (from freshly-reloaded
    /// `Settings`) — the `config.reload`/`config.setProvider` counterpart to
    /// `connect_mcp_servers_in_background`, which only ever ran once at
    /// daemon startup. Before this, `apply_reloaded_settings` re-read
    /// `mcp_servers` into the new `Settings` (so it was visible to
    /// `doctor`/`get_providers`) but never actually reconnected anything —
    /// a hand-edited `mcp_servers` entry picked up via `config.reload` was
    /// invisible to every session, including ones rebuilt afterward via
    /// `config_generation` (`create()` sources MCP from the same
    /// `mcp_by_project` cache, not from `Settings` directly) — only
    /// `mcp.addServer` or a full daemon restart actually connected it.
    ///
    /// Runs a full `connect_all` over the *complete* new config rather than
    /// an incremental diff against the old one — simpler and still correct:
    /// `connect_all` doesn't special-case an empty map, so a config that
    /// removed every server naturally produces an empty `McpManager`,
    /// cleanly disconnecting everything. Awaited inline (not backgrounded
    /// like the startup path) since `config.reload`/`config.setProvider` are
    /// already explicit, human-triggered operations where the caller
    /// reasonably wants the RPC response to reflect the *actual* post-reload
    /// connection state rather than a "kicked off, check back later" status.
    ///
    /// Only replaces `mcp_by_project`'s entry for `project_root` — both
    /// current callers pass the pool's default project (the only project
    /// `config.reload`/`config.setProvider` ever re-read settings for), so
    /// every *other* project's cached connections are left untouched rather
    /// than being torn down by an edit that has nothing to do with them.
    ///
    /// Returns `(connected, failed)` server names for `ReloadReport`.
    async fn reconcile_mcp_servers(
        &self,
        project_root: Option<&Path>,
        mcp_servers: &HashMap<String, serde_json::Value>,
    ) -> (Vec<String>, Vec<String>) {
        let mut parsed = HashMap::new();
        let mut invalid = Vec::new();
        for (name, v) in mcp_servers {
            match serde_json::from_value::<mcp::config::McpServerConfig>(v.clone()) {
                Ok(cfg) => {
                    parsed.insert(name.clone(), cfg);
                }
                Err(e) => {
                    tracing::warn!(server = %name, error = %e, "invalid mcp_servers config, skipping");
                    invalid.push(name.clone());
                }
            }
        }

        let requested: std::collections::HashSet<String> = parsed.keys().cloned().collect();
        let manager = McpManager::connect_all(parsed).await;
        let statuses = manager.server_statuses();
        let connected: Vec<String> = statuses.iter().map(|s| s.name.clone()).collect();
        let connected_set: std::collections::HashSet<String> = connected.iter().cloned().collect();
        let mut failed: Vec<String> = requested.difference(&connected_set).cloned().collect();
        failed.extend(invalid);

        for status in &statuses {
            self.emit_event(serde_json::json!({
                "kind": "mcp_connected",
                "server": status.name,
                "transport": status.transport,
                "tool_count": status.tool_count,
            }));
        }
        for name in &failed {
            self.emit_event(serde_json::json!({
                "kind": "mcp_connect_failed",
                "server": name,
                "error": "connect failed — see daemon log for the specific reason",
            }));
        }

        let startup_scene = self.settings.read().await.paths.scope.clone();
        self.mcp_by_project.lock().await.insert(
            (project_root.map(Path::to_path_buf), startup_scene),
            Arc::new(manager),
        );
        (connected, failed)
    }

    /// Merge an MCP server config patch into the project-tier settings.json
    /// (same partial-patch semantics as `set_provider` — see its doc
    /// comment for why raw JSON, not a typed struct), then connect it live
    /// so it's usable immediately without a daemon restart.
    pub async fn add_mcp_server(
        &self,
        name: &str,
        config: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        if name.trim().is_empty() {
            return Err("name must not be empty".into());
        }
        if !config.is_object() {
            return Err("config must be a JSON object".into());
        }
        let cfg: mcp::config::McpServerConfig = serde_json::from_value(config.clone())
            .map_err(|e| format!("invalid mcp server config: {e}"))?;

        let settings_path = self.project_settings_path();
        let mut project_json: serde_json::Value = if settings_path.exists() {
            let text = tokio::fs::read_to_string(&settings_path)
                .await
                .map_err(|e| format!("read {}: {e}", settings_path.display()))?;
            let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
                format!(
                    "existing {} is not valid JSON, refusing to overwrite it: {e}",
                    settings_path.display()
                )
            })?;
            if !parsed.is_object() {
                return Err(format!(
                    "existing {} is not a JSON object at the top level, refusing to overwrite it",
                    settings_path.display()
                ));
            }
            parsed
        } else {
            serde_json::json!({})
        };
        let obj = project_json
            .as_object_mut()
            .expect("verified to be an object above");
        let mcp_servers = obj
            .entry("mcp_servers")
            .or_insert_with(|| serde_json::json!({}));
        if !mcp_servers.is_object() {
            *mcp_servers = serde_json::json!({});
        }
        mcp_servers
            .as_object_mut()
            .expect("just normalized to object above")
            .insert(name.to_string(), config);

        if let Some(parent) = settings_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let pretty = serde_json::to_string_pretty(&project_json).map_err(|e| e.to_string())?;
        tokio::fs::write(&settings_path, pretty)
            .await
            .map_err(|e| format!("write {}: {e}", settings_path.display()))?;

        // `add_mcp_server` only ever writes the default project's
        // settings.json (`project_settings_path()`), so it only ever
        // mutates that project's cache entry, under the daemon's own scene.
        let default_project = (
            Some(self.cwd.clone()),
            self.settings.read().await.paths.scope.clone(),
        );
        let statuses = {
            let mut cache = self.mcp_by_project.lock().await;
            let current = cache
                .get(&default_project)
                .cloned()
                .unwrap_or_else(|| Arc::new(McpManager::empty()));
            let mut updated = McpManager::from_clients(current.clients().to_vec());
            updated.refresh_tools().await;
            updated.add_server(name, &cfg).await;
            let statuses = updated.server_statuses();
            cache.insert(default_project, Arc::new(updated));
            statuses
        };

        // Same notification contract as the startup background connect —
        // see `connect_mcp_servers_in_background`.
        match statuses.iter().find(|s| s.name == name) {
            Some(s) => self.emit_event(serde_json::json!({
                "kind": "mcp_connected",
                "server": s.name,
                "transport": s.transport,
                "tool_count": s.tool_count,
            })),
            None => self.emit_event(serde_json::json!({
                "kind": "mcp_connect_failed",
                "server": name,
                "error": "connect failed — see daemon log for the specific reason",
            })),
        }

        Ok(serde_json::json!({
            "written_to": settings_path.display().to_string(),
            "servers": statuses.into_iter().map(|s| serde_json::json!({
                "name": s.name, "transport": s.transport, "tool_count": s.tool_count,
            })).collect::<Vec<_>>(),
        }))
    }

    /// Read back the current effective provider/task_models configuration —
    /// the read-side counterpart to `set_provider`. `api_key` is redacted
    /// (`***<last 4 chars>`) unless `include_secrets` is `true` — default
    /// to redacted so a caller that just wants to know *which* providers
    /// are configured doesn't accidentally end up with plaintext keys in
    /// its own logs/UI. See the trust-boundary note on `config.setProvider`
    /// in `daemon/src/server.rs` — `include_secrets: true` is still gated
    /// by nothing more than "can reach this socket", same as every other
    /// method here.
    pub async fn get_providers(&self, include_secrets: bool) -> serde_json::Value {
        let settings = self.settings.read().await;
        let providers: serde_json::Map<String, serde_json::Value> = settings
            .providers
            .iter()
            .map(|(id, cfg)| {
                let api_key = match &cfg.api_key {
                    None => serde_json::Value::Null,
                    Some(k) if include_secrets => serde_json::Value::String(k.clone()),
                    Some(k) => serde_json::Value::String(redact_secret(k)),
                };
                (
                    id.clone(),
                    serde_json::json!({
                        "api_type": cfg.api_type,
                        "base_url": cfg.base_url,
                        "api_key": api_key,
                        "default_model": cfg.default_model,
                        "models": cfg.models,
                    }),
                )
            })
            .collect();
        serde_json::json!({
            "providers": providers,
            "default_provider": settings.default_provider,
            "task_models": settings.task_models,
        })
    }

    /// Detect importable cross-tool configuration sources (Claude Code/
    /// Codex/Cursor) in this pool's project directory — pure filesystem
    /// detection, no LLM turn needed. The manual `/import` path (the bundled
    /// `import` skill) does its own detection with the file tools.
    pub async fn list_import_sources(&self) -> Vec<serde_json::Value> {
        base::frozen::detect_import_sources(&self.cwd)
            .await
            .iter()
            .map(|s| serde_json::json!({"source": s.kind().as_str(), "description": s.describe()}))
            .collect()
    }

    /// Execute an import for a previously-detected `source` (one of the
    /// `source` values `list_import_sources` returns) and record the
    /// decision so the automatic startup detection doesn't ask again.
    pub async fn run_import(&self, source: &str) -> Result<serde_json::Value, String> {
        let kind = base::frozen::ImportSourceKind::try_parse(source).ok_or_else(|| {
            format!("unknown source `{source}` — expected claude_code, codex, or cursor")
        })?;
        let sources = base::frozen::detect_import_sources(&self.cwd).await;
        let Some(matched) = sources.iter().find(|s| s.kind() == kind) else {
            return Err(format!(
                "source `{source}` not currently detected in this project"
            ));
        };
        let summary = base::frozen::execute_import(&self.cwd, matched)
            .await
            .map_err(|e| e.to_string())?;
        base::frozen::mark_imported(&self.cwd, &sources, kind)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({
            "source": summary.kind.as_str(),
            "actions": summary.actions,
        }))
    }

    /// Read-only config/wiring diagnostics — see `crate::doctor`.
    pub async fn doctor_report(&self) -> serde_json::Value {
        let settings = self.settings.read().await;
        crate::doctor::run_doctor(
            self.paths.as_ref(),
            self.scene.id(),
            &settings,
            self.history_store.is_some(),
            self.plugins.status(),
        )
    }

    /// The currently-active settings snapshot (for config-inspection RPCs).
    pub async fn settings(&self) -> Arc<Settings> {
        self.settings.read().await.clone()
    }

    /// Where this pool's project-tier settings.json lives
    /// (`{project_root}/.atta/settings.json`) — the tier `config.setProvider`
    /// writes to.
    pub fn project_settings_path(&self) -> PathBuf {
        self.paths
            .project_root()
            .join(".atta")
            .join("settings.json")
    }

    /// Merge a provider config patch (and/or `default_provider` /
    /// `task_models` patches) into the project-tier settings.json, then
    /// reload the full effective `Settings` (global → scene → project) so
    /// validation and any session created after this call see the change —
    /// see `crate::doctor` module docs for the same tri-tier layout.
    ///
    /// `config_patch`/`task_models_patch` are raw partial JSON objects
    /// merged key-by-key onto the existing project-tier file (see
    /// `base::interface::settings::merge_json_values`) — deliberately not
    /// typed `ProviderConfig` structs, since deserializing a partial patch
    /// into a `#[serde(default)]` struct would silently null out every field
    /// the caller didn't mention, clobbering unrelated existing values.
    /// `delete: true` removes the `providers.<id>` entry outright rather
    /// than merging; any `task_models` entry that referenced it degrades to
    /// `default_provider` automatically the next time routing is resolved
    /// (see `base::provider::resolve_task_models`'s soft-fallback) — no
    /// separate "downgrade" write is needed.
    pub async fn set_provider(
        &self,
        provider_id: &str,
        delete: bool,
        config_patch: Option<serde_json::Value>,
        default_provider: Option<String>,
        task_models_patch: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        if provider_id.trim().is_empty() {
            return Err("provider_id must not be empty".into());
        }

        if let Some(ref cfg) = config_patch {
            if !cfg.is_object() {
                return Err("config must be a JSON object".into());
            }
            serde_json::from_value::<base::provider::ProviderConfig>(cfg.clone())
                .map_err(|e| format!("invalid provider config: {e}"))?;
        }
        if let Some(ref tm) = task_models_patch {
            let obj = tm.as_object().ok_or("task_models must be a JSON object")?;
            for (task, ov) in obj {
                serde_json::from_value::<base::provider::TaskModelOverride>(ov.clone())
                    .map_err(|e| format!("invalid task_models.{task}: {e}"))?;
            }
        }

        let settings_path = self.project_settings_path();
        let mut project_json: serde_json::Value = if settings_path.exists() {
            let text = tokio::fs::read_to_string(&settings_path)
                .await
                .map_err(|e| format!("read {}: {e}", settings_path.display()))?;
            // Refuse to touch the file if it's not currently valid — falling
            // back to `{}` here would silently discard every other field the
            // user has configured (sandbox/prompt_append/permission_rules/...),
            // not just providers, the moment this RPC writes back to disk.
            let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
                format!(
                    "existing {} is not valid JSON, refusing to overwrite it: {e}",
                    settings_path.display()
                )
            })?;
            if !parsed.is_object() {
                return Err(format!(
                    "existing {} is not a JSON object at the top level, refusing to overwrite it",
                    settings_path.display()
                ));
            }
            parsed
        } else {
            serde_json::json!({})
        };
        let obj = project_json
            .as_object_mut()
            .expect("verified to be an object above");

        if delete {
            if let Some(providers) = obj.get_mut("providers").and_then(|v| v.as_object_mut()) {
                providers.remove(provider_id);
            }
        } else if let Some(cfg) = config_patch {
            let providers = obj
                .entry("providers")
                .or_insert_with(|| serde_json::json!({}));
            if !providers.is_object() {
                *providers = serde_json::json!({});
            }
            let providers_obj = providers
                .as_object_mut()
                .expect("just normalized to object above");
            let mut existing = providers_obj
                .remove(provider_id)
                .unwrap_or_else(|| serde_json::json!({}));
            base::interface::settings::merge_json_values(&mut existing, cfg);
            providers_obj.insert(provider_id.to_string(), existing);
        }

        if let Some(dp) = default_provider {
            obj.insert(
                "default_provider".to_string(),
                serde_json::Value::String(dp),
            );
        }

        if let Some(tm) = task_models_patch {
            let task_models = obj
                .entry("task_models")
                .or_insert_with(|| serde_json::json!({}));
            if !task_models.is_object() {
                *task_models = serde_json::json!({});
            }
            base::interface::settings::merge_json_values(task_models, tm);
        }

        if let Some(parent) = settings_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let pretty = serde_json::to_string_pretty(&project_json).map_err(|e| e.to_string())?;
        tokio::fs::write(&settings_path, pretty)
            .await
            .map_err(|e| format!("write {}: {e}", settings_path.display()))?;

        let (reloaded, report) = self.apply_reloaded_settings().await;
        let result = serde_json::json!({
            "written_to": settings_path.display().to_string(),
            "providers": reloaded.providers.keys().collect::<Vec<_>>(),
            "default_provider": reloaded.default_provider,
            "task_models": reloaded.task_models.keys().collect::<Vec<_>>(),
            "routing": report.to_json(),
        });

        Ok(result)
    }

    /// Re-read whatever is currently on disk across all three settings.json
    /// tiers and apply it — the `config.reload` RPC's backing implementation,
    /// used when a human (or another process) hand-edited a settings.json
    /// file directly instead of going through `config.setProvider`. Shares
    /// `apply_reloaded_settings()` with `set_provider()`, so both entry
    /// points converge on identical semantics (routing re-resolved,
    /// `task_router` rebuilt, `config_generation` bumped so already-running
    /// sessions recreate themselves lazily — see `run_turn`).
    pub async fn reload_settings(&self) -> serde_json::Value {
        let (reloaded, report) = self.apply_reloaded_settings().await;
        serde_json::json!({
            "providers": reloaded.providers.keys().collect::<Vec<_>>(),
            "default_provider": reloaded.default_provider,
            "task_models": reloaded.task_models.keys().collect::<Vec<_>>(),
            "routing": report.to_json(),
        })
    }

    /// Re-read the three-tier merged settings.json from disk, re-resolve
    /// `providers`/`task_models`, rebuild `self.task_router` from the result,
    /// swap `self.settings`, and bump `self.config_generation` so already
    /// running sessions know to recreate themselves before their next turn
    /// (see `run_turn`). Shared by `set_provider()` (called after writing a
    /// patch) and `reload_settings()` (called directly, no patch — just
    /// "whatever's on disk right now").
    ///
    /// Never fails outright — misconfiguration degrades to "keep the
    /// previous `task_router`, report why in the returned `ReloadReport`",
    /// matching the pre-existing `resolve_task_models`-is-advisory
    /// philosophy this function's callers already had (`set_provider`
    /// used to write settings.json unconditionally even when the routing
    /// preview it computed was invalid).
    async fn apply_reloaded_settings(&self) -> (Arc<Settings>, ReloadReport) {
        let default_model = self.settings.read().await.model.model_name.clone();
        let reloaded = Settings::load(
            self.paths.global_root(),
            self.paths.config_root(),
            self.paths.project_root().join(".atta"),
            self.scene.id(),
            &default_model,
        );

        let mut report = ReloadReport::default();

        if reloaded.providers.is_empty() {
            // No providers configured (or the last one was just deleted) —
            // routing reverts to "no router", same as if multi-provider
            // config was never touched.
            *self.task_router.write().await = None;
        } else {
            match base::provider::resolve_task_models(
                &reloaded.providers,
                reloaded.default_provider.as_deref(),
                &reloaded.task_models,
            ) {
                Ok((resolved, warnings)) => {
                    report.warnings = warnings;
                    // `resolve_task_models` succeeding guarantees
                    // `default_provider` is `Some` and non-empty (it's one
                    // of the function's own hard-error checks).
                    let default_provider = reloaded
                        .default_provider
                        .as_deref()
                        .expect("resolve_task_models already validated default_provider is set");
                    match crate::model_router::build_task_router(
                        &reloaded.providers,
                        default_provider,
                        resolved,
                    ) {
                        Ok(router) => {
                            *self.task_router.write().await = Some(Arc::new(router));
                            report.router_rebuilt = true;
                        }
                        Err(e) => {
                            // Structural failure building a client (bad
                            // api_key/base_url/unsupported api_type) — keep
                            // serving on whatever `task_router` was already
                            // live rather than tearing down working routing
                            // over a bad edit. Deliberately `router_error`,
                            // not `error` — `resolve_task_models` itself
                            // succeeded (`report.warnings` above already
                            // reflects that), only the build step failed;
                            // see `ReloadReport`'s doc comment for why these
                            // stay separate.
                            warn!(error = %e, "task router rebuild failed; keeping previous router");
                            report.router_error = Some(e);
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "provider config no longer resolves; keeping previous router");
                    report.error = Some(e);
                }
            }
        }

        let (mcp_connected, mcp_failed) = self
            .reconcile_mcp_servers(Some(self.cwd.as_path()), &reloaded.mcp_servers)
            .await;
        report.mcp_connected = mcp_connected;
        report.mcp_failed = mcp_failed;

        let reloaded = Arc::new(reloaded);
        *self.settings.write().await = reloaded.clone();
        self.config_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        (reloaded, report)
    }

    /// `session.create` — P3's explicit multi-project entry point, extended
    /// in P4 with an optional `scene` (resolved against `active_scenes` —
    /// see `resolve_scene`; omitted keeps the pre-P4 behavior of always
    /// using the pool's default scene exactly as before). Builds and starts
    /// a new primary session bound to `project_root`, or this pool's
    /// default project for `ProjectSelector::Default`. Unlike the implicit
    /// `run_turn(session_id: None)` path, this is the one place a caller
    /// can ask for a session bound to a project (P3) or scene (P4) other
    /// than the pool's own.
    pub async fn create_session(
        &self,
        scene_id: Option<&str>,
        project_root: ProjectSelector,
        options: Option<&SessionOptions>,
    ) -> Result<serde_json::Value, RpcError> {
        let Some(scene) = self.resolve_scene(scene_id).await else {
            return Err((
                codes::SCENE_NOT_FOUND,
                format!(
                    "scene is not active on this daemon: {}",
                    scene_id.unwrap_or("")
                ),
            ));
        };

        // Settings are resolved against *this session's* scene, not the
        // daemon's. The `Default` arm goes through the same call as the others
        // so that a non-startup scene on the default project still picks up
        // its own tier; `settings_for_project` short-circuits to the
        // hot-swappable `self.settings` for the one pair that is the pool's
        // own.
        // A scene that requires a project is refused here, not later: with no
        // project root there is nothing for its tools to read or write, and
        // the failure is far more legible at creation than at the first turn.
        if scene.requires_project() && matches!(project_root, ProjectSelector::NoProject) {
            return Err((
                codes::PROJECT_REQUIRED,
                format!(
                    "scene `{}` requires a project; pass project_root",
                    scene.id()
                ),
            ));
        }

        let session_scene_id = scene.id().to_string();
        let (base_settings, project_root_for_meta) = match project_root {
            ProjectSelector::Default => (
                self.settings_for_project(Some(&self.cwd.clone()), &session_scene_id)
                    .await,
                Some(self.cwd.clone()),
            ),
            ProjectSelector::NoProject => (
                self.settings_for_project(None, &session_scene_id).await,
                None,
            ),
            ProjectSelector::Path(p) => {
                if !p.is_dir() {
                    return Err((
                        codes::PROJECT_NOT_FOUND,
                        format!(
                            "project_root does not exist or is not a directory: {}",
                            p.display()
                        ),
                    ));
                }
                (
                    self.settings_for_project(Some(&p), &session_scene_id).await,
                    Some(p),
                )
            }
        };

        let scene_id_for_result = scene.id().to_string();
        let sid = Id::new().to_string();
        let created_id = self
            .create(
                sid,
                scene,
                base_settings,
                project_root_for_meta.clone(),
                options,
                false,
            )
            .await
            .map_err(|e| (codes::INTERNAL_ERROR, e))?;

        let created_at = {
            let sessions = self.sessions.lock().await;
            sessions
                .get(&created_id)
                .map(|live| format_instant(live.created_at))
                .unwrap_or_default()
        };

        Ok(serde_json::json!({
            "session_id": created_id,
            "scene": scene_id_for_result,
            "session_kind": "primary",
            "project_root": project_root_for_meta.map(|p| p.display().to_string()),
            "created_at": created_at,
        }))
    }

    /// 创建新 session 并启动 Agent 后台 run loop。
    /// `options` 仅在新创建 session 时生效；已有 session 时忽略。
    ///
    /// `base_settings`/`project_root_for_meta` are the already-resolved
    /// settings for whichever project this session belongs to (see
    /// `settings_for_project`) and the value to stamp on `Meta.project_root`
    /// (`None` = no-project session) — callers that just want "this pool's
    /// default project, unchanged" pass `self.settings.read().await.clone()`
    /// / `Some(self.cwd.clone())`, preserving pre-P3 behavior exactly.
    async fn create(
        &self,
        session_id: String,
        scene: Arc<dyn AgentScene>,
        base_settings: Arc<Settings>,
        project_root_for_meta: Option<PathBuf>,
        options: Option<&SessionOptions>,
        resume: bool,
    ) -> Result<String, String> {
        let scene_id = scene.id().to_string();
        // Apply VCR wrapping if configured
        let model: Arc<dyn base::interface::model::Model> =
            match options.and_then(|o| o.vcr.as_ref()) {
                Some(vcr) => {
                    let mode = match vcr.mode.as_str() {
                        "record" => VcrMode::Record,
                        _ => VcrMode::Replay,
                    };
                    Arc::new(VcrModel::new(
                        self.model.clone(),
                        Some(VcrConfig {
                            mode,
                            scenario: vcr.scenario.clone(),
                            fallback_on_miss: !vcr.strict,
                        }),
                        std::path::PathBuf::from("/tmp/atta_vcr_nonexistent"),
                        std::path::PathBuf::from(&vcr.dir),
                    ))
                }
                None => self.model.clone(),
            };

        // The prompt timeout is a daemon-level flag but the wait it bounds
        // lives in the engine, so it travels as a setting rather than as a
        // daemon-side wrapper around `Permission`. `Settings` is behind an
        // `Arc`, so override on a private clone rather than in place.
        let settings_snapshot = {
            let mut snap = (*base_settings).clone();
            snap.execution.permission_prompt_timeout_secs =
                self.permission_prompt_timeout.as_secs();
            Arc::new(snap)
        };
        let permission =
            resolve_session_permission(&self.permission, &self.tools, &settings_snapshot, options);

        let mut builder = Builder::new()
            .scene(scene)
            .model(model)
            .tools(self.tools.clone())
            .settings(settings_snapshot.clone())
            .permission(permission)
            .memory_store(self.memory_store.clone())
            .session_id(session_id.clone())
            .commands_override(self.commands.read().await.clone())
            .shared_agent_types(self.agent_type_catalog.handle())
            .skill_catalog(self.skill_catalog.clone());
        if let Some(ref store) = self.history_store {
            builder = builder.history_store(store.clone());
        }
        if let Some(router) = self.task_router.read().await.clone() {
            builder = builder.task_router(router);
        }
        builder = builder.scene_registry(self.scene_registry());
        if let Some(host) = self.plugins.host() {
            builder = builder.plugin_host(host);
        }

        // Give this session its own owned `McpManager` built from this
        // project's centrally-connected client handles (cheap `Arc` clones
        // — not a reconnect) — see `SessionPool.mcp_by_project`'s doc
        // comment for why this isn't shared as a single `Arc<McpManager>`
        // directly (`McpManager` needs `&mut self` for `refresh_tools()`,
        // which a shared `Agent` field can't give it). Skipped entirely
        // when nothing's connected, so the common "no MCP servers
        // configured" case pays zero cost.
        let mcp_server_count = {
            let central = self
                .mcp_for_project(project_root_for_meta.as_deref(), &scene_id)
                .await;
            let count = central.server_count();
            if count > 0 {
                let mut per_session = McpManager::from_clients(central.clients().to_vec());
                per_session.refresh_tools().await;
                builder = builder.mcp_manager(per_session);
            }
            count
        };

        // ── Telemetry ──
        //
        // On by default. This used to attach a recorder *only* when a caller
        // passed `options.telemetry.output`, and `Builder::build()`'s
        // fallback constructs a handle whose receiver is dropped on the spot
        // — so with nothing passed, every event a session produced was
        // silently discarded and there was no transcript of tool calls,
        // permission decisions or turn costs to debug from. A session now
        // always gets a JSONL recorder; the caller's `output` only chooses
        // *where*, and `telemetry.mode: disabled` in settings.json opts out.
        //
        // `TELEMETRY_DIR/<session-id>.jsonl` under the local data dir keeps
        // one file per session, next to the transcripts, so a recording is
        // findable by session id without the caller having had to plan for
        // it in advance.
        // The default location is only used when `local_data_dir` is a real
        // absolute path. It is not always set (tests, embedders that never
        // configure `paths`), and joining onto an empty or relative value
        // would scatter `telemetry/` directories into whatever the process's
        // working directory happens to be — including a user's repository.
        // Recording nothing is much better than writing somewhere surprising.
        let telemetry_path = match options.and_then(|o| o.telemetry.as_ref()) {
            Some(t) => Some(std::path::PathBuf::from(&t.output)),
            None => {
                let dir = &settings_snapshot.paths.local_data_dir;
                dir.is_absolute()
                    .then(|| dir.join("telemetry").join(format!("{session_id}.jsonl")))
            }
        };
        if let (true, Some(telemetry_path)) = (settings_snapshot.telemetry_enabled, telemetry_path)
        {
            match FileRecorder::new(&telemetry_path) {
                Ok(rec) => {
                    // `telemetry` crate never touches the network itself —
                    // this is where the daemon decides where events go:
                    // always the per-session JSONL file, plus OTel when an
                    // endpoint is configured. `start_otel` installs the
                    // global meter/tracer providers, so that one is a
                    // process-level side effect done once, guarded by a
                    // `OnceLock` rather than per session (see `otel_sink`).
                    let mut recorders: Vec<std::sync::Arc<dyn telemetry::TelemetryRecorder>> =
                        vec![std::sync::Arc::new(rec)];
                    if let Some(otel) = otel_sink(settings_snapshot.telemetry_url.as_deref()) {
                        recorders.push(otel);
                    }
                    let telemetry_config = telemetry::TelemetryConfig {
                        enabled: true,
                        mode: telemetry::TelemetryMode::Enabled,
                        redact_prompts: true,
                        redact_tool_content: true,
                        default_event_enabled: true,
                        ..Default::default()
                    };
                    match telemetry::spawn(telemetry_config, recorders) {
                        Ok((handle, consumer)) => {
                            tokio::spawn(consumer);
                            builder = builder.telemetry_handle(handle);
                        }
                        Err(e) => {
                            warn!(error = %e, "failed to start telemetry pipeline; this session records nothing");
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        path = %telemetry_path.display(),
                        error = %e,
                        "could not open the session telemetry file; this session records nothing"
                    );
                }
            }
        }

        let (mut agent, event_rx, input_tx) =
            builder.build().map_err(|e| format!("build agent: {e}"))?;

        let _ = agent
            .telemetry()
            .record(telemetry::TelemetryEvent::session_start(
                &session_id,
                0,
                None,
                telemetry::SessionStartPayload {
                    permission_mode: serde_json::to_value(settings_snapshot.permission_mode)
                        .ok()
                        .and_then(|v| v.as_str().map(|s| s.to_string()))
                        .unwrap_or_else(|| "default".to_string()),
                    model: settings_snapshot.model.model_name.clone(),
                    max_tokens: settings_snapshot.model.max_tokens,
                    thinking_mode: !matches!(
                        settings_snapshot.model.thinking_mode,
                        base::interface::settings::ThinkingMode::Off
                    ),
                    sandbox_enabled: !settings_snapshot.sandbox.dangerously_disable_sandbox,
                    resume_from: resume.then(|| session_id.clone()),
                    auth_modes: Vec::new(),
                    mcp_server_count,
                    plugin_count: self.plugins.count(),
                    skill_count: self.skill_catalog.list().len(),
                    output_format: "jsonrpc".to_string(),
                    started_at_ms: (time::OffsetDateTime::now_utc().unix_timestamp_nanos()
                        / 1_000_000) as i64,
                },
            ));

        // First `Meta` line for a genuinely new primary session — resumed
        // sessions already have one from when they were first created, and
        // must not get a second (that would break the "Meta is always
        // entries[0]" assumption `SessionManager::resume`/`session.resume`
        // rely on). Best-effort: a write failure here doesn't fail session
        // creation — the session still works, it's just not identifiable by
        // scene/project on a later resume (falls back to inference, same as
        // any other pre-v2 file).
        if !resume {
            if let Some(ref store) = self.history_store {
                if let Ok(sid) = base::session::SessionId::parse(&session_id) {
                    let project_root_str = project_root_for_meta
                        .as_ref()
                        .map(|p| p.display().to_string());
                    let meta = history::entry::LogEntry::Meta {
                        cwd: project_root_str.clone().unwrap_or_else(|| {
                            settings_snapshot
                                .paths
                                .global_data_dir
                                .display()
                                .to_string()
                        }),
                        started_at: time::OffsetDateTime::now_utc(),
                        model: settings_snapshot.model.model_name.clone(),
                        permission_mode: serde_json::to_value(settings_snapshot.permission_mode)
                            .ok()
                            .and_then(|v| v.as_str().map(|s| s.to_string()))
                            .unwrap_or_else(|| "default".to_string()),
                        engine_version: env!("CARGO_PKG_VERSION").to_string(),
                        attacode_version: env!("CARGO_PKG_VERSION").to_string(),
                        parent_session_id: None,
                        scene: Some(scene_id.clone()),
                        project_root: project_root_str,
                        session_kind: history::entry::SessionKind::Primary,
                        schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
                    };
                    if let Err(e) = store.append(sid, meta).await {
                        warn!(%session_id, error = %e, "failed to write session Meta entry");
                    }
                }
            }
        }

        // Load prior messages back into memory before the turn loop starts —
        // `resume_or_create` only decided *that* history exists on disk, the
        // actual projection back into `Agent.session.messages` happens here.
        // A failure here (corrupt entry, race with a concurrent delete, ...)
        // degrades to the same "id reused, empty context" behavior as before
        // this was wired up, rather than failing session creation outright.
        if resume {
            let resume_start = std::time::Instant::now();
            let resume_result = agent.resume_session(&session_id).await;
            let outcome = match &resume_result {
                Ok(()) => telemetry::ResumeOutcome::Succeeded,
                // Doesn't fail session creation — degrades to the same
                // "id reused, empty context" behavior as before this was
                // wired up (see the warn! below), which is exactly what
                // `Degraded` (not `Failed`) means here.
                Err(_) => telemetry::ResumeOutcome::Degraded,
            };
            if let Err(e) = &resume_result {
                warn!(%session_id, error = %e, "resume_session failed, continuing with empty context");
            }
            let (projected_message_count, entry_count) = self.history_counts(&session_id).await;
            let entries = self.load_entries(&session_id).await.unwrap_or_default();
            let compact_boundary_count = entries
                .iter()
                .filter(|e| matches!(e.entry, history::entry::LogEntry::Compact { .. }))
                .count();
            let sidechain_entry_count = entries.iter().filter(|e| e.is_sidechain).count();
            let _ = agent
                .telemetry()
                .record(telemetry::TelemetryEvent::resume_action(
                    &session_id,
                    0,
                    None,
                    telemetry::ResumeActionPayload {
                        outcome,
                        source: "jsonl".into(),
                        entry_count,
                        projected_message_count,
                        compact_boundary_count,
                        sidechain_entry_count,
                        warning_kind: resume_result
                            .err()
                            .map(|_| "resume_session_failed".to_string()),
                        latency_ms: resume_start.elapsed().as_millis() as u64,
                    },
                ));
        }

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        // 启动 Agent 后台事件循环
        tokio::spawn(async move {
            let mut agent = agent;
            let _ = agent.run(cancel_clone).await;
        });

        let mut sessions = self.sessions.lock().await;

        // 容量检查：驱逐最久未活跃的 session。只有这次 insert 真的会让
        // session **总数**增加时才需要驱逐——`session_id` 已经在 map 里的话
        // （config-generation 触发的原地重建，见 `run_turn`），这次操作是
        // "替换同一个 key"，不是"新增一个"，`sessions.len()` 不会变，不该
        // 因为这次重建就凭空多驱逐一个无关的、真正最久未用的 session。
        if !sessions.contains_key(&session_id) && sessions.len() >= self.cap {
            evict_lru(&mut sessions);
        }

        let live = LiveSession {
            input_tx,
            event_rx: Arc::new(AsyncMutex::new(Some(event_rx))),
            cancel,
            name: None,
            created_at: Instant::now(),
            last_active: Instant::now(),
            is_first_turn: true,
            config_generation: self
                .config_generation
                .load(std::sync::atomic::Ordering::Relaxed),
            current_turn: None,
        };
        // `insert` returning `Some` means a live entry already existed under
        // this exact sid — only reachable via the config-generation recreate
        // path in `run_turn` (every other caller uses a freshly minted id
        // confirmed absent from the map). Its `CancellationToken` isn't tied
        // to `Drop`, so the old Agent's background loop would otherwise keep
        // running forever as a leaked task if we didn't cancel it here.
        if let Some(old) = sessions.insert(session_id.clone(), live) {
            old.cancel.cancel();
        }
        info!(%session_id, "session created");
        Ok(session_id)
    }

    /// The turn currently running on `session_id`, if any. `session.get`
    /// reports it as `turn_state`; a missing or unknown session reads as idle,
    /// same as one sitting between turns.
    pub async fn current_turn_of(&self, session_id: &str) -> Option<String> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|live| live.current_turn.clone())
    }

    /// Cancel the in-flight turn, keeping the session.
    ///
    /// `EngineCommand::CancelTurn` cancels the turn's own child token, not
    /// the session's — the agent loop keeps running and the next `run_turn`
    /// is served normally. Returns whether there was a turn to interrupt;
    /// there being none is an answer, not an error.
    pub async fn interrupt_session(&self, session_id: &str) -> Result<bool, RpcError> {
        let (input_tx, running) = {
            let sessions = self.sessions.lock().await;
            let Some(live) = sessions.get(session_id) else {
                return Err((
                    codes::SESSION_NOT_FOUND,
                    format!("session not found: {session_id}"),
                ));
            };
            (live.input_tx.clone(), live.current_turn.is_some())
        };
        if !running {
            return Ok(false);
        }
        let _ = input_tx.send(InputMessage::System {
            kind: runtime::agent::EngineCommand::CancelTurn,
            content: String::new(),
        });
        Ok(true)
    }

    /// Mark the session idle again. Called on every path that gives
    /// `event_rx` back, because "busy" is exactly "someone holds the channel".
    async fn clear_current_turn(&self, sid: &str) {
        let mut sessions = self.sessions.lock().await;
        if let Some(live) = sessions.get_mut(sid) {
            live.current_turn = None;
        }
    }

    /// 执行一个 turn：发送消息 → 流式返回事件 → 返回结果。
    /// session_id 为 None 时自动创建新 session。
    #[allow(clippy::too_many_arguments)]
    pub async fn run_turn(
        &self,
        session_id: Option<String>,
        message: String,
        attachments: Vec<runtime::agent::Attachment>,
        turn_id: String,
        sink: crate::rpc::Sink,
        id: serde_json::Value,
        options: Option<SessionOptions>,
    ) -> RpcResponse {
        // ── 解析或创建 session ──
        let sid = match session_id {
            Some(ref sid) => {
                // 三种情况：① 不在内存里（走原有的 resume_or_create）；
                // ② 在内存里且 config_generation 和池子当前代数一致（直接用，
                // 最常见的路径）；③ 在内存里但代数落后——期间发生过
                // `config.setProvider`/`config.reload`，需要在真正处理这条
                // 消息之前原地重建，让它追上新配置。
                //
                // ③ 只有在这个 session 当前空闲（`event_rx` 是 `Some`，不是
                // 正被另一个并发 turn 占用）时才重建——重建会取消旧 Agent、
                // 换上新的，如果旧 Agent 正在处理另一条消息，这么做会把那条
                // 消息腰斩。忙的话本次跳过重建，照旧用旧配置服务这条消息，
                // 下一次调用到这个 session 时再检查一次（代数还是落后的，
                // 不会漏检）。
                let stale_but_idle_check = {
                    let sessions = self.sessions.lock().await;
                    match sessions.get(sid) {
                        None => Err(()), // 不在内存里
                        Some(live)
                            if live.config_generation
                                == self
                                    .config_generation
                                    .load(std::sync::atomic::Ordering::Relaxed) =>
                        {
                            Ok(None) // 在内存里，代数一致
                        }
                        Some(live) => Ok(Some(live.event_rx.clone())), // 代数落后，带上 event_rx 供忙检查
                    }
                };

                let needs_resume = match stale_but_idle_check {
                    Err(()) => true,
                    Ok(None) => false,
                    Ok(Some(event_rx_mutex)) => {
                        // 只锁一下看现在是不是 Some（空闲），不 take——这里
                        // 不负责真正 drain 它，只是探测忙不忙。
                        event_rx_mutex.lock().await.is_some()
                    }
                };
                if needs_resume {
                    match self.resume_or_create(sid.clone(), options.as_ref()).await {
                        Ok(sid) => sid,
                        Err(e) => {
                            return RpcResponse::err(id, codes::INTERNAL_ERROR, e);
                        }
                    }
                } else {
                    sid.clone()
                }
            }
            None => {
                let sid = Id::new().to_string();
                match self
                    .create(
                        sid.clone(),
                        self.scene.clone(),
                        self.settings.read().await.clone(),
                        Some(self.cwd.clone()),
                        options.as_ref(),
                        false,
                    )
                    .await
                {
                    Ok(sid) => sid,
                    Err(e) => {
                        return RpcResponse::err(id, codes::INTERNAL_ERROR, e);
                    }
                }
            }
        };

        // ── 获取 session 的 input_tx 和 event_rx ──
        let (input_tx, event_rx_mutex, busy_with) = {
            let mut sessions = self.sessions.lock().await;
            let live = match sessions.get_mut(&sid) {
                Some(s) => s,
                None => {
                    return RpcResponse::err(
                        id,
                        codes::SESSION_NOT_FOUND,
                        format!("session not found: {sid}"),
                    );
                }
            };
            live.last_active = Instant::now();
            (
                live.input_tx.clone(),
                live.event_rx.clone(),
                live.current_turn.clone(),
            )
        };

        // 取出 event_rx（独占 drain）——**在发消息之前**。
        //
        // 顺序是关键：这个 take 就是"一个会话同一时刻只能有一个 turn"的实现，
        // 而消息一旦 send 进去就归正在跑的那个 Agent 消费了。先发后取的话，
        // 第二个 run_turn 会把用户这句话塞进第一个 turn 的输入队列，然后自己
        // 报错返回——调用方看到失败，消息却已经被别的 turn 吃掉了。
        let mut event_rx = match event_rx_mutex.lock().await.take() {
            Some(rx) => rx,
            None => {
                return RpcResponse::err_with_data(
                    id,
                    codes::SESSION_BUSY,
                    "session is busy",
                    serde_json::json!({
                        "session_id": sid,
                        "current_turn_id": busy_with,
                    }),
                );
            }
        };

        {
            let mut sessions = self.sessions.lock().await;
            if let Some(live) = sessions.get_mut(&sid) {
                live.current_turn = Some(turn_id.clone());
            }
        }

        // 发送用户消息
        let _ = input_tx.send(InputMessage::User {
            content: message.clone(),
            attachments,
            turn_id: turn_id.clone(),
        });

        let mut api_calls = 0u32;
        let mut writer_broken = false;
        loop {
            match event_rx.recv().await {
                Some(AgentEvent::SystemInit { .. }) => continue,
                Some(AgentEvent::TextDelta { text, .. }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({"kind":"text_delta","text":text}),
                    );
                    if !crate::rpc::send_frame(&sink, &f).await {
                        writer_broken = true;
                        break;
                    }
                }
                Some(AgentEvent::ToolUse {
                    id: tid,
                    name,
                    input,
                    ..
                }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({"kind":"tool_use","id":tid,"name":name,"input":input}),
                    );
                    if !crate::rpc::send_frame(&sink, &f).await {
                        writer_broken = true;
                        break;
                    }
                }
                Some(AgentEvent::ToolResult {
                    id: tid,
                    name,
                    content,
                    is_error,
                    ..
                }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({"kind":"tool_result","id":tid,"name":name,"content":content,"is_error":is_error}),
                    );
                    if !crate::rpc::send_frame(&sink, &f).await {
                        writer_broken = true;
                        break;
                    }
                }
                Some(AgentEvent::PermissionPrompt {
                    prompt_id,
                    tool_name,
                    message,
                    paths,
                    ..
                }) => {
                    // NOT a terminal event — the turn is still in progress,
                    // waiting on `session.respondToPrompt` before the tool
                    // call resolves and `tool_use`/`tool_result` continue as
                    // normal. Same `kind: "prompt"` / `prompt_type` shape
                    // documented in `docs/daemon_rpc_protocol.md`, deliberately
                    // generic so a future non-permission "stop and ask" need
                    // can reuse it without a new RPC method.
                    //
                    // **Not the path daemon permission prompts take today.**
                    // A `Permission` that returns
                    // `PermissionOutcome::Prompt` makes the engine wait on a
                    // channel that cannot be delivered to mid-turn (see
                    // `crate::permission_prompt`'s module docs), so the pool
                    // hands sessions an `AskingPermission` that emits its
                    // own — byte-identical — frame and resolves the answer
                    // itself. This arm stays because the event is part of
                    // the engine's public surface and relaying it is still
                    // correct; it just isn't reached from here.
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({
                            "kind":"prompt","prompt_type":"permission","prompt_id":prompt_id,
                            "tool_name":tool_name,"message":message,
                            "paths":paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>()
                        }),
                    );
                    if !crate::rpc::send_frame(&sink, &f).await {
                        writer_broken = true;
                        break;
                    }
                }
                // Sub-agent activity, mirrored from the sub-agent's own
                // channel (see `runtime::agent_tool`'s `SubagentTag`). Relayed
                // under a distinct `kind` so a client that doesn't know about
                // it ignores it, and one that does can render the sub-agent's
                // tool calls live under a node keyed by `agent_label` instead
                // of showing a stalled `Agent` tool call. Not terminal — the
                // parent turn continues.
                Some(AgentEvent::SubagentProgress {
                    agent_label,
                    agent_session_id,
                    agent_type,
                    parent_turn,
                    event,
                    ..
                }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({
                            "kind":"subagent_progress","agent_label":agent_label,
                            "agent_session_id":agent_session_id,"agent_type":agent_type,
                            "parent_turn":parent_turn,
                            "event":serde_json::to_value(&*event).unwrap_or(serde_json::Value::Null)
                        }),
                    );
                    if !crate::rpc::send_frame(&sink, &f).await {
                        writer_broken = true;
                        break;
                    }
                }
                // Skill files changed under a live session — forwarded so a
                // client caching `commands.list` knows to re-fetch. This
                // match is a whitelist (`_ => continue` at the bottom), so an
                // unlisted variant never reaches the client no matter what
                // the engine emits.
                Some(AgentEvent::SkillsChanged { added, removed, .. }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({
                            "kind":"skills_changed","added":added,"removed":removed
                        }),
                    );
                    if !crate::rpc::send_frame(&sink, &f).await {
                        writer_broken = true;
                        break;
                    }
                }
                // Team stage lifecycle — same non-terminal treatment.
                Some(AgentEvent::TeamProgress {
                    team,
                    team_id,
                    stage,
                    stage_index,
                    stage_count,
                    status,
                    members,
                    failed,
                }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({
                            "kind":"team_progress","team":team,"team_id":team_id,"stage":stage,
                            "stage_index":stage_index,"stage_count":stage_count,
                            "status":status,"members":members,"failed":failed
                        }),
                    );
                    if !crate::rpc::send_frame(&sink, &f).await {
                        writer_broken = true;
                        break;
                    }
                }
                Some(AgentEvent::TurnComplete {
                    stop_reason,
                    api_calls: ac,
                    usage,
                    ..
                }) => {
                    api_calls = ac;
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({
                            "kind":"turn_complete","stop_reason":stop_reason,"api_calls":api_calls,
                            "usage":{"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens}
                        }),
                    );
                    let _ = crate::rpc::send_frame(&sink, &f).await;
                    break;
                }
                Some(AgentEvent::Error { code, message, .. }) => {
                    // 归还 event_rx
                    *event_rx_mutex.lock().await = Some(event_rx);
                    self.clear_current_turn(&sid).await;
                    return RpcResponse::err(id, codes::ENGINE_ERROR, format!("{code}: {message}"));
                }
                _ => continue,
            }
        }

        // The turn is over (completed, or the client vanished mid-stream).
        // Nothing raised during it can still be answered — detach first so a
        // late prompt can't be written onto a connection that has already
        // seen its final response, then fail any straggler closed.

        // Client disconnected during turn — cancel the session immediately so
        // the Agent stops processing and any child processes (e.g. BashTool)
        // are killed, rather than waiting up to 5 minutes for the janitor.
        if writer_broken {
            drop(event_rx);
            self.clear_current_turn(&sid).await;
            self.shutdown_session(&sid).await;
            return RpcResponse::ok(
                id,
                serde_json::json!({
                    "session_id": sid,
                    "turn_id": turn_id,
                    "disconnected": true,
                }),
            );
        }

        // 归还 event_rx
        *event_rx_mutex.lock().await = Some(event_rx);
        self.clear_current_turn(&sid).await;

        // ── 首轮自动命名 ──
        let mut session_name = None;
        {
            let mut sessions = self.sessions.lock().await;
            if let Some(live) = sessions.get_mut(&sid) {
                if live.is_first_turn {
                    live.is_first_turn = false;
                    // 尝试通过场景判断是否需要命名（现在是这个 daemon 实例唯一
                    // 配置的那个 scene 说了算，不再写死查 chat 场景）
                    if self.scene.auto_name_session() {
                        if let Some(prompt) = self.scene.session_name_prompt(&message) {
                            match self.generate_session_name(&prompt).await {
                                Ok(name) => {
                                    live.name = Some(name.clone());
                                    session_name = Some(name);
                                }
                                Err(e) => {
                                    warn!(%sid, error=%e, "session name generation failed");
                                }
                            }
                        }
                    }
                } else {
                    session_name = live.name.clone();
                }
            }
        }

        RpcResponse::ok(
            id,
            serde_json::json!({
                "session_id": sid,
                "turn_id": turn_id,
                "name": session_name,
                "api_calls": api_calls,
            }),
        )
    }

    /// A session's recorded `session_kind`/`parent_session_id`/`resumable`,
    /// read off its `Meta` line (and terminal marker, for `resumable`).
    /// Sessions with no `Meta` at all (pre-fix files predating this daemon
    /// writing one — see `create()`) default to `(Primary, None, true)`,
    /// matching what they always were before sidechains existed as a
    /// concept. `resumable` is `false` only for a sidechain that has a
    /// `LogEntry::SessionEnd` marker —
    /// primary sessions and still-running/externally-cut-off sidechains are
    /// always `true`. One `load_entries` scan covers both, since listing
    /// every session would otherwise pay for the same disk read twice.
    async fn session_kind_parent_and_resumable(
        &self,
        session_id: &str,
    ) -> (history::entry::SessionKind, Option<String>, bool) {
        let Ok(entries) = self.load_entries(session_id).await else {
            return (history::entry::SessionKind::Primary, None, true);
        };
        let (session_kind, parent_session_id) = match find_meta(&entries) {
            Some(history::entry::LogEntry::Meta {
                session_kind,
                parent_session_id,
                ..
            }) => (*session_kind, parent_session_id.clone()),
            _ => (history::entry::SessionKind::Primary, None),
        };
        let has_terminal_marker = matches!(session_kind, history::entry::SessionKind::Sidechain)
            && entries
                .iter()
                .any(|env| matches!(env.entry, history::entry::LogEntry::SessionEnd { .. }));
        (session_kind, parent_session_id, !has_terminal_marker)
    }

    /// `scene.describe {scene, project_root, include_secrets}` — what this
    /// scene is and what a session in it would run with.
    ///
    /// Reports what the daemon can actually answer: the scene's capability
    /// bits, its tool surface, and the settings a session created here would
    /// resolve to. It deliberately does **not** report which tier each field
    /// came from — settings are merged tier by tier and the provenance is not
    /// retained, so a `sources` map would have to be reconstructed by
    /// re-reading and re-merging every tier. That is a feature, not a field.
    pub async fn describe_scene(
        &self,
        scene_id: &str,
        project_root: Option<&Path>,
        include_secrets: bool,
    ) -> Result<serde_json::Value, RpcError> {
        let Some(scene) = self.scene_registry().resolve(scene_id) else {
            return Err((codes::SCENE_NOT_FOUND, format!("unknown scene: {scene_id}")));
        };
        let settings = self.settings_for_project(project_root, scene_id).await;
        let mut settings_json =
            serde_json::to_value(settings.as_ref()).unwrap_or_else(|_| serde_json::json!({}));
        if !include_secrets {
            redact_settings_secrets(&mut settings_json);
        }

        Ok(serde_json::json!({
            "scene": scene.id(),
            "name": scene.name(),
            "description": scene.description(),
            "project_root": project_root.map(|p| p.display().to_string()),
            "active": self.active_scenes.read().await.contains_key(scene_id),
            "capabilities": {
                "requires_project": scene.requires_project(),
                "supports_team": scene.supports_team(),
            },
            "tools": {
                // `AgentScene::tools()` is a whitelist, and empty means "no
                // whitelist — every registered tool". Reporting that as `[]`
                // would read to a client as "no tools at all", the opposite;
                // `null` says there is no restriction.
                "allowed": if scene.tools().is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(scene.tools())
                },
                "disallowed": scene.disallowed_tools(),
                "deferred": scene.deferred_tools(),
            },
            "settings": settings_json,
        }))
    }

    /// `config.get {scene, project_root, tier}` — one settings tier as it
    /// sits on disk, or `effective` for the merged result.
    ///
    /// A tier that has no file reads as `null`, which is the honest answer:
    /// "nothing at this layer" and "an empty object here" are different
    /// states and a client showing the config should be able to tell them
    /// apart.
    pub async fn config_tier(
        &self,
        scene_id: &str,
        project_root: Option<&Path>,
        tier: &str,
        include_secrets: bool,
    ) -> Result<serde_json::Value, RpcError> {
        if self.scene_registry().resolve(scene_id).is_none() {
            return Err((codes::SCENE_NOT_FOUND, format!("unknown scene: {scene_id}")));
        }

        let paths = self.paths.as_ref();
        let path = match tier {
            "global" => Some(paths.global_root().join("settings.json")),
            "scene" => Some(
                paths
                    .global_root()
                    .join("scenes")
                    .join(scene_id)
                    .join("settings.json"),
            ),
            "project" => project_root.map(|p| p.join(".atta").join("settings.json")),
            "effective" => None,
            other => {
                return Err((
                    codes::INVALID_PARAMS,
                    format!("tier must be global|scene|project|effective, got `{other}`"),
                ))
            }
        };

        let mut value = match tier {
            "effective" => {
                let settings = self.settings_for_project(project_root, scene_id).await;
                serde_json::to_value(settings.as_ref()).unwrap_or_else(|_| serde_json::json!({}))
            }
            _ => match path {
                Some(p) => std::fs::read_to_string(&p)
                    .ok()
                    .and_then(|raw| serde_json::from_str(&raw).ok())
                    .unwrap_or(serde_json::Value::Null),
                // `project` with no project: there is no such tier to read.
                None => serde_json::Value::Null,
            },
        };
        if !include_secrets {
            redact_settings_secrets(&mut value);
        }

        Ok(serde_json::json!({
            "scene": scene_id,
            "project_root": project_root.map(|p| p.display().to_string()),
            "tier": tier,
            "settings": value,
        }))
    }

    /// One session's summary plus what only the live pool knows.
    ///
    /// Built from `list_all` rather than a second, parallel assembly: a
    /// `session.get` that disagreed with the `session.list` entry for the
    /// same session would be worse than not having it.
    pub async fn session_detail(&self, session_id: &str) -> Result<serde_json::Value, RpcError> {
        let info = self
            .list_all(true, None)
            .await
            .into_iter()
            .find(|s| s.session_id == session_id)
            .ok_or_else(|| {
                (
                    codes::SESSION_NOT_FOUND,
                    format!("session not found: {session_id}"),
                )
            })?;

        let (_, scene) = self.session_kind_and_scene(session_id).await;
        let scene_active = match &scene {
            Some(id) => self.active_scenes.read().await.contains_key(id),
            None => false,
        };
        let current_turn = self.current_turn_of(session_id).await;

        let mut detail = serde_json::to_value(&info).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(obj) = detail.as_object_mut() {
            obj.insert("scene".into(), serde_json::json!(scene));
            obj.insert("scene_active".into(), serde_json::json!(scene_active));
            // `turn_state` is the pair a client actually acts on: whether to
            // enable its send button, and which turn to interrupt if not.
            obj.insert(
                "turn_state".into(),
                serde_json::json!(if current_turn.is_some() {
                    "running"
                } else {
                    "idle"
                }),
            );
            obj.insert("current_turn_id".into(), serde_json::json!(current_turn));
        }
        Ok(detail)
    }

    /// 列出所有 session（活跃的 + 磁盘上历史的），合并去重。
    ///
    /// `include_children == false` (the default) hides sidechain sessions —
    /// they're sub-agent/team-member transcripts, not something a user
    /// browsing their conversation list should see. `parent_session_id`,
    /// when given, ignores `include_children` entirely and instead returns
    /// exactly that session's sidechains (delegating to
    /// `HistoryStore::child_sessions`, the same path `agent.list`'s
    /// post-restart fallback uses).
    pub async fn list_all(
        &self,
        include_children: bool,
        parent_session_id: Option<&str>,
    ) -> Vec<SessionInfo> {
        if let Some(parent) = parent_session_id {
            let Some(ref store) = self.history_store else {
                return Vec::new();
            };
            let Ok(children) = store.child_sessions(parent).await else {
                return Vec::new();
            };
            let active_ids: std::collections::HashSet<String> = {
                let sessions = self.sessions.lock().await;
                sessions.keys().cloned().collect()
            };
            let mut out = Vec::with_capacity(children.len());
            for sid in children {
                let sid_str = sid.to_string();
                let active = active_ids.contains(&sid_str);
                let (_, _, resumable) = self.session_kind_parent_and_resumable(&sid_str).await;
                out.push(SessionInfo {
                    session_id: sid_str,
                    name: None,
                    preview: None,
                    message_count: 0,
                    created_at: String::new(),
                    last_active: String::new(),
                    status: if active {
                        SessionStatus::Active
                    } else {
                        SessionStatus::Inactive
                    },
                    session_kind: history::entry::SessionKind::Sidechain,
                    parent_session_id: Some(parent.to_string()),
                    resumable,
                });
            }
            return out;
        }

        // Snapshot active sessions and release the lock before the disk
        // reads below (`session_kind_and_parent` per session, plus
        // `store.list_sessions()`) — holding `self.sessions` across that
        // much I/O would block every other pool operation for the duration.
        let active_snapshot: Vec<(String, Option<String>, Instant, Instant)> = {
            let sessions = self.sessions.lock().await;
            sessions
                .iter()
                .map(|(sid, live)| {
                    (
                        sid.clone(),
                        live.name.clone(),
                        live.created_at,
                        live.last_active,
                    )
                })
                .collect()
        };

        let mut out: Vec<SessionInfo> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for (sid, name, created_at, last_active) in active_snapshot {
            let (session_kind, parent, resumable) =
                self.session_kind_parent_and_resumable(&sid).await;
            seen.insert(sid.clone());
            if !include_children && matches!(session_kind, history::entry::SessionKind::Sidechain) {
                continue;
            }
            out.push(SessionInfo {
                session_id: sid,
                name,
                preview: None,
                message_count: 0,
                created_at: format_instant(created_at),
                last_active: format_instant(last_active),
                status: SessionStatus::Active,
                session_kind,
                parent_session_id: parent,
                resumable,
            });
        }

        // 从 HistoryStore 查磁盘历史（inactive sessions）
        if let Some(ref store) = self.history_store {
            if let Ok(sids) = store.list_sessions().await {
                for sid in sids {
                    let sid_str = sid.to_string();
                    if seen.contains(&sid_str) {
                        continue;
                    }
                    let (session_kind, parent, resumable) =
                        self.session_kind_parent_and_resumable(&sid_str).await;
                    if !include_children
                        && matches!(session_kind, history::entry::SessionKind::Sidechain)
                    {
                        continue;
                    }
                    out.push(SessionInfo {
                        session_id: sid_str,
                        name: None,
                        preview: None,
                        message_count: 0,
                        created_at: String::new(),
                        last_active: String::new(),
                        status: SessionStatus::Inactive,
                        session_kind,
                        parent_session_id: parent,
                        resumable,
                    });
                }
            }
        }

        out
    }

    /// 获取活跃 session 数量。
    pub async fn active_count(&self) -> usize {
        self.sessions.lock().await.len()
    }

    /// Run `fut` while holding this pool's per-`session_id` lock — see the
    /// `session_locks` field doc comment. Closes the race between a
    /// sidechain's cascade delete (`delete_sidechain_list`, `delete_session`)
    /// and a concurrent `session.resume(that_same_id, create_if_missing:
    /// true)`: whichever side gets here first now runs to completion before
    /// the other starts, so a resume can't observe the id mid-cascade (files
    /// half gone) and a completed cascade can't delete out from under a
    /// resume that just finished recreating the id. It does *not* stop the
    /// id from being reused afterward — recreating a just-deleted sidechain
    /// as a fresh orphan session is still possible, just no longer racy: the
    /// two operations fully serialize instead of interleaving.
    async fn with_session_lock<T>(
        &self,
        session_id: &str,
        fut: impl std::future::Future<Output = T>,
    ) -> T {
        let lock = {
            let mut locks = self.session_locks.lock().await;
            locks
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let result = {
            let _guard = lock.lock().await;
            fut.await
        };
        drop(lock);
        let mut locks = self.session_locks.lock().await;
        if locks
            .get(session_id)
            .is_some_and(|entry| Arc::strong_count(entry) == 1)
        {
            locks.remove(session_id);
        }
        result
    }

    /// `parent_session_id`'s children, or `[]` if there's no history store
    /// or the scan itself fails — logged either way so `session.close`/
    /// `.delete` reporting "0 sidechains" is distinguishable in the daemon
    /// log from "there genuinely were none" instead of silently looking the
    /// same as the empty case.
    async fn fetch_child_sessions(&self, parent_session_id: &str) -> Vec<base::session::SessionId> {
        let Some(ref store) = self.history_store else {
            return Vec::new();
        };
        // `HistoryStore::child_sessions`' default impl is O(n) in the total
        // number of sessions this project has ever recorded (full
        // load-and-parse per file — see its doc comment); there's no cheap
        // index to check "does this session have sidechains" first, so
        // every `session.close`/`.delete` pays this once regardless of
        // whether the session actually has any. Accepted as-is: `delete_session`
        // (this function's other caller) shares this one scan rather than
        // running it twice, which was the fixable half of the cost.
        match store.child_sessions(parent_session_id).await {
            Ok(children) => children,
            Err(e) => {
                warn!(
                    parent_session_id,
                    error = %e,
                    "failed to scan for child sessions; reporting no sidechains"
                );
                Vec::new()
            }
        }
    }

    /// Cancel (if live) and delete each of `children`, one `with_session_lock`
    /// critical section per child. Best-effort throughout — see
    /// `shutdown_session`'s doc comment.
    async fn delete_sidechain_list(
        &self,
        parent_session_id: &str,
        children: Vec<base::session::SessionId>,
    ) -> usize {
        let Some(ref store) = self.history_store else {
            return 0;
        };
        let mut deleted = 0;
        for child in children {
            let child_str = child.to_string();
            let ok = self
                .with_session_lock(&child_str, async {
                    {
                        let mut sessions = self.sessions.lock().await;
                        if let Some(live) = sessions.remove(&child_str) {
                            live.cancel.cancel();
                        }
                    }
                    match store.delete(child).await {
                        Ok(()) => true,
                        Err(e) => {
                            warn!(
                                session_id = %child_str,
                                parent_session_id,
                                error = %e,
                                "failed to delete sidechain during cascade; leaving for startup GC"
                            );
                            false
                        }
                    }
                })
                .await;
            if ok {
                deleted += 1;
            }
        }
        deleted
    }

    /// Remove `session_id` from the live map (if present) and cascade-delete
    /// `children` — the shared body of `shutdown_session` (which scans for
    /// its own children) and `delete_session` (which reuses a scan it
    /// already did for its response body).
    async fn shutdown_session_with_children(
        &self,
        session_id: &str,
        children: Vec<base::session::SessionId>,
    ) -> usize {
        {
            let mut sessions = self.sessions.lock().await;
            if let Some(live) = sessions.remove(session_id) {
                live.cancel.cancel();
                info!(%session_id, "session removed");
            }
        }
        self.delete_sidechain_list(session_id, children).await
    }

    /// `session.close` — destroys the runtime entity; the parent's
    /// transcript stays on disk and remains resumable. This
    /// **cascades**: every sidechain of this session (sub-agent / team
    /// member) is deleted too — a closed parent has no path left to reach
    /// them (`session.list {parent_session_id}` only makes sense while the
    /// parent might still resume that context; once it's gone, so are they,
    /// per §5.6's "保留结果，不保留过程"). Returns how many sidechains were
    /// actually deleted.
    ///
    /// A sidechain still mid-run (background / team member) is cancelled
    /// before its transcript is deleted, but this doesn't wait for that
    /// cancellation to finish landing on disk (N2's suggested grace window
    /// is intentionally not implemented here — it would make an interactive
    /// `session.close` call noticeably slower for what's normally a rare
    /// race). A delete that loses that race, or fails for any other reason,
    /// is logged and left for the startup GC to clean up later (N1) — it
    /// never fails `session.close` itself.
    pub async fn shutdown_session(&self, session_id: &str) -> usize {
        let children = self.fetch_child_sessions(session_id).await;
        self.shutdown_session_with_children(session_id, children)
            .await
    }

    /// `session.delete` — irreversibly deletes the parent's own transcript
    /// too (not just the runtime entity `session.close` destroys), cascading
    /// sidechains the same way. `dry_run: true` reports what *would* be
    /// deleted (sidechain count) without deleting anything, so a host can
    /// show a confirmation dialog first.
    pub async fn delete_session(
        &self,
        session_id: &str,
        dry_run: bool,
    ) -> Result<serde_json::Value, RpcError> {
        let store = self.require_history_store()?;
        let sid = base::session::SessionId::parse(session_id).map_err(|_| {
            (
                codes::INVALID_PARAMS,
                format!("invalid session_id: {session_id}"),
            )
        })?;

        // One scan shared by both the (possibly dry-run) report below and
        // the actual cascade delete — `shutdown_session_with_children`
        // takes this same `Vec` instead of re-scanning, which used to mean
        // a second full `child_sessions` pass (and a window where the two
        // scans could disagree) on every non-dry-run delete.
        let children = self.fetch_child_sessions(session_id).await;
        let sidechain_ids: Vec<String> = children.iter().map(|id| id.to_string()).collect();

        if dry_run {
            return Ok(serde_json::json!({
                "session_id": session_id,
                "deleted": false,
                "sidechains_deleted": sidechain_ids.len(),
                "sidechain_ids": sidechain_ids,
            }));
        }

        // Cascade and the primary delete share one `with_session_lock`
        // critical section — see that method's doc comment for the resume
        // race this closes.
        let (sidechains_deleted, delete_result) = self
            .with_session_lock(session_id, async {
                let deleted = self
                    .shutdown_session_with_children(session_id, children)
                    .await;
                let result = store.delete(sid).await;
                (deleted, result)
            })
            .await;
        delete_result.map_err(|e| (codes::INTERNAL_ERROR, e.to_string()))?;

        Ok(serde_json::json!({
            "session_id": session_id,
            "deleted": true,
            "sidechains_deleted": sidechains_deleted,
            "sidechain_ids": sidechain_ids,
        }))
    }

    /// `session.respondToPrompt` — deliver a decision for a pending
    /// `AgentEvent::PermissionPrompt` (see `session_pool.rs`'s `run_turn`
    /// event loop). Unlike `shutdown_session`, an unknown `session_id` here
    /// is a real caller error (answering a prompt for a session that isn't
    /// running can't silently no-op the way closing an already-closed
    /// session can) — `Err` on a miss rather than treating it as done.
    pub async fn respond_to_prompt(
        &self,
        session_id: &str,
        prompt_id: String,
        decision: runtime::agent::PermissionDecision,
    ) -> Result<(), String> {
        let input_tx = {
            let sessions = self.sessions.lock().await;
            match sessions.get(session_id) {
                Some(live) => live.input_tx.clone(),
                None => return Err(format!("session not found: {session_id}")),
            }
        };

        // Straight to the engine, which parks the blocked tool call on
        // `pending_permissions` and is woken by `Agent::run`'s input
        // demultiplexer. A `prompt_id` nobody is waiting on (already
        // answered, timed out, stale) is a silent no-op — the documented
        // behaviour.
        let _ = input_tx.send(InputMessage::PermissionResponse {
            prompt_id,
            decision,
        });
        Ok(())
    }

    // ── Session read / branch surface (`session.history` / `.fork` / `.resume`) ──

    /// The `HistoryStore` these three methods read and write, or a ready-made
    /// RPC error explaining that this daemon has no session persistence at
    /// all (`JsonlHistoryStore::with_root` failed at startup — see
    /// `daemon/src/main.rs`; `daemon.doctor`'s
    /// `session_persistence.history_store_wired` reports the same fact).
    fn require_history_store(&self) -> Result<&Arc<dyn history::store::HistoryStore>, RpcError> {
        self.history_store.as_ref().ok_or_else(|| {
            (
                codes::INTERNAL_ERROR,
                "session history persistence is not enabled on this daemon \
                 (see daemon.doctor's session_persistence.history_store_wired)"
                    .to_string(),
            )
        })
    }

    /// Parse + load one session's raw log, mapping both failure kinds onto
    /// the error codes `docs/daemon_rpc_protocol.md` documents.
    async fn load_entries(
        &self,
        session_id: &str,
    ) -> Result<Vec<history::entry::EnvelopedEntry>, RpcError> {
        let store = self.require_history_store()?;
        let sid = base::session::SessionId::parse(session_id).map_err(|e| {
            (
                codes::INVALID_PARAMS,
                format!("invalid session_id `{session_id}`: {e}"),
            )
        })?;
        store.load(sid).await.map_err(|e| match e {
            history::error::HistoryError::SessionNotFound(_) => (
                codes::SESSION_NOT_FOUND,
                format!("session not found: {session_id}"),
            ),
            other => (
                codes::INTERNAL_ERROR,
                format!("failed to read session history: {other}"),
            ),
        })
    }

    /// `session.history` — a bounded page of a session's projected
    /// transcript.
    ///
    /// Reads the on-disk log, not the live `Agent`'s in-memory message list:
    /// the store is the only view that exists for an *inactive* session
    /// (the whole point — an IDE that restarted has nothing in memory), and
    /// for an active one it's the same content one turn-boundary behind
    /// (`SessionManager::persist` runs once per turn). A turn currently in
    /// flight therefore isn't visible here until it completes; `active` in
    /// the response tells a caller when that caveat applies.
    ///
    /// Paging is `offset`/`limit` over **projected messages** (what
    /// `history::transcript::project_messages` produces — compaction
    /// boundaries already applied, metadata/sidechain entries already
    /// dropped), not raw log entries, so the indices line up with what the
    /// client renders. `total` always reports the full count regardless of
    /// the page, so a client wanting the tail of a long session can ask for
    /// `limit: 0` first (cheap, no message payload) and then request
    /// `offset: total - n`.
    pub async fn session_history(
        &self,
        session_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<serde_json::Value, RpcError> {
        let entries = self.load_entries(session_id).await?;
        let messages = history::transcript::project_messages(&entries);
        let total = messages.len();
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);
        let page = serde_json::to_value(&messages[start..end]).map_err(|e| {
            (
                codes::INTERNAL_ERROR,
                format!("failed to serialize transcript: {e}"),
            )
        })?;
        let active = self.sessions.lock().await.contains_key(session_id);

        Ok(serde_json::json!({
            "session_id": session_id,
            "messages": page,
            "total": total,
            "offset": start,
            "limit": limit,
            "has_more": end < total,
            "entry_count": entries.len(),
            "active": active,
        }))
    }

    /// `session.fork` — branch a session at a message boundary into a new
    /// session id whose history is a copy of everything up to that point.
    ///
    /// `at_message` counts **projected messages** (same coordinates
    /// `session.history` returns); `None` forks the whole transcript. The cut
    /// is resolved to the shortest *entry* prefix whose projection already
    /// holds that many messages (see `history::transcript::projected_lengths`
    /// — a `Compact` entry can shrink the running count, so this is a scan,
    /// not arithmetic on entry kinds).
    ///
    /// The fork is written to disk and is **not** made live in the pool:
    /// nothing about the parent is touched, no engine is spawned, and the
    /// new id behaves exactly like any other id with history — the next
    /// `session.resume`/`session.run_turn` carrying it resumes the copy.
    /// That's what makes the two independent: writes to the fork append to a
    /// different jsonl file.
    ///
    /// Two deliberate lossy points, both invisible to the projected view:
    /// sidechain entries (sub-agent side conversations) are dropped, because
    /// `HistoryStore::append` takes a `LogEntry` and can't carry the
    /// envelope's `is_sidechain`/`parent_id` flags through; and the source's
    /// `Meta` line is replaced with a fresh one recording
    /// `parent_session_id`, which is what makes the lineage queryable via
    /// `HistoryStore::child_sessions`.
    pub async fn fork_session(
        &self,
        session_id: &str,
        at_message: Option<usize>,
    ) -> Result<serde_json::Value, RpcError> {
        let entries = self.load_entries(session_id).await?;
        let store = self.require_history_store()?;

        let lengths = history::transcript::projected_lengths(&entries);
        let total_messages = lengths.last().copied().unwrap_or(0);
        let target = at_message.unwrap_or(total_messages).min(total_messages);
        // Shortest prefix already holding `target` messages. `target == 0`
        // keeps nothing (a fork of the bare session start), which is a legal
        // request: "give me this conversation's setup with none of its turns".
        let cut = if target == 0 {
            0
        } else {
            lengths
                .iter()
                .position(|&len| len >= target)
                .map(|i| i + 1)
                .unwrap_or(entries.len())
        };

        let new_sid = base::session::SessionId::new();
        // Fork inherits the source session's own recorded scene and project
        // when known (pre-v2 sources have neither) rather than always
        // stamping the pool's defaults — the two already coincide for a
        // single-scene, single-default-project pool, but this keeps the
        // fork correct once a source session lives under a different
        // project (P3) or scene (P4).
        let (source_scene, source_project_root, source_schema_version) = entries
            .iter()
            .find_map(|env| match &env.entry {
                history::entry::LogEntry::Meta {
                    scene,
                    project_root,
                    schema_version,
                    ..
                } => Some((scene.clone(), project_root.clone(), *schema_version)),
                _ => None,
            })
            .unwrap_or((None, None, 1));
        let project_root_for_fork = if source_schema_version >= 2 {
            source_project_root.map(PathBuf::from)
        } else {
            Some(self.cwd.clone())
        };
        // A fork inherits the source session's scene, so its settings must be
        // resolved against that scene's tier — not the daemon's startup one.
        let fork_scene = match source_scene.clone() {
            Some(s) => s,
            None => self.settings.read().await.paths.scope.clone(),
        };
        let settings = match &project_root_for_fork {
            Some(p) => self.settings_for_project(Some(p), &fork_scene).await,
            None => self.settings_for_project(None, &fork_scene).await,
        };
        let meta = history::entry::LogEntry::Meta {
            cwd: project_root_for_fork
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| settings.paths.global_data_dir.display().to_string()),
            started_at: time::OffsetDateTime::now_utc(),
            model: settings.model.model_name.clone(),
            permission_mode: serde_json::to_value(settings.permission_mode)
                .ok()
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| "default".to_string()),
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
            attacode_version: env!("CARGO_PKG_VERSION").to_string(),
            parent_session_id: Some(session_id.to_string()),
            scene: Some(source_scene.unwrap_or_else(|| self.scene.id().to_string())),
            project_root: project_root_for_fork.map(|p| p.display().to_string()),
            session_kind: history::entry::SessionKind::Primary,
            schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
        };

        // Best-effort cleanup on a partial copy: a half-written fork that a
        // client then resumes would silently present a truncated
        // conversation as if it were complete. Deleting is safer than
        // leaving it, and the id was minted here so nothing else can be
        // referencing it yet.
        let mut copied = 0usize;
        if let Err(e) = store.append(new_sid, meta).await {
            let _ = store.delete(new_sid).await;
            return Err((
                codes::INTERNAL_ERROR,
                format!("failed to write forked session metadata: {e}"),
            ));
        }
        for env in entries.iter().take(cut) {
            if env.is_sidechain || matches!(env.entry, history::entry::LogEntry::Meta { .. }) {
                continue;
            }
            if let Err(e) = store.append(new_sid, env.entry.clone()).await {
                let _ = store.delete(new_sid).await;
                return Err((
                    codes::INTERNAL_ERROR,
                    format!("failed to copy session history into the fork: {e}"),
                ));
            }
            copied += 1;
        }

        let message_count = history::transcript::project_messages(&entries[..cut]).len();
        info!(
            source = %session_id,
            fork = %new_sid,
            at_message = message_count,
            "session forked"
        );
        Ok(serde_json::json!({
            "session_id": new_sid.to_string(),
            "parent_session_id": session_id,
            "forked_at_message": message_count,
            "message_count": message_count,
            "entry_count": copied,
            "source_message_count": total_messages,
            "active": false,
        }))
    }

    /// This daemon instance's *default* scene id — see the `scene` field.
    pub fn scene_id(&self) -> &str {
        self.scene.id()
    }

    /// Resolve `scene_id` against the currently-active scenes, or `None`
    /// (the pool default) against `self.scene` directly. `session.create`'s
    /// resolution rule: omitted `scene` param always means the default,
    /// regardless of what else is active.
    async fn resolve_scene(&self, scene_id: Option<&str>) -> Option<Arc<dyn AgentScene>> {
        match scene_id {
            None => Some(self.scene.clone()),
            Some(id) => self.active_scenes.read().await.get(id).cloned(),
        }
    }

    /// `scene.list` — every scene this binary registers, each flagged with
    /// whether it's currently active. `sessions` counts *in-memory* live
    /// sessions recorded under that scene (an approximation — matches what
    /// `session.list`'s own active/inactive split already accepts; a
    /// disk-wide count would need scanning every session's `Meta`, not
    /// worth it just for this summary view).
    pub async fn list_scenes(&self) -> Vec<serde_json::Value> {
        let active = self.active_scenes.read().await;
        let mut counts: HashMap<String, usize> = HashMap::new();
        {
            let sessions = self.sessions.lock().await;
            for sid in sessions.keys() {
                if let (_, Some(scene)) = self.session_kind_and_scene(sid).await {
                    *counts.entry(scene).or_insert(0) += 1;
                }
            }
        }
        self.scene_registry()
            .list_all()
            .into_iter()
            .map(|info| {
                serde_json::json!({
                    "scene": info.id,
                    "name": info.name,
                    "description": info.description,
                    "active": active.contains_key(&info.id),
                    "sessions": counts.get(&info.id).copied().unwrap_or(0),
                    "requires_project": info.requires_project,
                    "supports_team": info.supports_team,
                })
            })
            .collect()
    }

    /// `scene.activate {scene}` — idempotent. `Err` when `scene_id` isn't
    /// one this binary registers at all (`scene.list` would never show it).
    pub async fn activate_scene(&self, scene_id: &str) -> Result<(), RpcError> {
        if self.active_scenes.read().await.contains_key(scene_id) {
            return Ok(());
        }
        let Some(resolved) = self.scene_registry().resolve(scene_id) else {
            return Err((codes::SCENE_NOT_FOUND, format!("unknown scene: {scene_id}")));
        };
        self.active_scenes
            .write()
            .await
            .insert(scene_id.to_string(), resolved);
        Ok(())
    }

    /// `scene.deactivate {scene}` — refuses while any *in-memory* session is
    /// recorded under this scene (`SCENE_HAS_ACTIVE_SESSIONS`, with the
    /// blocking session ids so the caller can close them and retry).
    /// Deactivating only removes it from `active_scenes` — no session
    /// history is touched, and a deactivated scene's sessions can still be
    /// listed (`session.list`) and resumed once it's reactivated.
    pub async fn deactivate_scene(&self, scene_id: &str) -> Result<(), RpcError> {
        let blocking: Vec<String> = {
            let sessions = self.sessions.lock().await;
            let mut out = Vec::new();
            for sid in sessions.keys() {
                if let (_, Some(scene)) = self.session_kind_and_scene(sid).await {
                    if scene == scene_id {
                        out.push(sid.clone());
                    }
                }
            }
            out
        };
        if !blocking.is_empty() {
            return Err((
                codes::SCENE_HAS_ACTIVE_SESSIONS,
                format!(
                    "scene {scene_id} has active sessions: {}",
                    blocking.join(", ")
                ),
            ));
        }
        self.active_scenes.write().await.remove(scene_id);
        Ok(())
    }

    /// Like `session_kind_and_parent`, but returns the recorded scene
    /// instead of the parent — `list_scenes`/`deactivate_scene`'s shared
    /// lookup. `None` for the scene half covers "no Meta at all" the same
    /// way `session_kind_and_parent` does for kind/parent.
    async fn session_kind_and_scene(
        &self,
        session_id: &str,
    ) -> (history::entry::SessionKind, Option<String>) {
        let Ok(entries) = self.load_entries(session_id).await else {
            return (history::entry::SessionKind::Primary, None);
        };
        match find_meta(&entries) {
            Some(history::entry::LogEntry::Meta {
                session_kind,
                scene,
                ..
            }) => (*session_kind, scene.clone()),
            _ => (history::entry::SessionKind::Primary, None),
        }
    }

    /// Merged settings for `project_root` (global → scene → project tiers),
    /// built once and cached — see the `projects` field doc comment.
    /// `None` is the global no-project tier (docs/session_and_scene_invariants.md §2):
    /// `local_data_dir` becomes
    /// `global_data_dir` itself rather than `<project>/.atta`, so project-
    /// layer settings are skipped entirely, same as the design's "no
    /// specific project" case.
    ///
    /// Does not validate that `project_root` exists — `Settings::load`
    /// already degrades a missing/unreadable `settings.json` to defaults
    /// (soft-degrade, same as every other tier). Callers that need to
    /// reject a nonexistent project outright (`session.create`) check that
    /// separately, since resume callers deliberately do **not**: a project
    /// directory that moved after the session was created should still
    /// resume (`docs/daemon_rpc_protocol.md` §6.3's `project_root_changed`), not fail.
    async fn settings_for_project(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> Arc<Settings> {
        let startup_scene = self.settings.read().await.paths.scope.clone();
        let key = (project_root.map(Path::to_path_buf), scene_id.to_string());

        // The pool's own `(cwd, startup scene)` pair, and only that pair,
        // answers from the hot-swappable settings `config.reload` mutates.
        if key.0.as_deref() == Some(self.cwd.as_path()) && scene_id == startup_scene {
            return self.settings.read().await.clone();
        }
        if let Some(existing) = self.projects.lock().await.get(&key) {
            return existing.clone();
        }

        let default_model = self.settings.read().await.model.model_name.clone();
        let local_dir = match &key.0 {
            Some(root) => root.join(".atta"),
            None => self.paths.global_root(),
        };
        let built = Arc::new(Settings::load(
            self.paths.global_root(),
            self.scene_root(scene_id, &startup_scene),
            local_dir,
            scene_id,
            &default_model,
        ));
        self.projects.lock().await.insert(key, built.clone());
        built
    }

    /// The scene tier directory for `scene_id`.
    ///
    /// The daemon's *own* scene keeps whatever `DaemonPaths::config_root()`
    /// resolves to — tests inject a `StaticDaemonPaths` whose config root is a
    /// tempdir bearing no relation to `global_root()/scenes/<id>`, and
    /// recomputing it here would move that scene's settings out from under
    /// them. Any other scene has no such injected path, so it is derived the
    /// way `DefaultDaemonPaths::from_env` derives the first one.
    fn scene_root(&self, scene_id: &str, startup_scene: &str) -> PathBuf {
        if scene_id == startup_scene {
            self.paths.config_root()
        } else {
            self.paths.global_root().join("scenes").join(scene_id)
        }
    }

    /// This project's centrally-connected `McpManager` — the MCP
    /// counterpart to `settings_for_project`, same `None`/`Some(root)` key
    /// convention. Connects lazily and caches the result on first use
    /// rather than eagerly for every project this pool has ever seen a
    /// session for: most daemons only ever serve their own default project,
    /// and eagerly connecting every project mentioned in a `session.create`
    /// call would pay for MCP servers nobody's session ever actually needs.
    /// The default project is the one exception — it's still connected
    /// eagerly at startup by `connect_mcp_servers_in_background`, so by the
    /// time this is first called for it the cache is typically already warm.
    ///
    /// Not deduplicated across projects with an identical `mcp_servers`
    /// config — two projects that happen to declare the same server each
    /// get their own connection. Sharing a handle across projects is future
    /// work; this only needed to stop the wrong thing from happening (a
    /// non-default project silently inheriting the default project's
    /// connections, or never connecting its own at all).
    async fn mcp_for_project(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> Arc<McpManager> {
        let key = (project_root.map(Path::to_path_buf), scene_id.to_string());
        if let Some(existing) = self.mcp_by_project.lock().await.get(&key) {
            return existing.clone();
        }

        let settings = self.settings_for_project(project_root, scene_id).await;
        let mut parsed = HashMap::new();
        for (name, v) in &settings.mcp_servers {
            match serde_json::from_value::<mcp::config::McpServerConfig>(v.clone()) {
                Ok(cfg) => {
                    parsed.insert(name.clone(), cfg);
                }
                Err(e) => {
                    tracing::warn!(server = %name, error = %e, "invalid mcp_servers config, skipping");
                }
            }
        }
        let manager = Arc::new(McpManager::connect_all(parsed).await);
        self.mcp_by_project
            .lock()
            .await
            .insert(key, manager.clone());
        manager
    }

    /// A session's own recorded `(schema_version, project_root)`, read off
    /// its `Meta` line — used by `resume_or_create` so resuming a session
    /// rebuilds it against *its own* project, not this pool's default one.
    /// `None` covers both "no `Meta` at all" (a session that predates this
    /// daemon writing one) and "pre-v2 `Meta`" (no `project_root` field
    /// existed yet) — both mean "nothing recorded, infer the pool default",
    /// same spirit as `SceneCheck::Inferred`. Distinguishing those from a
    /// v2 `Meta` that explicitly recorded `project_root: null` (a genuine
    /// no-project session) is exactly why this returns the raw
    /// `schema_version` instead of collapsing straight to `Option<PathBuf>`.
    /// The recorded `scene` rides along because it comes from the same
    /// `Meta` and reattaching needs both: a session's settings are resolved
    /// against its own project *and* its own scene tier.
    async fn recorded_project(
        &self,
        session_id: &str,
    ) -> Option<(u32, Option<String>, Option<String>)> {
        let entries = self.load_entries(session_id).await.ok()?;
        let history::entry::LogEntry::Meta {
            schema_version,
            project_root,
            scene,
            ..
        } = find_meta(&entries)?
        else {
            return None;
        };
        Some((*schema_version, project_root.clone(), scene.clone()))
    }

    /// See [`SceneCheck`]. Safe to call unconditionally, including for a
    /// session id with no on-disk history at all (degrades to `Inferred`).
    ///
    /// Matches against any *currently active* scene (`self.active_scenes`),
    /// not just this pool's default — a session legitimately created via
    /// `session.create {scene: "chat"}` after `scene.activate {scene:
    /// "chat"}` is exactly as resumable/closeable/forkable as one under the
    /// default scene; only a scene this daemon isn't currently serving at
    /// all counts as a mismatch.
    pub async fn check_scene(&self, session_id: &str) -> SceneCheck {
        let recorded: Option<String> = match self.load_entries(session_id).await {
            Ok(entries) => match find_meta(&entries) {
                Some(history::entry::LogEntry::Meta { scene, .. }) => scene.clone(),
                _ => None,
            },
            Err(_) => None,
        };
        match recorded {
            None => SceneCheck::Inferred,
            Some(s) if self.active_scenes.read().await.contains_key(&s) => SceneCheck::Matches,
            Some(s) => SceneCheck::Mismatch(s),
        }
    }

    /// A sidechain (sub-agent/team-member) session that has a
    /// `LogEntry::SessionEnd` marker has run its one-shot task to
    /// conclusion and has no continuation semantics — `session.resume`
    /// must reject it with `SIDECHAIN_TERMINAL` instead of silently
    /// reattaching to a "finished" transcript. Primary sessions and
    /// sidechains with no marker (still running, or cut off externally)
    /// return `None` and proceed normally.
    pub async fn sidechain_terminal_state(
        &self,
        session_id: &str,
    ) -> Option<(Option<String>, history::entry::SessionEndState)> {
        let entries = self.load_entries(session_id).await.ok()?;
        let history::entry::LogEntry::Meta {
            session_kind,
            parent_session_id,
            ..
        } = find_meta(&entries)?
        else {
            return None;
        };
        if !matches!(session_kind, history::entry::SessionKind::Sidechain) {
            return None;
        }
        entries.iter().rev().find_map(|env| match &env.entry {
            history::entry::LogEntry::SessionEnd { state } => {
                Some((parent_session_id.clone(), *state))
            }
            _ => None,
        })
    }

    /// `session.resume` — the explicit, inspectable form of what
    /// `session.run_turn` has always done implicitly when handed a known
    /// `session_id`.
    ///
    /// Reattaches (loading the transcript back into the engine's context via
    /// the same `resume_or_create` → `Agent::resume_session` path
    /// `run_turn` uses) and reports what was reattached to, so a restarted
    /// client can tell "I'm continuing a 40-message conversation" from "this
    /// id was never used".
    ///
    /// `create_if_missing` is the one place this deliberately differs from
    /// `run_turn`: an id with no history is an error here by default
    /// (`SESSION_NOT_FOUND`), because an explicit "resume" that silently
    /// invents an empty session is how a client ends up rendering a blank
    /// window and calling it a restored one. `run_turn` keeps its
    /// create-on-miss behavior unchanged — sending a message to an unknown
    /// id is a different intent.
    ///
    /// An already-live session is reported as `already_active` and is *not*
    /// recreated: recreating swaps the `Agent` out from under any turn
    /// currently running on it (see `create()`'s same-sid replace path).
    /// This is also this method's fast path: an already-active id returns
    /// straight from the in-memory `sessions` map, never acquiring
    /// `with_session_lock` or reading `sidechain_terminal_state`'s full
    /// transcript off disk — both only matter for an id this pool isn't
    /// already actively serving.
    pub async fn resume_session(
        &self,
        session_id: &str,
        create_if_missing: bool,
        options: Option<&SessionOptions>,
    ) -> Result<serde_json::Value, ResumeError> {
        let resume_start = Instant::now();
        let settings = self.settings.read().await.clone();
        let mode = effective_permission_mode(&settings, options);

        // Already live: report state, touch nothing. The lock is released
        // before the (disk-reading) history lookup — `sessions` is the
        // pool's hottest mutex and every other holder keeps it CPU-only.
        let already_active = {
            let mut sessions = self.sessions.lock().await;
            sessions.get_mut(session_id).map(|live| {
                live.last_active = Instant::now();
                (
                    live.name.clone(),
                    format_instant(live.created_at),
                    format_instant(live.last_active),
                )
            })
        };
        if let Some((name, created_at, last_active)) = already_active {
            let (message_count, entry_count) = self.history_counts(session_id).await;
            let _ = self
                .pool_telemetry
                .record(telemetry::TelemetryEvent::resume_action(
                    session_id,
                    0,
                    None,
                    telemetry::ResumeActionPayload {
                        outcome: telemetry::ResumeOutcome::Succeeded,
                        source: "already_active".into(),
                        entry_count,
                        projected_message_count: message_count,
                        compact_boundary_count: 0,
                        sidechain_entry_count: 0,
                        warning_kind: None,
                        latency_ms: resume_start.elapsed().as_millis() as u64,
                    },
                ));
            return Ok(self.resume_report(
                session_id,
                "already_active",
                message_count,
                entry_count,
                name,
                created_at,
                last_active,
                mode,
            ));
        }

        // Not already active — serialize against a concurrent cascade
        // delete of this same id (see `with_session_lock`'s doc comment):
        // the terminal-state check and `resume_or_create` below must both
        // run as one atomic step relative to `delete_sidechain_list`/
        // `delete_session`, not race them.
        self.with_session_lock(session_id, async {
            // §5.6/§9: a sidechain that already reached a terminal state has
            // no continuation semantics — reject before touching engine
            // state.
            if let Some((parent_session_id, state)) =
                self.sidechain_terminal_state(session_id).await
            {
                let _ = self
                    .pool_telemetry
                    .record(telemetry::TelemetryEvent::resume_action(
                        session_id,
                        0,
                        None,
                        telemetry::ResumeActionPayload {
                            outcome: telemetry::ResumeOutcome::Failed,
                            source: "sidechain_terminal".into(),
                            entry_count: 0,
                            projected_message_count: 0,
                            compact_boundary_count: 0,
                            sidechain_entry_count: 0,
                            warning_kind: Some("sidechain_terminal".into()),
                            latency_ms: resume_start.elapsed().as_millis() as u64,
                        },
                    ));
                return Err(ResumeError::SidechainTerminal {
                    session_id: session_id.to_string(),
                    parent_session_id,
                    final_state: state,
                });
            }

            let (message_count, entry_count) = self.history_counts(session_id).await;
            let has_history = entry_count > 0;
            if !has_history && !create_if_missing {
                let _ = self
                    .pool_telemetry
                    .record(telemetry::TelemetryEvent::resume_action(
                        session_id,
                        0,
                        None,
                        telemetry::ResumeActionPayload {
                            outcome: telemetry::ResumeOutcome::Failed,
                            source: "not_found".into(),
                            entry_count: 0,
                            projected_message_count: 0,
                            compact_boundary_count: 0,
                            sidechain_entry_count: 0,
                            warning_kind: Some("not_found".into()),
                            latency_ms: resume_start.elapsed().as_millis() as u64,
                        },
                    ));
                return Err(ResumeError::Rpc((
                    codes::SESSION_NOT_FOUND,
                    format!(
                        "session not found: {session_id} \
                         (pass create_if_missing: true to start a new session under this id)"
                    ),
                )));
            }

            let sid = self
                .resume_or_create(session_id.to_string(), options)
                .await
                .map_err(|e| ResumeError::Rpc((codes::INTERNAL_ERROR, e)))?;

            let (created_at, last_active) = {
                let sessions = self.sessions.lock().await;
                match sessions.get(&sid) {
                    Some(live) => (
                        format_instant(live.created_at),
                        format_instant(live.last_active),
                    ),
                    None => (String::new(), String::new()),
                }
            };
            // `resume_or_create` falls back to a fresh id if creation under
            // the requested one failed — report what actually came back,
            // and don't claim history was restored onto an id that never
            // had any.
            let status = if has_history && sid == session_id {
                "resumed"
            } else {
                "created"
            };
            let (message_count, entry_count) = if sid == session_id {
                (message_count, entry_count)
            } else {
                (0, 0)
            };

            Ok(self.resume_report(
                &sid,
                status,
                message_count,
                entry_count,
                None,
                created_at,
                last_active,
                mode,
            ))
        })
        .await
    }

    /// Projected message count + raw entry count for a session id, or
    /// `(0, 0)` when there's no store, the id doesn't parse, or nothing is
    /// on disk — all three mean the same thing to `session.resume`'s caller
    /// ("no prior conversation"), so they don't need distinguishing here.
    async fn history_counts(&self, session_id: &str) -> (usize, usize) {
        match self.load_entries(session_id).await {
            Ok(entries) => (
                history::transcript::project_messages(&entries).len(),
                entries.len(),
            ),
            Err(_) => (0, 0),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn resume_report(
        &self,
        session_id: &str,
        status: &str,
        message_count: usize,
        entry_count: usize,
        name: Option<String>,
        created_at: String,
        last_active: String,
        mode: base::interface::settings::PermissionMode,
    ) -> serde_json::Value {
        let prompts_enabled = !matches!(
            mode,
            base::interface::settings::PermissionMode::BypassPermissions
        );
        serde_json::json!({
            "session_id": session_id,
            "status": status,
            "active": true,
            "message_count": message_count,
            "entry_count": entry_count,
            "name": name,
            "created_at": created_at,
            "last_active": last_active,
            "scene": self.scene.id(),
            "permission": {
                "mode": serde_json::to_value(mode).unwrap_or(serde_json::Value::Null),
                "prompts": prompts_enabled,
                "prompt_timeout_secs": self.permission_prompt_timeout.as_secs(),
            },
        })
    }

    /// 关闭所有 session。
    pub async fn shutdown_all(&self) {
        let mut sessions = self.sessions.lock().await;
        for (sid, live) in sessions.drain() {
            live.cancel.cancel();
            info!(%sid, "session removed (shutdown)");
        }
    }

    /// 后台回收任务：定期驱逐超时 idle session。
    pub fn start_janitor(self: &Arc<Self>) {
        let pool = self.clone();
        // Sweep interval scales with `idle_timeout` instead of a flat 300s:
        // a short-configured `session_idle_timeout_secs` (e.g. 60s for a
        // high-churn deployment) used to still wait up to ~5 minutes before
        // the janitor actually looked, silently defeating the configured
        // value. Clamped to [30s, 300s] so a very short timeout doesn't spin
        // the loop and a very long one doesn't sweep needlessly often.
        let sweep_interval =
            (pool.idle_timeout / 4).clamp(Duration::from_secs(30), Duration::from_secs(300));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(sweep_interval).await;
                let mut sessions = pool.sessions.lock().await;
                let to_evict = idle_session_ids(&sessions, Instant::now(), pool.idle_timeout);
                for sid in &to_evict {
                    if let Some(live) = sessions.remove(sid) {
                        live.cancel.cancel();
                        info!(%sid, "session evicted (idle timeout)");
                    }
                }
                if !to_evict.is_empty() {
                    debug!(
                        evicted = to_evict.len(),
                        remaining = sessions.len(),
                        "janitor run"
                    );
                }
            }
        });
    }

    // ── 内部方法 ──

    /// 从 HistoryStore 恢复 session 或创建新的。
    /// Resume a session referenced by `sid` (loading history first, if a
    /// `history_store` is configured) or create it fresh under a new id if
    /// that fails. Mirrors `create()`'s `Result<String, String>` shape — a
    /// second failed attempt is a real (if rare) error condition, not a bug,
    /// so it's propagated to the caller instead of panicking (a
    /// `create()` failure here is
    /// systemic — e.g. engine construction failing, or a `daemon.shutdown`
    /// racing this call — so retrying under a new id is unlikely to help,
    /// but the caller still deserves a normal JSON-RPC error instead of the
    /// connection silently dying).
    async fn resume_or_create(
        &self,
        sid: String,
        options: Option<&SessionOptions>,
    ) -> Result<String, String> {
        // 尝试从 HistoryStore 加载历史消息。`sid` 来自 RPC 调用方（`session.run_turn`
        // 的 `session_id` 参数），不保证是合法的 BASE58 ID——解析失败当作"没有历史"
        // 处理（等价于查不到），而不是 unwrap 崩溃：一个格式错误的 session_id 不该
        // 让整条连接的处理任务 panic。
        let has_history = if let Some(ref store) = self.history_store {
            match base::session::SessionId::parse(&sid) {
                Ok(parsed) => match store.load(parsed).await {
                    Ok(entries) => !entries.is_empty(),
                    Err(_) => false,
                },
                Err(_) => false,
            }
        } else {
            false
        };

        // P3: rebuild against *this session's own* recorded project, not
        // always the pool default — otherwise a session created under
        // `session.create {project_root: X}` would silently come back with
        // project Y's settings the moment it needed reattaching (idle
        // eviction, daemon restart, explicit `session.resume`).
        let startup_scene = self.settings.read().await.paths.scope.clone();
        let (base_settings, project_root_for_meta) = match self.recorded_project(&sid).await {
            Some((schema_version, project_root, scene)) if schema_version >= 2 => {
                let scene = scene.unwrap_or_else(|| startup_scene.clone());
                match project_root {
                    Some(p) => {
                        let path = PathBuf::from(p);
                        (
                            self.settings_for_project(Some(&path), &scene).await,
                            Some(path),
                        )
                    }
                    None => (self.settings_for_project(None, &scene).await, None),
                }
            }
            _ => (self.settings.read().await.clone(), Some(self.cwd.clone())),
        };

        match self
            .create(
                sid.clone(),
                self.scene.clone(),
                base_settings,
                project_root_for_meta,
                options,
                has_history,
            )
            .await
        {
            Ok(s) => Ok(s),
            Err(e) => {
                warn!(%sid, error=%e, has_history, "create failed, retrying with a fresh id");
                let new_sid = Id::new().to_string();
                // A fresh id has no on-disk history of its own, so this retry
                // never resumes — even if the original `sid` had history we
                // failed to load.
                // Degraded fallback path — the pool's default project
                // rather than re-deriving `sid`'s original one, since this
                // only runs when something about `sid` was already broken.
                self.create(
                    new_sid.clone(),
                    self.scene.clone(),
                    self.settings.read().await.clone(),
                    Some(self.cwd.clone()),
                    options,
                    false,
                )
                .await
                .map_err(|e| format!("create {sid} failed, retry also failed: {e}"))
            }
        }
    }

    /// LRU 驱逐：移除最久未活跃的 session。
    /// 调用 LLM 生成 session 名称。
    async fn generate_session_name(&self, prompt: &str) -> Result<String, String> {
        use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage, StreamParams};
        use base::interface::prompt::PromptBlock;
        use futures::StreamExt;

        let system = PromptBlock::system(
            "你是一个简洁的标题生成器。只输出 3-5 个词的中文标题，不要任何解释。",
        );
        let messages = vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: prompt.to_string(),
            }],
        }];

        let stream = self
            .model
            .stream(
                vec![system],
                vec![],
                messages,
                StreamParams {
                    model: "claude-haiku-4-5-20251001".into(),
                    max_tokens: 50,
                    thinking_mode: base::interface::settings::ThinkingMode::Off,
                    fallback_model: None,
                    cache_edits: vec![],
                },
                CancellationToken::new(),
            )
            .await
            .map_err(|e| format!("LLM name error: {e}"))?;

        tokio::pin!(stream);
        let mut name = String::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(base::interface::model::ModelEvent::TextDelta { text }) => {
                    name.push_str(&text);
                }
                Ok(base::interface::model::ModelEvent::EndTurn { .. }) => break,
                Err(e) => return Err(format!("LLM name stream error: {e}")),
                _ => {}
            }
        }

        let name = name.trim().trim_matches('"').trim().to_string();
        if name.is_empty() {
            Err("empty name generated".into())
        } else {
            Ok(name)
        }
    }
}

/// LRU 驱逐：从 `sessions` 里移除最久未活跃的一个（若非空）。不依赖 `SessionPool`
/// 的任何字段，拆成自由函数而非 `&self` 方法——一是本来就没用到 `self`，二是
/// 让测试不用搭一整个 `SessionPool` 就能直接验证驱逐选中的是最旧的那个。
fn evict_lru(sessions: &mut HashMap<String, LiveSession>) {
    if let Some((sid, _)) = sessions
        .iter()
        .min_by_key(|(_, live)| live.last_active)
        .map(|(k, v)| (k.clone(), v.last_active))
    {
        if let Some(live) = sessions.remove(&sid) {
            live.cancel.cancel();
            info!(%sid, "session evicted (LRU, pool full)");
        }
    }
}

/// Session id 列表：`last_active` 距 `now` 超过 `timeout` 的那些。拆成纯函数
/// （不做任何驱逐动作，只挑出 id）——原逻辑内嵌在 `start_janitor` 真实跑
/// 300 秒一次的循环体里，没法在测试里等 5 分钟去验证；抽出来之后可以直接用
/// 人为构造的" last_active 已经很旧"的 session 测，不用真的睡觉。
fn idle_session_ids(
    sessions: &HashMap<String, LiveSession>,
    now: Instant,
    timeout: Duration,
) -> Vec<String> {
    sessions
        .iter()
        .filter(|(_, live)| now.duration_since(live.last_active) > timeout)
        .map(|(sid, _)| sid.clone())
        .collect()
}

fn format_instant(t: Instant) -> String {
    let ago = Instant::now().duration_since(t);
    let secs_ago = ago.as_secs();
    let now = time::OffsetDateTime::now_utc();
    let abs = now - time::Duration::seconds(secs_ago as i64);
    abs.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
}

/// Build the pool-level telemetry handle — see `SessionPool::pool_telemetry`'s
/// doc comment for what this is for. Same shape as the per-session pipeline
/// `create()` builds (`FileRecorder` + optional OTel sink via
/// `telemetry::spawn`), just writing to one shared `pool.jsonl` instead of
/// per-session files, since these events happen before any session exists.
fn build_pool_telemetry(settings: &Settings) -> telemetry::TelemetryHandle {
    if !settings.telemetry_enabled {
        return telemetry::TelemetryHandle::noop();
    }
    let dir = &settings.paths.local_data_dir;
    if !dir.is_absolute() {
        // Same reasoning as `create()`'s per-session path: recording
        // nothing is better than scattering a `telemetry/` directory into
        // whatever the process's working directory happens to be.
        return telemetry::TelemetryHandle::noop();
    }
    let path = dir.join("telemetry").join("pool.jsonl");
    let rec = match FileRecorder::new(&path) {
        Ok(rec) => rec,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "could not open pool-level telemetry file; pool-level events record nothing");
            return telemetry::TelemetryHandle::noop();
        }
    };
    let mut recorders: Vec<std::sync::Arc<dyn telemetry::TelemetryRecorder>> =
        vec![std::sync::Arc::new(rec)];
    if let Some(otel) = otel_sink(settings.telemetry_url.as_deref()) {
        recorders.push(otel);
    }
    let config = telemetry::TelemetryConfig {
        enabled: true,
        mode: telemetry::TelemetryMode::Enabled,
        redact_prompts: true,
        redact_tool_content: true,
        default_event_enabled: true,
        ..Default::default()
    };
    match telemetry::spawn(config, recorders) {
        Ok((handle, consumer)) => {
            tokio::spawn(consumer);
            handle
        }
        Err(e) => {
            warn!(error = %e, "failed to start pool-level telemetry pipeline");
            telemetry::TelemetryHandle::noop()
        }
    }
}

/// A `TelemetryRecorder` that forwards turn/tool events to OpenTelemetry, or
/// `None` when no OTLP endpoint is configured.
///
/// `crate::telemetry_otel::start_otel` installs *global* meter and tracer
/// providers, so it must run at most once per process however many sessions
/// are created — hence the `OnceLock`. The exporter itself is kept alive for
/// the process's lifetime (dropping it would tear the global providers down
/// under any session still recording).
///
/// The `telemetry` crate itself never talks to the network — this recorder
/// is how the daemon opts in to sending data anywhere. Compiled out unless
/// the daemon's own `otel` feature is on — the OTLP SDK is a heavy
/// dependency tree and the JSONL recorder is what makes a session debuggable
/// by default. Build the daemon with `--features otel` to get this.
#[cfg(feature = "otel")]
fn otel_sink(endpoint: Option<&str>) -> Option<std::sync::Arc<dyn telemetry::TelemetryRecorder>> {
    let endpoint = endpoint
        .map(str::to_string)
        .or_else(|| std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok())
        .filter(|e| !e.trim().is_empty())?;

    static OTEL: std::sync::OnceLock<Option<std::sync::Arc<crate::telemetry_otel::OtelExporter>>> =
        std::sync::OnceLock::new();
    let exporter = OTEL.get_or_init(|| match crate::telemetry_otel::start_otel(&endpoint) {
        Ok(e) => {
            info!(%endpoint, "OpenTelemetry exporter started");
            Some(std::sync::Arc::new(e))
        }
        Err(e) => {
            warn!(%endpoint, error = %e, "could not start the OpenTelemetry exporter; \
                      telemetry still records to file");
            None
        }
    });
    exporter
        .clone()
        .map(|e| e as std::sync::Arc<dyn telemetry::TelemetryRecorder>)
}

/// No-op stand-in when the `otel` feature is off.
#[cfg(not(feature = "otel"))]
fn otel_sink(endpoint: Option<&str>) -> Option<std::sync::Arc<dyn telemetry::TelemetryRecorder>> {
    if endpoint.is_some_and(|e| !e.trim().is_empty()) {
        warn!(
            "an OTLP endpoint is configured but this build has no OpenTelemetry support; \
             rebuild with `--features otel`"
        );
    }
    None
}

/// The permission mode a session will actually run under.
///
/// The caller's `options.permission_mode` is honored, but **clamped** to the
/// daemon's configured `settings.permission_mode`: a client may tighten the
/// mode for its own session and may not loosen it. Without the clamp,
/// `settings.permission_mode` was only a default — any client that could
/// authenticate could ask for `bypassPermissions` and get it, regardless of
/// how the daemon was configured, and `daemon/src/server.rs` is explicit
/// that there is no per-method authorization on this socket.
///
/// `settings.allow_client_permission_override` (default `false`) restores
/// the old "client wins outright" behavior for deployments where every
/// client is as trusted as the daemon.
///
/// With nothing configured anywhere the result is `PermissionMode::Default`
/// — i.e. *ask*.
fn effective_permission_mode(
    settings: &Settings,
    options: Option<&SessionOptions>,
) -> base::interface::settings::PermissionMode {
    let Some(requested) = options.and_then(|o| o.permission_mode) else {
        return settings.permission_mode;
    };
    if settings.allow_client_permission_override {
        return requested;
    }
    let clamped = requested.min_permissive(settings.permission_mode);
    if clamped != requested {
        warn!(
            requested = ?requested,
            configured = ?settings.permission_mode,
            effective = ?clamped,
            "session requested a more permissive mode than this daemon allows; clamped \
             (set `allow_client_permission_override: true` to permit it)"
        );
    }
    clamped
}

/// Which `Permission` a new session gets.
///
/// **This is where the daemon's trust boundary moved.** It used to return
/// the pool's shared allow-all `Arc` for every session that didn't pass an
/// explicit `options.permission_mode`, making "no permission checks at all"
/// the daemon default. It now resolves a real mode
/// (`effective_permission_mode`) and builds a `RuleSetPermission` for it;
/// with nothing configured that mode is `Default`, so an unlisted tool call
/// raises a `kind:"prompt"` event and waits for `session.respondToPrompt`.
///
/// `bypassPermissions` — from settings.json (daemon-wide) or
/// `options.permission_mode` (one session) — is the explicit opt-out, and it
/// returns the *literal same* shared `Arc` the pool was built with, not an
/// equivalent instance: a host that sandboxes the daemon itself gets exactly
/// the old zero-overhead path back, and the `Arc::ptr_eq` assertion in the
/// unit test below keeps it that way.
///
/// Rules come from every tier, settings first — `settings.permission_rules`
/// and the `settings.local.json` overlay, each tagged with its own
/// `RuleSource` (see `RuleSetPermission::from_settings`) — then
/// `options.permission_rules` appended, so a session-scoped rule is
/// evaluated alongside the configured ones rather than replacing them.
///
/// An `ask` verdict from the rule engine is turned into a real, answerable
/// prompt by wrapping the result in `AskingPermission` — see
/// `crate::permission_prompt` for why the daemon does the asking itself
/// instead of returning `PermissionOutcome::Prompt` to the engine.
///
/// Free function (not inlined into `create()`) specifically so this decision
/// is unit-testable without spinning up a real session.
#[allow(clippy::too_many_arguments)]
fn resolve_session_permission(
    bypass_instance: &Arc<dyn Permission>,
    tools: &Arc<base::tool::InMemoryToolRegistry>,
    settings: &Settings,
    options: Option<&SessionOptions>,
) -> Arc<dyn Permission> {
    let mode = effective_permission_mode(settings, options);
    if matches!(
        mode,
        base::interface::settings::PermissionMode::BypassPermissions
    ) {
        return bypass_instance.clone();
    }

    let extra = options
        .map(|o| o.permission_rules.clone())
        .unwrap_or_default();

    let rule_set: Arc<dyn Permission> = Arc::new(
        permissions::rule_set_permission::RuleSetPermission::from_settings(
            settings,
            mode.into(),
            tools.clone(),
            extra,
        ),
    );

    rule_set
}

/// Redact a secret to `***<last 4 chars>` (char-boundary safe — slices by
/// `char`, not byte, so this never panics on non-ASCII input); too-short
/// secrets (≤4 chars) redact to a bare `***` rather than revealing the
/// whole thing.
/// Redact every credential-shaped field in a settings blob.
///
/// Walks by key name rather than by type, because the same secrets appear at
/// several depths (`providers.<id>.api_key`, `model.auth_token`) and a
/// settings shape that grows a new provider field should not silently start
/// returning it in the clear.
fn redact_settings_secrets(value: &mut serde_json::Value) {
    const SECRET_KEYS: &[&str] = &["api_key", "auth_token", "token", "secret", "password"];
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                match v {
                    serde_json::Value::String(s) if SECRET_KEYS.contains(&k.as_str()) => {
                        if !s.is_empty() {
                            *v = serde_json::Value::String(redact_secret(s));
                        }
                    }
                    other => redact_settings_secrets(other),
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_settings_secrets(item);
            }
        }
        _ => {}
    }
}

fn redact_secret(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 4 {
        return "***".to_string();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("***{tail}")
}

#[cfg(test)]
mod pool_telemetry_tests {
    use super::*;

    #[tokio::test]
    async fn relative_local_data_dir_yields_a_noop_handle() {
        // `Settings::defaults_for` leaves `local_data_dir` at "." (relative)
        // — the same "don't scatter a telemetry/ dir into the working
        // directory" guard `create()`'s per-session path uses.
        let settings = Settings::defaults_for("claude-sonnet-4-6");
        let handle = build_pool_telemetry(&settings);
        // A noop handle's `record` always succeeds and writes nothing
        // anywhere observable; the real assertion is in the next test,
        // which proves the non-noop path actually persists.
        assert!(handle
            .record(telemetry::TelemetryEvent::resume_action(
                "sess",
                0,
                None,
                telemetry::ResumeActionPayload {
                    outcome: telemetry::ResumeOutcome::Failed,
                    source: "not_found".into(),
                    entry_count: 0,
                    projected_message_count: 0,
                    compact_boundary_count: 0,
                    sidechain_entry_count: 0,
                    warning_kind: Some("not_found".into()),
                    latency_ms: 0,
                },
            ))
            .is_ok());
    }

    #[tokio::test]
    async fn telemetry_disabled_yields_a_noop_handle_even_with_an_absolute_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Settings::defaults_for("claude-sonnet-4-6");
        settings.telemetry_enabled = false;
        settings.paths.local_data_dir = dir.path().to_path_buf();
        let _handle = build_pool_telemetry(&settings);
        assert!(!dir.path().join("telemetry").join("pool.jsonl").exists());
    }

    #[tokio::test]
    async fn absolute_dir_and_telemetry_enabled_persists_events_to_pool_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Settings::defaults_for("claude-sonnet-4-6");
        settings.telemetry_enabled = true;
        settings.paths.local_data_dir = dir.path().to_path_buf();
        let handle = build_pool_telemetry(&settings);

        handle
            .record(telemetry::TelemetryEvent::resume_action(
                "sess-1",
                0,
                None,
                telemetry::ResumeActionPayload {
                    outcome: telemetry::ResumeOutcome::Failed,
                    source: "not_found".into(),
                    entry_count: 0,
                    projected_message_count: 0,
                    compact_boundary_count: 0,
                    sidechain_entry_count: 0,
                    warning_kind: Some("not_found".into()),
                    latency_ms: 5,
                },
            ))
            .expect("record should succeed");

        // `FileRecorder` writes synchronously, but the event still has to
        // cross the channel to the fan-out task first — poll briefly rather
        // than assume it has landed the instant `record()` returns.
        let path = dir.path().join("telemetry").join("pool.jsonl");
        let content = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(content) = tokio::fs::read_to_string(&path).await {
                    if !content.is_empty() {
                        return content;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("pool.jsonl should have received the event");

        assert!(content.contains("resume_action"));
        assert!(content.contains("not_found"));
    }
}

#[cfg(test)]
mod redact_tests {
    use super::redact_secret;

    #[test]
    fn redacts_long_secret_keeping_last_four() {
        assert_eq!(redact_secret("sk-ant-abcdef1234"), "***1234");
    }

    #[test]
    fn short_secret_fully_redacted() {
        assert_eq!(redact_secret("abcd"), "***");
        assert_eq!(redact_secret(""), "***");
    }

    #[test]
    fn handles_multibyte_chars_without_panicking() {
        // 4-byte-per-char emoji tail — a byte-slice `&s[len-4..]` would
        // panic here; char-based slicing must not.
        let s = "sk-😀😀😀😀";
        assert_eq!(redact_secret(s), "***😀😀😀😀");
    }
}

#[cfg(test)]
mod resolve_session_permission_tests {
    use super::*;

    struct AllowAllTestPermission;
    #[async_trait::async_trait]
    impl base::interface::permission::Permission for AllowAllTestPermission {
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

    fn defaults() -> Settings {
        Settings::defaults_for("claude-sonnet-4-6")
    }

    /// Everything the resolver needs beyond the settings/options under test.
    fn resolve(
        settings: &Settings,
        options: Option<&SessionOptions>,
    ) -> (Arc<dyn Permission>, Arc<dyn Permission>) {
        let bypass: Arc<dyn Permission> = Arc::new(AllowAllTestPermission);
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        let resolved = resolve_session_permission(&bypass, &tools, settings, options);
        (bypass, resolved)
    }

    /// The behavior change this round is *about*: with nothing configured,
    /// a session no longer gets the allow-all instance.
    #[test]
    fn nothing_configured_now_builds_a_real_asking_permission() {
        let (bypass, resolved) = resolve(&defaults(), None);
        assert!(
            !Arc::ptr_eq(&bypass, &resolved),
            "the daemon default is no longer allow-all — it must build a real asking permission"
        );

        let opts = SessionOptions::default();
        let (bypass, resolved) = resolve(&defaults(), Some(&opts));
        assert!(
            !Arc::ptr_eq(&bypass, &resolved),
            "an options object that simply omits permission_mode must not re-enable allow-all"
        );
    }

    #[test]
    fn settings_bypass_mode_is_the_opt_out_and_restores_the_shared_instance() {
        let mut settings = defaults();
        settings.permission_mode = base::interface::settings::PermissionMode::BypassPermissions;

        let (bypass, resolved) = resolve(&settings, None);
        assert!(
            Arc::ptr_eq(&bypass, &resolved),
            "opting out must hand back the literal same shared instance, not an equivalent one"
        );
    }

    /// N-6: a client asking for a *more permissive* mode than the daemon is
    /// configured with gets clamped, not obeyed. Before this, `settings.json`'s
    /// `permission_mode` was only a default: anyone who could authenticate to
    /// the socket could open a `bypassPermissions` session regardless of it.
    #[test]
    fn session_options_cannot_loosen_an_asking_settings_default() {
        let opts = SessionOptions {
            permission_mode: Some(base::interface::settings::PermissionMode::BypassPermissions),
            ..Default::default()
        };
        let (bypass, resolved) = resolve(&defaults(), Some(&opts));
        assert!(
            !Arc::ptr_eq(&bypass, &resolved),
            "a client must not be able to talk the daemon into allow-all"
        );
        assert_eq!(
            effective_permission_mode(&defaults(), Some(&opts)),
            base::interface::settings::PermissionMode::Default,
            "the request should clamp down to the configured mode"
        );
    }

    /// ...unless the operator explicitly says clients are as trusted as the
    /// daemon itself.
    #[test]
    fn allow_client_permission_override_restores_client_wins() {
        let mut settings = defaults();
        settings.allow_client_permission_override = true;
        let opts = SessionOptions {
            permission_mode: Some(base::interface::settings::PermissionMode::BypassPermissions),
            ..Default::default()
        };
        let (bypass, resolved) = resolve(&settings, Some(&opts));
        assert!(Arc::ptr_eq(&bypass, &resolved));
    }

    /// Tightening is always allowed, in both directions of the comparison:
    /// a `bypassPermissions` daemon still honors a client that wants to be
    /// asked.
    #[test]
    fn a_client_may_always_tighten() {
        let mut settings = defaults();
        settings.permission_mode = base::interface::settings::PermissionMode::BypassPermissions;
        let opts = SessionOptions {
            permission_mode: Some(base::interface::settings::PermissionMode::Plan),
            ..Default::default()
        };
        assert_eq!(
            effective_permission_mode(&settings, Some(&opts)),
            base::interface::settings::PermissionMode::Plan
        );
    }

    #[test]
    fn settings_ask_default_is_overridable_per_session_without_becoming_allow_all() {
        let opts = SessionOptions {
            permission_mode: Some(base::interface::settings::PermissionMode::DontAsk),
            ..Default::default()
        };
        let (bypass, resolved) = resolve(&defaults(), Some(&opts));
        assert!(!Arc::ptr_eq(&bypass, &resolved));
    }

    #[test]
    fn effective_mode_prefers_session_options_then_settings_then_ask() {
        use base::interface::settings::PermissionMode;
        let mut settings = defaults();
        assert_eq!(
            effective_permission_mode(&settings, None),
            PermissionMode::Default,
            "an unconfigured daemon asks"
        );

        settings.permission_mode = PermissionMode::BypassPermissions;
        assert_eq!(
            effective_permission_mode(&settings, None),
            PermissionMode::BypassPermissions
        );

        let opts = SessionOptions {
            permission_mode: Some(PermissionMode::Plan),
            ..Default::default()
        };
        assert_eq!(
            effective_permission_mode(&settings, Some(&opts)),
            PermissionMode::Plan,
            "a per-session mode wins over the settings default"
        );
    }
}

#[cfg(test)]
mod eviction_tests {
    use super::*;

    /// A `LiveSession` with no real agent behind it — only `last_active`
    /// (and `cancel`, so eviction's `.cancel()` call doesn't panic) matter
    /// for these tests.
    fn dummy_live_session(last_active: Instant) -> LiveSession {
        let (input_tx, _input_rx): (InputSender, _) = tokio::sync::mpsc::unbounded_channel();
        let (_event_tx, event_rx): (_, EventReceiver) = tokio::sync::mpsc::unbounded_channel();
        LiveSession {
            input_tx,
            event_rx: Arc::new(AsyncMutex::new(Some(event_rx))),
            cancel: CancellationToken::new(),
            name: None,
            created_at: last_active,
            last_active,
            is_first_turn: true,
            config_generation: 0,
            current_turn: None,
        }
    }

    #[test]
    fn evict_lru_removes_the_least_recently_active_session() {
        let now = Instant::now();
        let mut sessions = HashMap::new();
        sessions.insert(
            "oldest".to_string(),
            dummy_live_session(now.checked_sub(Duration::from_secs(60)).unwrap()),
        );
        sessions.insert(
            "middle".to_string(),
            dummy_live_session(now.checked_sub(Duration::from_secs(30)).unwrap()),
        );
        sessions.insert("newest".to_string(), dummy_live_session(now));

        evict_lru(&mut sessions);

        assert_eq!(
            sessions.len(),
            2,
            "sessions: {:?}",
            sessions.keys().collect::<Vec<_>>()
        );
        assert!(!sessions.contains_key("oldest"));
        assert!(sessions.contains_key("middle"));
        assert!(sessions.contains_key("newest"));
    }

    #[test]
    fn inserting_over_an_existing_sid_must_cancel_the_replaced_sessions_token() {
        // Mirrors the pattern `create()` uses: `sessions.insert(sid, new_live)`
        // returning `Some(old)` means a live entry already existed under that
        // exact sid — reachable via the config-generation recreate path in
        // `run_turn`. `CancellationToken` isn't tied to `Drop`, so failing to
        // call `.cancel()` on the returned old value would leak the old
        // Agent's background `tokio::spawn` loop forever. This test isolates
        // just that HashMap-replace-then-cancel pattern (not the full
        // `create()`/`SessionPool`, which needs a live client/settings/etc.).
        let mut sessions = HashMap::new();
        let old_session = dummy_live_session(Instant::now());
        let old_cancel = old_session.cancel.clone();
        sessions.insert("sid".to_string(), old_session);
        assert!(!old_cancel.is_cancelled());

        let new_session = dummy_live_session(Instant::now());
        if let Some(old) = sessions.insert("sid".to_string(), new_session) {
            old.cancel.cancel();
        }

        assert!(
            old_cancel.is_cancelled(),
            "replaced session's token must be cancelled"
        );
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn evict_lru_on_empty_map_is_a_noop() {
        let mut sessions: HashMap<String, LiveSession> = HashMap::new();
        evict_lru(&mut sessions); // must not panic
        assert!(sessions.is_empty());
    }

    #[test]
    fn idle_session_ids_selects_only_sessions_past_timeout() {
        let now = Instant::now();
        let mut sessions = HashMap::new();
        sessions.insert(
            "stale".to_string(),
            dummy_live_session(now.checked_sub(Duration::from_secs(600)).unwrap()),
        );
        sessions.insert("fresh".to_string(), dummy_live_session(now));

        let idle = idle_session_ids(&sessions, now, Duration::from_secs(300));
        assert_eq!(idle, vec!["stale".to_string()]);
    }

    #[test]
    fn idle_session_ids_empty_when_nothing_past_timeout() {
        let now = Instant::now();
        let mut sessions = HashMap::new();
        sessions.insert("fresh".to_string(), dummy_live_session(now));

        let idle = idle_session_ids(&sessions, now, Duration::from_secs(300));
        assert!(idle.is_empty());
    }

    #[test]
    fn idle_session_ids_exactly_at_timeout_boundary_is_not_evicted() {
        // 边界用 `>` 不是 `>=`（见 idle_session_ids 实现）——正好等于超时
        // 阈值的 session 这一轮还不驱逐，下一轮才会（因为到那时肯定 `>` 了）。
        let now = Instant::now();
        let mut sessions = HashMap::new();
        sessions.insert(
            "exactly_at_boundary".to_string(),
            dummy_live_session(now.checked_sub(Duration::from_secs(300)).unwrap()),
        );

        let idle = idle_session_ids(&sessions, now, Duration::from_secs(300));
        assert!(idle.is_empty(), "idle: {idle:?}");
    }
}

#[cfg(test)]
mod mcp_for_project_tests {
    use super::*;

    struct AllowAllTestPermission;
    #[async_trait::async_trait]
    impl base::interface::permission::Permission for AllowAllTestPermission {
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

    /// A `SessionPool` with no MCP servers configured anywhere — every
    /// project's `mcp_for_project` connect resolves to an empty
    /// `McpManager` immediately (no subprocess, no network), which is all
    /// these tests need: they're about the *cache* around the connect, not
    /// the connect itself (that's `mcp_toy_server_smoke.rs`'s job).
    pub(super) async fn test_pool() -> (SessionPool, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let memory_store = Arc::new(MemoryStore::new(
            dir.path().join("user").join("memory"),
            dir.path().join("local").join("memory"),
        ));
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);
        let permission: Arc<dyn Permission> = Arc::new(AllowAllTestPermission);
        let paths: Arc<dyn crate::config::DaemonPaths> = Arc::new(
            crate::config::StaticDaemonPaths::new(dir.path().to_path_buf()),
        );
        let client: Arc<dyn AnthropicClient> = Arc::new(model::mock::MockAnthropicClient::new());

        let pool = SessionPool::new(
            8,
            3600,
            client,
            Arc::new(Settings::defaults_for("claude-sonnet-4-6")),
            scene,
            permission,
            memory_store,
            cwd,
            None,
            paths,
            None,
        );
        (pool, dir)
    }

    #[tokio::test]
    async fn cache_hit_returns_the_same_arc_without_reconnecting() {
        let (pool, _dir) = test_pool().await;
        let first = pool.mcp_for_project(Some(pool.cwd()), "coding").await;
        let second = pool.mcp_for_project(Some(pool.cwd()), "coding").await;
        assert!(
            Arc::ptr_eq(&first, &second),
            "a cache hit must return the exact cached Arc, not a freshly connected one"
        );
    }

    #[tokio::test]
    async fn different_projects_get_independent_cache_entries() {
        let (pool, dir) = test_pool().await;
        let other_project = dir.path().join("other");
        std::fs::create_dir_all(&other_project).unwrap();

        let default_manager = pool.mcp_for_project(Some(pool.cwd()), "coding").await;
        let other_manager = pool.mcp_for_project(Some(&other_project), "coding").await;
        assert!(
            !Arc::ptr_eq(&default_manager, &other_manager),
            "a project seen for the first time must get its own cache miss and connect, \
             not silently inherit another project's connections"
        );

        let other_again = pool.mcp_for_project(Some(&other_project), "coding").await;
        assert!(
            Arc::ptr_eq(&other_manager, &other_again),
            "the second lookup for the same non-default project must be a cache hit"
        );
    }

    #[tokio::test]
    async fn no_project_key_is_cached_independently_of_the_default_project() {
        let (pool, _dir) = test_pool().await;
        let none_manager = pool.mcp_for_project(None, "coding").await;
        let default_manager = pool.mcp_for_project(Some(pool.cwd()), "coding").await;
        assert!(
            !Arc::ptr_eq(&none_manager, &default_manager),
            "the no-project tier (`None`) must not share a cache slot with the default project"
        );
    }
}

/// Multiple scenes coexisting in one daemon process.
#[cfg(test)]
mod multi_scene_tests {
    use super::mcp_for_project_tests::test_pool;
    use super::*;

    /// Two scenes on one daemon must each read their own settings tier.
    ///
    /// `settings_for_project` used to key on project alone and take `scope`
    /// from the pool's own settings, so whichever scene built an entry first
    /// answered for every scene afterwards: a `chat` session on a daemon
    /// started as `--scene coding` silently ran on coding's tier.
    #[tokio::test]
    async fn each_scene_resolves_its_own_settings_tier() {
        let (pool, dir) = test_pool().await;

        // `scenes/chat/settings.json` sets a model the pool's own tier never
        // mentions — if the chat session picks it up, it read chat's tier.
        let chat_tier = dir.path().join("scenes").join("chat");
        std::fs::create_dir_all(&chat_tier).unwrap();
        std::fs::write(
            chat_tier.join("settings.json"),
            r#"{"model": {"model_name": "model-from-chat-tier"}}"#,
        )
        .unwrap();

        let coding = pool.settings_for_project(None, "coding").await;
        let chat = pool.settings_for_project(None, "chat").await;

        assert_eq!(chat.model.model_name, "model-from-chat-tier");
        assert_ne!(
            coding.model.model_name, chat.model.model_name,
            "two scenes must not share one settings entry"
        );
        assert_eq!(
            chat.paths.scope, "chat",
            "scope must be the session's scene"
        );
        assert_eq!(coding.paths.scope, "coding");
    }

    /// The same project under two scenes gets two cache entries, and each is
    /// stable across calls.
    #[tokio::test]
    async fn settings_cache_is_keyed_by_project_and_scene_together() {
        let (pool, dir) = test_pool().await;
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();

        let a1 = pool.settings_for_project(Some(&other), "coding").await;
        let a2 = pool.settings_for_project(Some(&other), "coding").await;
        let b = pool.settings_for_project(Some(&other), "chat").await;

        assert!(
            Arc::ptr_eq(&a1, &a2),
            "same (project, scene) must be cached"
        );
        assert!(
            !Arc::ptr_eq(&a1, &b),
            "one project under two scenes must not collapse to one entry"
        );
        assert_eq!(a1.paths.scope, "coding");
        assert_eq!(b.paths.scope, "chat");
    }

    /// The daemon's own scene keeps the injected `config_root()`; any other
    /// scene is derived under `global_root()/scenes/`.
    ///
    /// Tests inject a `StaticDaemonPaths` whose config root is a tempdir that
    /// bears no relation to `global_root()/scenes/<id>`, so recomputing the
    /// startup scene's directory would move its settings out from under them.
    #[tokio::test]
    async fn the_startup_scene_keeps_its_injected_config_root() {
        let (pool, _dir) = test_pool().await;

        assert_eq!(
            pool.scene_root("coding", "coding"),
            pool.paths.config_root(),
            "the startup scene must not be recomputed"
        );
        assert_eq!(
            pool.scene_root("chat", "coding"),
            pool.paths.global_root().join("scenes").join("chat"),
            "another scene is derived the way DefaultDaemonPaths derives one"
        );
    }

    /// A scene that was never activated must be refused, not silently served
    /// by the daemon's default.
    #[tokio::test]
    async fn creating_a_session_in_an_inactive_scene_is_refused() {
        let (pool, _dir) = test_pool().await;
        assert!(
            pool.resolve_scene(Some("research")).await.is_none(),
            "research was never activated on this pool"
        );
        assert!(
            pool.resolve_scene(Some("chat")).await.is_none(),
            "chat was never activated either"
        );
        assert!(
            pool.resolve_scene(None).await.is_some(),
            "no scene named falls back to the daemon's own"
        );
    }

    /// `scene.activate` makes a second scene usable in the same process, and
    /// sessions created under each are recorded under their own scene.
    #[tokio::test]
    async fn two_scenes_can_be_active_in_one_process() {
        let (pool, _dir) = test_pool().await;

        pool.activate_scene("chat").await.expect("activate chat");

        let coding = pool.resolve_scene(Some("coding")).await.expect("coding");
        let chat = pool.resolve_scene(Some("chat")).await.expect("chat");
        assert_eq!(coding.id(), "coding");
        assert_eq!(chat.id(), "chat");

        // Distinct scenes, so distinct tool surfaces — this is what makes
        // running both in one process meaningful rather than cosmetic.
        assert!(
            coding.tools().is_empty(),
            "coding allows every registered tool"
        );
        assert!(
            !chat.tools().is_empty(),
            "chat restricts its tools to a whitelist"
        );
    }
}

#[cfg(test)]
mod with_session_lock_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct AllowAllTestPermission;
    #[async_trait::async_trait]
    impl base::interface::permission::Permission for AllowAllTestPermission {
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

    async fn test_pool() -> (Arc<SessionPool>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let memory_store = Arc::new(MemoryStore::new(
            dir.path().join("user").join("memory"),
            dir.path().join("local").join("memory"),
        ));
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);
        let permission: Arc<dyn Permission> = Arc::new(AllowAllTestPermission);
        let paths: Arc<dyn crate::config::DaemonPaths> = Arc::new(
            crate::config::StaticDaemonPaths::new(dir.path().to_path_buf()),
        );
        let client: Arc<dyn AnthropicClient> = Arc::new(model::mock::MockAnthropicClient::new());

        let pool = Arc::new(SessionPool::new(
            8,
            3600,
            client,
            Arc::new(Settings::defaults_for("claude-sonnet-4-6")),
            scene,
            permission,
            memory_store,
            cwd,
            None,
            paths,
            None,
        ));
        (pool, dir)
    }

    /// The regression this closes: `session.close`'s cascade delete and a
    /// concurrent `session.resume(..., create_if_missing: true)` on the same
    /// sidechain id used to run with no shared lock at all, so they could
    /// interleave arbitrarily. This drives many concurrent critical sections
    /// for the *same* id and asserts they never overlap.
    #[tokio::test]
    async fn concurrent_calls_for_the_same_id_never_overlap() {
        let (pool, _dir) = test_pool().await;
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let pool = pool.clone();
            let concurrent = concurrent.clone();
            let max_concurrent = max_concurrent.clone();
            tasks.push(tokio::spawn(async move {
                pool.with_session_lock("shared-id", async {
                    let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                    max_concurrent.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    concurrent.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }

        assert_eq!(
            max_concurrent.load(Ordering::SeqCst),
            1,
            "critical sections for the same session_id must never overlap"
        );
    }

    /// The flip side: locking must be per-id, not a single pool-wide mutex —
    /// otherwise every unrelated `session.close`/`.resume` pair would
    /// serialize against each other too.
    #[tokio::test]
    async fn distinct_ids_run_concurrently() {
        let (pool, _dir) = test_pool().await;
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(tokio::sync::Barrier::new(4));

        let mut tasks = Vec::new();
        for i in 0..4 {
            let pool = pool.clone();
            let concurrent = concurrent.clone();
            let max_concurrent = max_concurrent.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                pool.with_session_lock(&format!("id-{i}"), async {
                    barrier.wait().await;
                    let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                    max_concurrent.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    concurrent.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }

        assert!(
            max_concurrent.load(Ordering::SeqCst) > 1,
            "distinct session ids must not serialize against each other"
        );
    }

    #[tokio::test]
    async fn registry_entry_is_cleaned_up_after_use() {
        let (pool, _dir) = test_pool().await;
        pool.with_session_lock("temp-id", async {}).await;
        assert!(
            pool.session_locks.lock().await.is_empty(),
            "a lock nothing else references must not linger in the registry forever"
        );
    }
}
