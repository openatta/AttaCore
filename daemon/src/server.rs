//! Daemon server — JSON-RPC over Unix socket / TCP.
//!
//! Accepts newline-delimited JSON-RPC 2.0 requests, dispatches them
//! to the agent engine, and streams events back as `StreamFrame` lines.
//!
//! **Trust boundary**: every RPC method here — including `config.setProvider`
//! (writes `providers.<id>.api_key`/`base_url` into settings.json) and
//! `session.run_turn` (already runs with `PermissionMode::BypassPermissions`,
//! "IDE plugins manage their own sandbox") — trusts whoever can reach this
//! socket. There is no per-method authorization on top of that: Unix socket
//! file permissions are the entire access-control story locally, and the TCP
//! listener's shared token (`--token`/`ATTACORE_DAEMON_TOKEN`) is the entire
//! story remotely. In particular, `config.setProvider` lets any caller who
//! can reach this socket point future LLM traffic at an attacker-controlled
//! `base_url`/`api_key` — treat socket/token exposure with the same care as
//! exposing the LLM credentials themselves.

use crate::rpc::{codes, RpcRequest, RpcResponse};
use crate::session_pool::SessionPool;
use base::id::Id;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

type Writer = Arc<AsyncMutex<Box<dyn AsyncWrite + Send + Unpin + 'static>>>;
type Reader = Box<dyn AsyncRead + Send + Unpin + 'static>;

pub struct DaemonServer {
    pool: Arc<SessionPool>,
    started_at: Instant,
    shutdown_token: CancellationToken,
    tcp_token: tokio::sync::RwLock<Option<String>>,
}

impl DaemonServer {
    pub fn new(
        pool: Arc<SessionPool>,
        shutdown_token: CancellationToken,
    ) -> Self {
        Self {
            pool,
            started_at: Instant::now(),
            shutdown_token,
            tcp_token: tokio::sync::RwLock::new(None),
        }
    }

    pub async fn set_tcp_token(&self, token: String) {
        *self.tcp_token.write().await = Some(token);
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown_token.clone()
    }

