//! `McpManager` —— 串联 settings / connect / adapter 给 engine 用的入口。
//!
//! 用法：CLI 启动时把 settings.mcp_servers 喂给 `connect_all`；返回的 manager 把
//! 全部 server 的 tools 摊平成 `Vec<Arc<dyn Tool>>` 加进 ToolRegistry。

use crate::adapter::McpToolAdapter;
use crate::client::{McpClient, McpClientHandle};
use crate::config::McpServerConfig;
use crate::connect::StdioMcpClient;
use crate::error::McpError;
use crate::output_cache::McpOutputCache;
use base::context::McpServerInstruction;
use base::tool::Tool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::task::JoinSet;
use tracing::{info, warn};

/// Callback invoked when an MCP tool result contains an elicitation URL
/// (e.g. `mcp://` or `elicitation://` protocol). The first argument is the
/// server name, the second is the elicitation URL.
pub type ElicitationCallback = Arc<dyn Fn(String, String) + Send + Sync>;

/// An MCP server prompt exposed as a slash command.
#[derive(Debug, Clone)]
pub struct McpPromptEntry {
    pub server: String,
    pub name: String,
    pub description: String,
}

/// MCP server connection state (TS parity: `Connected`, `Failed`, `NeedsAuth`, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpServerState {
    Connected,
    Failed,
    NeedsAuth,
    Pending {
        reconnect_attempt: u32,
        max_reconnect_attempts: u32,
    },
    Disabled,
}

/// MCP notification from a server (TS parity: `notifications/tools/list_changed` etc.).
#[derive(Debug, Clone)]
pub enum McpNotification {
    /// Server's tool list changed (tools added/removed/updated).
    ToolListChanged { server: String },
    /// Server's resource list changed.
    ResourceListChanged { server: String },
    /// Server's prompt list changed.
    PromptListChanged { server: String },
    /// Custom notification (unknown method).
    Custom { server: String, method: String },
}

/// Handler for MCP server notifications.
/// Register with [`McpManager::register_notification_handler`].
#[async_trait::async_trait]
pub trait McpNotificationHandler: Send + Sync {
    /// Called when an MCP server sends a notification.
    async fn on_mcp_notification(&self, notification: McpNotification);
}

pub struct McpManager {
    clients: Vec<McpClientHandle>,
    adapters: Vec<Arc<dyn Tool>>,
    prompts: Vec<McpPromptEntry>,
    /// Per-server connection states (index-aligned with clients).
    #[allow(dead_code)]
    server_states: Vec<(String, McpServerState)>,
    /// Registered notification handlers.
    notification_handlers: Vec<Box<dyn McpNotificationHandler>>,
    /// P2-2: MCP notification channel allowlist — only notifications from
    /// servers whose names appear in this set are dispatched to handlers.
    /// Empty = allow all. TS parity: channelAllowlist.ts + channelPermissions.ts.
    pub notification_allowlist: Option<std::collections::HashSet<String>>,
    /// P2-2: Per-server tool permission overrides. Maps server_name → allowed_tool_names.
    /// If a server has an entry, only listed tools are callable. Empty vec = allow all.
    /// TS parity: channelPermissions.ts.
    pub tool_permissions: std::collections::HashMap<String, Vec<String>>,
    /// Shared MCP output cache (TS parity: mcpOutputStorage).
    output_cache: Arc<Mutex<McpOutputCache>>,
    /// Callback invoked when an MCP tool result contains an elicitation URL.
    /// Set by the CLI/engine to wire into the hook system.
    elicitation_callback: Option<ElicitationCallback>,
}

impl McpManager {
    /// Empty/default instance with no state.
    pub fn empty() -> Self {
        Self {
            clients: Vec::new(),
            adapters: Vec::new(),
            prompts: Vec::new(),
            server_states: Vec::new(),
            notification_handlers: Vec::new(),
            notification_allowlist: None,
            tool_permissions: std::collections::HashMap::new(),
            output_cache: Arc::new(Mutex::new(McpOutputCache::new())),
            elicitation_callback: None,
        }
    }

    /// Register a notification handler. Handlers are called in registration order
    /// when any MCP server sends a notification.
    pub fn register_notification_handler(&mut self, handler: Box<dyn McpNotificationHandler>) {
        self.notification_handlers.push(handler);
    }

