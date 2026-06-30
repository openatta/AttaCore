//! 真实 MCP server 连接：用 rmcp 1.6 的 client API 起 stdio / streamable-http。
//!
//! 注：rmcp 把 RunningService 暴露成 generic over transport；我们用 trait
//! object 屏蔽，让上层只看 `dyn McpClient`。
//!
//! ## 重连策略
//!
//! Server 子进程 crash 或管道断了之后：
//! - **检测**：list_tools / call_tool 拿到 transport-shape 的错误（rmcp 报错文本
//!   含 `closed` / `eof` / `broken` 或我们自己看到 inner=None）
//! - **重连**：drop 旧 service → 用原 config 重 spawn 子进程 → 重做 MCP 握手
//! - **节流**：两次 reconnect 至少隔 [`MIN_RECONNECT_INTERVAL`]；连续失败超
//!   [`MAX_CONSECUTIVE_FAILURES`] 之后回到 NotConnected，留给下次显式调用再试
//! - **不做主动 keepalive**：纯 lazy 检测；下个请求触发恢复
//!
//! 见 attacode-mcp/ATTA.md。

use crate::client::{
    McpCallResult, McpClient, McpContent, McpPromptArg, McpPromptMeta, McpResourceMeta, McpToolMeta,
};
use crate::config::McpServerConfig;
use crate::error::McpError;
use async_trait::async_trait;
use futures::StreamExt;
use rmcp::{model::CallToolRequestParams, service::ServiceExt};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// 两次自动 reconnect 之间至少这么久 —— 防 server 启动慢时被洪水重连击垮
const MIN_RECONNECT_INTERVAL: Duration = Duration::from_millis(500);
/// 连续 reconnect 失败超过这个数就放弃，转 NotConnected
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
const MAX_MCP_DESCRIPTION_LENGTH: usize = 10_000;

fn truncate_mcp_instructions(mut instructions: String) -> String {
    if instructions.len() <= MAX_MCP_DESCRIPTION_LENGTH {
        return instructions;
    }
    instructions.truncate(MAX_MCP_DESCRIPTION_LENGTH);
    instructions.push_str("... [truncated]");
    instructions
}

/// stdio 子进程连接的 MCP client。`Arc<Self>` 共享给多个 adapter。
///
/// 内部存 rmcp 的 `RunningService` —— 真正干活的 RPC 引擎。具体类型用 `()` 作
/// ClientHandler（最简模式，不实现自定义回调；用 default capabilities）。
pub struct StdioMcpClient {
    server_name: String,
    config: McpServerConfig,
    instructions: Option<String>,
    inner: Mutex<Option<rmcp::service::RunningService<rmcp::RoleClient, ()>>>,
    last_reconnect_at: Mutex<Option<Instant>>,
    consecutive_failures: AtomicU32,
}

impl StdioMcpClient {
    /// 启动子进程并完成 MCP 握手。
    pub async fn connect(
        server_name: &str,
        config: &McpServerConfig,
    ) -> Result<Arc<Self>, McpError> {
        let service = spawn_service(server_name, config).await?;
        let instructions = service
            .peer_info()
            .and_then(|info| info.instructions.clone())
            .map(truncate_mcp_instructions);
        Ok(Arc::new(Self {
            server_name: server_name.into(),
            config: config.clone(),
            instructions,
            inner: Mutex::new(Some(service)),
            last_reconnect_at: Mutex::new(None),
            consecutive_failures: AtomicU32::new(0),
        }))
    }

    /// 主动关连接 —— shutdown 子进程。
    pub async fn shutdown(&self) {
        if let Some(svc) = self.inner.lock().await.take() {
            let _ = svc.cancel().await;
        }
    }

    /// 把 inner 里残留的死 service 丢掉，用原 config 重 spawn 一份。
    /// 节流 + 失败计数；超阈值返回 NotConnected。
    ///
    /// 如果 config 有 oauth_provider，先清除缓存的 OAuth token 以确保
    /// 重连时重新走 OAuth 流程（适用 401 / token 过期场景）。
    async fn try_reconnect(&self) -> Result<(), McpError> {
        let fails = self.consecutive_failures.load(Ordering::SeqCst);
        if fails >= MAX_CONSECUTIVE_FAILURES {
            return Err(McpError::NotConnected {
                name: self.server_name.clone(),
            });
        }

        // 节流：上次 reconnect 还不到 MIN_RECONNECT_INTERVAL 的话先等
        {
            let mut last = self.last_reconnect_at.lock().await;
            if let Some(t) = *last {
                let since = t.elapsed();
                if since < MIN_RECONNECT_INTERVAL {
                    tokio::time::sleep(MIN_RECONNECT_INTERVAL - since).await;
                }
            }
            *last = Some(Instant::now());
        }

        // 如果 config 有 oauth_provider，清除缓存的 OAuth token 以确保
        // 重连时重新 resolve 一次（处理 401 / token 过期）。
        if let Some(provider) = self.config.oauth_provider() {
            crate::oauth::clear_oauth_token(provider).await;
        }

        // 先把旧 service 清掉（cancel 会 kill 子进程）
        if let Some(old) = self.inner.lock().await.take() {
            let _ = old.cancel().await;
        }

        match spawn_service(&self.server_name, &self.config).await {
            Ok(new_svc) => {
                *self.inner.lock().await = Some(new_svc);
                self.consecutive_failures.store(0, Ordering::SeqCst);
                debug!(server = %self.server_name, "MCP server reconnected");
                Ok(())
            }
            Err(e) => {
                self.consecutive_failures.fetch_add(1, Ordering::SeqCst);
                warn!(
                    server = %self.server_name,
                    fails = fails + 1,
                    error = %e,
                    "MCP reconnect attempt failed"
                );
                Err(e)
            }
        }
    }

