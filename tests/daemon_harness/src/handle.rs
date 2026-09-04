//! A running daemon, and the connection to it.
//!
//! What a scenario needs from a daemon is a socket to talk to. How that
//! daemon came to exist is this module's business and nobody else's, which
//! is what lets the same case run against a server built in this process and
//! — later — against a spawned `attacored`.
//!
//! The in-process build goes through `load_daemon_config` and
//! `SessionPool::new` the way `main.rs` does, rather than assembling a pool
//! by hand: the settings merge, the path resolution and the history layout
//! are all things a daemon-level test is supposed to be exercising, and a
//! harness that shortcuts them tests its own shortcut.
//!
//! It takes its paths as arguments where `main.rs` reads the environment.
//! That is not a shortcut but a requirement: environment variables belong to
//! the whole process, and several of these run at once in one test binary.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base::interface::memory::MemoryStore;
use base::interface::permission::{Permission, PermissionOutcome};
use daemon::config::{load_daemon_config, DaemonPaths, StaticDaemonPaths};
use daemon::{DaemonServer, SessionPool};
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};
use rpc_client::DaemonRpcClient;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::provider::ProviderStub;
use crate::world::World;

pub const TOKEN: &str = "harness-token";

/// Which transport a connection uses. The daemon claims everything above
/// framing is identical on all three; a scenario that runs on each is how
/// that claim gets checked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wire {
    Unix,
    Tcp,
    Ws,
}

pub struct DaemonOptions {
    pub scene: String,
    /// The daemon's own working directory — the project a session gets when
    /// `session.create` names none.
    pub project: PathBuf,
    pub session_cap: usize,
    pub prompt_timeout: Duration,
    /// Bind TCP and WebSocket listeners too. Off unless a scenario asks,
    /// because two extra listeners per daemon is two extra things to leak.
    pub extra_wires: bool,
}

impl DaemonOptions {
    pub fn new(project: PathBuf) -> Self {
        Self {
            scene: "coding".to_string(),
            project,
            session_cap: 8,
            prompt_timeout: Duration::from_secs(10),
            extra_wires: false,
        }
    }

    pub fn scene(mut self, scene: &str) -> Self {
        self.scene = scene.to_string();
        self
    }

    pub fn with_extra_wires(mut self) -> Self {
        self.extra_wires = true;
        self
    }
}

pub struct Daemon {
    socket: PathBuf,
    tcp: Option<SocketAddr>,
    ws: Option<SocketAddr>,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl Daemon {
    pub async fn start(
        world: &World,
        provider: &ProviderStub,
        opts: DaemonOptions,
    ) -> anyhow::Result<Self> {
        let paths =
            StaticDaemonPaths::with_project(world.config_root(&opts.scene), opts.project.clone())
                .with_global(world.global_root());
        std::fs::create_dir_all(paths.config_root())?;

        let socket = world.socket();
        let config = load_daemon_config(
            "claude-sonnet-4-6",
            2000,
            Some(&socket),
            &opts.scene,
            &paths,
        );

        let client: Arc<dyn AnthropicClient> = Arc::new(HttpAnthropicClient::with_base(
            AuthMode::ApiKey("harness".to_string()),
            provider.base_url(),
        )?);

        let global_dir = config.settings.paths.global_data_dir.clone();
        let local_dir = config.settings.paths.local_data_dir.clone();
        let memory = Arc::new(MemoryStore::new(
            global_dir.join("memory"),
            local_dir.join("memory"),
        ));

        let history = history::store::JsonlHistoryStore::with_roots(
            &opts.project,
            history::path::HistoryRoots::under(&global_dir),
        )
        .await
        .ok()
        .map(|s| Arc::new(s) as Arc<dyn history::store::HistoryStore>);

        let mut registry = scene::scene::SceneRegistry::new();
        registry.register_builtin();
        let agent_scene = registry
            .resolve(&opts.scene)
            .ok_or_else(|| anyhow::anyhow!("unknown scene `{}`", opts.scene))?;

        let pool = Arc::new(
            SessionPool::new(
                opts.session_cap,
                3600,
                client,
                Arc::new(config.settings.clone()),
                agent_scene,
                Arc::new(AllowAll) as Arc<dyn Permission>,
                memory,
                opts.project.clone(),
                history,
                config.paths.clone(),
                None,
            )
            .with_permission_prompt_timeout(opts.prompt_timeout),
        );

        let cancel = CancellationToken::new();
        let server = Arc::new(DaemonServer::new(pool, cancel.clone()));
        server.set_tcp_token(TOKEN.to_string()).await;

        let mut tasks = Vec::new();
        {
            let server = server.clone();
            let socket = socket.clone();
            tasks.push(tokio::spawn(async move {
                let _ = server.serve_unix(&socket).await;
            }));
        }

        let (tcp, ws) = if opts.extra_wires {
            let tcp_listener = TcpListener::bind("127.0.0.1:0").await?;
            let tcp = tcp_listener.local_addr()?;
            {
                let server = server.clone();
                tasks.push(tokio::spawn(async move {
                    let _ = server.serve_tcp_listener(tcp_listener).await;
                }));
            }
            let ws_listener = TcpListener::bind("127.0.0.1:0").await?;
            let ws = ws_listener.local_addr()?;
            {
                let server = server.clone();
                tasks.push(tokio::spawn(async move {
                    let _ = daemon::ws::serve_ws_listener(server, ws_listener).await;
                }));
            }
            (Some(tcp), Some(ws))
        } else {
            (None, None)
        };

        wait_for_socket(&socket).await?;
        Ok(Self {
            socket,
            tcp,
            ws,
            cancel,
            tasks,
        })
    }

    pub fn socket(&self) -> &std::path::Path {
        &self.socket
    }

    pub async fn connect(&self) -> anyhow::Result<DaemonRpcClient> {
        self.connect_via(Wire::Unix).await
    }

    pub async fn connect_via(&self, wire: Wire) -> anyhow::Result<DaemonRpcClient> {
        match wire {
            Wire::Unix => DaemonRpcClient::connect(&self.socket).await,
            Wire::Tcp => {
                let addr = self
                    .tcp
                    .ok_or_else(|| anyhow::anyhow!("daemon has no TCP listener"))?;
                DaemonRpcClient::connect_tcp(addr, TOKEN).await
            }
            Wire::Ws => {
                let addr = self
                    .ws
                    .ok_or_else(|| anyhow::anyhow!("daemon has no WebSocket listener"))?;
                DaemonRpcClient::connect_ws(addr, TOKEN, None).await
            }
        }
    }

    pub async fn stop(mut self) {
        self.cancel.cancel();
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

async fn wait_for_socket(path: &std::path::Path) -> anyhow::Result<()> {
    for _ in 0..200 {
        if path.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    anyhow::bail!("daemon never created its socket at {}", path.display())
}

/// The pool's permission of last resort, matching what `main.rs` hands it.
/// A session only ever receives it in `bypassPermissions`; everything else
/// builds a real `RuleSetPermission` from its own settings.
struct AllowAll;

#[async_trait::async_trait]
impl Permission for AllowAll {
    async fn check(
        &self,
        _tool: &str,
        _input: &serde_json::Value,
        _cwd: &std::path::Path,
        _session: &str,
    ) -> PermissionOutcome {
        PermissionOutcome::Permit
    }
}
