//! Daemon server — JSON-RPC over Unix socket / TCP.
//!
//! Accepts newline-delimited JSON-RPC 2.0 requests, dispatches them
//! to the agent engine, and streams events back as `StreamFrame` lines.

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
            "daemon.shutdown" => {
                self.pool.shutdown_all().await;
                self.shutdown_token.cancel();
                RpcResponse::ok(id, serde_json::json!({"shutting_down":true}))
            }
            "session.list" => {
                let sessions = self.pool.list_all().await;
                RpcResponse::ok(id, serde_json::json!({"sessions": sessions}))
            }
            "session.run_turn" => self.method_session_run_turn(id, req.params, writer).await,
            _ => RpcResponse::err(
                id,
                codes::METHOD_NOT_FOUND,
                format!("unknown: {}", req.method),
            ),
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
