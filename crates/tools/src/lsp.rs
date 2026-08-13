//! LspTool — "LSP".
//! Code intelligence via Language Server Protocol.
//!
//! Communicates with language servers via JSON-RPC 2.0 over stdio.
//! Each of the 9 operations sends the appropriate LSP request and
//! formats the response as human-readable text.

use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Timeout for the entire LSP request lifecycle (initialize + request + shutdown).
const LSP_TIMEOUT_MS: u64 = 15_000;

// ── Input ──

#[derive(Debug, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum LspOperation {
    GoToDefinition,
    FindReferences,
    Hover,
    DocumentSymbol,
    WorkspaceSymbol,
    GoToImplementation,
    PrepareCallHierarchy,
    IncomingCalls,
    OutgoingCalls,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LspInput {
    /// The LSP operation to perform.
    pub operation: LspOperation,

    /// The absolute or relative path to the file (required for all except workspaceSymbol).
    #[serde(default)]
    pub file_path: Option<String>,

    /// The line number (1-based, as shown in editors).
    pub line: Option<u32>,

    /// The character offset (1-based, as shown in editors).
    pub character: Option<u32>,

    /// Query string for workspaceSymbol search.
    #[serde(default)]
    pub query: Option<String>,
}

// ── JSON-RPC types ──

#[derive(Debug, Serialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    #[serde(default)]
    #[allow(dead_code)]
    id: Option<u64>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

// ── Tool struct ──

pub struct LspTool {
    manager: Option<LspManager>,
}

impl LspTool {
    /// Create an LspTool that uses the given pool for server reuse.
    pub fn new(manager: LspManager) -> Self {
        Self {
            manager: Some(manager),
        }
    }

    /// Create an LspTool without a pool — every call spawns and shuts down
    /// its own server process. Useful for tests and one-off invocations.
    pub fn ephemeral() -> Self {
        Self { manager: None }
    }

    /// Execute one request via the pool: acquire → execute → release on success,
    /// drop handle on error (so next call spawns a fresh server).
    async fn run_pooled(
        manager: &LspManager,
        pool_key: PoolKey,
        server_cmd: &str,
        root_path: &Path,
        method: &str,
        params: &Value,
    ) -> Result<String, String> {
        let mut handle = manager.acquire(server_cmd, root_path).await?;
        let result = execute_request_on_handle(&mut handle, method, params).await;
        match result {
            Ok(_) => {
                manager.release(pool_key, handle);
            }
            Err(_) => {
                // Don't return a potentially broken connection to the pool.
                // Let it drop — next acquire will spawn a fresh one.
                shutdown_handle(handle).await;
            }
        }
        result
    }
}

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &str {
        "LSP"
    }

    fn description(&self) -> &str {
        "Interact with Language Server Protocol (LSP) servers to get code intelligence features"
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(LspInput))
            .expect("schemars output is valid JSON")
    }

    fn is_deferred(&self) -> bool {
        true
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/lsp.prompt.md").to_string()
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let parsed: Result<LspInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) => {
                if p.operation == LspOperation::WorkspaceSymbol {
                    if p.query.as_ref().map_or(true, |q| q.trim().is_empty()) {
                        return ValidationResult::err("query is required for workspaceSymbol", 1);
                    }
                } else if p.file_path.as_ref().map_or(true, |f| f.trim().is_empty()) {
                    return ValidationResult::err("filePath is required for this operation", 2);
                }

                // Position-based operations need line and character.
                let needs_position = matches!(
                    p.operation,
                    LspOperation::GoToDefinition
                        | LspOperation::FindReferences
                        | LspOperation::Hover
                        | LspOperation::GoToImplementation
                        | LspOperation::PrepareCallHierarchy
                        | LspOperation::IncomingCalls
                        | LspOperation::OutgoingCalls
                );
                if needs_position && (p.line.is_none() || p.character.is_none()) {
                    return ValidationResult::err(
                        "line and character are required for this operation",
                        3,
                    );
                }

                ValidationResult::Ok
            }
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 4),
        }
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: LspInput = serde_json::from_value(input)?;
        let file_path = input.file_path.as_deref().unwrap_or("");

        // Determine the language and find the appropriate LSP server.
        let server_cmd = match detect_language_server(file_path) {
            Some(cmd) => cmd,
            None => {
                return Ok(ToolResult::error_text(format!(
                    "No LSP server configured for file type: {}. \
                     Configure an LSP server in your project settings or install one \
                     (e.g., rust-analyzer for Rust, typescript-language-server for TS).",
                    file_path
                )));
            }
        };

        // Build the LSP request based on the operation.
        let (method, params) = build_request(&input, &ctx.cwd);

        let timeout_dur = Duration::from_millis(LSP_TIMEOUT_MS);

        let result = if let Some(ref manager) = self.manager {
            // ── Pooled path ──
            let pool_key: PoolKey = (server_cmd.clone(), canonical_root(&ctx.cwd));
            let r = tokio::time::timeout(
                timeout_dur,
                Self::run_pooled(
                    manager,
                    pool_key.clone(),
                    &server_cmd,
                    &ctx.cwd,
                    &method,
                    &params,
                ),
            )
            .await;
            r
        } else {
            // ── Ephemeral path ──
            tokio::time::timeout(
                timeout_dur,
                execute_lsp_request(&server_cmd, &ctx.cwd, &method, &params),
            )
            .await
        };

        match result {
            Ok(Ok(response_text)) => Ok(ToolResult::text(response_text)),
            Ok(Err(e)) => Ok(ToolResult::error_text(e)),
            Err(_) => Err(ToolError::Timeout(timeout_dur)),
        }
    }
}

