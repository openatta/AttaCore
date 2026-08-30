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

    /// Bind a WebSocket listener at this addr (e.g. 127.0.0.1:7879), so a
    /// browser front end can reach the daemon directly. Loopback only, and
    /// requires --token or ATTACORE_DAEMON_TOKEN env.
    #[arg(long)]
    listen_ws: Option<String>,

    /// Shared secret for TCP and WebSocket auth. Falls back to
    /// `ATTACORE_DAEMON_TOKEN`.
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

    /// Additional scenes to activate alongside `--scene`, comma-separated
    /// (e.g. `--scenes chat,research`). `--scene`'s own value doesn't need
    /// repeating here — it's always active. Each must be one
    /// `scene::scene::SceneRegistry::register_builtin` actually registers;
    /// an unknown name fails startup the same way an unsupported `--scene`
    /// does. Session creation still defaults to `--scene`'s scene;
    /// `session.create {"scene": "..."}` is how a caller reaches one of
    /// these instead.
    #[arg(long, value_delimiter = ',')]
    scenes: Vec<String>,

    /// Named identity for this daemon instance — the filename of its
    /// `~/.atta/daemon/instances.d/<instance>.json` discovery entry
    /// (`docs/daemon_rpc_protocol.md` §2). Must be unique among daemons running on
    /// the same machine; two live instances racing to write the same name
    /// isn't reconciled here (each just overwrites the file — see
    /// `discovery::write_instance_file`), so operators are expected to pick
    /// distinct names when running more than one daemon.
    ///
    /// Default (when omitted): every active scene (`--scene` plus
    /// `--scenes`), deduplicated, sorted, and comma-joined — e.g.
    /// `--scene coding --scenes chat,research` defaults to
    /// `chat,coding,research`. See `default_instance_name()`.
    #[arg(long)]
    instance: Option<String>,

    /// Seconds a permission prompt may go unanswered before the daemon
    /// denies it on the client's behalf (0 = wait forever).
    ///
    /// Only reachable now that the default permission mode is `ask`: the
    /// engine itself waits indefinitely for `session.respondToPrompt`, so a
    /// client that never implements it would otherwise hang its
    /// `session.run_turn` call forever. The timeout fails *closed* — it
    /// denies the tool call, never approves it — and the turn then continues
    /// normally with an error `tool_result`.
    #[arg(long, default_value = "300")]
    permission_prompt_timeout: u64,
}

/// The closed set of scenes this daemon knows how to construct. Keep in sync
/// with `crates/scene/src/scene/*.rs` and the sandbox's per-scene settings.json
/// protection list (`crates/tools/src/bash/sandbox.rs::KNOWN_SCENES`).
/// Resolves against `scene::scene::SceneRegistry` (P2: the registry existed
/// with zero production callers before this — every scene was a hardcoded
/// match arm here instead) rather than a hardcoded match, so `--scenes`
/// (plural, P4) can register more than the four built-ins without touching
/// this function again.
fn resolve_scene(name: &str) -> anyhow::Result<Arc<dyn base::interface::scene::AgentScene>> {
    let mut registry = scene::scene::SceneRegistry::new();
    registry.register_builtin();
    registry.resolve(name).ok_or_else(|| {
        let mut known = registry.ids();
        known.sort();
        anyhow::anyhow!(
            "unsupported --scene `{name}` — supported scenes: {}",
            known.join(", ")
        )
    })
}

/// Every scene this daemon activates at startup — `--scene` plus
/// `--scenes`, deduplicated and sorted. Sorting makes this (and anything
/// derived from it, like [`default_instance_name`]) independent of the
/// order `--scenes` happened to list them in.
fn active_scenes(scene: &str, scenes: &[String]) -> Vec<String> {
    let mut all: Vec<String> = std::iter::once(scene.to_string())
        .chain(scenes.iter().cloned())
        .collect();
    all.sort();
    all.dedup();
    all
}

