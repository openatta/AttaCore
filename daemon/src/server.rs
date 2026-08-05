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
//! file permissions are the entire access-control story locally. For TCP,
//! each connection must open with a `daemon.auth` handshake carrying the
//! shared token (`--token`/`ATTACORE_DAEMON_TOKEN`, see
//! [`DaemonServer::authenticate_tcp`]) before any other method is dispatched
//! — once a connection passes the handshake it is trusted for its lifetime,
//! same as a Unix socket connection. In particular, `config.setProvider` lets
//! any authenticated caller point future LLM traffic at an
//! attacker-controlled `base_url`/`api_key` — treat socket/token exposure
//! with the same care as exposing the LLM credentials themselves.

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
type LineReader = tokio::io::Lines<BufReader<Reader>>;

/// Fixed-time byte comparison — avoids leaking the TCP token's length-matched
/// prefix via response-time side channels. `a`/`b` differing in length is not
/// itself timing-sensitive (tokens are a fixed shared secret, not attacker-
/// influenced length), so a cheap length check up front is fine.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub struct DaemonServer {
    pool: Arc<SessionPool>,
    started_at: Instant,
    shutdown_token: CancellationToken,
    tcp_token: tokio::sync::RwLock<Option<String>>,
}

impl DaemonServer {
    pub fn new(pool: Arc<SessionPool>, shutdown_token: CancellationToken) -> Self {
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
        self.serve_tcp_listener(listener).await
    }

    /// Accept loop over an already-bound TCP listener — split out from
    /// [`Self::serve_tcp`] so tests can bind an ephemeral port (`127.0.0.1:0`),
    /// read the OS-assigned port via `listener.local_addr()`, and drive this
    /// loop directly instead of guessing a fixed port.
    ///
    /// Re-checks (doesn't just trust `serve_tcp`'s check) that a token is
    /// configured — this is a `pub` method, so a caller could reach it
    /// without going through `serve_tcp` at all (e.g. binding the listener
    /// itself for socket-activation setups) and forgetting `set_tcp_token`
    /// first. Without this, that mistake wouldn't fail loudly — every
    /// connection would just silently get rejected by `authenticate_tcp`
    /// forever, which is safe but confusing to debug.
    pub async fn serve_tcp_listener(self: Arc<Self>, listener: TcpListener) -> anyhow::Result<()> {
        if self.tcp_token.read().await.is_none() {
            anyhow::bail!("TCP requires token");
        }
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
        tcp: bool,
    ) -> anyhow::Result<()> {
        let mut lines = BufReader::new(reader).lines();

        // Unix socket connections are trusted for their lifetime by file
        // permissions alone (see the module doc comment's trust-boundary
        // note) — no handshake needed. TCP connections must authenticate
        // once, up front, before any RPC method is dispatched.
        if tcp && !self.authenticate_tcp(&mut lines, &writer).await {
            return Ok(());
        }

        while let Ok(Some(line)) = lines.next_line().await {
            let req: RpcRequest = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let resp = self.dispatch(req, writer.clone()).await;
            if !self.write_response(&writer, &resp).await {
                break;
            }
        }
        Ok(())
    }