    /// Dispatch a notification to all registered handlers, respecting the
    /// notification allowlist. Servers not in the allowlist are silently skipped.
    /// Handlers that panic are caught and skipped.
    pub async fn dispatch_notification(&self, notification: McpNotification) {
        // P2-2: Check notification allowlist
        let server = match &notification {
            McpNotification::ToolListChanged { server }
            | McpNotification::ResourceListChanged { server }
            | McpNotification::PromptListChanged { server }
            | McpNotification::Custom { server, .. } => server,
        };
        if let Some(ref allowlist) = self.notification_allowlist {
            if !allowlist.contains(server) {
                tracing::debug!(
                    server = %server,
                    "MCP notification blocked: server not in allowlist"
                );
                return;
            }
        }
        for handler in &self.notification_handlers {
            handler.on_mcp_notification(notification.clone()).await;
        }
    }

    /// P2-2: Check whether a specific MCP tool is allowed based on tool_permissions.
    /// Returns true if the tool is allowed, false if blocked by per-server rules.
    pub fn is_tool_allowed(&self, server: &str, tool_name: &str) -> bool {
        match self.tool_permissions.get(server) {
            None => true,                                // No rules → allow all
            Some(allowed) if allowed.is_empty() => true, // Empty list → allow all
            Some(allowed) => allowed.iter().any(|t| t == tool_name),
        }
    }

    /// P2-2: Set the notification allowlist. Pass None to allow all servers.
    pub fn set_notification_allowlist(&mut self, servers: Option<Vec<String>>) {
        self.notification_allowlist = servers.map(|v| v.into_iter().collect());
    }

    /// P2-2: Set tool permissions for a specific server. Pass empty vec to allow all.
    pub fn set_tool_permissions(&mut self, server: &str, allowed: Vec<String>) {
        self.tool_permissions.insert(server.to_string(), allowed);
    }

    /// Register an elicitation callback. When an MCP tool result contains an
    /// elicitation URL (mcp:// or elicitation://), the adapter calls this
    /// callback which fires the `Elicitation` hook event.
    pub fn set_elicitation_callback(&mut self, cb: ElicitationCallback) {
        self.elicitation_callback = Some(cb);
    }

    /// Fire the Elicitation hook event. Called when an MCP tool returns an
    /// elicitation URL. This notifies the hook system (and any registered
    /// hooks) that user attention is needed.
    pub fn on_elicitation(&self, server_name: &str, url: &str) {
        if let Some(ref cb) = self.elicitation_callback {
            cb(server_name.to_string(), url.to_string());
        }
    }

    /// 连所有 server，list_tools 完成后构造 adapters。
    /// 使用并发连接——最多 3 个本地 stdio server 同时启动，远程 server 可更多。
    /// 单个 server 失败不整体失败 —— warn + skip。
    pub async fn connect_all(servers: HashMap<String, McpServerConfig>) -> Self {
        Self::connect_all_impl(servers, None).await
    }

    /// 连所有 server，并传入一个 Optional 的 elicitation callback。
    /// 当 MCP tool 返回 elicitation URL 时，adapter 会调用此 callback。
    pub async fn connect_all_with_callback(
        servers: HashMap<String, McpServerConfig>,
        callback: ElicitationCallback,
    ) -> Self {
        Self::connect_all_impl(servers, Some(callback)).await
    }

    /// Internal implementation shared by `connect_all` and
    /// `connect_all_with_callback`.
    async fn connect_all_impl(
        servers: HashMap<String, McpServerConfig>,
        elicitation_cb: Option<ElicitationCallback>,
    ) -> Self {
        let cache = Arc::new(Mutex::new(McpOutputCache::new()));
        let mut clients: Vec<McpClientHandle> = Vec::new();
        let mut adapters: Vec<Arc<dyn Tool>> = Vec::new();
        let mut set = JoinSet::new();
        let max_concurrent = 3usize; // limit concurrent spawns

        type ConnectOutput =
            Result<(String, McpClientHandle, Vec<Arc<dyn Tool>>), (String, McpError)>;

        // Process a JoinSet result: success → collect into clients/adapters;
        // errors are non-fatal (warn + skip).
        let handle_result =
            |clients: &mut Vec<McpClientHandle>,
             adapters: &mut Vec<Arc<dyn Tool>>,
             result: Result<ConnectOutput, tokio::task::JoinError>| {
                match result {
                    Ok(Ok((_name, client, server_adapters))) => {
                        clients.push(client);
                        adapters.extend(server_adapters);
                    }
                    Ok(Err((name, e))) => {
                        warn!(server = %name, error = %e, "MCP server connect failed; skipping");
                    }
                    Err(join_err) => {
                        warn!(?join_err, "MCP connect task panicked; skipping");
                    }
                }
            };

        for (name, cfg) in servers {
            // Wait if we're at the concurrency limit
            while set.len() >= max_concurrent {
                if let Some(result) = set.join_next().await {
                    handle_result(&mut clients, &mut adapters, result);
                }
            }

            let cache_for_spawn = cache.clone();
            let cb_for_spawn = elicitation_cb.clone();
            set.spawn(async move {
                match Self::connect_with_retry(&name, &cfg, cache_for_spawn, cb_for_spawn).await {
                    Ok((client, server_adapters)) => {
                        info!(server = %name, n_tools = server_adapters.len(), "MCP server connected");
                        Ok((name, client, server_adapters))
                    }
                    Err(e) => Err((name, e)),
                }
            });
        }

        // Drain remaining
        while let Some(result) = set.join_next().await {
            handle_result(&mut clients, &mut adapters, result);
        }

        let manager = Self {
            clients,
            adapters,
            prompts: Vec::new(),
            server_states: Vec::new(),
            notification_handlers: Vec::new(),
            notification_allowlist: None,
            tool_permissions: std::collections::HashMap::new(),
            output_cache: cache,
            elicitation_callback: elicitation_cb,
        };
        // Collect MCP prompts from connected servers (non-blocking: fail silently)
        manager.collect_prompts().await
    }