    async fn list_tools_inner(&self) -> Result<Vec<McpToolMeta>, McpError> {
        let guard = self.inner.lock().await;
        let svc = guard.as_ref().ok_or_else(|| McpError::NotConnected {
            name: self.server_name.clone(),
        })?;
        let list = svc
            .list_tools(Default::default())
            .await
            .map_err(|e| McpError::RmcpService(format!("list_tools: {e}")))?;
        Ok(list
            .tools
            .into_iter()
            .map(|t| McpToolMeta {
                name: t.name.to_string(),
                description: t.description.map(|d| d.to_string()),
                input_schema: serde_json::Value::Object((*t.input_schema).clone()),
            })
            .collect())
    }

    async fn call_tool_inner(
        &self,
        tool_name: &str,
        args: serde_json::Map<String, serde_json::Value>,
    ) -> Result<McpCallResult, McpError> {
        let guard = self.inner.lock().await;
        let svc = guard.as_ref().ok_or_else(|| McpError::NotConnected {
            name: self.server_name.clone(),
        })?;

        let req = CallToolRequestParams::new(tool_name.to_string()).with_arguments(args);
        let result = svc
            .call_tool(req)
            .await
            .map_err(|e| McpError::RmcpService(format!("call_tool: {e}")))?;

        let mut content_blocks = Vec::new();
        for c in result.content.iter() {
            content_blocks.push(rmcp_content_to_ours(c));
        }

        Ok(McpCallResult {
            content: content_blocks,
            is_error: result.is_error.unwrap_or(false),
            meta: None,
        })
    }
}

#[async_trait]
impl McpClient for StdioMcpClient {
    fn server_name(&self) -> &str {
        &self.server_name
    }

