//! SessionPool —— daemon 多 session 实例管理器。
//!
//! 每个 session 对应一个独立的 Agent 实例（后台 run loop + 独立 event channel）。
//! 支持：
//! - 按 session_id 查找/创建
//! - 容量上限 + LRU 驱逐
//! - 空闲超时回收
//! - session.list 合并活跃 + 历史

use crate::rpc::{codes, RpcResponse, SessionOptions, StreamFrame};
use base::context::EngineConfig;
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
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::RwLock as AsyncRwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

type Writer = Arc<AsyncMutex<Box<dyn AsyncWrite + Send + Unpin + 'static>>>;

/// The two plugin-tier root directories (plain `plugins/`, not
/// `plugins/cache/` — see `plugin::cache` module docs) for this daemon
/// instance's paths: global (shared across scenes) and scene (this
/// daemon's `--scene`).
fn plugin_tier_dirs(paths: &dyn crate::config::DaemonPaths) -> (PathBuf, PathBuf) {
    (
        paths.global_root().join("plugins"),
        paths.config_root().join("plugins"),
    )
}

/// Every plugin discovered on disk + built-ins, regardless of enable state
/// — used by `plugin.list` so disabled plugins still show up (with
/// `enabled: false`), not just the active set sessions actually use.
fn discover_all_plugins(paths: &dyn crate::config::DaemonPaths) -> Vec<plugin::manifest::Plugin> {
    let (global, scene) = plugin_tier_dirs(paths);
    plugin::discover_plugins(&global, &scene)
}

/// Subset of `all` that's currently enabled (see `plugin::state`) — what
/// actually gets wired into new sessions' hooks/MCP/commands/agent types.
fn active_plugins(
    paths: &dyn crate::config::DaemonPaths,
    all: &[plugin::manifest::Plugin],
) -> Vec<plugin::manifest::Plugin> {
    let (global, scene) = plugin_tier_dirs(paths);
    let global_state = plugin::state::EnableState::new(global);
    let scene_state = plugin::state::EnableState::new(scene);
    all.iter()
        .filter(|p| {
            plugin::state::resolve_enabled(&p.manifest.plugin.name, &global_state, &scene_state)
        })
        .cloned()
        .collect()
}

/// Build the command catalog shared by every session: skill-derived + the 5
/// built-in local commands (via `CommandRegistry::from_skill_manager`), then
/// each plugin's `slash_commands` merged in. Scans the same three skill
/// tiers `runtime::agent::Builder::build()` does (global → scene → project)
/// — duplicated here rather than shared because this needs to run once at
/// daemon startup, independent of any particular session.
fn build_shared_commands(
    settings: &Settings,
    plugins: &[plugin::manifest::Plugin],
) -> Arc<runtime::commands::CommandRegistry> {
    let skill_mgr = skills::manager::SkillManager::new();
    let project_skills_dir = settings.paths.project_root().join(".agents").join("skills");
    let _ = skill_mgr.load_dir_subdirs(
        &settings.paths.global_data_dir.join("skills"),
        skills::manager::SkillSource::User,
    );
    let _ = skill_mgr.load_dir_subdirs(
        &settings.paths.user_data_dir.join("skills"),
        skills::manager::SkillSource::User,
    );
    let _ = skill_mgr.load_dir_subdirs(&project_skills_dir, skills::manager::SkillSource::Project);
    for bundled in skills::bundled::bundled_skills() {
        skill_mgr.register_bundled(bundled);
    }

    let mut registry = runtime::commands::CommandRegistry::from_skill_manager(&skill_mgr);
    for plugin in plugins {
        plugin.install_slash_commands(&mut registry, &plugin.manifest.plugin.name, &plugin.root);
    }
    Arc::new(registry)
}