// ── LSP server detection ──

/// Detect the appropriate LSP server command for a file type.
fn detect_language_server(file_path: &str) -> Option<String> {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    match ext {
        "rs" => try_resolve(&["rust-analyzer"]),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" => {
            try_resolve(&["typescript-language-server", "--stdio"])
        }
        "py" => try_resolve(&["pyright-langserver", "--stdio"]),
        "go" => try_resolve(&["gopls"]),
        "vue" => try_resolve(&["vue-language-server", "--stdio"]),
        "css" | "scss" | "less" => try_resolve(&["vscode-css-language-server", "--stdio"]),
        "json" | "jsonc" => try_resolve(&["vscode-json-language-server", "--stdio"]),
        "html" | "htm" => try_resolve(&["vscode-html-language-server", "--stdio"]),
        "yaml" | "yml" => try_resolve(&["yaml-language-server", "--stdio"]),
        "toml" => try_resolve(&["taplo"]),
        "md" | "markdown" => try_resolve(&["marksman"]),
        "dart" => try_resolve(&["dart", "language-server", "--protocol=lsp"]),
        "sh" | "bash" | "zsh" => try_resolve(&["bash-language-server", "start"]),
        "rb" => try_resolve(&["solargraph", "stdio"]),
        "java" => try_resolve(&["jdtls"]),
        "kt" | "kts" => try_resolve(&["kotlin-language-server"]),
        "swift" => try_resolve(&["sourcekit-lsp"]),
        "c" | "h" | "cpp" | "hpp" | "cxx" | "hxx" | "cc" | "hh" => try_resolve(&["clangd"]),
        _ => None,
    }
}

/// Try to find an LSP server binary on PATH. Returns the full command string
/// if the first binary in the chain is found, otherwise returns None.
fn try_resolve(candidates: &[&str]) -> Option<String> {
    let binary = candidates.first().unwrap_or(&"");
    if which::which(binary).is_ok() {
        Some(candidates.join(" "))
    } else {
        None
    }
}

// ── Request building ──

fn build_request(input: &LspInput, cwd: &Path) -> (String, Value) {
    let file_uri = input.file_path.as_ref().map(|p| {
        let path = Path::new(p);
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        format!("file://{}", abs.display())
    });

    // Convert from 1-based (user-facing) to 0-based (LSP wire).
    let line = input.line.unwrap_or(1).saturating_sub(1);
    let character = input.character.unwrap_or(1).saturating_sub(1);

    match input.operation {
        LspOperation::GoToDefinition => (
            "textDocument/definition".into(),
            serde_json::json!({
                "textDocument": { "uri": file_uri },
                "position": { "line": line, "character": character }
            }),
        ),

        LspOperation::GoToImplementation => (
            "textDocument/implementation".into(),
            serde_json::json!({
                "textDocument": { "uri": file_uri },
                "position": { "line": line, "character": character }
            }),
        ),

        LspOperation::FindReferences => (
            "textDocument/references".into(),
            serde_json::json!({
                "textDocument": { "uri": file_uri },
                "position": { "line": line, "character": character },
                "context": { "includeDeclaration": false }
            }),
        ),

        LspOperation::Hover => (
            "textDocument/hover".into(),
            serde_json::json!({
                "textDocument": { "uri": file_uri },
                "position": { "line": line, "character": character }
            }),
        ),

        LspOperation::DocumentSymbol => (
            "textDocument/documentSymbol".into(),
            serde_json::json!({
                "textDocument": { "uri": file_uri }
            }),
        ),

        LspOperation::WorkspaceSymbol => (
            "workspace/symbol".into(),
            serde_json::json!({
                "query": input.query.as_deref().unwrap_or("")
            }),
        ),

        LspOperation::PrepareCallHierarchy => (
            "textDocument/prepareCallHierarchy".into(),
            serde_json::json!({
                "textDocument": { "uri": file_uri },
                "position": { "line": line, "character": character }
            }),
        ),

        LspOperation::IncomingCalls | LspOperation::OutgoingCalls => {
            let method = match input.operation {
                LspOperation::IncomingCalls => "callHierarchy/incomingCalls",
                LspOperation::OutgoingCalls => "callHierarchy/outgoingCalls",
                _ => unreachable!(),
            };
            (
                method.into(),
                serde_json::json!({
                    "item": {
                        "uri": file_uri,
                        "name": "",
                        "kind": 12,
                        "range": {
                            "start": { "line": line, "character": character },
                            "end": { "line": line, "character": character + 1 }
                        },
                        "selectionRange": {
                            "start": { "line": line, "character": character },
                            "end": { "line": line, "character": character + 1 }
                        }
                    }
                }),
            )
        }
    }
}

