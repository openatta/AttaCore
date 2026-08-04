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
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};
use daemon::{config::*, write_lock_file, DaemonServer, SessionPool};
use clap::Parser;
use telemetry::perf::PerfCollector;
use telemetry::events::StartupTimingPayload;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[derive(Parser, Debug)]
#[command(version, about = "AttaCore daemon: multi-session agent engine over JSON-RPC")]
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
        other => anyhow::bail!(
            "unsupported --scene `{other}` — supported scenes: coding, chat, demo"
        ),
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

    // ── Resolve paths ──────────────────────────────────────────────────
    let paths = DefaultDaemonPaths::from_env(&scope);
    let mut daemon_config =
        load_daemon_config(&cli.model, cli.max_tokens, cli.socket.as_deref(), &paths);
    daemon_config.session_cap = cli.session_cap;
    daemon_config.session_idle_timeout_secs = cli.session_idle_timeout;

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
            if !url.ends_with('/') { url.push('/'); }
            let base = reqwest::Url::parse(&url)
                .map_err(|e| anyhow::anyhow!("invalid ANTHROPIC_BASE_URL: {e}"))?;
            Arc::new(HttpAnthropicClient::with_base(auth, base)?)
        }
        None => Arc::new(HttpAnthropicClient::new(auth)?),
    };

    // ── Build settings ─────────────────────────────────────────────────
    // Single source of truth for user/local roots — replaces what used to be
    // three independent `.atta/code` literal constructions (here, plus
    // `DefaultDaemonPaths::from_env` above, plus the project settings.json
    // lookup in `load_daemon_config`). `local_data_dir` is intentionally flat
    // (no scope segment) — see `base::paths::ConfigPaths` docs.
    let config_paths = base::paths::ConfigPaths::from_env(std::path::Path::new("."), &scope);
    let user_dir = config_paths.user_data_dir.clone();
    let local_dir = config_paths.local_data_dir.clone();

    use base::interface::settings::{
        CompactionConfig, ExecutionSettings, ModelSettings, PathSettings, SandboxConfig,
        Settings, ThinkingMode,
    };

    // Convert the settings.json-parsed server map into `Settings.mcp_servers`
    // (previously silently discarded — hardcoded `Vec::new()` regardless of
    // what settings.json configured). NOTE: this only stops the value from
    // being thrown away; it does not yet make these servers actually connect.
    // `Builder::build()` only wires MCP servers when the caller explicitly
    // constructs an `McpManager` and passes it via `.mcp_manager(...)` —
    // `SessionPool::create()` doesn't do that today, so configured servers
    // are visible in `Settings` but still inert. Actually connecting them is
    // a separate, larger feature (build an `McpManager` from this list at
    // daemon startup), not done as part of this fix.
    let mcp_servers: Vec<serde_json::Value> = daemon_config
        .mcp_servers
        .iter()
        .filter_map(|(name, cfg)| {
            let mut v = serde_json::to_value(cfg).ok()?;
            if let serde_json::Value::Object(ref mut map) = v {
                map.insert("name".to_string(), serde_json::Value::String(name.clone()));
            }
            Some(v)
        })
        .collect();

    let settings = Arc::new(Settings {
        model: ModelSettings {
            api_type: base::provider::ApiType::Anthropic,
            base_url: String::new(),
            auth_token: String::new(),
            model_name: daemon_config.model.clone(),
            max_tokens: daemon_config.max_tokens,
            thinking_mode: ThinkingMode::Auto,
            fallback_model: None,
        },
        paths: PathSettings {
            user_data_dir: user_dir.clone(),
            local_data_dir: local_dir.clone(),
            scope: scope.clone(),
        },
        execution: ExecutionSettings::default(),
        compaction: CompactionConfig::default(),
        sandbox: SandboxConfig::default(),
        instruction_file: None,
        prompt_append: None,
        prompt_override: None,
        vcr: None,
        telemetry_url: None,
        session_dir: Some(local_dir.clone()),
        memory_enabled: true,
        permission_mode: base::interface::settings::PermissionMode::default(),
        permission_rules: Vec::new(),
        hooks_config: None,
        mcp_servers,
        language: None,
        feature_flags: Default::default(),
    });

    // ── Startup checkpoint: config_loaded ──────────────────────────────
    let _config_load_ms = perf.checkpoint("config_loaded");
    info!(elapsed_ms = _config_load_ms, "startup: config loaded");

    let memory_store = Arc::new(base::interface::memory::MemoryStore::new(
        user_dir.join("memory"),
        local_dir.join("memory"),
    ));

    // ── Startup checkpoint: memory_loaded ──────────────────────────────
    let _memory_load_ms = perf.checkpoint("memory_loaded");
    info!(elapsed_ms = _memory_load_ms, "startup: memory store initialised");

    // ── Engine config ──────────────────────────────────────────────────
    let mut engine_config = EngineConfig::defaults_for(&daemon_config.model);
    engine_config.max_tokens = daemon_config.max_tokens;
    engine_config.permission_mode = base::permission::PermissionMode::BypassPermissions;

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // ── Cross-tool config import (process-level, once) ─────────────────
    // `daemon` is headless — no client is connected yet at this point in
    // startup, so there's nobody to synchronously ask "import from X?".
    // v1 scope decision: daemon registers no `ImportCallback` (passes
    // `None`), so this call is effectively a fast no-op today; the manual
    // `/import` slash command (ImportTool) is the real user-facing path.
    // Spawned so a future non-`None` callback here can never block startup.
    // See docs/design/2026-08-03-agents-config-migration.md §3.3/§3.7.
    {
        let import_cwd = cwd.clone();
        tokio::spawn(async move {
            let _ = base::interface::import_callback::maybe_detect_and_import(
                &import_cwd,
                None,
                std::time::Duration::from_secs(30),
            )
            .await;
        });
    }

    let permission: Arc<dyn base::interface::permission::Permission> =
        Arc::new(AllowAllPermission);

    // ── Startup checkpoint: skills_scanned ─────────────────────────────
    // (No skills scanning currently performed in the daemon — placeholder
    //  checkpoint for future integration.)
    let _skills_scan_ms = perf.checkpoint("skills_scanned");
    info!(elapsed_ms = _skills_scan_ms, "startup: skills scanned (noop)");

    // ── Startup checkpoint: mcp_connected ──────────────────────────────
    // (No MCP servers are connected at startup in this minimal daemon —
    //  placeholder checkpoint for future integration.)
    let _mcp_connect_ms = perf.checkpoint("mcp_connected");
    info!(elapsed_ms = _mcp_connect_ms, "startup: mcp connected (noop)");

    // ── Startup checkpoint: tools_registered ───────────────────────────
    // (Tools are registered implicitly via the session engine. Placeholder
    //  checkpoint for explicit registration timing.)
    let _tools_reg_ms = perf.checkpoint("tools_registered");
    info!(elapsed_ms = _tools_reg_ms, "startup: tools registered (noop)");

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
        None, // history_store: None = no persistence (TODO: wire up JSONL)
    ));

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

    info!(
        total_ms = total_startup_ms,
        ?timing,
        "startup complete"
    );

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