/// Default `--instance` name: the active scene set, sorted and joined with
/// `,` — `docs/daemon_rpc_protocol.md` §2 specifies "排序后连接" for the analogous
/// default socket name without pinning a delimiter; comma matches
/// `--scenes`' own list syntax.
fn default_instance_name(scene: &str, scenes: &[String]) -> String {
    active_scenes(scene, scenes).join(",")
}

/// Allow-everything permission — **the opt-out**, no longer the default.
///
/// The original design comment here read "always-allow permission for daemon
/// sessions (IDE plugins manage their own sandbox)". That was a real
/// decision, not an oversight: the daemon's callers were assumed to be
/// editor plugins that had already sandboxed the workspace themselves, so a
/// second permission layer inside the engine would only have been friction,
/// and nothing on the wire could answer a prompt anyway.
///
/// **That boundary moved on purpose.** Two things changed underneath it.
/// First, the daemon grew a real answer channel — `session.respondToPrompt`
/// plus the `kind:"prompt"` stream frame — so "stop and ask" is now
/// expressible over JSON-RPC instead of being a dead end. Second, the
/// caller population stopped being "editor plugins we shipped": anything
/// that can open the socket gets a fully-tooled agent with Bash and file
/// writes, and "the host sandboxes it" became an assumption the daemon was
/// making on the host's behalf rather than one the host had stated.
///
/// So the default is now `PermissionMode::Default` (ask), and a host that
/// genuinely does sandbox the daemon states so explicitly — either
/// `"permission_mode": "bypassPermissions"` in any settings.json tier
/// (daemon-wide) or `options.permission_mode: "bypassPermissions"` on
/// `session.run_turn` (one session). Both resolve to *this* instance; see
/// `daemon::session_pool::resolve_session_permission` and
/// `docs/daemon_rpc_protocol.md`'s "权限默认值" section.
///
/// Kept as a distinct type rather than folded into `RuleSetPermission`'s
/// bypass mode because the two aren't quite the same: `PermissionGate` keeps
/// a small bypass-immune deny list (`is_path_bypass_immune`) that still
/// fires under `bypassPermissions`, whereas this really does permit
/// everything — which is what "I have my own sandbox" asks for.
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
    // instead of just logging what *would* happen.
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
                    let default_provider =
                        daemon_config.settings.default_provider.as_deref().expect(
                            "resolve_task_models already validated default_provider is set",
                        );
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

    if let Some(ref addr_str) = cli.listen_ws {
        let addr: std::net::SocketAddr = addr_str
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid --listen-ws `{addr_str}`: {e}"))?;
        // Refused rather than warned about: this transport is what a browser
        // connects to, and a browser reaching it from another machine would
        // mean the token is the only thing between the daemon and the network.
        if !addr.ip().is_loopback() {
            anyhow::bail!(
                "--listen-ws must bind a loopback address; `{addr}` is reachable from the network"
            );
        }
        let token = cli
            .token
            .clone()
            .or_else(|| std::env::var("ATTACORE_DAEMON_TOKEN").ok())
            .or_else(|| daemon_config.tcp_token.clone())
            .ok_or_else(|| {
                anyhow::anyhow!("--listen-ws requires --token or env ATTACORE_DAEMON_TOKEN")
            })?;
        daemon_config.ws_addr = Some(addr);
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

    // Write the multi-scene instances.d discovery entry alongside the
    // legacy single-scene `daemon.lock` above — additive, see
    // `daemon::discovery`'s module docs. Lives under the *global* root
    // (shared by every scene), not `config_root` (scene-specific), since
    // one instance file must be reachable regardless of which of this
    // daemon's scenes a client cares about.
    let instance_scenes = active_scenes(&cli.scene, &cli.scenes);
    let instance_name = cli
        .instance
        .clone()
        .unwrap_or_else(|| default_instance_name(&cli.scene, &cli.scenes));
    let instances_dir = daemon_config
        .paths
        .global_root()
        .join("daemon")
        .join("instances.d");
    let identity = base::process_identity::ProcessIdentity::current();
    let instance_file = daemon::discovery::InstanceFile {
        instance: instance_name.clone(),
        pid: identity.pid,
        pid_start_time: identity.started_at,
        socket: socket.clone(),
        tcp: daemon_config.tcp_addr.map(|a| a.to_string()),
        ws: daemon_config.ws_addr.map(|a| a.to_string()),
        scenes: instance_scenes,
        protocol_version: daemon::discovery::INSTANCE_PROTOCOL_VERSION,
        started_at: rfc3339_now(),
    };
    match daemon::discovery::write_instance_file(&instances_dir, &instance_name, &instance_file) {
        Ok(()) => info!(instance = %instance_name, "wrote instances.d discovery entry"),
        Err(e) => tracing::warn!(
            instance = %instance_name,
            error = %e,
            "failed to write instances.d discovery entry; multi-scene discovery clients won't see this daemon"
        ),
    }

    // ── Auth ───────────────────────────────────────────────────────────
    // Through the contract rather than reading the environment inline, so
    // this and the per-provider clients built by `model_router` get their
    // credentials the same way — a host that swaps the source swaps both.
    let credentials = base::interface::credentials::EnvCredentials::anthropic();
    let api_key = base::interface::credentials::CredentialSource::api_key(
        &credentials,
        "default",
        &base::provider::ProviderConfig::default(),
    )
    .map_err(|_| anyhow::anyhow!("set ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY"))?;
    let auth = AuthMode::ApiKey(api_key.expose().to_string());
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

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mcp_servers = daemon_config.settings.mcp_servers.clone();

    // Not the default any more — see `AllowAllPermission`'s doc comment.
    // The pool holds this instance and hands it out only to sessions whose
    // effective `permission_mode` is `bypassPermissions`; everything else
    // gets a real `RuleSetPermission` built from
    // `settings.permission_mode`/`settings.permission_rules`.
    let permission: Arc<dyn base::interface::permission::Permission> = Arc::new(AllowAllPermission);
    if matches!(
        daemon_config.settings.permission_mode,
        base::interface::settings::PermissionMode::BypassPermissions
    ) {
        tracing::warn!(
            "settings.permission_mode = bypassPermissions — every tool call in every session \
             runs unchecked; this daemon is trusting its host to sandbox it"
        );
    }

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
    // Transcripts are partitioned by project, so they belong under
    // `projects/`, not `sessions/`. Passing `global_dir.join("sessions")`
    // here put `<sanitized-cwd>/` directories in the same root the
    // session-id-keyed sidecars use, which is how `~/.atta/sessions/` ended
    // up holding two different naming schemes side by side.
    //
    // Both roots come from `global_dir`, the one this instance was given —
    // having either side assemble its own root is what let them drift apart
    // in the first place.
    let history_roots = history::path::HistoryRoots::under(&global_dir);

    // Relocate anything still sitting in the pre-0.1.5 layout, before the
    // store is built against the new one — otherwise a returning user's
    // existing sessions are simply not where anything looks for them.
    // Idempotent and move-only; a clean tree is a no-op.
    let migration = history::migrate::migrate_layout(&global_dir);
    if !migration.did_nothing() {
        info!(
            transcripts = migration.transcripts_moved,
            sidecars = migration.sidecars_moved,
            skipped = migration.skipped_existing,
            failed = migration.failed,
            "migrated session state to the projects/ + sessions/ layout"
        );
    }

    let history_store: Option<Arc<dyn history::store::HistoryStore>> =
        match history::store::JsonlHistoryStore::with_roots(&cwd, history_roots).await {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                tracing::warn!(error = %e, "failed to initialize session history store; sessions will be in-memory only for this run");
                None
            }
        };

    // ── Build SessionPool ──────────────────────────────────────────────
    let pool = Arc::new(
        SessionPool::new(
            daemon_config.session_cap,
            daemon_config.session_idle_timeout_secs,
            client,
            settings,
            scene,
            permission,
            memory_store,
            cwd,
            history_store,
            daemon_config.paths.clone(),
            task_router,
        )
        .with_permission_prompt_timeout(std::time::Duration::from_secs(
            cli.permission_prompt_timeout,
        )),
    );

    // `--scenes` activates additional scenes at startup, on top of
    // `--scene`'s (always active). An unknown name fails startup outright —
    // same posture as an unsupported `--scene` — rather than silently
    // starting with fewer scenes than asked for.
    for extra in &cli.scenes {
        if extra == &cli.scene {
            continue; // already active; not an error to repeat it
        }
        pool.activate_scene(extra)
            .await
            .map_err(|(_, message)| anyhow::anyhow!("--scenes: {message}"))?;
    }

    // Connect configured MCP servers in the background — never blocks
    // startup; failures warn + emit a `mcp_connect_failed` `daemon.event`
    // (subscribe via `daemon.subscribeEvents`) rather than aborting.
    // Plugin-declared MCP servers (`plugin.toml`'s `[mcp] servers`) are
    // merged in here so they go through the exact same centrally-connected/
    // shared-across-sessions path as user-configured ones — see
    // `SessionPool::plugin_mcp_servers`.
    // Before anything can create a session: a session built while this is
    // still running would be missing every plugin tool, with nothing to
    // indicate why.
    pool.load_plugin_components().await;

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
    // `/import` slash command (the bundled `import` skill) remains available
    // regardless and always re-detects.
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

    // One token covers both network transports; either flag alone sets it.
    if let Some(token) = daemon_config.tcp_token.clone() {
        server.set_tcp_token(token).await;
    }

    if let Some(addr) = daemon_config.tcp_addr {
        let s = server.clone();
        info!(%addr, "TCP listener bound (token-auth required)");
        tokio::spawn(async move { s.serve_tcp(addr).await });
    }

    // Bound here rather than inside the task so a port already in use fails
    // startup, instead of leaving a daemon running without the transport the
    // user asked for.
    if let Some(addr) = daemon_config.ws_addr {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| anyhow::anyhow!("could not bind --listen-ws `{addr}`: {e}"))?;
        info!(%addr, "WebSocket listener bound (token-auth required)");
        let s = server.clone();
        tokio::spawn(async move { daemon::ws::serve_ws_listener(s, listener).await });
    }

    server.serve_unix(&socket).await?;

    let _ = std::fs::remove_file(&lock_path);
    daemon::discovery::remove_instance_file(&instances_dir, &instance_name);
    info!("daemon stopped");
    Ok(())
}