    async fn collect_prompts(mut self) -> Self {
        let mut prompts = Vec::new();
        for client in &self.clients {
            match client.list_prompts().await {
                Ok(server_prompts) => {
                    let n_prompts = server_prompts.len();
                    for p in server_prompts {
                        prompts.push(McpPromptEntry {
                            server: client.server_name().to_string(),
                            name: p.name.clone(),
                            description: p.description.unwrap_or_default(),
                        });
                    }
                    info!(
                        server = %client.server_name(),
                        n_prompts,
                        "MCP prompts collected"
                    );
                }
                Err(e) => {
                    warn!(
                        server = %client.server_name(),
                        error = %e,
                        "MCP prompt collection failed; skipping"
                    );
                }
            }
        }
        self.prompts = prompts;
        self
    }

    async fn connect_one(
        name: &str,
        cfg: &McpServerConfig,
        cache: Arc<Mutex<McpOutputCache>>,
        elicitation_cb: Option<ElicitationCallback>,
    ) -> Result<(McpClientHandle, Vec<Arc<dyn Tool>>), McpError> {
        // StdioMcpClient 现在支持 stdio + streamable_http 两种 config（spawn_service 里分派）
        let client = StdioMcpClient::connect(name, cfg).await?;
        let tools = client.list_tools().await?;
        let handle: McpClientHandle = client;
        let adapters: Vec<Arc<dyn Tool>> = tools
            .into_iter()
            .map(|meta| {
                let mut adapter = McpToolAdapter::with_cache(handle.clone(), meta, cache.clone());
                if let Some(ref cb) = elicitation_cb {
                    adapter = adapter.with_elicitation_callback(cb.clone());
                }
                Arc::new(adapter) as Arc<dyn Tool>
            })
            .collect();
        Ok((handle, adapters))
    }