// ── Process pool ──

/// Key for the LSP server pool: (server_command, canonical_root_path).
type PoolKey = (String, PathBuf);

/// A live, initialized LSP server connection.
pub(crate) struct LspHandle {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    #[allow(dead_code)]
    capabilities: Value,
    last_used: Instant,
    next_request_id: u64,
}

/// Process-pool manager for LSP servers. Clone-friendly (inner state behind `Arc<Mutex<>>`).
///
/// Created via [`LspTool::new`]; call [`LspManager::evict_idle`] periodically to
/// clean up stale connections.
#[derive(Clone)]
pub struct LspManager {
    inner: Arc<Mutex<LspManagerInner>>,
    idle_timeout: Duration,
}

struct LspManagerInner {
    servers: HashMap<PoolKey, LspHandle>,
}

impl LspManager {
    /// Create a new pool. `idle_timeout` of `Duration::ZERO` means never evict.
    pub fn new(idle_timeout: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LspManagerInner {
                servers: HashMap::new(),
            })),
            idle_timeout,
        }
    }

    /// Get or create an initialized LSP server for `(server_cmd, root_path)`.
    pub(crate) async fn acquire(
        &self,
        server_cmd: &str,
        root_path: &Path,
    ) -> Result<LspHandle, String> {
        let key: PoolKey = (server_cmd.to_string(), canonical_root(root_path));

        // Try to pull an existing handle from the pool (short lock).
        let existing = {
            let mut inner = self.inner.lock().unwrap();
            inner.servers.remove(&key)
        };

        if let Some(mut handle) = existing {
            match handle.child.try_wait() {
                Ok(None) => {
                    // Process still alive — reuse.
                    handle.last_used = Instant::now();
                    return Ok(handle);
                }
                _ => {
                    // Process died — drop and create a fresh one.
                    drop(handle);
                }
            }
        }

        // No usable handle in pool — spawn + initialize (long, lock-free).
        spawn_and_initialize(server_cmd, root_path).await
    }

    pub(crate) fn release(&self, key: PoolKey, mut handle: LspHandle) {
        handle.last_used = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        inner.servers.insert(key, handle);
    }

    /// Evict handles that have been idle longer than `idle_timeout`.
    /// Dropping the handle sends SIGKILL (via `kill_on_drop`).
    pub fn evict_idle(&self) {
        let mut inner = self.inner.lock().unwrap();
        if self.idle_timeout.is_zero() {
            return;
        }
        let now = Instant::now();
        inner.servers.retain(|_, h| {
            if now.duration_since(h.last_used) > self.idle_timeout {
                false // remove from map → LspHandle::drop → Child killed
            } else {
                true
            }
        });
    }

    /// Number of active connections in the pool (for tests).
    pub fn active_servers(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        inner.servers.len()
    }
}