    fn transport_kind(&self) -> &'static str {
        match &self.config {
            McpServerConfig::Stdio { .. } => "stdio",
            McpServerConfig::StreamableHttp { .. } => "streamable_http",
            McpServerConfig::Sse { .. } => "sse",
            McpServerConfig::InProcess { .. } => "in_process",
            McpServerConfig::WebSocket { .. } => "web_socket",
        }
    }

    fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    async fn list_tools(&self) -> Result<Vec<McpToolMeta>, McpError> {
        match self.list_tools_inner().await {
            Ok(r) => Ok(r),
            Err(e) if is_auth_error(&e) => {
                warn!(
                    server = %self.server_name,
                    error = %e,
                    "MCP list_tools auth error; attempting reconnect with fresh token"
                );
                self.try_reconnect().await?;
                self.list_tools_inner().await
            }
            Err(e) if is_transport_error(&e) => {
                warn!(
                    server = %self.server_name,
                    error = %e,
                    "MCP list_tools transport error; attempting reconnect"
                );
                self.try_reconnect().await?;
                self.list_tools_inner().await
            }
            Err(e) => Err(e),
        }
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        args: serde_json::Map<String, serde_json::Value>,
    ) -> Result<McpCallResult, McpError> {
        match self.call_tool_inner(tool_name, args.clone()).await {
            Ok(r) => Ok(r),
            Err(e) if is_auth_error(&e) => {
                warn!(
                    server = %self.server_name,
                    tool = tool_name,
                    error = %e,
                    "MCP call_tool auth error; attempting reconnect with fresh token"
                );
                self.try_reconnect().await?;
                self.call_tool_inner(tool_name, args).await
            }
            Err(e) if is_transport_error(&e) => {
                warn!(
                    server = %self.server_name,
                    tool = tool_name,
                    error = %e,
                    "MCP call_tool transport error; attempting reconnect"
                );
                self.try_reconnect().await?;
                self.call_tool_inner(tool_name, args).await
            }
            Err(e) => Err(e),
        }
    }

    async fn list_resources(&self) -> Result<Vec<McpResourceMeta>, McpError> {
        let guard = self.inner.lock().await;
        let svc = guard.as_ref().ok_or_else(|| McpError::NotConnected {
            name: self.server_name.clone(),
        })?;
        // rmcp 的 list_all_resources 自动分页直到拿完
        let resources = svc
            .list_all_resources()
            .await
            .map_err(|e| McpError::RmcpService(format!("list_resources: {e}")))?;
        Ok(resources
            .into_iter()
            .map(|r| {
                // Annotated<RawResource> derefs to RawResource; clone fields out
                let raw = &r.raw;
                McpResourceMeta {
                    uri: raw.uri.clone(),
                    name: raw.name.clone(),
                    description: raw.description.clone(),
                    mime_type: raw.mime_type.clone(),
                }
            })
            .collect())
    }

    async fn read_resource(&self, uri: &str) -> Result<Vec<McpContent>, McpError> {
        use rmcp::model::ReadResourceRequestParams;
        let guard = self.inner.lock().await;
        let svc = guard.as_ref().ok_or_else(|| McpError::NotConnected {
            name: self.server_name.clone(),
        })?;
        let result = svc
            .read_resource(ReadResourceRequestParams::new(uri))
            .await
            .map_err(|e| McpError::RmcpService(format!("read_resource: {e}")))?;
        // rmcp ReadResourceResult.contents 是 Vec<ResourceContents>；转成我们的 McpContent
        let mut out = Vec::new();
        for c in result.contents {
            match c {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
                    out.push(McpContent::Text(text));
                }
                rmcp::model::ResourceContents::BlobResourceContents {
                    blob, mime_type, ..
                } => {
                    out.push(McpContent::Image {
                        data: blob,
                        media_type: mime_type.unwrap_or_else(|| "application/octet-stream".into()),
                    });
                }
            }
        }
        Ok(out)
    }

    async fn list_prompts(&self) -> Result<Vec<McpPromptMeta>, McpError> {
        let guard = self.inner.lock().await;
        let svc = guard.as_ref().ok_or_else(|| McpError::NotConnected {
            name: self.server_name.clone(),
        })?;
        let prompts = svc
            .list_all_prompts()
            .await
            .map_err(|e| McpError::RmcpService(format!("list_prompts: {e}")))?;
        Ok(prompts
            .into_iter()
            .map(|p| McpPromptMeta {
                name: p.name.to_string(),
                description: p.description.map(|d| d.to_string()),
                arguments: p
                    .arguments
                    .unwrap_or_default()
                    .into_iter()
                    .map(|a| McpPromptArg {
                        name: a.name.to_string(),
                        description: a.description.map(|d| d.to_string()),
                        required: a.required,
                    })
                    .collect(),
            })
            .collect())
    }

    async fn get_prompt(
        &self,
        prompt_name: &str,
        _args: &std::collections::HashMap<String, String>,
    ) -> Result<String, McpError> {
        let guard = self.inner.lock().await;
        let _svc = guard.as_ref().ok_or_else(|| McpError::NotConnected {
            name: self.server_name.clone(),
        })?;
        // MCP prompts/get is available via rmcp but the client-side
        // invocation path differs by transport. For now, return a
        // descriptive placeholder — the prompt metadata is already
        // visible in the slash command listing.
        Ok(format!(
            "MCP prompt '{prompt_name}' on server '{}' — use `/mcp` to see server details.",
            self.server_name
        ))
    }
}

/// 根据 server config 起一个新 rmcp service（连过 + 握手完成）。
async fn spawn_service(
    server_name: &str,
    config: &McpServerConfig,
) -> Result<rmcp::service::RunningService<rmcp::RoleClient, ()>, McpError> {
    match config {
        McpServerConfig::Stdio {
            command, args, env, ..
        } => {
            // TS parity: expandEnvVarsInString — expand $VAR/${VAR}/${VAR:-default}/$$
            // in command/args/env values before spawning.
            let command = crate::config::expand_env_vars(command);
            let args: Vec<String> = args
                .iter()
                .map(|a| crate::config::expand_env_vars(a))
                .collect();
            let env: std::collections::HashMap<String, String> = env
                .iter()
                .map(|(k, v)| (k.clone(), crate::config::expand_env_vars(v)))
                .collect();
            spawn_stdio_service(server_name, &command, &args, &env).await
        }
        McpServerConfig::StreamableHttp {
            url,
            headers,
            oauth_provider,
            ..
        } => {
            let url = crate::config::expand_env_vars(url);
            let headers: std::collections::HashMap<String, String> = headers
                .iter()
                .map(|(k, v)| (k.clone(), crate::config::expand_env_vars(v)))
                .collect();
            spawn_streamable_http_service(server_name, &url, &headers, oauth_provider.as_deref())
                .await
        }
        McpServerConfig::Sse {
            url,
            headers,
            oauth_provider,
            ..
        } => {
            let url = crate::config::expand_env_vars(url);
            let headers: std::collections::HashMap<String, String> = headers
                .iter()
                .map(|(k, v)| (k.clone(), crate::config::expand_env_vars(v)))
                .collect();
            tracing::info!(
                server = %server_name,
                url = %url,
                "connecting to MCP server via SSE transport"
            );
            spawn_sse_service(server_name, &url, &headers, oauth_provider.as_deref()).await
        }
        McpServerConfig::InProcess { name, .. } => {
            // In-process servers are handled by InProcessMcpClient via
            // the process-local registry. StdioMcpClient cannot spawn them.
            Err(McpError::ConnectFailed {
                name: server_name.into(),
                source: anyhow::anyhow!(
                    "in-process MCP server '{}' must be connected via \
                     InProcessMcpClient::connect, not spawned as a subprocess",
                    name
                ),
            })
        }
        McpServerConfig::WebSocket { url, headers, .. } => {
            let url = crate::config::expand_env_vars(url);
            let headers: std::collections::HashMap<String, String> = headers
                .iter()
                .map(|(k, v)| (k.clone(), crate::config::expand_env_vars(v)))
                .collect();
            spawn_websocket_service(server_name, &url, &headers).await
        }
    }
}

