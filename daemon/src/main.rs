//! `attacored` — daemon binary.
//!
//! Builds a multi-session agent engine pool at startup and exposes it via
//! JSON-RPC 2.0 over Unix socket (or TCP with token auth).
//!
//! # Startup profiling
//!
//! A [`PerfCollector`] tracks elapsed time at each startup phase. After all
//! phases complete, a [`StartupTimingPayload`] telemetry event is emitted
//! (if telemetry is enabled).

use std::path::PathBuf;
use std::sync::Arc;

use base::context::EngineConfig;
use base::interface::permission::PermissionOutcome;
use clap::Parser;
use daemon::{config::*, write_lock_file, DaemonServer, SessionPool};
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};
use telemetry::events::StartupTimingPayload;
use telemetry::perf::PerfCollector;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "AttaCore daemon: multi-session agent engine over JSON-RPC"
)]
struct Cli {
    /// Unix socket path (default: $HOME/.atta/<scene>/daemon.sock)
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Maximum concurrent sessions
    #[arg(long, default_value = "32")]
    session_cap: usize,

    /// Session idle timeout in seconds (default: 3600 = 1 hour)
    #[arg(long, default_value = "3600")]
    session_idle_timeout: u64,

    /// Model name for sessions
    #[arg(long, default_value = "claude-sonnet-4-6")]
    model: String,

    /// Max tokens per turn
    #[arg(long, default_value = "2000")]
    max_tokens: u32,

    /// Bind a TCP listener at this addr (e.g. 127.0.0.1:7878).
    /// Requires --token or ATTACORE_DAEMON_TOKEN env.
    #[arg(long)]
    listen: Option<String>,

    /// Shared secret for TCP auth. Falls back to `ATTACORE_DAEMON_TOKEN`.
    #[arg(long)]
    token: Option<String>,

    /// Which scene this daemon serves (must be one the code actually
    /// registers — see `resolve_scene()`). Determines both session behavior
    /// (system prompt/tools) and the user-level state root (`~/.atta/<scene>/`).
    /// There is no default scope concept separate from this — an unsupported
    /// value fails startup outright rather than silently falling back to
    /// anything.
    #[arg(long, default_value = "coding")]
    scene: String,
}

/// The closed set of scenes this daemon knows how to construct. Keep in sync
/// with `crates/scene/src/scene/*.rs` and the sandbox's per-scene settings.json
/// protection list (`crates/tools/src/bash/sandbox.rs::KNOWN_SCENES`).
fn resolve_scene(name: &str) -> anyhow::Result<Arc<dyn base::interface::scene::AgentScene>> {
    match name {
        "coding" => Ok(Arc::new(scene::scene::coding::CodingScene)),
        "chat" => Ok(Arc::new(scene::scene::chat::ChatScene)),
        "demo" => Ok(Arc::new(scene::scene::demo::DemoScene)),
        "research" => Ok(Arc::new(scene::scene::research::ResearchScene)),
        other => {
            anyhow::bail!(
                "unsupported --scene `{other}` — supported scenes: coding, chat, demo, research"
            )
        }
    }
}

/// Always-allow permission for daemon sessions (IDE plugins manage their own sandbox).
struct AllowAllPermission;

