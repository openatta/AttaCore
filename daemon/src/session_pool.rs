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
    let _ = skill_mgr.load_dir(
        &settings.paths.global_data_dir.join("skills"),
        skills::manager::SkillSource::User,
    );
    let _ = skill_mgr.load_dir(
        &settings.paths.user_data_dir.join("skills"),
        skills::manager::SkillSource::User,
    );
    let _ = skill_mgr.load_dir(&project_skills_dir, skills::manager::SkillSource::Project);
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
    ) -> Self {
        let model = Arc::new(AnthropicModel::new(client.clone()));
        let (events_tx, _) = tokio::sync::broadcast::channel(256);

        let all_plugins = discover_all_plugins(paths.as_ref());
        let plugins = Arc::new(active_plugins(paths.as_ref(), &all_plugins));
        let commands = build_shared_commands(&settings, &plugins);

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
            cwd,
            engine_config,
            history_store,
            paths,
            mcp: AsyncRwLock::new(Arc::new(McpManager::empty())),
            events_tx,
            plugins: AsyncRwLock::new(plugins),
            commands: AsyncRwLock::new(commands),
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

        // Reload the fully merged effective settings so the response (and
        // any new session) reflects global/scene tiers too, not just what we
        // just wrote at the project tier.
        let default_model = self.settings.read().await.model.model_name.clone();
        let reloaded = Settings::load(
            self.paths.global_root(),
            self.paths.config_root(),
            self.paths.project_root().join(".atta"),
            self.scene.id(),
            &default_model,
        );

        let (routing_ok, warnings, error) = if reloaded.providers.is_empty() {
            (true, Vec::new(), None)
        } else {
            match base::provider::resolve_task_models(
                &reloaded.providers,
                reloaded.default_provider.as_deref(),
                &reloaded.task_models,
            ) {
                Ok((_, w)) => (true, w, None),
                Err(e) => (false, Vec::new(), Some(e)),
            }
        };

        let result = serde_json::json!({
            "written_to": settings_path.display().to_string(),
            "providers": reloaded.providers.keys().collect::<Vec<_>>(),
            "default_provider": reloaded.default_provider,
            "task_models": reloaded.task_models.keys().collect::<Vec<_>>(),
            "routing": { "ok": routing_ok, "warnings": warnings, "error": error },
        });

        *self.settings.write().await = Arc::new(reloaded);

        Ok(result)
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
                            fallback_on_miss: true,
                        }),
                        std::path::PathBuf::from("/tmp/atta_vcr_nonexistent"),
                        std::path::PathBuf::from(&vcr.dir),
                    ))
                }
                None => self.model.clone(),
            };

        let mut builder = Builder::new()
            .scene(scene)
            .model(model)
            .settings(self.settings.read().await.clone())
            .permission(self.permission.clone())
            .memory_store(self.memory_store.clone())
            .session_id(session_id.clone())
            .plugins(self.plugins.read().await.clone())
            .commands_override(self.commands.read().await.clone());
        if let Some(ref store) = self.history_store {
            builder = builder.history_store(store.clone());
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

        // 容量检查：驱逐最久未活跃的 session
        if sessions.len() >= self.cap {
            self.evict_lru(&mut sessions).await;
        }

        let live = LiveSession {
            input_tx,
            event_rx: Arc::new(AsyncMutex::new(Some(event_rx))),
            cancel,
            name: None,
            created_at: Instant::now(),
            last_active: Instant::now(),
            is_first_turn: true,
        };
        sessions.insert(session_id.clone(), live);
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
                let sessions = self.sessions.lock().await;
                if sessions.contains_key(sid) {
                    sid.clone()
                } else {
                    drop(sessions);
                    self.resume_or_create(sid.clone(), options.as_ref()).await
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
                let timeout = pool.idle_timeout;
                let now = Instant::now();
                let to_evict: Vec<String> = sessions
                    .iter()
                    .filter(|(_, live)| now.duration_since(live.last_active) > timeout)
                    .map(|(sid, _)| sid.clone())
                    .collect();
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
    async fn resume_or_create(&self, sid: String, options: Option<&SessionOptions>) -> String {
        // 尝试从 HistoryStore 加载历史消息
        let has_history = if let Some(ref store) = self.history_store {
            match store
                .load(base::session::SessionId::parse(&sid).unwrap())
                .await
            {
                Ok(entries) => !entries.is_empty(),
                Err(_) => false,
            }
        } else {
            false
        };

        if has_history {
            match self.create(sid.clone(), self.scene.clone(), options).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(%sid, error=%e, "resume failed, creating new");
                    let new_sid = Id::new().to_string();
                    self.create(new_sid.clone(), self.scene.clone(), options)
                        .await
                        .unwrap_or_else(|e| panic!("create session: {e}"))
                }
            }
        } else {
            match self.create(sid.clone(), self.scene.clone(), options).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(sid, error=%e, "create with given sid failed");
                    let new_sid = Id::new().to_string();
                    self.create(new_sid.clone(), self.scene.clone(), options)
                        .await
                        .unwrap_or_else(|e| panic!("create session: {e}"))
                }
            }
        }
    }

    /// LRU 驱逐：移除最久未活跃的 session。
    async fn evict_lru(&self, sessions: &mut HashMap<String, LiveSession>) {
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

fn format_instant(t: Instant) -> String {
    let ago = Instant::now().duration_since(t);
    let secs_ago = ago.as_secs();
    let now = time::OffsetDateTime::now_utc();
    let abs = now - time::Duration::seconds(secs_ago as i64);
    abs.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
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