/// MCP server configs declared by installed plugins (`plugin.toml`'s `[mcp]
/// servers = [...]` entries), keyed the same way
/// `Plugin::install_mcp_servers` would (`"{plugin_name}-mcp-{idx}"`) so they
/// merge into `settings.mcp_servers` without colliding across plugins.
/// Read-only JSON parsing — connecting is still done exactly once by the
/// existing `connect_mcp_servers_in_background` flow (see
/// `SessionPool::plugin_mcp_servers`), not duplicated here.
fn plugin_mcp_server_configs(
    plugins: &[plugin::manifest::Plugin],
) -> HashMap<String, serde_json::Value> {
    let mut out = HashMap::new();
    for plugin in plugins {
        for (idx, path) in plugin.mcp_server_paths.iter().enumerate() {
            match std::fs::read_to_string(path) {
                Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
                    Ok(v) => {
                        out.insert(format!("{}-mcp-{idx}", plugin.manifest.plugin.name), v);
                    }
                    Err(e) => tracing::warn!(
                        plugin = %plugin.manifest.plugin.name,
                        path = %path.display(),
                        error = %e,
                        "invalid plugin mcp server config json, skipping"
                    ),
                },
                Err(e) => tracing::warn!(
                    plugin = %plugin.manifest.plugin.name,
                    path = %path.display(),
                    error = %e,
                    "failed to read plugin mcp server config, skipping"
                ),
            }
        }
    }
    out
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
    /// `SessionPool.config_generation` 的值，建这个 Agent 时记录的快照。
    /// `run_turn` 每次分派给一个已存在的 session 前，会拿这个值跟池子当前的
    /// 代数比——落后就说明期间有过 `config.setProvider`/`config.reload`，
    /// 在真正发这条消息之前原地重建（见 `run_turn` 里的重建逻辑）。用代数
    /// 而不是"reload 时主动去改每个 session"，是因为改动只需要在两处发生：
    /// reload 时 `fetch_add(1)`，用到时惰性比较一次，不用遍历/标记所有
    /// session。
    config_generation: u64,
}

// ── SessionPool ─────────────────────────────────────────────────────────