fn canonical_root(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

// ── LSP communication ──

/// Spawn an LSP server, run the `initialize` handshake, and send `initialized`.
/// Returns the live handle ready for sending requests.
async fn spawn_and_initialize(server_cmd: &str, root_path: &Path) -> Result<LspHandle, String> {
    let parts: Vec<&str> = server_cmd.split_whitespace().collect();
    let mut cmd = Command::new(parts[0]);
    for arg in &parts[1..] {
        cmd.arg(arg);
    }
    cmd.current_dir(root_path);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to start LSP server '{}': {}", server_cmd, e))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Failed to open stdin for LSP server".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to open stdout for LSP server".to_string())?;

    // ── Initialize ──
    let init_req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: 1,
        method: "initialize".into(),
        params: Some(serde_json::json!({
            "processId": null,
            "rootUri": format!("file://{}", root_path.display()),
            "capabilities": {
                "textDocument": {
                    "hover": { "contentFormat": ["plaintext", "markdown"] },
                    "definition": { "linkSupport": true },
                    "references": {},
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "implementation": { "linkSupport": true },
                    "callHierarchy": {}
                },
                "workspace": {
                    "symbol": {}
                }
            }
        })),
    };
    send_lsp_message(&mut stdin, &init_req).await?;

    let mut reader = BufReader::new(stdout);
    let init_response: JsonRpcResponse = read_lsp_message(&mut reader).await?;
    let _ = init_response.error; // Many servers return partial capabilities — tolerate.

    // ── Initialized notification ──
    let notif = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    });
    let notif_str = serde_json::to_string(&notif).map_err(|e| format!("Serialize: {e}"))?;
    let header = format!("Content-Length: {}\r\n\r\n", notif_str.len());
    stdin
        .write_all(header.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stdin
        .write_all(notif_str.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stdin.flush().await.map_err(|e| e.to_string())?;

    Ok(LspHandle {
        child,
        stdin,
        stdout: reader,
        capabilities: init_response.result.unwrap_or_default(),
        last_used: Instant::now(),
        next_request_id: 2,
    })
}

/// Send one LSP request on an already-initialized handle and read the response.
async fn execute_request_on_handle(
    handle: &mut LspHandle,
    method: &str,
    params: &Value,
) -> Result<String, String> {
    let id = handle.next_request_id;
    handle.next_request_id += 1;

    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id,
        method: method.to_string(),
        params: Some(params.clone()),
    };
    send_lsp_message(&mut handle.stdin, &req).await?;

    let response: JsonRpcResponse = read_lsp_message(&mut handle.stdout).await?;

    match response.result {
        Some(result) => Ok(format_lsp_result(method, &result)),
        None => {
            if let Some(err) = response.error {
                Err(format!("LSP error: {} (code: {})", err.message, err.code))
            } else {
                Err("LSP returned empty result".to_string())
            }
        }
    }
}

/// Gracefully shut down an LSP server handle.
async fn shutdown_handle(mut handle: LspHandle) {
    let shutdown_req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: handle.next_request_id,
        method: "shutdown".into(),
        params: None,
    };
    let _ = send_lsp_message(&mut handle.stdin, &shutdown_req).await;
    drop(handle.stdin);
    let _ = handle.child.wait().await;
}

/// Ephemeral (non-pooled) LSP request: spawn, initialize, request, shutdown.
/// Used by `LspTool::ephemeral()` and as a fallback when no pool is available.
async fn execute_lsp_request(
    server_cmd: &str,
    cwd: &Path,
    method: &str,
    params: &Value,
) -> Result<String, String> {
    let mut handle = spawn_and_initialize(server_cmd, cwd).await?;
    let result = execute_request_on_handle(&mut handle, method, params).await;
    shutdown_handle(handle).await;
    result
}