async fn spawn_stdio_service(
    server_name: &str,
    command: &str,
    args: &[String],
    env: &std::collections::HashMap<String, String>,
) -> Result<rmcp::service::RunningService<rmcp::RoleClient, ()>, McpError> {
    let mut cmd = tokio::process::Command::new(command);
    for a in args {
        cmd.arg(a);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }

    let transport = rmcp::transport::child_process::TokioChildProcess::new(cmd).map_err(|e| {
        McpError::ConnectFailed {
            name: server_name.into(),
            source: anyhow::Error::new(e),
        }
    })?;

    ().serve(transport)
        .await
        .map_err(|e| McpError::ConnectFailed {
            name: server_name.into(),
            source: anyhow::anyhow!("{e}"),
        })
}

/// StreamableHttp transport：rmcp 1.6 的 reqwest-based client。
///
/// **headers 支持限制**: 仅识别 `Authorization` 头（进 transport
/// config.auth_header）。其它 header 会 warn 后忽略 —— 自定义 header 需要
/// `http` crate 类型，跨 crate 维护成本不值。
///
/// **v2-4  — OAuth token 注入**：当 config 给了 `oauth_provider` 名字
/// 时，尝试从 `~/.atta/code/tokens/<provider>.json` 读 stored token（必要时
/// 刷新）作为 Authorization。`headers.Authorization` 在此场景下被覆盖。
/// **caveat**：rmcp 1.6 transport 在创建时锁定 auth_header；token 中途过期
/// 不会自动刷新。daemon 长连接场景下用户需重启 MCP 子系统（重连）。
async fn spawn_streamable_http_service(
    server_name: &str,
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    oauth_provider: Option<&str>,
) -> Result<rmcp::service::RunningService<rmcp::RoleClient, ()>, McpError> {
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    use rmcp::transport::StreamableHttpClientTransport;

    let mut config = StreamableHttpClientTransportConfig::with_uri(url);

    // **v2-4 **: prefer fresh OAuth token over headers.Authorization
    let resolved_auth: Option<String> = if let Some(provider) = oauth_provider {
        match resolve_oauth_bearer(provider).await {
            Ok(tok) => Some(format!("Bearer {tok}")),
            Err(e) => {
                warn!(
                    server = %server_name,
                    provider = %provider,
                    error = %e,
                    "OAuth provider lookup failed; falling back to static headers"
                );
                None
            }
        }
    } else {
        None
    };
    if let Some(a) = resolved_auth {
        config = config.auth_header(a);
    }

    // 静态 header（仅 Authorization 进 config，其余 warn）
    let mut warned_other = false;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("authorization") {
            // 若已经被 OAuth 注入则跳过，避免覆盖
            if oauth_provider.is_none() {
                config = config.auth_header(v.clone());
            }
        } else if !warned_other {
            warn!(
                server = %server_name,
                ignored_header = %k,
                "StreamableHttp transport currently only honors `Authorization` header; \
                 other headers ignored"
            );
            warned_other = true;
        }
    }

    let transport = StreamableHttpClientTransport::from_config(config);

    ().serve(transport)
        .await
        .map_err(|e| McpError::ConnectFailed {
            name: server_name.into(),
            source: anyhow::anyhow!("{e}"),
        })
}