#[async_trait::async_trait]
impl base::interface::permission::Permission for AllowAllPermission {
    async fn check(
        &self,
        _tool: &str,
        _input: &serde_json::Value,
        _cwd: &std::path::Path,
        _session_id: &str,
    ) -> PermissionOutcome {
        PermissionOutcome::Permit
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── Startup profiler ────────────────────────────────────────────────────
    let perf = PerfCollector::new();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // ── Resolve scene (fails fast on an unsupported value) ──────────────
    let scene = resolve_scene(&cli.scene)?;
    let scope = scene.id().to_string();

    // ── Resolve paths + load settings ────────────────────────────────────
    // `load_daemon_config` delegates all settings.json parsing/merging to
    // `Settings::load()` — the single canonical loader (global → scene →
    // project). See `daemon::config` module docs for why this replaced a
    // separate hand-rolled `Settings{}` literal that used to live in this
    // function (that duplication is exactly how `permission_rules` ended up
    // parsed nowhere despite being a real `Settings` field).
    let paths = DefaultDaemonPaths::from_env(&scope);
    let mut daemon_config = load_daemon_config(
        &cli.model,
        cli.max_tokens,
        cli.socket.as_deref(),
        &scope,
        &paths,
    );
    daemon_config.session_cap = cli.session_cap;
    daemon_config.session_idle_timeout_secs = cli.session_idle_timeout;
    daemon_config.settings.session_dir = Some(daemon_config.settings.paths.local_data_dir.clone());

    // Multi-provider LLM routing config is purely additive: if `providers`
    // isn't configured, `task_router` stays `None` and every session behaves
    // exactly as before multi-provider routing existed (sub-agent spawns
    // inherit the parent's model, which is the single env-var-based
    // Anthropic client built below). When it *is* configured: resolve +
    // validate `task_models` against it, then build one `Arc<dyn Model>`
    // instance per provider (`daemon::model_router::build_task_router`) so
    // `SessionPool`/`AgentTool`'s sub-agent spawn points can actually route
    // through it (see `crates/runtime/src/agent_tool.rs::model_for_subagent`)
    // instead of just logging what *would* happen — see
    // `docs/design/2026-08-04-multi-provider-llm-migration.md` §5/§6 phase 3.
    let task_router: Option<Arc<base::provider::TaskRouter>> =
        if daemon_config.settings.providers.is_empty() {
            None
        } else {
            match base::provider::resolve_task_models(
                &daemon_config.settings.providers,
                daemon_config.settings.default_provider.as_deref(),
                &daemon_config.settings.task_models,
            ) {
                Ok((resolved, warnings)) => {
                    for w in &warnings {
                        tracing::warn!("model routing: {w}");
                    }
                    for (task, r) in &resolved {
                        tracing::info!(
                            task = %task,
                            provider = %r.provider_id,
                            model = %r.model,
                            "model routing resolved"
                        );
                    }
                    let default_provider = daemon_config
                        .settings
                        .default_provider
                        .as_deref()
                        .expect("resolve_task_models already validated default_provider is set");
                    match daemon::model_router::build_task_router(
                        &daemon_config.settings.providers,
                        default_provider,
                        resolved,
                    ) {
                        Ok(router) => Some(Arc::new(router)),
                        Err(e) => anyhow::bail!("failed to build multi-provider model router: {e}"),
                    }
                }
                Err(e) => {
                    anyhow::bail!("invalid multi-provider LLM config: {e}");
                }
            }
        };

    // TCP listener config
    if let Some(ref addr_str) = cli.listen {
        let addr: std::net::SocketAddr = addr_str
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid --listen `{addr_str}`: {e}"))?;
        let token = cli
            .token
            .clone()
            .or_else(|| std::env::var("ATTACORE_DAEMON_TOKEN").ok())
            .ok_or_else(|| {
                anyhow::anyhow!("--listen requires --token or env ATTACORE_DAEMON_TOKEN")
            })?;
        daemon_config.tcp_addr = Some(addr);
        daemon_config.tcp_token = Some(token);
    }

    let socket = daemon_config.socket_path.clone();
    let lock_path = daemon_config.lock_path.clone();
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Write discovery lock
    match write_lock_file(&lock_path, &socket) {
        Ok(lock) => info!(pid = lock.pid, "wrote discovery lock"),
        Err(e) => anyhow::bail!("could not claim daemon lock: {e}"),
    }

    // ── Auth ───────────────────────────────────────────────────────────
    let api_key = std::env::var("ANTHROPIC_AUTH_TOKEN")
        .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        .map_err(|_| anyhow::anyhow!("set ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY"))?;
    let auth = AuthMode::ApiKey(api_key);
    let client: Arc<dyn AnthropicClient> = match std::env::var("ANTHROPIC_BASE_URL").ok() {
        Some(mut url) => {
            // Ensure trailing slash so Url::join appends instead of replacing
            if !url.ends_with('/') {
                url.push('/');
            }
            let base = reqwest::Url::parse(&url)
                .map_err(|e| anyhow::anyhow!("invalid ANTHROPIC_BASE_URL: {e}"))?;
            Arc::new(HttpAnthropicClient::with_base(auth, base)?)
        }
        None => Arc::new(HttpAnthropicClient::new(auth)?),
    };

    // `settings.paths` was already resolved correctly by `Settings::load()`
    // (called from `load_daemon_config`) from `paths.{global_root,
    // config_root, project_root}` — no separate `ConfigPaths::from_env` call
    // needed here anymore. NOTE on `mcp_servers`: `Settings.mcp_servers` is
    // now populated directly by `Settings::load()`'s generic JSON merge (no
    // hand-written conversion needed) — but connecting to these servers is
    // still a separate, larger feature (`Builder::build()` only wires MCP
    // when the caller explicitly constructs an `McpManager` and passes it
    // via `.mcp_manager(...)`; `SessionPool::create()` doesn't do that
    // today), not done as part of this fix.
    let global_dir = daemon_config.settings.paths.global_data_dir.clone();
    let local_dir = daemon_config.settings.paths.local_data_dir.clone();
    let settings = Arc::new(daemon_config.settings.clone());

    // ── Startup checkpoint: config_loaded ──────────────────────────────
    let _config_load_ms = perf.checkpoint("config_loaded");
    info!(elapsed_ms = _config_load_ms, "startup: config loaded");

    // memory 不分 scene，只分全局/项目——见 base::paths 模块文档。
    let memory_store = Arc::new(base::interface::memory::MemoryStore::new(
        global_dir.join("memory"),
        local_dir.join("memory"),
    ));

    // ── Startup checkpoint: memory_loaded ──────────────────────────────
    let _memory_load_ms = perf.checkpoint("memory_loaded");
    info!(
        elapsed_ms = _memory_load_ms,
        "startup: memory store initialised"
    );

    // ── Engine config ──────────────────────────────────────────────────
    let mut engine_config = EngineConfig::defaults_for(&daemon_config.settings.model.model_name);
    engine_config.max_tokens = daemon_config.settings.model.max_tokens;
    engine_config.permission_mode = base::permission::PermissionMode::BypassPermissions;

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mcp_servers = daemon_config.settings.mcp_servers.clone();

    let permission: Arc<dyn base::interface::permission::Permission> = Arc::new(AllowAllPermission);

    // ── Startup checkpoint: skills_scanned ─────────────────────────────
    // (No skills scanning currently performed in the daemon — placeholder
    //  checkpoint for future integration.)
    let _skills_scan_ms = perf.checkpoint("skills_scanned");
    info!(
        elapsed_ms = _skills_scan_ms,
        "startup: skills scanned (noop)"
    );

    // ── Startup checkpoint: mcp_connected ──────────────────────────────
    // MCP servers connect in the background *after* the pool is built and
    // serving (see below, `pool.connect_mcp_servers_in_background(...)`) —
    // this checkpoint just marks "we decided how many servers there are to
    // connect", not "connection finished"; connecting is intentionally
    // async so a slow/unreachable server can never delay startup.
    let _mcp_connect_ms = perf.checkpoint("mcp_connected");
    info!(
        elapsed_ms = _mcp_connect_ms,
        n_configured = mcp_servers.len(),
        "startup: mcp connect kicked off in background"
    );

    // ── Startup checkpoint: tools_registered ───────────────────────────
    // Real registration happens just below, inside `SessionPool::new()`
    // (`tools::register_builtin_tools`), whose result every session's
    // `Builder` shares via `.tools(self.tools.clone())`. This checkpoint
    // used to be a placeholder marking a step that didn't exist yet — that
    // was a real bug (no session ever had Bash/Read/Write/... registered,
    // only the handful `Builder::build()` wires internally), now fixed; the
    // marker stays purely for startup-timing telemetry.
    let _tools_reg_ms = perf.checkpoint("tools_registered");
    info!(elapsed_ms = _tools_reg_ms, "startup: tools registered");

    // ── Session transcript persistence ──────────────────────────────────
    // `sessions/` follows the same "global + project, no scene" rule as
    // memory/vcr/mcp (see `base::paths` module docs) — `JsonlHistoryStore`
    // itself partitions by project under one shared root, so `global_dir`
    // (not a scene-specific dir) is the right root here.
    let history_store: Option<Arc<dyn history::store::HistoryStore>> =
        match history::store::JsonlHistoryStore::with_root(&cwd, global_dir.join("sessions")).await
        {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                tracing::warn!(error = %e, "failed to initialize session history store; sessions will be in-memory only for this run");
                None
            }
        };

