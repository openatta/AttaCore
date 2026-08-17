//! 类型化 daemon JSON-RPC 客户端 — 供集成测试复用，避免每处测试各自手写
//! JSON 拼接/逐行解析（这曾经导致 `cli_runner.rs` 里调用了一个根本不存在的
//! `"session.delete"` 方法而没人发现）。
//!
//! 复用 `daemon::rpc` 里的 wire 类型（`RpcRequest`/`RpcResponse`/`SessionOptions` 等），
//! 不重复定义，daemon 侧协议变了这里能在编译期感知到。
//!
//! # 传输与流式
//!
//! 客户端不绑定传输：Unix socket / TCP / WebSocket 三条路都实现同一个
//! [`Transport`]，上层一行代码都不用改。这不只是省事——「协议在分帧之上完全一致」
//! 是 daemon 侧的声明，能对同一段交互换三种传输各跑一遍，才算验证过。
//!
//! 一个 turn 有两种读法：
//!
//! - [`DaemonRpcClient::session_run_turn`] 收满整轮再返回，问「这一轮说了什么」；
//! - [`DaemonRpcClient::begin_turn`] 一帧一帧地拿，问「这一帧**什么时候**到」。
//!
//! 后者不是锦上添花。聚合式的读法看不出 token 是边生成边到达、还是憋到最后一次性
//! 吐出来——两者聚合结果完全一样。daemon 曾经有 8 个写点一个都没 flush，正是这类
//! bug，而当时的客户端结构上就测不出来。

pub mod transport;

use daemon::rpc::{RpcError, RpcResponse};
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
pub use transport::Transport;

/// Mirrors `daemon::rpc::SessionOptions` field-for-field, but with `Serialize`
/// instead of `Deserialize` — the daemon side only ever needs to *receive*
/// this shape, so it derives the other direction. Test clients need to send it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RunTurnOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vcr: Option<VcrRunOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<TelemetryRunOptions>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VcrRunOptions {
    /// "record" | "replay"
    pub mode: String,
    pub scenario: String,
    /// Absolute path to the VCR cassette directory.
    pub dir: String,
    /// See `daemon::rpc::VcrOptions::strict`'s doc comment — disables the
    /// network fallback on a replay miss so a hash mismatch fails loudly
    /// instead of silently making a real, billed API call.
    pub strict: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TelemetryRunOptions {
    /// Absolute path to the telemetry output file.
    pub output: String,
}

pub struct DaemonRpcClient {
    transport: Box<dyn Transport>,
    next_id: AtomicI64,
}

/// One item from a turn in progress.
#[derive(Debug)]
pub enum TurnItem {
    /// A `session.event` stream frame — the event object itself, not the
    /// envelope.
    Event(Value),
    /// The turn's final RPC response. Nothing follows it.
    Response(RpcResponse),
}

/// A turn being read frame by frame.
///
/// Borrows the client for the duration, which is the point: a turn owns the
/// connection until it ends, exactly as the daemon's own exclusion rule
/// says.
pub struct TurnStream<'a> {
    client: &'a mut DaemonRpcClient,
    id: i64,
    finished: bool,
    session_id: Option<String>,
}