pub struct SessionPool {
    sessions: AsyncMutex<HashMap<String, LiveSession>>,
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
    /// The single scene this daemon instance serves (resolved + validated
    /// from `--scene` at startup — see `main.rs::resolve_scene`). Every
    /// session created by this pool uses this scene; there is no per-session
    /// scene selection.
    scene: Arc<dyn AgentScene>,
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
    engine_config: EngineConfig,
    history_store: Option<Arc<dyn history::store::HistoryStore>>,
    paths: Arc<dyn crate::config::DaemonPaths>,
    /// Centrally-connected MCP servers, shared by every session — connected
    /// once (in the background, see `connect_mcp_servers_in_background`),
    /// not reconnected per session. Each new session gets its own owned
    /// `McpManager` built from this one's `McpClientHandle`s (cheap `Arc`
    /// clones, no new connection) — see `create()`.
    mcp: AsyncRwLock<Arc<McpManager>>,
    /// Daemon-level async notifications (MCP connect outcomes, import
    /// auto-detection, future error events) — see `daemon.subscribeEvents`.
    /// `send()` returning `Err` just means "no subscribers right now", not
    /// a real error, so callers of `emit_event` ignore it.
    events_tx: tokio::sync::broadcast::Sender<serde_json::Value>,
    /// *Active* (enabled) plugins available to this daemon instance — see
    /// `discover_all_plugins`/`active_plugins`. Hot-swappable like
    /// `settings`/`mcp`: `refresh_plugins()` (called after
    /// `plugin.install`/`uninstall`/`enable`/`disable`/`update`) updates
    /// this for sessions created *after* the call; already-running sessions
    /// keep whatever plugin set they were built with.
    plugins: AsyncRwLock<Arc<Vec<plugin::manifest::Plugin>>>,
    /// Command catalog shared by every session — skill-derived + built-in
    /// local commands + plugin-contributed slash commands, rebuilt by
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
}

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
        engine_config: EngineConfig,
        history_store: Option<Arc<dyn history::store::HistoryStore>>,
        paths: Arc<dyn crate::config::DaemonPaths>,
        task_router: Option<Arc<base::provider::TaskRouter>>,
    ) -> Self {
        let model = Arc::new(AnthropicModel::new(client.clone()));
        let (events_tx, _) = tokio::sync::broadcast::channel(256);

        let all_plugins = discover_all_plugins(paths.as_ref());
        let plugins = Arc::new(active_plugins(paths.as_ref(), &all_plugins));
        let commands = build_shared_commands(&settings, &plugins);

        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools::register_builtin_tools(&tools);

        Self {
            sessions: AsyncMutex::new(HashMap::new()),
            cap,
            idle_timeout: Duration::from_secs(idle_timeout_secs),
            _client: client,
            model,
            settings: AsyncRwLock::new(settings),
            scene,
            permission,
            memory_store,
            tools,
            cwd,
            engine_config,
            history_store,
            paths,
            mcp: AsyncRwLock::new(Arc::new(McpManager::empty())),
            events_tx,
            plugins: AsyncRwLock::new(plugins),
            commands: AsyncRwLock::new(commands),
            task_router: AsyncRwLock::new(task_router),
            config_generation: std::sync::atomic::AtomicU64::new(0),
        }
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

    /// Full slash command catalog — built-in locals + skill-derived +
    /// plugin-contributed (see `build_shared_commands`) — for the
    /// `commands.list` RPC. Executing one is not a separate RPC: send
    /// `/name args` as the `message` of `session.run_turn`, which already
    /// intercepts and runs it (see `runtime::turn::process_turn`).
    pub async fn list_commands(&self) -> Vec<runtime::commands::CommandInfo> {
        self.commands.read().await.list_detailed()
    }

    /// MCP server configs declared by *active* installed plugins — see the
    /// `plugin_mcp_server_configs` free function. Callers merge this into
    /// `settings.mcp_servers` before the single startup
    /// `connect_mcp_servers_in_background` call, so plugin-declared servers
    /// get the exact same centrally-connected/shared-across-sessions
    /// treatment as user-configured ones.
    pub async fn plugin_mcp_servers(&self) -> HashMap<String, serde_json::Value> {
        plugin_mcp_server_configs(&self.plugins.read().await)
    }

    /// List every installed plugin (built-in + disk, both tiers) with its
    /// current enabled state — for the `plugin.list` RPC. Unlike
    /// `self.plugins` (the active set wired into sessions), this includes
    /// disabled plugins too, since a management UI needs to show and
    /// re-enable them.
    pub async fn list_plugins(&self) -> Vec<serde_json::Value> {
        let all = discover_all_plugins(self.paths.as_ref());
        let (global, scene) = plugin_tier_dirs(self.paths.as_ref());
        let global_state = plugin::state::EnableState::new(global);
        let scene_state = plugin::state::EnableState::new(scene);
        all.iter()
            .map(|p| {
                let name = &p.manifest.plugin.name;
                serde_json::json!({
                    "name": name,
                    "version": p.manifest.plugin.version,
                    "description": p.manifest.plugin.description,
                    "enabled": plugin::state::resolve_enabled(name, &global_state, &scene_state),
                })
            })
            .collect()
    }

    /// Resolve `scope` ("global" or "scene") to its plugins-tier root
    /// directory (plain `plugins/`, not `plugins/cache/`).
    fn plugin_tier_root(&self, scope: &str) -> Result<PathBuf, String> {
        let (global, scene) = plugin_tier_dirs(self.paths.as_ref());
        match scope {
            "global" => Ok(global),
            "scene" => Ok(scene),
            other => Err(format!(
                "invalid scope '{other}' — expected 'global' or 'scene'"
            )),
        }
    }

    /// Install a plugin from an explicit source (no marketplace needed —
    /// see `plugin::cli::PluginCommands::install_source`). `scope` picks
    /// which tier's cache the plugin lands in ("global", the default a
    /// caller should use unless it specifically wants a scene-only
    /// install, or "scene"). Refreshes the active plugin/command set on
    /// success so sessions created after this call see it — already-running
    /// sessions are unaffected, same as `config.setProvider`/`mcp.addServer`.
    pub async fn install_plugin(
        &self,
        name: &str,
        version: &str,
        download_url: &str,
        checksum: Option<&str>,
        scope: &str,
    ) -> Result<serde_json::Value, String> {
        let tier_root = self.plugin_tier_root(scope)?;
        let cache = plugin::cache::PluginCache::new(tier_root.join("cache"));
        let commands = plugin::cli::PluginCommands::new(cache, None);
        let source = plugin::marketplace::PluginSource {
            download_url: download_url.to_string(),
            checksum: checksum.map(|s| s.to_string()),
            version: version.to_string(),
        };
        let result = commands
            .install_source(name, &source)
            .await
            .map_err(|e| e.to_string())?;
        self.refresh_plugins().await;
        Ok(serde_json::json!({
            "success": result.success,
            "message": result.message,
        }))
    }

    /// Uninstall a plugin (all versions, or a specific one) from `scope`'s
    /// tier. Refreshes the active plugin/command set — see `install_plugin`.
    pub async fn uninstall_plugin(
        &self,
        name: &str,
        version: Option<&str>,
        scope: &str,
    ) -> Result<serde_json::Value, String> {
        let tier_root = self.plugin_tier_root(scope)?;
        let cache = plugin::cache::PluginCache::new(tier_root.join("cache"));
        let commands = plugin::cli::PluginCommands::new(cache, None);
        let result = commands
            .uninstall(name, version)
            .await
            .map_err(|e| e.to_string())?;
        self.refresh_plugins().await;
        Ok(serde_json::json!({
            "success": result.success,
            "message": result.message,
        }))
    }

    /// Enable or disable a plugin by name in `scope`'s tier (see
    /// `plugin::state`). Refreshes the active plugin/command set — see
    /// `install_plugin`.
    pub async fn set_plugin_enabled(
        &self,
        name: &str,
        enabled: bool,
        scope: &str,
    ) -> Result<serde_json::Value, String> {
        let tier_root = self.plugin_tier_root(scope)?;
        let state = plugin::state::EnableState::new(tier_root);
        state
            .set_enabled(name, enabled)
            .map_err(|e| e.to_string())?;
        self.refresh_plugins().await;
        Ok(serde_json::json!({"name": name, "enabled": enabled, "scope": scope}))
    }

    /// Re-discover plugins from disk and rebuild the shared command
    /// catalog — see the `plugins`/`commands` field doc comments for the
    /// hot-swap semantics (new sessions only).
    async fn refresh_plugins(&self) {
        let all = discover_all_plugins(self.paths.as_ref());
        let plugins = Arc::new(active_plugins(self.paths.as_ref(), &all));
        let settings = self.settings.read().await.clone();
        let commands = build_shared_commands(&settings, &plugins);
        *self.plugins.write().await = plugins;
        *self.commands.write().await = commands;
    }

    /// Current MCP connection status — see `mcp::manager::McpManager::server_statuses`.
    pub async fn mcp_status(&self) -> Vec<serde_json::Value> {
        self.mcp
            .read()
            .await
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
    /// Once connection attempts finish, the pool's shared manager is
    /// replaced — new sessions created after that point see the connected
    /// servers; sessions already running are unaffected until they're
    /// recreated.
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
            let manager = McpManager::connect_all(parsed).await;
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
            *pool.mcp.write().await = Arc::new(manager);
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
    /// `config_generation` (`create()` sources MCP from this same
    /// `self.mcp` cache, not from `Settings` directly) — only `mcp.addServer`
    /// or a full daemon restart actually connected it.
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
    /// Returns `(connected, failed)` server names for `ReloadReport`.
    async fn reconcile_mcp_servers(
        &self,
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

        *self.mcp.write().await = Arc::new(manager);
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

        let mut manager = self.mcp.write().await;
        let mut updated = McpManager::from_clients(manager.clients().to_vec());
        updated.refresh_tools().await;
        updated.add_server(name, &cfg).await;
        let statuses = updated.server_statuses();
        *manager = Arc::new(updated);
        drop(manager);

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
    /// detection, no LLM turn needed. Same underlying primitive the manual
    /// `/import` slash command (`ImportTool`) uses.
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
        let kind = base::frozen::ImportSourceKind::from_str(source).ok_or_else(|| {
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

        let (mcp_connected, mcp_failed) = self.reconcile_mcp_servers(&reloaded.mcp_servers).await;
        report.mcp_connected = mcp_connected;
        report.mcp_failed = mcp_failed;

        let reloaded = Arc::new(reloaded);
        *self.settings.write().await = reloaded.clone();
        self.config_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        (reloaded, report)
    }

    /// 创建新 session 并启动 Agent 后台 run loop。
    /// `options` 仅在新创建 session 时生效；已有 session 时忽略。
    async fn create(
        &self,
        session_id: String,
        scene: Arc<dyn AgentScene>,
        options: Option<&SessionOptions>,
    ) -> Result<String, String> {
        let mut config = self.engine_config.clone();
        config.permission_mode = base::permission::PermissionMode::BypassPermissions;

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

        let permission = resolve_session_permission(&self.permission, &self.tools, options);

        let mut builder = Builder::new()
            .scene(scene)
            .model(model)
            .tools(self.tools.clone())
            .settings(self.settings.read().await.clone())
            .permission(permission)
            .memory_store(self.memory_store.clone())
            .session_id(session_id.clone())
            .plugins(self.plugins.read().await.clone())
            .commands_override(self.commands.read().await.clone());
        if let Some(ref store) = self.history_store {
            builder = builder.history_store(store.clone());
        }
        if let Some(router) = self.task_router.read().await.clone() {
            builder = builder.task_router(router);
        }

        // Give this session its own owned `McpManager` built from the
        // centrally-connected client handles (cheap `Arc` clones — not a
        // reconnect) — see `SessionPool.mcp`'s doc comment for why this
        // isn't shared as a single `Arc<McpManager>` directly (`McpManager`
        // needs `&mut self` for `refresh_tools()`, which a shared `Agent`
        // field can't give it). Skipped entirely when nothing's connected,
        // so the common "no MCP servers configured" case pays zero cost.
        {
            let central = self.mcp.read().await;
            if central.server_count() > 0 {
                let mut per_session = McpManager::from_clients(central.clients().to_vec());
                per_session.refresh_tools().await;
                builder = builder.mcp_manager(per_session);
            }
        }

        // Apply telemetry file recorder if configured
        if let Some(telemetry_path) = options
            .and_then(|o| o.telemetry.as_ref())
            .map(|t| t.output.clone())
        {
            if let Ok(rec) = FileRecorder::new(&telemetry_path) {
                let rec = std::sync::Arc::new(rec);
                let (tx, mut rx) =
                    tokio::sync::mpsc::channel::<telemetry::events::TelemetryEvent>(1024);
                let rec_clone = rec.clone();
                tokio::spawn(async move {
                    use telemetry::TelemetryRecorder;
                    while let Some(event) = rx.recv().await {
                        let _ = rec_clone.record(event);
                    }
                });
                builder = builder.telemetry_handle(telemetry::TelemetryHandle::new(tx));
            }
        }

        let (agent, event_rx, input_tx) =
            builder.build().map_err(|e| format!("build agent: {e}"))?;

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

    /// 执行一个 turn：发送消息 → 流式返回事件 → 返回结果。
    /// session_id 为 None 时自动创建新 session。
    pub async fn run_turn(
        &self,
        session_id: Option<String>,
        message: String,
        turn_id: String,
        writer: Writer,
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
                    .create(sid.clone(), self.scene.clone(), options.as_ref())
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
        let (input_tx, event_rx_mutex) = {
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
            (live.input_tx.clone(), live.event_rx.clone())
        };

        // 发送用户消息
        let _ = input_tx.send(InputMessage::User {
            content: message.clone(),
            attachments: vec![],
            turn_id: turn_id.clone(),
        });

        // 取出 event_rx（独占 drain）
        let mut event_rx = match event_rx_mutex.lock().await.take() {
            Some(rx) => rx,
            None => {
                return RpcResponse::err(id, codes::INTERNAL_ERROR, "event channel busy");
            }
        };

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
                    if let Ok(mut b) = serde_json::to_vec(&f) {
                        b.push(b'\n');
                        if writer.lock().await.write_all(&b).await.is_err() {
                            writer_broken = true;
                            break;
                        }
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
                    if let Ok(mut b) = serde_json::to_vec(&f) {
                        b.push(b'\n');
                        if writer.lock().await.write_all(&b).await.is_err() {
                            writer_broken = true;
                            break;
                        }
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
                    if let Ok(mut b) = serde_json::to_vec(&f) {
                        b.push(b'\n');
                        if writer.lock().await.write_all(&b).await.is_err() {
                            writer_broken = true;
                            break;
                        }
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
                    // waiting on `session.respondToPrompt` (or session
                    // cancellation) before the tool call resolves and
                    // `tool_use`/`tool_result` continue as normal. Same
                    // `kind: "prompt"` / `prompt_type` shape documented in
                    // `docs/DAEMON_RPC.md`, deliberately generic so a future
                    // non-permission "stop and ask" need can reuse it
                    // without a new RPC method.
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({
                            "kind":"prompt","prompt_type":"permission","prompt_id":prompt_id,
                            "tool_name":tool_name,"message":message,
                            "paths":paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>()
                        }),
                    );
                    if let Ok(mut b) = serde_json::to_vec(&f) {
                        b.push(b'\n');
                        if writer.lock().await.write_all(&b).await.is_err() {
                            writer_broken = true;
                            break;
                        }
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
                    if let Ok(mut b) = serde_json::to_vec(&f) {
                        b.push(b'\n');
                        let _ = writer.lock().await.write_all(&b).await;
                    }
                    break;
                }
                Some(AgentEvent::Error { code, message, .. }) => {
                    // 归还 event_rx
                    *event_rx_mutex.lock().await = Some(event_rx);
                    return RpcResponse::err(id, codes::ENGINE_ERROR, format!("{code}: {message}"));
                }
                _ => continue,
            }
        }

        // Client disconnected during turn — cancel the session immediately so
        // the Agent stops processing and any child processes (e.g. BashTool)
        // are killed, rather than waiting up to 5 minutes for the janitor.
        if writer_broken {
            drop(event_rx);
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

    /// 列出所有 session（活跃的 + 磁盘上历史的），合并去重。
    pub async fn list_all(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.lock().await;
        let mut out: Vec<SessionInfo> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // 活跃 session
        for (sid, live) in sessions.iter() {
            seen.insert(sid.clone());
            out.push(SessionInfo {
                session_id: sid.clone(),
                name: live.name.clone(),
                preview: None,
                message_count: 0,
                created_at: format_instant(live.created_at),
                last_active: format_instant(live.last_active),
                status: SessionStatus::Active,
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
                    out.push(SessionInfo {
                        session_id: sid_str,
                        name: None,
                        preview: None,
                        message_count: 0,
                        created_at: String::new(),
                        last_active: String::new(),
                        status: SessionStatus::Inactive,
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

    /// 关闭指定 session。
    pub async fn shutdown_session(&self, session_id: &str) {
        let mut sessions = self.sessions.lock().await;
        if let Some(live) = sessions.remove(session_id) {
            live.cancel.cancel();
            info!(%session_id, "session removed");
        }
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
        let _ = input_tx.send(InputMessage::PermissionResponse {
            prompt_id,
            decision,
        });
        Ok(())
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
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(300)).await; // 每 5 分钟
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
    /// so it's propagated to the caller instead of panicking (see
    /// `docs/design/2026-08-04-...` follow-up: a `create()` failure here is
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

        if has_history {
            match self.create(sid.clone(), self.scene.clone(), options).await {
                Ok(s) => Ok(s),
                Err(e) => {
                    warn!(%sid, error=%e, "resume failed, creating new");
                    let new_sid = Id::new().to_string();
                    self.create(new_sid.clone(), self.scene.clone(), options)
                        .await
                        .map_err(|e| format!("resume {sid} failed, retry also failed: {e}"))
                }
            }
        } else {
            match self.create(sid.clone(), self.scene.clone(), options).await {
                Ok(s) => Ok(s),
                Err(e) => {
                    warn!(sid, error=%e, "create with given sid failed");
                    let new_sid = Id::new().to_string();
                    self.create(new_sid.clone(), self.scene.clone(), options)
                        .await
                        .map_err(|e| format!("create {sid} failed, retry also failed: {e}"))
                }
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

/// Which `Permission` a new session gets, per `SessionOptions.permission_mode`.
///
/// `None` (the default — no `permission_mode` in `session.run_turn`'s
/// `options`) returns the exact same `Arc` as `default` — not just an
/// equivalent `AllowAllPermission`, the literal same shared instance — so
/// existing callers that don't know about `session.respondToPrompt` see
/// zero behavior change. `Some(mode)` builds a fresh, session-owned
/// `RuleSetPermission` from `options.permission_rules` instead — opt-in,
/// per session, not a pool-wide switch. Free function (not inlined into
/// `create()`) specifically so this decision is unit-testable without
/// spinning up a real session.
fn resolve_session_permission(
    default: &Arc<dyn Permission>,
    tools: &Arc<base::tool::InMemoryToolRegistry>,
    options: Option<&SessionOptions>,
) -> Arc<dyn Permission> {
    match options.and_then(|o| o.permission_mode) {
        Some(mode) => Arc::new(permissions::rule_set_permission::RuleSetPermission::new(
            Arc::new(permissions::gate::PermissionGate::new(
                permissions::ruleset::RuleSet::new(
                    options.map(|o| o.permission_rules.clone()).unwrap_or_default(),
                ),
            )),
            tools.clone(),
            mode.into(),
        )),
        None => default.clone(),
    }
}

/// Redact a secret to `***<last 4 chars>` (char-boundary safe — slices by
/// `char`, not byte, so this never panics on non-ASCII input); too-short
/// secrets (≤4 chars) redact to a bare `***` rather than revealing the
/// whole thing.
fn redact_secret(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 4 {
        return "***".to_string();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("***{tail}")
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

    #[test]
    fn no_permission_mode_reuses_the_exact_default_instance() {
        let default: Arc<dyn Permission> = Arc::new(AllowAllTestPermission);
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        let resolved = resolve_session_permission(&default, &tools, None);
        assert!(
            Arc::ptr_eq(&default, &resolved),
            "must be the literal same shared instance, not just an equivalent one"
        );

        let opts = SessionOptions::default();
        let resolved2 = resolve_session_permission(&default, &tools, Some(&opts));
        assert!(Arc::ptr_eq(&default, &resolved2));
    }

    #[test]
    fn permission_mode_set_builds_a_fresh_rule_set_permission() {
        let default: Arc<dyn Permission> = Arc::new(AllowAllTestPermission);
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        let opts = SessionOptions {
            permission_mode: Some(base::interface::settings::PermissionMode::Default),
            ..Default::default()
        };
        let resolved = resolve_session_permission(&default, &tools, Some(&opts));
        assert!(
            !Arc::ptr_eq(&default, &resolved),
            "opting in must construct a session-owned instance, not reuse the default"
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