    pub async fn serve_tcp(self: Arc<Self>, addr: SocketAddr) -> anyhow::Result<()> {
        if self.tcp_token.read().await.is_none() {
            anyhow::bail!("TCP requires token");
        }
        let listener = TcpListener::bind(addr).await?;
        info!(addr=%addr, "TCP listener bound");
        loop {
            tokio::select! {
                _ = self.shutdown_token.cancelled() => break,
                result = listener.accept() => {
                    match result {
                        Ok((stream, peer)) => {
                            debug!(peer=%peer);
                            let this = self.clone();
                            tokio::spawn(async move {
                                let (r, w) = stream.into_split();
                                let writer: Writer = Arc::new(AsyncMutex::new(Box::new(w)));
                                if let Err(e) = this.handle_connection(Box::new(r), writer, true).await {
                                    warn!(error=%e);
                                }
                            });
                        }
                        Err(e) => warn!(error=%e),
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn serve_unix(self: Arc<Self>, socket_path: &Path) -> anyhow::Result<()> {
        if socket_path.exists() {
            std::fs::remove_file(socket_path)?;
        }
        let listener = UnixListener::bind(socket_path)?;
        info!(path=%socket_path.display());
        loop {
            tokio::select! {
                _ = self.shutdown_token.cancelled() => break,
                result = listener.accept() => {
                    match result {
                        Ok((stream, _addr)) => {
                            let this = self.clone();
                            tokio::spawn(async move {
                                let (r, w) = stream.into_split();
                                let writer: Writer = Arc::new(AsyncMutex::new(Box::new(w)));
                                if let Err(e) = this.handle_connection(Box::new(r), writer, false).await {
                                    warn!(error=%e);
                                }
                            });
                        }
                        Err(e) => warn!(error=%e),
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_connection(
        &self,
        reader: Reader,
        writer: Writer,
        _tcp: bool,
    ) -> anyhow::Result<()> {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let req: RpcRequest = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let resp = self.dispatch(req, writer.clone()).await;
            let mut buf = serde_json::to_vec(&resp).unwrap_or_default();
            buf.push(b'\n');
            let mut w = writer.lock().await;
            if w.write_all(&buf).await.is_err() {
                break;
            }
            let _ = w.flush().await;
        }
        Ok(())
    }

    async fn dispatch(&self, req: RpcRequest, writer: Writer) -> RpcResponse {
        let id = req.id.unwrap_or(serde_json::Value::Null);
        match req.method.as_str() {
            "daemon.status" => {
                let count = self.pool.active_count().await;
                RpcResponse::ok(
                    id,
                    serde_json::json!({
                        "version": env!("CARGO_PKG_VERSION"),
                        "uptime_secs": self.started_at.elapsed().as_secs(),
                        "sessions": count,
                    }),
                )
            }
            "daemon.doctor" => RpcResponse::ok(id, self.pool.doctor_report().await),
            "daemon.subscribeEvents" => self.method_daemon_subscribe_events(id, writer).await,
            "config.setProvider" => self.method_config_set_provider(id, req.params).await,
            "config.getProvider" => self.method_config_get_provider(id, req.params).await,
            "mcp.status" => RpcResponse::ok(id, serde_json::json!({"servers": self.pool.mcp_status().await})),
            "mcp.addServer" => self.method_mcp_add_server(id, req.params).await,
            "commands.list" => RpcResponse::ok(id, serde_json::json!({"commands": self.pool.list_commands()})),
            "import.list" => {
                let sources = self.pool.list_import_sources().await;
                RpcResponse::ok(id, serde_json::json!({"sources": sources}))
            }
            "import.run" => self.method_import_run(id, req.params).await,
            "daemon.shutdown" => {
                self.pool.shutdown_all().await;
                self.shutdown_token.cancel();
                RpcResponse::ok(id, serde_json::json!({"shutting_down":true}))
            }
            "session.list" => {
                let sessions = self.pool.list_all().await;
                RpcResponse::ok(id, serde_json::json!({"sessions": sessions}))
            }
            "session.close" => {
                let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
                    Some(s) => s.to_string(),
                    None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id"),
                };
                self.pool.shutdown_session(&session_id).await;
                RpcResponse::ok(id, serde_json::json!({"closed": session_id}))
            }
            "session.run_turn" => self.method_session_run_turn(id, req.params, writer).await,
            _ => RpcResponse::err(
                id,
                codes::METHOD_NOT_FOUND,
                format!("unknown: {}", req.method),
            ),
        }
    }

    /// `daemon.subscribeEvents` — no params. Returns an immediate ack
    /// (`{"subscribed": true}`) and, from then on, pushes every future
    /// `daemon.event` notification (see `SessionPool::emit_event`) to this
    /// same connection as a `StreamFrame` — indefinitely, until the
    /// connection closes. Does not replay past events; subscribe before the
    /// event you care about can happen (e.g. right after startup, before
    /// relying on `mcp_connected`/`import_detected`).
    async fn method_daemon_subscribe_events(&self, id: serde_json::Value, writer: Writer) -> RpcResponse {
        let mut rx = self.pool.subscribe_events();
        let forward_writer = writer.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let frame = crate::rpc::StreamFrame::daemon_event(event);
                        let Ok(mut b) = serde_json::to_vec(&frame) else { continue };
                        b.push(b'\n');
                        if forward_writer.lock().await.write_all(&b).await.is_err() {
                            break; // connection closed
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        RpcResponse::ok(id, serde_json::json!({"subscribed": true}))
    }

    /// `config.getProvider` params: `{"include_secrets": false}` (optional,
    /// default `false`). Read-side counterpart to `config.setProvider` — see
    /// `SessionPool::get_providers` doc comment for the redaction default.
    async fn method_config_get_provider(&self, id: serde_json::Value, params: serde_json::Value) -> RpcResponse {
        let include_secrets = params.get("include_secrets").and_then(|v| v.as_bool()).unwrap_or(false);
        RpcResponse::ok(id, self.pool.get_providers(include_secrets).await)
    }

    /// `mcp.addServer` params: `{"name": "...", "config": {"type": "stdio", ...}}`
    /// (`config` is an `mcp::config::McpServerConfig`, tagged by `type`).
    async fn method_mcp_add_server(&self, id: serde_json::Value, params: serde_json::Value) -> RpcResponse {
        let name = match params.get("name").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing name"),
        };
        let config = match params.get("config").cloned() {
            Some(c) => c,
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing config"),
        };
        match self.pool.add_mcp_server(&name, config).await {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => RpcResponse::err(id, codes::INVALID_PARAMS, e),
        }
    }

    /// `import.run` params: `{"source": "claude_code" | "codex" | "cursor"}`
    /// (one of the `source` values `import.list` returned).
    async fn method_import_run(&self, id: serde_json::Value, params: serde_json::Value) -> RpcResponse {
        let source = match params.get("source").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing source"),
        };
        match self.pool.run_import(&source).await {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => RpcResponse::err(id, codes::INVALID_PARAMS, e),
        }
    }

    /// `config.setProvider` params:
    /// ```json
    /// {
    ///   "provider_id": "deepseek",
    ///   "config": { "api_type": "openai_compatible", "base_url": "...", "api_key": "...", "default_model": "..." },
    ///   "default_provider": "deepseek",       // optional
    ///   "task_models": { "subagent": "deepseek" }, // optional, merged
    ///   "delete": false                        // optional, default false
    /// }
    /// ```
    /// `config`/`task_models` are partial patches (see `SessionPool::set_provider`
    /// doc comment for why they're raw JSON, not typed structs). Writes to the
    /// project-tier `settings.json` and returns the reloaded effective config
    /// plus routing validation.
    async fn method_config_set_provider(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let provider_id = match params.get("provider_id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing provider_id"),
        };
        let delete = params.get("delete").and_then(|v| v.as_bool()).unwrap_or(false);
        let config_patch = params.get("config").cloned();
        let default_provider = params
            .get("default_provider")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let task_models_patch = params.get("task_models").cloned();

        match self
            .pool
            .set_provider(&provider_id, delete, config_patch, default_provider, task_models_patch)
            .await
        {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => RpcResponse::err(id, codes::INVALID_PARAMS, e),
        }
    }

    async fn method_session_run_turn(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
        writer: Writer,
    ) -> RpcResponse {
        // session_id 可选：不传则自动新建 session
        let session_id = params
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let user_msg = match params.get("message").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing message"),
        };

        let turn_id = params
            .get("turn_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| Id::new().to_string());

        let options: Option<crate::rpc::SessionOptions> = params
            .get("options")
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        self.pool
            .run_turn(session_id, user_msg, turn_id, writer, id, options)
            .await
    }
}