    // ── Build SessionPool ──────────────────────────────────────────────
    let pool = Arc::new(SessionPool::new(
        daemon_config.session_cap,
        daemon_config.session_idle_timeout_secs,
        client,
        settings,
        scene,
        permission,
        memory_store,
        cwd,
        engine_config,
        history_store,
        daemon_config.paths.clone(),
        task_router,
    ));

    // Connect configured MCP servers in the background — never blocks
    // startup; failures warn + emit a `mcp_connect_failed` `daemon.event`
    // (subscribe via `daemon.subscribeEvents`) rather than aborting.
    // Plugin-declared MCP servers (`plugin.toml`'s `[mcp] servers`) are
    // merged in here so they go through the exact same centrally-connected/
    // shared-across-sessions path as user-configured ones — see
    // `SessionPool::plugin_mcp_servers`.
    let mut mcp_servers = mcp_servers;
    mcp_servers.extend(pool.plugin_mcp_servers().await);
    pool.connect_mcp_servers_in_background(mcp_servers);

    // ── Cross-tool config import (process-level, once) ─────────────────
    // `daemon` is headless — no client is connected yet at this point in
    // startup, so there's nobody to synchronously ask "import from X?" the
    // way `ImportCallback` expects. Rather than wire a callback with no
    // sensible synchronous answer, detect directly and — if anything's
    // found — emit an `import_detected` `daemon.event` notification instead
    // of blocking for a decision; a subscribed client can act on it via the
    // `import.list`/`import.run` RPCs whenever it's ready, no LLM turn
    // required. Respects the same `.imported.json` marker the automatic
    // path always has, so this doesn't nag on every restart. The manual
    // `/import` slash command (`ImportTool`) remains available regardless
    // and always re-detects. See docs/design/2026-08-03-agents-config-migration.md §3.3/§3.7.
    {
        let pool = pool.clone();
        let import_cwd = pool.cwd().to_path_buf();
        tokio::spawn(async move {
            if base::frozen::import_already_decided(&import_cwd).await {
                return;
            }
            let sources = base::frozen::detect_import_sources(&import_cwd).await;
            if sources.is_empty() {
                return;
            }
            pool.emit_event(serde_json::json!({
                "kind": "import_detected",
                "sources": sources.iter().map(|s| serde_json::json!({
                    "source": s.kind().as_str(),
                    "description": s.describe(),
                })).collect::<Vec<_>>(),
            }));
        });
    }

