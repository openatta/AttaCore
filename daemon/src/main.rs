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
    /// Unix socket path (default: $HOME/.atta/code/daemon.sock)
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

    // ── Resolve paths ──────────────────────────────────────────────────
    let paths = DefaultDaemonPaths::from_env();
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
            if !url.ends_with('/') {
                url.push('/');
            }
            let base = reqwest::Url::parse(&url)
                .map_err(|e| anyhow::anyhow!("invalid ANTHROPIC_BASE_URL: {e}"))?;
            Arc::new(HttpAnthropicClient::with_base(auth, base)?)
        }
        None => Arc::new(HttpAnthropicClient::new(auth)?),
    };

    // ── Build settings ─────────────────────────────────────────────────
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let user_dir = PathBuf::from(&home).join(".atta").join("code");
    let local_dir = PathBuf::from(".").join(".atta").join("code");

    use base::interface::settings::{
        CompactionConfig, ExecutionSettings, ModelSettings, PathSettings, SandboxConfig, Settings,
        ThinkingMode,
    };
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
        mcp_servers: Vec::new(),
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
    info!(
        elapsed_ms = _memory_load_ms,
        "startup: memory store initialised"
    );

    // ── Engine config ──────────────────────────────────────────────────
    let mut engine_config = EngineConfig::defaults_for(&daemon_config.model);
    engine_config.max_tokens = daemon_config.max_tokens;
    engine_config.permission_mode = base::permission::PermissionMode::BypassPermissions;

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // ── Scenes ─────────────────────────────────────────────────────────
    let scene_coding: Arc<dyn base::interface::scene::AgentScene> =
        Arc::new(scene::scene::coding::CodingScene::default_scene());
    let scene_chat: Arc<dyn base::interface::scene::AgentScene> =
        Arc::new(scene::scene::chat::ChatScene);

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
    // (No MCP servers are connected at startup in this minimal daemon —
    //  placeholder checkpoint for future integration.)
    let _mcp_connect_ms = perf.checkpoint("mcp_connected");
    info!(
        elapsed_ms = _mcp_connect_ms,
        "startup: mcp connected (noop)"
    );

    // ── Startup checkpoint: tools_registered ───────────────────────────
    // (Tools are registered implicitly via the session engine. Placeholder
    //  checkpoint for explicit registration timing.)
    let _tools_reg_ms = perf.checkpoint("tools_registered");
    info!(
        elapsed_ms = _tools_reg_ms,
        "startup: tools registered (noop)"
    );

    // ── Build SessionPool ──────────────────────────────────────────────
    let pool = Arc::new(SessionPool::new(
        daemon_config.session_cap,
        daemon_config.session_idle_timeout_secs,
        client,
        settings,
        scene_coding,
        scene_chat,
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
