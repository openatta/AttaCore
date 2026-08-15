//! 类型化 daemon JSON-RPC 客户端 — 供集成测试复用，避免每处测试各自手写
//! JSON 拼接/逐行解析（这曾经导致 `cli_runner.rs` 里调用了一个根本不存在的
//! `"session.delete"` 方法而没人发现）。
//!
//! 方法覆盖 `docs/daemon_rpc_protocol.md` 列出的全部 13 个 daemon RPC。
//! 复用 `daemon::rpc` 里的 wire 类型（`RpcRequest`/`RpcResponse`/`SessionOptions` 等），
//! 不重复定义，daemon 侧协议变了这里能在编译期感知到。

use daemon::rpc::{RpcError, RpcResponse};
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

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
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: AtomicI64,
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
    pub async fn connect(socket_path: &Path) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(socket_path).await?;
        let (read_half, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(read_half),
            writer,
            next_id: AtomicI64::new(1),
        })
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
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        Ok(())
    }

    /// 简单请求-响应调用：发送请求，读到匹配 id 的响应即返回；期间任何没有
    /// 匹配 id 的行（StreamFrame 等）会被丢弃——调用方不关心中间流事件时用这个。
    pub async fn call(&mut self, method: &str, params: Value) -> anyhow::Result<RpcResponse> {
        let id = self.next_id();
        self.write_request(method, params, id).await?;
        self.read_response_with_id(id).await
    }

    async fn read_response_with_id(&mut self, id: i64) -> anyhow::Result<RpcResponse> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line).await?;
            anyhow::ensure!(n > 0, "daemon connection closed before response id={id}");
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(trimmed)?;
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

    /// 发一轮消息，收集期间所有 `session.event` StreamFrame，直到看到
    /// `turn_complete` 事件 + 匹配 id 的最终响应。
    pub async fn session_run_turn(
        &mut self,
        session_id: &str,
        message: &str,
        turn_id: &str,
        options: Option<RunTurnOptions>,
    ) -> anyhow::Result<TurnEvents> {
        let id = self.next_id();
        let mut params = serde_json::json!({
            "session_id": session_id,
            "message": message,
            "turn_id": turn_id,
        });
        if let Some(opts) = options {
            params["options"] = serde_json::to_value(opts)?;
        }
        self.write_request("session.run_turn", params, id).await?;

        let mut out = TurnEvents::default();
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line).await?;
            anyhow::ensure!(
                n > 0,
                "daemon connection closed mid-turn (session={session_id})"
            );
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let msg: Value = serde_json::from_str(trimmed)?;

            if msg.get("method").and_then(|m| m.as_str()) == Some("session.event") {
                if let Some(event) = msg.get("params").and_then(|p| p.get("event")) {
                    match event.get("kind").and_then(|k| k.as_str()) {
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
                            let input = event.get("input").cloned().unwrap_or_default();
                            out.tool_uses.push((name, input));
                        }
                        Some("turn_complete") => out.turn_complete = true,
                        _ => {}
                    }
                }
                continue;
            }

            if msg.get("id").and_then(|i| i.as_i64()) == Some(id) {
                if msg.get("error").is_some() {
                    // Turn failed outright — no turn_complete event will follow.
                    out.response = Some(to_rpc_response(msg));
                    return Ok(out);
                }
                if msg.get("result").is_some() {
                    out.response = Some(to_rpc_response(msg));
                    if out.turn_complete {
                        return Ok(out);
                    }
                    // Response can race ahead of the turn_complete event on
                    // the wire; keep draining until it shows up so callers
                    // always see the full transcript.
                }
            }
        }
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