    /// TCP-only handshake: the first line on the connection must be a
    /// `daemon.auth` request carrying `params.token` equal to the token
    /// configured via `--token`/`ATTACORE_DAEMON_TOKEN`. Writes back an ok/err
    /// response either way. Returns `true` if the connection may proceed to
    /// the normal dispatch loop, `false` if it was rejected (or closed before
    /// sending anything) and the caller should just drop it.
    async fn authenticate_tcp(&self, lines: &mut LineReader, writer: &Writer) -> bool {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            _ => return false,
        };
        let req: RpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => {
                self.write_response(
                    writer,
                    &RpcResponse::err(
                        serde_json::Value::Null,
                        codes::UNAUTHORIZED,
                        "first message must be a valid daemon.auth request",
                    ),
                )
                .await;
                return false;
            }
        };
        let id = req.id.clone().unwrap_or(serde_json::Value::Null);
        if req.method != "daemon.auth" {
            self.write_response(
                writer,
                &RpcResponse::err(id, codes::UNAUTHORIZED, "must authenticate via daemon.auth first"),
            )
            .await;
            return false;
        }
        let provided = req.params.get("token").and_then(|v| v.as_str()).unwrap_or("");
        let expected = self.tcp_token.read().await;
        let authenticated = expected
            .as_deref()
            .is_some_and(|t| constant_time_eq(t.as_bytes(), provided.as_bytes()));
        drop(expected);

        if !authenticated {
            self.write_response(
                writer,
                &RpcResponse::err(id, codes::UNAUTHORIZED, "invalid token"),
            )
            .await;
            return false;
        }
        self.write_response(writer, &RpcResponse::ok(id, serde_json::json!({"authenticated": true})))
            .await;
        true
    }

    /// Serialize + write one response line, flush, and report whether the
    /// write succeeded (`false` means the peer is gone — caller should stop
    /// writing to this connection).
    async fn write_response(&self, writer: &Writer, resp: &RpcResponse) -> bool {
        let mut buf = serde_json::to_vec(resp).unwrap_or_default();
        buf.push(b'\n');
        let mut w = writer.lock().await;
        if w.write_all(&buf).await.is_err() {
            return false;
        }
        let _ = w.flush().await;
        true
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
            "mcp.status" => RpcResponse::ok(
                id,
                serde_json::json!({"servers": self.pool.mcp_status().await}),
            ),
            "mcp.addServer" => self.method_mcp_add_server(id, req.params).await,
            "commands.list" => RpcResponse::ok(
                id,
                serde_json::json!({"commands": self.pool.list_commands().await}),
            ),
            "plugin.list" => RpcResponse::ok(
                id,
                serde_json::json!({"plugins": self.pool.list_plugins().await}),
            ),
            "plugin.install" => self.method_plugin_install(id, req.params).await,
            "plugin.uninstall" => self.method_plugin_uninstall(id, req.params).await,
            "plugin.enable" => self.method_plugin_set_enabled(id, req.params, true).await,
            "plugin.disable" => self.method_plugin_set_enabled(id, req.params, false).await,
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
                    None => {
                        return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id")
                    }
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
    async fn method_daemon_subscribe_events(
        &self,
        id: serde_json::Value,
        writer: Writer,
    ) -> RpcResponse {
        let mut rx = self.pool.subscribe_events();
        let forward_writer = writer.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let frame = crate::rpc::StreamFrame::daemon_event(event);
                        let Ok(mut b) = serde_json::to_vec(&frame) else {
                            continue;
                        };
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
    async fn method_config_get_provider(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let include_secrets = params
            .get("include_secrets")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        RpcResponse::ok(id, self.pool.get_providers(include_secrets).await)
    }

    /// `mcp.addServer` params: `{"name": "...", "config": {"type": "stdio", ...}}`
    /// (`config` is an `mcp::config::McpServerConfig`, tagged by `type`).
    /// `plugin.install` params:
    /// ```json
    /// {
    ///   "name": "code-review-helper",
    ///   "version": "1.0.0",
    ///   "download_url": "file:///path/to/plugin.zip",   // or https://...
    ///   "checksum": "<sha256 hex>",                       // required for http(s), optional for file://
    ///   "scope": "global"                                 // optional, default "global"; or "scene"
    /// }
    /// ```
    /// No marketplace lookup — installs directly from the given source (see
    /// `plugin::cli::PluginCommands::install_source`). Verifies the
    /// checksum before extracting; a network (`http(s)://`) source without
    /// a `checksum` is rejected outright.
    async fn method_plugin_install(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let name = match params.get("name").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing name"),
        };
        let version = match params.get("version").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing version"),
        };
        let download_url = match params.get("download_url").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing download_url"),
        };
        let checksum = params
            .get("checksum")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let scope = params
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("global")
            .to_string();

        match self
            .pool
            .install_plugin(&name, &version, &download_url, checksum.as_deref(), &scope)
            .await
        {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => RpcResponse::err(id, codes::INVALID_PARAMS, e),
        }
    }

    /// `plugin.uninstall` params: `{"name": "...", "version": "..." (optional, omit = all), "scope": "global"|"scene" (optional, default "global")}`
    async fn method_plugin_uninstall(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let name = match params.get("name").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing name"),
        };
        let version = params
            .get("version")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let scope = params
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("global")
            .to_string();

        match self
            .pool
            .uninstall_plugin(&name, version.as_deref(), &scope)
            .await
        {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => RpcResponse::err(id, codes::INVALID_PARAMS, e),
        }
    }

    /// Shared handler for `plugin.enable`/`plugin.disable`. Params:
    /// `{"name": "...", "scope": "global"|"scene" (optional, default "global")}`
    async fn method_plugin_set_enabled(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
        enabled: bool,
    ) -> RpcResponse {
        let name = match params.get("name").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing name"),
        };
        let scope = params
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("global")
            .to_string();

        match self.pool.set_plugin_enabled(&name, enabled, &scope).await {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => RpcResponse::err(id, codes::INVALID_PARAMS, e),
        }
    }

    async fn method_mcp_add_server(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
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
    async fn method_import_run(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
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
        let delete = params
            .get("delete")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let config_patch = params.get("config").cloned();
        let default_provider = params
            .get("default_provider")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let task_models_patch = params.get("task_models").cloned();

        match self
            .pool
            .set_provider(
                &provider_id,
                delete,
                config_patch,
                default_provider,
                task_models_patch,
            )
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