    // 启动后台回收
    pool.start_janitor();

    // ── Startup checkpoint: pool_ready ─────────────────────────────────
    let _pool_ready_ms = perf.checkpoint("pool_ready");
    info!(elapsed_ms = _pool_ready_ms, "startup: session pool ready");

    // ── Emit StartupTimingPayload ──────────────────────────────────────
    let total_startup_ms = perf.elapsed_ms() as u64;
    let checkpoints: std::collections::HashMap<String, u64> = perf
        .checkpoint_snapshot()
        .into_iter()
        .map(|(k, v)| (k, v as u64))
        .collect();

    let timing = StartupTimingPayload {
        total_startup_ms,
        config_load_ms: checkpoints.get("config_loaded").copied().unwrap_or(0),
        tool_registration_ms: checkpoints.get("tools_registered").copied().unwrap_or(0),
        mcp_connect_ms: checkpoints.get("mcp_connected").copied().unwrap_or(0),
        plugin_load_ms: 0,
        skill_load_ms: checkpoints.get("skills_scanned").copied().unwrap_or(0),
        history_resume_ms: 0,
        first_api_call_ms: 0,
    };

    info!(total_ms = total_startup_ms, ?timing, "startup complete");

    // If telemetry is available, emit the startup timing event.
    // (The daemon currently does not wire up the full telemetry pipeline
    //  at startup, but this demonstrates the pattern.)

    // ── Start server ───────────────────────────────────────────────────
    let cancel = CancellationToken::new();

    // Ctrl-C graceful shutdown
    {
        let cancel = cancel.clone();
        let pool = pool.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            info!("ctrl-c received; shutting down");
            pool.shutdown_all().await;
            cancel.cancel();
        });
    }

    let server = Arc::new(DaemonServer::new(pool, cancel.clone()));

    // TCP listener (optional)
    if let (Some(addr), Some(token)) = (daemon_config.tcp_addr, daemon_config.tcp_token.clone()) {
        server.set_tcp_token(token).await;
        let s = server.clone();
        info!(%addr, "TCP listener bound (token-auth required)");
        tokio::spawn(async move { s.serve_tcp(addr).await });
    }

    server.serve_unix(&socket).await?;

    let _ = std::fs::remove_file(&lock_path);
    info!("daemon stopped");
    Ok(())
}