impl TurnStream<'_> {
    /// The session this turn is running on, once a frame has said so.
    ///
    /// A turn started without a `session_id` gets one assigned, and the
    /// stream frames carry it on the envelope. Answering a permission prompt
    /// needs it, and the alternative — listing sessions and guessing which
    /// is yours — is wrong the moment there are two.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// The next item, or `None` once the response has been delivered.
    ///
    /// Each call is a single await point, so a caller can put a timeout
    /// around it and assert *when* something arrived rather than only that
    /// it did.
    pub async fn next(&mut self) -> anyhow::Result<Option<TurnItem>> {
        if self.finished {
            return Ok(None);
        }
        loop {
            let Some(msg) = self.client.transport.recv().await? else {
                anyhow::bail!("connection closed mid-turn");
            };
            if msg.get("method").and_then(|m| m.as_str()) == Some("session.event") {
                let params = msg.get("params");
                if self.session_id.is_none() {
                    self.session_id = params
                        .and_then(|p| p.get("session_id"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                }
                if let Some(event) = params.and_then(|p| p.get("event")) {
                    return Ok(Some(TurnItem::Event(event.clone())));
                }
                continue;
            }
            if msg.get("id").and_then(|i| i.as_i64()) == Some(self.id)
                && (msg.get("result").is_some() || msg.get("error").is_some())
            {
                self.finished = true;
                return Ok(Some(TurnItem::Response(to_rpc_response(msg))));
            }
        }
    }
}

/// 一次 `session.run_turn` 收集到的完整结果：所有 `session.event` StreamFrame
/// 累积出的文本/工具调用，以及最终 RPC 响应本身（错误时调用方可以直接读 `.error`）。
#[derive(Debug, Default)]
pub struct TurnEvents {
    pub text: String,
    pub tool_uses: Vec<(String, Value)>,
    pub turn_complete: bool,
    pub response: Option<RpcResponse>,
}

impl DaemonRpcClient {
    /// Connect over the Unix socket — no handshake, file permissions are the
    /// access control.
    pub async fn connect(socket_path: &Path) -> anyhow::Result<Self> {
        Ok(Self::with_transport(Box::new(
            transport::unix(socket_path).await?,
        )))
    }

    /// Connect over TCP and complete the `daemon.auth` handshake.
    pub async fn connect_tcp(addr: std::net::SocketAddr, token: &str) -> anyhow::Result<Self> {
        let mut client = Self::with_transport(Box::new(transport::tcp(addr).await?));
        client.authenticate(token).await?;
        Ok(client)
    }

    /// Connect over WebSocket and complete the same handshake.
    ///
    /// `origin` is what a browser would send; `None` is what a CLI sends.
    pub async fn connect_ws(
        addr: std::net::SocketAddr,
        token: &str,
        origin: Option<&str>,
    ) -> anyhow::Result<Self> {
        let mut client = Self::with_transport(Box::new(transport::ws(addr, origin).await?));
        client.authenticate(token).await?;
        Ok(client)
    }

    pub fn with_transport(transport: Box<dyn Transport>) -> Self {
        Self {
            transport,
            next_id: AtomicI64::new(1),
        }
    }

    /// Which transport this client is on — for test failure messages that
    /// otherwise cannot say which of the three broke.
    pub fn transport_name(&self) -> &'static str {
        self.transport.name()
    }

    async fn authenticate(&mut self, token: &str) -> anyhow::Result<()> {
        let resp = self
            .call("daemon.auth", serde_json::json!({ "token": token }))
            .await?;
        anyhow::ensure!(
            resp.error.is_none(),
            "daemon.auth refused: {:?}",
            resp.error
        );
        Ok(())
    }

    fn next_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn write_request(&mut self, method: &str, params: Value, id: i64) -> anyhow::Result<()> {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "id": id,
            "params": params,
        });
        self.transport.send(serde_json::to_string(&req)?).await
    }

    /// 简单请求-响应调用：发送请求，读到匹配 id 的响应即返回；期间任何没有
    /// 匹配 id 的行（StreamFrame 等）会被丢弃——调用方不关心中间流事件时用这个。
    pub async fn call(&mut self, method: &str, params: Value) -> anyhow::Result<RpcResponse> {
        let id = self.next_id();
        self.write_request(method, params, id).await?;
        self.read_response_with_id(id).await
    }

    async fn read_response_with_id(&mut self, id: i64) -> anyhow::Result<RpcResponse> {
        loop {
            let Some(v) = self.transport.recv().await? else {
                anyhow::bail!("daemon connection closed before response id={id}");
            };
            if v.get("id").and_then(|i| i.as_i64()) == Some(id)
                && (v.get("result").is_some() || v.get("error").is_some())
            {
                return Ok(to_rpc_response(v));
            }
            // else: unrelated StreamFrame or a response to a different id — skip.
        }
    }

    // ── daemon.* ──

    pub async fn daemon_status(&mut self) -> anyhow::Result<RpcResponse> {
        self.call("daemon.status", Value::Null).await
    }

    pub async fn daemon_doctor(&mut self) -> anyhow::Result<RpcResponse> {
        self.call("daemon.doctor", Value::Null).await
    }

    pub async fn daemon_shutdown(&mut self) -> anyhow::Result<RpcResponse> {
        self.call("daemon.shutdown", Value::Null).await
    }

    // ── session.* ──

    pub async fn session_list(&mut self) -> anyhow::Result<RpcResponse> {
        self.call("session.list", Value::Null).await
    }

    /// 真正存在的关闭方法（不是 `"session.delete"` —— 那个方法从未在
    /// daemon 里注册过，见模块文档）。
    pub async fn session_close(&mut self, session_id: &str) -> anyhow::Result<RpcResponse> {
        self.call(
            "session.close",
            serde_json::json!({ "session_id": session_id }),
        )
        .await
    }

    /// 起一轮，一帧一帧地读。
    ///
    /// 调用方拿到 [`TurnStream`] 后每 `next()` 一次是一个 await 点——可以在外面套
    /// 超时，断言某一帧**什么时候**到，而不只是最后到没到。
    pub async fn begin_turn(
        &mut self,
        session_id: Option<&str>,
        message: &str,
        turn_id: &str,
        options: Option<RunTurnOptions>,
    ) -> anyhow::Result<TurnStream<'_>> {
        let id = self.next_id();
        let mut params = serde_json::json!({
            "message": message,
            "turn_id": turn_id,
        });
        if let Some(sid) = session_id {
            params["session_id"] = serde_json::json!(sid);
        }
        if let Some(opts) = options {
            params["options"] = serde_json::to_value(opts)?;
        }
        self.write_request("session.run_turn", params, id).await?;
        Ok(TurnStream {
            client: self,
            id,
            finished: false,
            session_id: session_id.map(str::to_string),
        })
    }

    /// 发一轮消息，收集期间所有 `session.event` StreamFrame，直到看到
    /// `turn_complete` 事件 + 匹配 id 的最终响应。
    ///
    /// 建在 [`Self::begin_turn`] 之上，不另起一套读法：两条路要是各读各的，
    /// 迟早会在「响应先于 turn_complete 到达」这类细节上给出不同答案。
    pub async fn session_run_turn(
        &mut self,
        session_id: &str,
        message: &str,
        turn_id: &str,
        options: Option<RunTurnOptions>,
    ) -> anyhow::Result<TurnEvents> {
        let mut stream = self
            .begin_turn(Some(session_id), message, turn_id, options)
            .await?;

        let mut out = TurnEvents::default();
        while let Some(item) = stream.next().await? {
            match item {
                TurnItem::Event(event) => match event.get("kind").and_then(|k| k.as_str()) {
                    Some("text_delta") => {
                        if let Some(t) = event.get("text").and_then(|v| v.as_str()) {
                            out.text.push_str(t);
                        }
                    }
                    Some("tool_use") => {
                        let name = event
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("?")
                            .to_string();
                        out.tool_uses
                            .push((name, event.get("input").cloned().unwrap_or_default()));
                    }
                    Some("turn_complete") => out.turn_complete = true,
                    _ => {}
                },
                TurnItem::Response(resp) => {
                    out.response = Some(resp);
                    // The response can race ahead of `turn_complete` on the
                    // wire; the stream ends here either way, and the caller
                    // reads `turn_complete` to tell which happened.
                    break;
                }
            }
        }
        Ok(out)
    }

    // ── config.* ──

    pub async fn config_set_provider(&mut self, params: Value) -> anyhow::Result<RpcResponse> {
        self.call("config.setProvider", params).await
    }

    pub async fn config_get_provider(
        &mut self,
        include_secrets: bool,
    ) -> anyhow::Result<RpcResponse> {
        self.call(
            "config.getProvider",
            serde_json::json!({ "include_secrets": include_secrets }),
        )
        .await
    }

    pub async fn config_reload(&mut self) -> anyhow::Result<RpcResponse> {
        self.call("config.reload", Value::Null).await
    }

    // ── mcp.* ──

    pub async fn mcp_status(&mut self) -> anyhow::Result<RpcResponse> {
        self.call("mcp.status", Value::Null).await
    }

    pub async fn mcp_add_server(
        &mut self,
        name: &str,
        config: Value,
    ) -> anyhow::Result<RpcResponse> {
        self.call(
            "mcp.addServer",
            serde_json::json!({ "name": name, "config": config }),
        )
        .await
    }

    // ── import.* ──

    pub async fn import_list(&mut self) -> anyhow::Result<RpcResponse> {
        self.call("import.list", Value::Null).await
    }

    pub async fn import_run(&mut self, source: &str) -> anyhow::Result<RpcResponse> {
        self.call("import.run", serde_json::json!({ "source": source }))
            .await
    }

    // ── commands.* / plugin.* ──

    pub async fn commands_list(&mut self) -> anyhow::Result<RpcResponse> {
        self.call("commands.list", Value::Null).await
    }

    pub async fn plugin_list(&mut self) -> anyhow::Result<RpcResponse> {
        self.call("plugin.list", Value::Null).await
    }

    pub async fn plugin_install(&mut self, params: Value) -> anyhow::Result<RpcResponse> {
        self.call("plugin.install", params).await
    }

    pub async fn plugin_uninstall(&mut self, params: Value) -> anyhow::Result<RpcResponse> {
        self.call("plugin.uninstall", params).await
    }

    pub async fn plugin_set_enabled(
        &mut self,
        name: &str,
        enabled: bool,
    ) -> anyhow::Result<RpcResponse> {
        let method = if enabled {
            "plugin.enable"
        } else {
            "plugin.disable"
        };
        self.call(method, serde_json::json!({ "name": name })).await
    }
}

fn to_rpc_response(v: Value) -> RpcResponse {
    // RpcResponse/RpcError only derive Serialize on the daemon side (they're
    // server-outbound types, no Deserialize) — rebuild by field access here
    // for client-side ergonomics (`.result` / `.error` access) instead of
    // leaving callers to grovel through raw `Value`.
    let error = v.get("error").map(|e| RpcError {
        code: e.get("code").and_then(|c| c.as_i64()).unwrap_or(0) as i32,
        message: e
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string(),
        data: e.get("data").cloned(),
    });
    RpcResponse {
        jsonrpc: "2.0",
        id: v.get("id").cloned().unwrap_or(Value::Null),
        result: v.get("result").cloned(),
        error,
    }
}