fn rfc3339_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    time::OffsetDateTime::from_unix_timestamp(secs as i64)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_scene_finds_every_builtin_via_the_registry() {
        for id in ["coding", "chat", "demo", "research"] {
            assert_eq!(
                resolve_scene(id)
                    .expect("builtin scene should resolve")
                    .id(),
                id
            );
        }
    }

    #[test]
    fn resolve_scene_rejects_an_unknown_name_and_lists_the_known_ones() {
        let err = match resolve_scene("not-a-real-scene") {
            Ok(_) => panic!("expected an error for an unknown scene name"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("not-a-real-scene"), "{err}");
        for id in ["coding", "chat", "demo", "research"] {
            assert!(err.contains(id), "expected `{id}` in error, got: {err}");
        }
    }

    #[test]
    fn default_instance_name_sorts_and_dedupes_scenes() {
        assert_eq!(
            default_instance_name("coding", &["chat".to_string(), "research".to_string()]),
            "chat,coding,research"
        );
        assert_eq!(
            default_instance_name("coding", &["coding".to_string()]),
            "coding",
            "--scene's own value repeated in --scenes must not appear twice"
        );
    }

    #[test]
    fn default_instance_name_is_order_independent() {
        assert_eq!(
            default_instance_name("research", &["chat".to_string(), "coding".to_string()]),
            default_instance_name("coding", &["chat".to_string(), "research".to_string()]),
        );
    }
}