/// Resolve OAuth bearer token via the process-wide resolver (set by CLI at startup).
/// If no resolver is installed, returns an error — caller falls back to
/// connecting without a bearer token.
async fn resolve_oauth_bearer(provider_name: &str) -> Result<String, anyhow::Error> {
    crate::oauth::resolve_oauth_bearer(provider_name)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// 启发式判断：错误是不是 transport / connectivity 类（值得重连）vs
/// 真正的 RPC / schema / business 错（不该重连）。
///
/// 假阳性的代价：多 spawn 一次子进程（轻）。假阴性的代价：用户错过自动恢复
/// （重）。所以这里偏宽松。
pub(crate) fn is_transport_error(e: &McpError) -> bool {
    match e {
        McpError::NotConnected { .. } => true,
        McpError::RmcpService(msg) => {
            let m = msg.to_ascii_lowercase();
            [
                "closed",
                "eof",
                "broken pipe",
                "connection",
                "channel",
                "timeout",
            ]
            .iter()
            .any(|k| m.contains(k))
        }
        McpError::Transport(_) => true,
        _ => false,
    }
}

/// 启发式判断：错误是不是 auth / 401 / token 过期类（值得重连 + 重新 resolve OAuth token）
/// vs 真正不可恢复的授权错。
///
/// 与 is_transport_error 的关系：auth 错也会触发 reconnect，区别在于 reconnect 前
/// 会重新 resolve OAuth bearer token（StreamableHttp transport 场景）。Stdio transport
/// 不存在 auth 错。
pub(crate) fn is_auth_error(e: &McpError) -> bool {
    match e {
        McpError::NotConnected { .. } => false,
        McpError::RmcpService(msg) => {
            let m = msg.to_ascii_lowercase();
            [
                "401",
                "403",
                "unauthorized",
                "forbidden",
                "token expired",
                "access denied",
                "auth failure",
                "authentication",
            ]
            .iter()
            .any(|k| m.contains(k))
        }
        _ => false,
    }
}

/// 把 rmcp::model::Content 转成我们的 McpContent。
fn rmcp_content_to_ours(c: &rmcp::model::Annotated<rmcp::model::RawContent>) -> McpContent {
    match &c.raw {
        rmcp::model::RawContent::Text(t) => McpContent::Text(t.text.to_string()),
        rmcp::model::RawContent::Image(i) => McpContent::Image {
            data: i.data.to_string(),
            media_type: i.mime_type.to_string(),
        },
        other => {
            // 其它 content 类型：序列化成 JSON 留给上层
            let v = serde_json::to_value(other).unwrap_or(serde_json::Value::Null);
            McpContent::Other(v)
        }
    }
}

// ── WebSocket transport ──

/// Type alias for the tokio_tungstenite WebSocket stream (with optional TLS).
type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Concrete error type for the WebSocket transport.
///
/// Required because `Box<dyn Error + Send + Sync>` does not satisfy
/// the `Sized` bound baked into rmcp's `Transport::Error` supertrait.
#[derive(Debug, thiserror::Error)]
enum WsTransportError {
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("websocket: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
}

/// rmcp `Transport` adapter for WebSocket connections.
///
/// Each WebSocket text frame carries one JSON-RPC message (`serde_json`
/// serialized/deserialized). Binary frames are also accepted on the
/// receive side. Ping/pong frames are handled internally by tungstenite.
///
/// The write half is behind `Arc<Mutex<…>>` so that `send()` can return a
/// `'static + Send` future per the `Transport` trait contract.
struct WebSocketTransport {
    write:
        Arc<Mutex<futures::stream::SplitSink<WsStream, tokio_tungstenite::tungstenite::Message>>>,
    read: futures::stream::SplitStream<WsStream>,
}

impl rmcp::transport::Transport<rmcp::RoleClient> for WebSocketTransport {
    type Error = WsTransportError;

    fn send(
        &mut self,
        item: rmcp::service::TxJsonRpcMessage<rmcp::RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let json = serde_json::to_string(&item);
        let write = self.write.clone();
        Box::pin(async move {
            let msg = tokio_tungstenite::tungstenite::Message::text(json?);
            let mut guard = write.lock().await;
            futures::SinkExt::send(&mut *guard, msg).await?;
            Ok(())
        })
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<Output = Option<rmcp::service::RxJsonRpcMessage<rmcp::RoleClient>>>
           + Send {
        let read = &mut self.read;
        Box::pin(async move {
            loop {
                match futures::StreamExt::next(read).await {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                        if let Ok(msg) = serde_json::from_str(&text) {
                            return Some(msg);
                        }
                        tracing::warn!("WebSocket: ignoring non-JSON-RPC text frame");
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(data))) => {
                        if let Ok(msg) = serde_json::from_slice(&data) {
                            return Some(msg);
                        }
                    }
                    // Close frame or stream end => connection closed
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => {
                        return None;
                    }
                    // Ping/Pong handled internally by tungstenite
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => {
                        tracing::error!("WebSocket receive error: {e}");
                        return None;
                    }
                }
            }
        })
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        let write = self.write.clone();
        Box::pin(async move {
            let mut guard = write.lock().await;
            futures::SinkExt::close(&mut *guard).await?;
            Ok(())
        })
    }
}

/// Try to establish a raw WebSocket connection for MCP.
///
/// Custom headers from the MCP config are currently **not** forwarded to
/// the WebSocket upgrade request — `tokio_tungstenite::connect_async` does
/// not accept custom headers in its simplest form. If headers are needed,
/// the StreamableHTTP fallback (which does support them) should be used.
async fn try_ws_connect(
    url: &str,
    _headers: &std::collections::HashMap<String, String>,
) -> Result<WebSocketTransport, anyhow::Error> {
    // Validate the URL first for a better error message
    url::Url::parse(url).map_err(|e| anyhow::anyhow!("Invalid WebSocket URL '{url}': {e}"))?;

    // Pass &str directly — url::Url does not implement tokio_tungstenite's
    // IntoClientRequest trait, but &str does.
    let (ws_stream, _response) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| anyhow::anyhow!("WebSocket connection to '{url}' failed: {e}"))?;

    let (write, read) = ws_stream.split();
    Ok(WebSocketTransport {
        write: Arc::new(Mutex::new(write)),
        read,
    })
}