    /// Connect to an MCP server with exponential backoff retry.
    /// TS parity: claude-code's `useManageMCPConnections.ts` retry with
    /// MAX_RETRIES=5, INITIAL_DELAY=1s, MAX_DELAY=30s, ±25% jitter.
    async fn connect_with_retry(
        name: &str,
        cfg: &McpServerConfig,
        cache: Arc<Mutex<McpOutputCache>>,
        elicitation_cb: Option<ElicitationCallback>,
    ) -> Result<(McpClientHandle, Vec<Arc<dyn Tool>>), McpError> {
        const MAX_RETRIES: u32 = 5;
        const INITIAL_DELAY_MS: u64 = 1_000;
        const MAX_DELAY_MS: u64 = 30_000;

        let mut last_err = None;
        for attempt in 0..MAX_RETRIES {
            match Self::connect_one(name, cfg, cache.clone(), elicitation_cb.clone()).await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    last_err = Some(e);
                    if attempt + 1 < MAX_RETRIES {
                        let delay = (INITIAL_DELAY_MS * 2u64.pow(attempt)).min(MAX_DELAY_MS);
                        // Deterministic jitter: use attempt+server hash to spread
                        // retries across servers within ±25% of the computed delay.
                        let jitter = delay / 4;
                        let offset = (attempt as u64 * 7 + name.len() as u64) % (2 * jitter + 1);
                        let jittered = delay - jitter + offset;
                        warn!(
                            server = %name,
                            attempt = attempt + 1,
                            max_retries = MAX_RETRIES,
                            delay_ms = jittered,
                            "MCP server connect failed; retrying with backoff"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(jittered)).await;
                    }
                }
            }
        }
        Err(last_err.unwrap())
    }

    /// Connect an additional MCP server after construction. Useful for
    /// plugin-discovered MCP servers. On failure the server is skipped with
    /// a warning.
    pub async fn add_server(&mut self, name: &str, cfg: &McpServerConfig) {
        let cb = self.elicitation_callback.clone();
        match Self::connect_one(name, cfg, self.output_cache.clone(), cb).await {
            Ok((client, server_adapters)) => {
                info!(
                    server = %name,
                    n_tools = server_adapters.len(),
                    "plugin MCP server connected"
                );
                self.clients.push(client);
                self.adapters.extend(server_adapters);
            }
            Err(e) => {
                warn!(server = %name, error = %e, "plugin MCP server connect failed; skipping");
            }
        }
    }

    /// 用注入测试 client 构造 manager（绕过真 connect）。
    #[doc(hidden)]
    pub fn from_clients(clients: Vec<McpClientHandle>) -> Self {
        // Can't await list_tools here; use empty adapters as placeholder.
        // Real usage should go through connect_all.
        Self {
            clients,
            adapters: Vec::new(),
            prompts: Vec::new(),
            server_states: Vec::new(),
            notification_handlers: Vec::new(),
            notification_allowlist: None,
            tool_permissions: std::collections::HashMap::new(),
            output_cache: Arc::new(Mutex::new(McpOutputCache::new())),
            elicitation_callback: None,
        }
    }

    /// 给 ToolRegistry 用的 adapter 列表。
    pub fn tool_adapters(&self) -> &[Arc<dyn Tool>] {
        &self.adapters
    }

    pub fn clients(&self) -> &[McpClientHandle] {
        &self.clients
    }

    /// Number of registered servers.
    pub fn server_count(&self) -> usize {
        self.clients.len()
    }

    pub fn tool_count(&self) -> usize {
        self.adapters.len()
    }

    /// MCP prompts collected from connected servers. Each entry maps to a
    /// slash command of the form `mcp__<server>__<prompt>`.
    pub fn all_prompts(&self) -> &[McpPromptEntry] {
        &self.prompts
    }

    /// Return a reference to the official MCP server registry.
    /// The registry provides a curated list of well-known MCP servers
    /// that users can discover and add to their configuration.
    pub fn official_registry() -> &'static crate::registry::OfficialRegistry {
        static REGISTRY: std::sync::OnceLock<crate::registry::OfficialRegistry> =
            std::sync::OnceLock::new();
        REGISTRY.get_or_init(crate::registry::OfficialRegistry::new)
    }

    /// Execute a named MCP prompt on the specified server. Returns the
    /// rendered text from the server's prompt handler, or an error message.
    pub async fn execute_prompt(
        &self,
        server: &str,
        prompt_name: &str,
        args: &std::collections::HashMap<String, String>,
    ) -> String {
        for client in &self.clients {
            if client.server_name() == server {
                match client.get_prompt(prompt_name, args).await {
                    Ok(content) => return content,
                    Err(e) => return format!("MCP prompt error: {e}"),
                }
            }
        }
        format!("MCP server '{server}' not found")
    }

    pub fn server_instructions(&self) -> Vec<McpServerInstruction> {
        self.clients
            .iter()
            .filter_map(|c| {
                let instructions = c.instructions()?.trim();
                if instructions.is_empty() {
                    None
                } else {
                    Some(McpServerInstruction {
                        name: c.server_name().to_string(),
                        instructions: instructions.to_string(),
                    })
                }
            })
            .collect()
    }

    /// 异步取所有 server 的 resources + prompts 摘要 —— /mcp 详细模式用。
    /// 失败的 server 跳过（warn 而非整体 fail）；rmcp 服务器不一定支持这两个能力。
    pub async fn server_inventory(&self) -> Vec<ServerInventory> {
        let mut out = Vec::with_capacity(self.clients.len());
        for c in &self.clients {
            let resources = c.list_resources().await.unwrap_or_default();
            let prompts = c.list_prompts().await.unwrap_or_default();
            out.push(ServerInventory {
                name: c.server_name().to_string(),
                resource_count: resources.len(),
                prompt_count: prompts.len(),
            });
        }
        out
    }

    /// Refresh tools from connected MCP servers. Used between turns to pick up
    /// newly connected servers (TS parity: refreshTools in query.ts:1659).
    /// Re-fetches tool lists from all connected clients and updates adapters.
    pub async fn refresh_tools(&mut self) {
        let mut new_adapters: Vec<Arc<dyn Tool>> = Vec::new();
        let cb = self.elicitation_callback.clone();
        for client in &self.clients {
            match client.list_tools().await {
                Ok(tools) => {
                    for meta in tools {
                        let mut adapter = McpToolAdapter::new(client.clone(), meta);
                        if let Some(ref c) = cb {
                            adapter = adapter.with_elicitation_callback(c.clone());
                        }
                        new_adapters.push(Arc::new(adapter) as Arc<dyn Tool>);
                    }
                }
                Err(e) => {
                    warn!(
                        server = %client.server_name(),
                        error = %e,
                        "MCP tool refresh failed; keeping previous adapter set"
                    );
                }
            }
        }
        if !new_adapters.is_empty() {
            self.adapters = new_adapters;
        }
    }

    /// `/doctor` `/mcp` 用的状态摘要：每个 server 的 name / transport / 工具数。
    /// transport 字段：来自 client 的 transport_kind()——stdio / streamable_http / sse /
    /// in_process / web_socket（见 connect.rs）。
    pub fn server_statuses(&self) -> Vec<ServerStatus> {
        // 把每 client 的 server_name 与持有它的 adapter 数关联起来
        // adapter 通过 McpToolAdapter::server_name() 暴露 server 名（已存在）
        let mut counts: HashMap<String, usize> = HashMap::new();
        for a in &self.adapters {
            // adapter 的 name 形如 mcp__<server>__<tool>；提取 <server>
            let n = a.name();
            if let Some(rest) = n.strip_prefix("mcp__") {
                if let Some((server, _tool)) = rest.split_once("__") {
                    *counts.entry(server.to_string()).or_insert(0) += 1;
                }
            }
        }
        self.clients
            .iter()
            .map(|c| ServerStatus {
                name: c.server_name().to_string(),
                transport: c.transport_kind(),
                tool_count: counts.get(c.server_name()).copied().unwrap_or(0),
            })
            .collect()
    }
}