/// Write a JSON-RPC message to the LSP server's stdin using the
/// Content-Length header format.
async fn send_lsp_message(
    stdin: &mut tokio::process::ChildStdin,
    msg: &impl Serialize,
) -> Result<(), String> {
    let body = serde_json::to_string(msg).map_err(|e| format!("Serialize error: {e}"))?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    stdin
        .write_all(header.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stdin
        .write_all(body.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stdin.flush().await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Read a single LSP message from the server's stdout.
/// Parses the Content-Length header, then reads exactly that many bytes.
async fn read_lsp_message<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<JsonRpcResponse, String> {
    let mut header_line = String::new();
    reader
        .read_line(&mut header_line)
        .await
        .map_err(|e| e.to_string())?;

    let content_length: usize = header_line
        .trim()
        .strip_prefix("Content-Length: ")
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| format!("Invalid LSP header: {}", header_line.trim()))?;

    // Read the blank line separating headers from the body.
    let mut blank = String::new();
    reader
        .read_line(&mut blank)
        .await
        .map_err(|e| e.to_string())?;

    // Read the exact body.
    let mut body = vec![0u8; content_length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| format!("Failed to read LSP body: {e}"))?;

    serde_json::from_slice::<JsonRpcResponse>(&body)
        .map_err(|e| format!("LSP response parse error: {e}"))
}

// ── Response formatting ──

fn format_lsp_result(method: &str, result: &Value) -> String {
    match method {
        "textDocument/definition" | "textDocument/implementation" => {
            format_definition_result(result)
        }
        "textDocument/references" => format_references_result(result),
        "textDocument/hover" => format_hover_result(result),
        "textDocument/documentSymbol" => format_symbols(result, 0),
        "workspace/symbol" => format_workspace_symbols(result),
        "textDocument/prepareCallHierarchy" => format_hierarchy_items(result, "call hierarchy"),
        "callHierarchy/incomingCalls" => format_call_hierarchy_calls(result, "incoming"),
        "callHierarchy/outgoingCalls" => format_call_hierarchy_calls(result, "outgoing"),
        _ => serde_json::to_string_pretty(result).unwrap_or_else(|_| format!("{:?}", result)),
    }
}

fn format_definition_result(result: &Value) -> String {
    // Some servers return a single Location, others a LocationLink array.
    if let Some(locations) = result.as_array() {
        if locations.is_empty() {
            return "No definition found.".to_string();
        }
        let lines: Vec<String> = locations.iter().filter_map(format_location).collect();
        format!("Found {} location(s):\n{}", lines.len(), lines.join("\n"))
    } else if result.is_object() {
        // Single location (Location object).
        if let Some(formatted) = format_location(result) {
            format!("Location: {}", formatted)
        } else {
            "No definition found.".to_string()
        }
    } else {
        "No definition found.".to_string()
    }
}

fn format_location(loc: &Value) -> Option<String> {
    // LSP LocationLink has `targetUri` / `targetRange` fields.
    // Plain Location has `uri` / `range`.
    let uri = loc
        .get("targetUri")
        .or_else(|| loc.get("uri"))
        .and_then(|v| v.as_str())?;
    let range = loc.get("targetRange").or_else(|| loc.get("range"))?;
    let path = uri.strip_prefix("file://").unwrap_or(uri);
    let start = range.get("start")?;
    let line = start
        .get("line")
        .and_then(|v| v.as_u64())
        .map(|l| l + 1)
        .unwrap_or(0);
    let col = start
        .get("character")
        .and_then(|v| v.as_u64())
        .map(|c| c + 1)
        .unwrap_or(0);
    Some(format!("  {}:{}:{}", path, line, col))
}

fn format_references_result(result: &Value) -> String {
    let Some(locations) = result.as_array() else {
        return "No references found.".to_string();
    };
    if locations.is_empty() {
        return "No references found.".to_string();
    }
    let lines: Vec<String> = locations
        .iter()
        .filter_map(|loc| {
            let uri = loc.get("uri").and_then(|v| v.as_str())?;
            let path = uri.strip_prefix("file://").unwrap_or(uri);
            let range = loc.get("range")?;
            let start = range.get("start")?;
            let line = start
                .get("line")
                .and_then(|v| v.as_u64())
                .map(|l| l + 1)
                .unwrap_or(0);
            let col = start
                .get("character")
                .and_then(|v| v.as_u64())
                .map(|c| c + 1)
                .unwrap_or(0);
            Some(format!("  {}:{}:{}", path, line, col))
        })
        .collect();
    format!("Found {} reference(s):\n{}", lines.len(), lines.join("\n"))
}

fn format_hover_result(result: &Value) -> String {
    // The `contents` field can be a MarkupContent, a MarkedString, or an
    // array of MarkedStrings.
    let contents = &result["contents"];
    if let Some(text) = contents.as_str() {
        return text.to_string();
    }
    if let Some(arr) = contents.as_array() {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(|v| {
                if let Some(s) = v.as_str() {
                    Some(s.to_string())
                } else if let Some(lang) = v.get("language").and_then(|l| l.as_str()) {
                    let value = v.get("value").and_then(|x| x.as_str()).unwrap_or("");
                    Some(format!("```{}\n{}\n```", lang, value))
                } else {
                    None
                }
            })
            .collect();
        if !parts.is_empty() {
            return parts.join("\n\n");
        }
    }
    if let Some(obj) = contents.as_object() {
        if let Some(kind) = obj.get("kind").and_then(|k| k.as_str()) {
            let value = obj.get("value").and_then(|v| v.as_str()).unwrap_or("");
            if kind == "markdown" {
                return value.to_string();
            }
            return format!("{}\n{}", kind, value);
        }
        if let Some(lang) = obj.get("language").and_then(|l| l.as_str()) {
            let value = obj.get("value").and_then(|v| v.as_str()).unwrap_or("");
            return format!("```{}\n{}\n```", lang, value);
        }
    }
    "(no hover info)".to_string()
}

fn format_symbols(value: &Value, depth: usize) -> String {
    let indent = "  ".repeat(depth);
    match value {
        Value::Array(arr) => arr
            .iter()
            .map(|s| {
                let name = s["name"].as_str().unwrap_or("?");
                let kind_num = s["kind"].as_u64().unwrap_or(0);
                let kind = symbol_kind_name(kind_num);
                let line = s["range"]["start"]["line"]
                    .as_u64()
                    .map(|l| l + 1)
                    .unwrap_or(0);
                let children = s
                    .get("children")
                    .map(|c| format_symbols(c, depth + 1))
                    .unwrap_or_default();
                if children.is_empty() {
                    format!("{}{} {} (line {})", indent, kind, name, line)
                } else {
                    format!("{}{} {} (line {})\n{}", indent, kind, name, line, children)
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn format_workspace_symbols(result: &Value) -> String {
    let count = result.as_array().map(|a| a.len()).unwrap_or(0);
    if count == 0 {
        return "No symbols found.".to_string();
    }
    let symbols: Vec<String> = result
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| {
                    let name = s["name"].as_str()?;
                    let kind_num = s["kind"].as_u64().unwrap_or(0);
                    let kind = symbol_kind_name(kind_num);
                    let uri = s["location"]["uri"].as_str()?;
                    let path = uri.strip_prefix("file://").unwrap_or(uri);
                    let line = s["location"]["range"]["start"]["line"]
                        .as_u64()
                        .map(|l| l + 1)
                        .unwrap_or(0);
                    let container = s
                        .get("containerName")
                        .and_then(|c| c.as_str())
                        .filter(|c| !c.is_empty());
                    match container {
                        Some(ctr) => {
                            Some(format!("  {} {} ({} — {}:{})", kind, name, ctr, path, line))
                        }
                        None => Some(format!("  {} {} — {}:{}", kind, name, path, line)),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    format!("Found {} symbol(s):\n{}", count, symbols.join("\n"))
}

fn format_hierarchy_items(result: &Value, label: &str) -> String {
    let Some(items) = result.as_array() else {
        return format!("No {} items found.", label);
    };
    if items.is_empty() {
        return format!("No {} items found.", label);
    }
    let lines: Vec<String> = items
        .iter()
        .filter_map(|item| {
            let name = item["name"].as_str()?;
            let kind_num = item["kind"].as_u64().unwrap_or(0);
            let kind = symbol_kind_name(kind_num);
            let uri = item["uri"].as_str()?;
            let path = uri.strip_prefix("file://").unwrap_or(uri);
            let detail = item
                .get("detail")
                .and_then(|d| d.as_str())
                .filter(|d| !d.is_empty());
            match detail {
                Some(d) => Some(format!("  {} {} ({}) — {}", kind, name, d, path)),
                None => Some(format!("  {} {} — {}", kind, name, path)),
            }
        })
        .collect();
    format!(
        "Found {} {} item(s):\n{}",
        lines.len(),
        label,
        lines.join("\n")
    )
}

fn format_call_hierarchy_calls(result: &Value, direction: &str) -> String {
    let Some(calls) = result.as_array() else {
        return format!("No {} calls found.", direction);
    };
    if calls.is_empty() {
        return format!("No {} calls found.", direction);
    }
    let lines: Vec<String> = calls
        .iter()
        .filter_map(|entry| {
            let from = entry.get("from")?;
            let name = from["name"].as_str()?;
            let kind_num = from["kind"].as_u64().unwrap_or(0);
            let kind = symbol_kind_name(kind_num);
            let uri = from["uri"].as_str()?;
            let path = uri.strip_prefix("file://").unwrap_or(uri);
            let range = from.get("selectionRange")?;
            let line = range["start"]["line"].as_u64().map(|l| l + 1).unwrap_or(0);
            // fromRanges shows the actual call sites.
            let from_ranges = entry
                .get("fromRanges")
                .and_then(|r| r.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|r| {
                            let start = r.get("start")?;
                            let l = start["line"].as_u64().map(|x| x + 1).unwrap_or(0);
                            Some(format!("(at line {})", l))
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let sites = if from_ranges.is_empty() {
                String::new()
            } else {
                format!(" {}", from_ranges.join(", "))
            };
            Some(format!("  {} {} — {}:{}{}", kind, name, path, line, sites))
        })
        .collect();
    format!(
        "Found {} {} call(s):\n{}",
        lines.len(),
        direction,
        lines.join("\n")
    )
}

/// Map LSP SymbolKind integer to a human-readable string.
fn symbol_kind_name(kind: u64) -> &'static str {
    match kind {
        1 => "File",
        2 => "Module",
        3 => "Namespace",
        4 => "Package",
        5 => "Class",
        6 => "Method",
        7 => "Property",
        8 => "Field",
        9 => "Constructor",
        10 => "Enum",
        11 => "Interface",
        12 => "Function",
        13 => "Variable",
        14 => "Constant",
        15 => "String",
        16 => "Number",
        17 => "Boolean",
        18 => "Array",
        19 => "Object",
        20 => "Key",
        21 => "Null",
        22 => "EnumMember",
        23 => "Struct",
        24 => "Event",
        25 => "Operator",
        26 => "TypeParameter",
        _ => "Unknown",
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_ctx() -> ToolContext {
        ToolContext::for_test("/tmp".into())
    }

    #[tokio::test]
    async fn validates_workspace_symbol_requires_query() {
        let tool = LspTool::ephemeral();
        let r = tool
            .validate_input(&json!({"operation": "workspaceSymbol"}), &test_ctx())
            .await;
        assert!(
            matches!(r, ValidationResult::Err(..)),
            "workspaceSymbol without query should fail"
        );
    }

    #[tokio::test]
    async fn validates_non_workspace_ops_require_filepath() {
        let tool = LspTool::ephemeral();
        let r = tool
            .validate_input(&json!({"operation": "goToDefinition"}), &test_ctx())
            .await;
        assert!(
            matches!(r, ValidationResult::Err(..)),
            "goToDefinition without filePath should fail"
        );
    }

    #[tokio::test]
    async fn validates_position_ops_require_line_and_character() {
        let tool = LspTool::ephemeral();
        let r = tool
            .validate_input(
                &json!({"operation": "hover", "filePath": "test.rs"}),
                &test_ctx(),
            )
            .await;
        assert!(
            matches!(r, ValidationResult::Err(..)),
            "hover without line/character should fail"
        );
    }

    #[tokio::test]
    async fn validates_document_symbol_does_not_need_position() {
        let tool = LspTool::ephemeral();
        let r = tool
            .validate_input(
                &json!({"operation": "documentSymbol", "filePath": "test.rs"}),
                &test_ctx(),
            )
            .await;
        assert!(
            matches!(r, ValidationResult::Ok),
            "documentSymbol should not need line/character: {:?}",
            r
        );
    }

    #[tokio::test]
    async fn validates_valid_input_passes() {
        let tool = LspTool::ephemeral();
        let r = tool
            .validate_input(
                &json!({
                    "operation": "goToDefinition",
                    "filePath": "/tmp/test.rs",
                    "line": 10,
                    "character": 5
                }),
                &test_ctx(),
            )
            .await;
        assert!(
            matches!(r, ValidationResult::Ok),
            "valid input should pass: {:?}",
            r
        );
    }

    #[tokio::test]
    async fn validates_workspace_symbol_with_query_passes() {
        let tool = LspTool::ephemeral();
        let r = tool
            .validate_input(
                &json!({"operation": "workspaceSymbol", "query": "myFunction"}),
                &test_ctx(),
            )
            .await;
        assert!(
            matches!(r, ValidationResult::Ok),
            "workspaceSymbol with query should pass: {:?}",
            r
        );
    }

    #[test]
    fn name_is_lsp() {
        let tool = LspTool::ephemeral();
        assert_eq!(tool.name(), "LSP");
        assert!(tool.is_read_only(&Value::Null));
        assert!(tool.is_concurrency_safe(&Value::Null));
        assert!(tool.is_deferred());
    }

    #[test]
    fn symbol_kind_mapping() {
        assert_eq!(symbol_kind_name(5), "Class");
        assert_eq!(symbol_kind_name(12), "Function");
        assert_eq!(symbol_kind_name(23), "Struct");
        assert_eq!(symbol_kind_name(99), "Unknown");
    }

    #[test]
    fn input_schema_is_valid_json_schema() {
        let tool = LspTool::ephemeral();
        let schema = tool.input_schema();
        assert!(schema.is_object(), "schema must be a JSON object");
        assert!(
            schema.get("properties").is_some(),
            "schema must have properties"
        );
    }

    #[test]
    fn try_resolve_nonexistent_returns_none() {
        let result = try_resolve(&["nonexistent-lsp-server-xyz"]);
        assert!(result.is_none());
    }

    #[test]
    fn detect_language_server_unknown_extension() {
        let result = detect_language_server("foo.xyz");
        assert!(result.is_none());
    }

    #[test]
    fn detect_language_server_rust_extension() {
        // rust-analyzer may not be installed in CI, but the function should
        // at least produce a candidate. We check the extension mapping,
        // not the availability on PATH.
        let _result = detect_language_server("lib.rs");
        // Acceptance: the mapping exists, even if the binary isn't on PATH.
    }

    #[test]
    fn format_location_from_object() {
        let loc = json!({
            "uri": "file:///home/user/project/src/main.rs",
            "range": {
                "start": { "line": 42, "character": 8 },
                "end": { "line": 42, "character": 16 }
            }
        });
        let result = format_location(&loc);
        assert_eq!(
            result,
            Some("  /home/user/project/src/main.rs:43:9".to_string())
        );
    }

    #[test]
    fn format_location_from_location_link() {
        let loc = json!({
            "targetUri": "file:///home/user/project/src/lib.rs",
            "targetRange": {
                "start": { "line": 10, "character": 0 },
                "end": { "line": 30, "character": 1 }
            },
            "originSelectionRange": {
                "start": { "line": 5, "character": 3 },
                "end": { "line": 5, "character": 10 }
            }
        });
        let result = format_location(&loc);
        assert_eq!(
            result,
            Some("  /home/user/project/src/lib.rs:11:1".to_string())
        );
    }

    #[test]
    fn format_hover_plain_text() {
        let result = json!({
            "contents": {
                "kind": "plaintext",
                "value": "Some hover info"
            }
        });
        let formatted = format_hover_result(&result);
        assert_eq!(formatted, "plaintext\nSome hover info");
    }

    #[test]
    fn format_hover_markdown() {
        let result = json!({
            "contents": {
                "kind": "markdown",
                "value": "# Header\n\nBody text"
            }
        });
        let formatted = format_hover_result(&result);
        assert_eq!(formatted, "# Header\n\nBody text");
    }

    #[test]
    fn format_hover_string() {
        let result = json!({ "contents": "Just a string" });
        let formatted = format_hover_result(&result);
        assert_eq!(formatted, "Just a string");
    }

    #[test]
    fn format_hover_empty() {
        let result = json!({});
        let formatted = format_hover_result(&result);
        assert_eq!(formatted, "(no hover info)");
    }

    #[test]
    fn format_hover_marked_string() {
        let result = json!({
            "contents": {
                "language": "rust",
                "value": "fn foo() -> i32"
            }
        });
        let formatted = format_hover_result(&result);
        assert_eq!(formatted, "```rust\nfn foo() -> i32\n```");
    }

    #[test]
    fn format_hover_marked_string_array() {
        let result = json!({
            "contents": [
                { "language": "rust", "value": "fn foo() -> i32" },
                "Some documentation text"
            ]
        });
        let formatted = format_hover_result(&result);
        assert_eq!(
            formatted,
            "```rust\nfn foo() -> i32\n```\n\nSome documentation text"
        );
    }

    #[test]
    fn format_references_none() {
        let result = json!([]);
        let formatted = format_references_result(&result);
        assert_eq!(formatted, "No references found.");
    }

    #[test]
    fn format_workspace_symbols_empty() {
        let result = json!([]);
        let formatted = format_workspace_symbols(&result);
        assert_eq!(formatted, "No symbols found.");
    }

    #[test]
    fn format_hierarchy_items_empty() {
        let result = json!([]);
        let formatted = format_hierarchy_items(&result, "call hierarchy");
        assert_eq!(formatted, "No call hierarchy items found.");
    }

    #[test]
    fn format_call_hierarchy_calls_empty() {
        let result = json!([]);
        let formatted = format_call_hierarchy_calls(&result, "incoming");
        assert_eq!(formatted, "No incoming calls found.");
    }

    #[test]
    fn test_check_permissions_always_allow() {
        let tool = LspTool::ephemeral();
        let ctx = test_ctx();
        let result = tool.check_permissions(&Value::Null, &ctx);
        // check_permissions is sync for LspTool.
        assert!(matches!(
            tokio::runtime::Runtime::new().unwrap().block_on(result),
            PermissionDecision::Allow { .. }
        ));
    }

    // ── Pool tests ──

    #[tokio::test]
    async fn ephemeral_mode_no_manager() {
        let tool = LspTool::ephemeral();
        assert!(tool.manager.is_none());
        // Validation still works.
        let r = tool
            .validate_input(
                &json!({"operation": "workspaceSymbol", "query": "test"}),
                &test_ctx(),
            )
            .await;
        assert!(matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn pool_manager_has_zero_servers_initially() {
        let manager = LspManager::new(Duration::from_secs(300));
        assert_eq!(manager.active_servers(), 0);
    }

    #[tokio::test]
    async fn pool_manager_clone_shares_state() {
        let manager = LspManager::new(Duration::from_secs(300));
        let m2 = manager.clone();
        assert_eq!(m2.active_servers(), 0);
    }

    #[tokio::test]
    async fn lsp_tool_with_pool_holds_manager() {
        let manager = LspManager::new(Duration::from_secs(300));
        let tool = LspTool::new(manager.clone());
        assert!(tool.manager.is_some());
    }

    #[tokio::test]
    async fn evict_idle_noop_when_timeout_is_zero() {
        let manager = LspManager::new(Duration::ZERO);
        // No-op — should not panic.
        manager.evict_idle();
        assert_eq!(manager.active_servers(), 0);
    }

    #[tokio::test]
    async fn evict_idle_does_not_crash_on_empty_pool() {
        let manager = LspManager::new(Duration::from_secs(1));
        manager.evict_idle();
        assert_eq!(manager.active_servers(), 0);
    }
}