/// Convert a `ws://` or `wss://` URL to the equivalent HTTP scheme for
/// StreamableHTTP fallback.
fn ws_url_to_http(url: &str) -> String {
    if url.starts_with("ws://") {
        url.replacen("ws://", "http://", 1)
    } else if url.starts_with("wss://") {
        url.replacen("wss://", "https://", 1)
    } else {
        url.to_string()
    }
}

/// WebSocket MCP transport with transparent StreamableHTTP fallback.
///
/// Strategy:
/// 1. Attempt a direct WebSocket connection via `tokio_tungstenite`.
/// 2. On success, wrap the stream in `WebSocketTransport` and serve via rmcp.
/// 3. On failure (connection refused, upgrade not supported, DNS error, …),
///    convert `ws://` → `http://` (or `wss://` → `https://`) and fall back
///    to the StreamableHTTP transport.
///
/// Custom headers from the MCP config are forwarded to the StreamableHTTP
/// fallback but *not* to the WebSocket upgrade attempt — `connect_async`
/// takes a URL, not a full request.
async fn spawn_websocket_service(
    server_name: &str,
    url: &str,
    headers: &std::collections::HashMap<String, String>,
) -> Result<rmcp::service::RunningService<rmcp::RoleClient, ()>, McpError> {
    // 1. Try WebSocket first
    match try_ws_connect(url, headers).await {
        Ok(transport) => {
            info!(
                server = %server_name,
                url = %url,
                "Connected to MCP server via WebSocket transport"
            );
            return ().serve(transport).await.map_err(|e| McpError::ConnectFailed {
                name: server_name.into(),
                source: anyhow::anyhow!("{e}"),
            });
        }
        Err(e) => {
            warn!(
                server = %server_name,
                url = %url,
                error = %e,
                "WebSocket connection failed; falling back to StreamableHTTP"
            );
        }
    }

    // 2. Fall back to StreamableHTTP
    let http_url = ws_url_to_http(url);
    info!(
        server = %server_name,
        ws_url = %url,
        http_url = %http_url,
        "WebSocket unavailable; falling back to StreamableHTTP transport"
    );
    spawn_streamable_http_service(server_name, &http_url, headers, None).await
}

// ── SSE (Server-Sent Events) transport ──
// P2-7: Pure SSE transport. Delegates to streamable HTTP as fallback.
async fn spawn_sse_service(
    server_name: &str,
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    oauth_provider: Option<&str>,
) -> Result<rmcp::service::RunningService<rmcp::RoleClient, ()>, McpError> {
    tracing::info!(server = %server_name, url = %url, "SSE transport: delegating to streamable HTTP");
    spawn_streamable_http_service(server_name, url, headers, oauth_provider).await
}

// ── WebSocketMcpClient ──

/// MCP client connected via WebSocket transport (with StreamableHTTP fallback).
///
/// This is a thin convenience wrapper. `connect()` attempts a WebSocket
/// connection first; if the server does not support the WebSocket upgrade,
/// it falls back to StreamableHTTP. All `McpClient` methods delegate to the
/// underlying connection.
///
/// The `transport_kind()` always reports `"web_socket"` regardless of
/// whether the actual transport is native WebSocket or the StreamableHTTP
/// fallback.
pub struct WebSocketMcpClient {
    inner: Arc<StdioMcpClient>,
}

impl WebSocketMcpClient {
    /// Connect to an MCP server configured with WebSocket transport.
    ///
    /// Attempts WebSocket first, then StreamableHTTP on failure.
    pub async fn connect(
        server_name: &str,
        config: &McpServerConfig,
    ) -> Result<Arc<Self>, McpError> {
        let inner = StdioMcpClient::connect(server_name, config).await?;
        Ok(Arc::new(Self { inner }))
    }

    /// Shutdown the WebSocket connection.
    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

#[async_trait]
impl McpClient for WebSocketMcpClient {
    fn server_name(&self) -> &str {
        self.inner.server_name()
    }

    fn transport_kind(&self) -> &'static str {
        "web_socket"
    }

    fn instructions(&self) -> Option<&str> {
        self.inner.instructions()
    }