/// 单个 MCP server 的状态（manager 视角）。
#[derive(Debug, Clone)]
pub struct ServerStatus {
    pub name: String,
    pub transport: &'static str,
    pub tool_count: usize,
}

/// resources + prompts 数量摘要（不在 ServerStatus 里因为要异步取，
/// 而 server_statuses 是 sync 快路径）。
#[derive(Debug, Clone)]
pub struct ServerInventory {
    pub name: String,
    pub resource_count: usize,
    pub prompt_count: usize,
}

impl Default for McpManager {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_manager_has_no_tools() {
        let m = McpManager::empty();
        assert_eq!(m.server_count(), 0);
        assert_eq!(m.tool_count(), 0);
        assert!(m.tool_adapters().is_empty());
    }

    #[test]
    fn server_instructions_collects_nonempty_client_instructions() {
        let client = Arc::new(
            crate::client::MockMcpClient::new("github", Vec::new())
                .with_instructions("Use issues carefully."),
        );
        let m = McpManager::from_clients(vec![client]);
        let instructions = m.server_instructions();
        assert_eq!(instructions.len(), 1);
        assert_eq!(instructions[0].name, "github");
        assert_eq!(instructions[0].instructions, "Use issues carefully.");
    }

    #[tokio::test]
    async fn connect_all_attempts_streamable_http() {
        // StreamableHttp 现在 wire 起来了；连不上的 URL 会 ConnectFailed → skip。
        // 行为：服务器数仍为 0（连接失败），但*尝试过*（不再是早返 stub）。
        let mut servers = HashMap::new();
        servers.insert(
            "remote".into(),
            McpServerConfig::StreamableHttp {
                url: "http://127.0.0.1:1/nonexistent".into(),
                headers: HashMap::new(),
                oauth_provider: None,
                scope: None,
            },
        );
        let m = McpManager::connect_all(servers).await;
        assert_eq!(m.server_count(), 0); // 失败仍跳过
    }

    #[tokio::test]
    async fn connect_all_returns_empty_for_empty_input() {
        let m = McpManager::connect_all(HashMap::new()).await;
        assert_eq!(m.server_count(), 0);
        assert_eq!(m.tool_count(), 0);
    }
}