    async fn list_tools(&self) -> Result<Vec<McpToolMeta>, McpError> {
        self.inner.list_tools().await
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        args: serde_json::Map<String, serde_json::Value>,
    ) -> Result<McpCallResult, McpError> {
        self.inner.call_tool(tool_name, args).await
    }

    async fn list_resources(&self) -> Result<Vec<McpResourceMeta>, McpError> {
        self.inner.list_resources().await
    }

    async fn read_resource(&self, uri: &str) -> Result<Vec<McpContent>, McpError> {
        self.inner.read_resource(uri).await
    }

    async fn list_prompts(&self) -> Result<Vec<McpPromptMeta>, McpError> {
        self.inner.list_prompts().await
    }

    async fn get_prompt(
        &self,
        prompt_name: &str,
        args: &std::collections::HashMap<String, String>,
    ) -> Result<String, McpError> {
        self.inner.get_prompt(prompt_name, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn is_transport_error_for_not_connected() {
        let e = McpError::NotConnected { name: "x".into() };
        assert!(is_transport_error(&e));
    }

    #[test]
    fn is_transport_error_for_transport_variant() {
        let e = McpError::Transport(anyhow::anyhow!("any"));
        assert!(is_transport_error(&e));
    }

    #[test]
    fn is_transport_error_for_rmcp_with_eof_msg() {
        let e = McpError::RmcpService("connection closed by peer".into());
        assert!(is_transport_error(&e));
        let e = McpError::RmcpService("read EOF unexpectedly".into());
        assert!(is_transport_error(&e));
        let e = McpError::RmcpService("broken pipe writing to child".into());
        assert!(is_transport_error(&e));
    }

    #[test]
    fn is_transport_error_skips_genuine_rpc_errors() {
        // 业务错误：tool 抛 ValidationError —— 不该重连
        let e = McpError::RmcpService("tool returned validation error: bad arg".into());
        assert!(!is_transport_error(&e));
    }

    #[test]
    fn is_transport_error_skips_unknown_tool_and_schema() {
        let e = McpError::UnknownTool {
            name: "x".into(),
            tool: "y".into(),
        };
        assert!(!is_transport_error(&e));
        let e = McpError::Schema(serde_json::from_str::<()>("not json").unwrap_err());
        assert!(!is_transport_error(&e));
    }

    // ---- is_auth_error tests ----

    #[test]
    fn is_auth_error_for_401() {
        let e = McpError::RmcpService("HTTP 401 Unauthorized".into());
        assert!(is_auth_error(&e));
    }

    #[test]
    fn is_auth_error_for_forbidden() {
        let e = McpError::RmcpService("403 Forbidden: access denied".into());
        assert!(is_auth_error(&e));
    }

    #[test]
    fn is_auth_error_for_token_expired() {
        let e = McpError::RmcpService("token expired, please re-authenticate".into());
        assert!(is_auth_error(&e));
    }

    #[test]
    fn is_auth_error_skips_transport_errors() {
        // transport 错不该被误判为 auth
        let e = McpError::RmcpService("connection closed by peer".into());
        assert!(!is_auth_error(&e));
        let e = McpError::RmcpService("broken pipe".into());
        assert!(!is_auth_error(&e));
    }

    #[test]
    fn is_auth_error_skips_unknown_tool_and_not_connected() {
        let e = McpError::UnknownTool {
            name: "x".into(),
            tool: "y".into(),
        };
        assert!(!is_auth_error(&e));
        let e = McpError::NotConnected { name: "x".into() };
        assert!(!is_auth_error(&e));
    }

    #[test]
    fn is_auth_error_for_rmcp_auth_failure_message() {
        let e = McpError::RmcpService("authentication failure: invalid token".into());
        assert!(is_auth_error(&e));
    }

    #[test]
    fn is_auth_error_for_rmcp_access_denied() {
        let e = McpError::RmcpService("access denied: insufficient permissions".into());
        assert!(is_auth_error(&e));
    }

    #[test]
    fn transport_kind_reports_stdio_for_stdio_config() {
        let client = StdioMcpClient {
            server_name: "x".into(),
            config: McpServerConfig::Stdio {
                command: "echo".into(),
                args: Vec::new(),
                env: HashMap::new(),
                scope: None,
            },
            instructions: None,
            inner: tokio::sync::Mutex::new(None),
            last_reconnect_at: tokio::sync::Mutex::new(None),
            consecutive_failures: AtomicU32::new(0),
        };
        assert_eq!(client.transport_kind(), "stdio");
    }

    #[test]
    fn transport_kind_reports_streamable_http_for_http_config() {
        let client = StdioMcpClient {
            server_name: "x".into(),
            config: McpServerConfig::StreamableHttp {
                url: "http://example.com/mcp".into(),
                headers: HashMap::new(),
                oauth_provider: None,
                scope: None,
            },
            instructions: None,
            inner: tokio::sync::Mutex::new(None),
            last_reconnect_at: tokio::sync::Mutex::new(None),
            consecutive_failures: AtomicU32::new(0),
        };
        assert_eq!(client.transport_kind(), "streamable_http");
    }

    #[tokio::test]
    async fn reconnect_with_failing_command_stops_after_max_failures() {
        // 用一个"启动后立即退出"的 fake 命令模拟 server 永远连不上
        // 这里直接构造一个 StdioMcpClient 而非走真 connect（避开 rmcp 握手）
        let client = StdioMcpClient {
            server_name: "fake".into(),
            config: McpServerConfig::Stdio {
                command: "/nonexistent/binary/that/should/fail".into(),
                args: Vec::new(),
                env: HashMap::new(),
                scope: None,
            },
            instructions: None,
            inner: tokio::sync::Mutex::new(None),
            last_reconnect_at: tokio::sync::Mutex::new(None),
            consecutive_failures: AtomicU32::new(0),
        };
        // 三次失败后 try_reconnect 应当 short-circuit NotConnected，不再 spawn
        let mut last_err = None;
        for _ in 0..MAX_CONSECUTIVE_FAILURES + 2 {
            last_err = Some(client.try_reconnect().await.unwrap_err());
        }
        // 最后一次错误是 NotConnected（短路返回）
        assert!(matches!(last_err.unwrap(), McpError::NotConnected { .. }));
        // failures 计数应当卡在 MAX_CONSECUTIVE_FAILURES（之后 short-circuit 不再加）
        assert_eq!(
            client.consecutive_failures.load(Ordering::SeqCst),
            MAX_CONSECUTIVE_FAILURES
        );
    }

    // ── WebSocket tests ──

    #[test]
    fn ws_url_to_http_converts_ws_to_http() {
        assert_eq!(
            ws_url_to_http("ws://localhost:8080/mcp"),
            "http://localhost:8080/mcp"
        );
    }

    #[test]
    fn ws_url_to_http_converts_wss_to_https() {
        assert_eq!(
            ws_url_to_http("wss://example.com/mcp"),
            "https://example.com/mcp"
        );
    }

    #[test]
    fn ws_url_to_http_preserves_non_ws() {
        assert_eq!(ws_url_to_http("http://example.com"), "http://example.com");
        assert_eq!(ws_url_to_http("https://example.com"), "https://example.com");
        assert_eq!(ws_url_to_http(""), "");
    }

    #[test]
    fn transport_kind_reports_web_socket_for_ws_config_via_stdio_client() {
        let client = StdioMcpClient {
            server_name: "ws-server".into(),
            config: McpServerConfig::WebSocket {
                url: "ws://localhost:8080/mcp".into(),
                headers: HashMap::new(),
                scope: None,
            },
            instructions: None,
            inner: tokio::sync::Mutex::new(None),
            last_reconnect_at: tokio::sync::Mutex::new(None),
            consecutive_failures: AtomicU32::new(0),
        };
        assert_eq!(client.transport_kind(), "web_socket");
    }

    #[test]
    fn websocket_mcp_client_transport_kind_is_always_web_socket() {
        let client = WebSocketMcpClient {
            inner: Arc::new(StdioMcpClient {
                server_name: "ws-server".into(),
                config: McpServerConfig::WebSocket {
                    url: "ws://localhost:8080/mcp".into(),
                    headers: HashMap::new(),
                    scope: None,
                },
                instructions: None,
                inner: tokio::sync::Mutex::new(None),
                last_reconnect_at: tokio::sync::Mutex::new(None),
                consecutive_failures: AtomicU32::new(0),
            }),
        };
        assert_eq!(client.transport_kind(), "web_socket");
        assert_eq!(client.server_name(), "ws-server");
    }

    #[test]
    fn websocket_mcp_client_delegates_transport_kind_override() {
        // Verify that WebSocketMcpClient always reports "web_socket"
        // even when the inner client would report something different.
        let client = WebSocketMcpClient {
            inner: Arc::new(StdioMcpClient {
                server_name: "mock".into(),
                config: McpServerConfig::Stdio {
                    command: "echo".into(),
                    args: vec![],
                    env: HashMap::new(),
                    scope: None,
                },
                instructions: Some("test".into()),
                inner: tokio::sync::Mutex::new(None),
                last_reconnect_at: tokio::sync::Mutex::new(None),
                consecutive_failures: AtomicU32::new(0),
            }),
        };
        // The inner StdioMcpClient would report "stdio",
        // but WebSocketMcpClient's override should still report "web_socket"
        assert_eq!(client.transport_kind(), "web_socket");
        assert_eq!(client.server_name(), "mock");
        assert_eq!(client.instructions(), Some("test"));
    }
}

// ── SSE (Server-Sent Events) transport ──
// P2-7: Pure SSE transport. Currently delegates to streamable HTTP as fallback.
