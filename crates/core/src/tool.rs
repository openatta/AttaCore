//! Tool trait — v7 unified. All tools implement this single trait.
//! Types moved here from tools/src/legacy.rs. legacy.rs is now a pure re-export.

use crate::context::config::NetworkModeConfig;
use crate::error::ToolError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

// ── Shared types (used by ALL tools) ──

#[derive(Debug, Clone, Default)]
pub struct SandboxSettings {
    pub deny_read: Vec<PathBuf>,
    pub allowed_domains: Vec<String>,
    pub network_mode: NetworkModeConfig,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptBehavior {
    Cancel,
    Block,
}
pub use crate::permission::PermissionMode;
#[derive(Debug, Clone)]
pub struct PromptContext {
    pub cwd: PathBuf,
    pub model: String,
    pub session_id: String,
    pub is_interactive: bool,
    pub all_tool_names: Vec<String>,
    pub allowed_agent_types: Vec<String>,
}
impl Default for PromptContext {
    fn default() -> Self {
        Self {
            cwd: PathBuf::new(),
            model: String::new(),
            session_id: String::new(),
            is_interactive: false,
            all_tool_names: vec![],
            allowed_agent_types: vec![],
        }
    }
}
pub trait SnapshotFile: Send + Sync + std::fmt::Debug {
    fn record(&self, p: &std::path::Path, n: &str);
}
pub trait EffectsCallback: Send + Sync + std::fmt::Debug {
    fn append_system_message(&self, k: &str, c: &str);
    fn os_notify(&self, _m: &str, _k: &str) {}
}
pub trait RunningTasksCallback: Send + Sync + std::fmt::Debug {
    fn find(&self, tid: &str) -> Option<(String, Vec<String>, crate::context::RunningStatus)>;
    fn cancel(&self, tid: &str) -> bool;
}

#[derive(Clone)]
pub struct ProgressSender {
    tool_use_id: String,
    callback: Option<Arc<dyn ProgressCallback>>,
}
pub trait ProgressCallback: Send + Sync {
    fn on_progress(&self, tool_use_id: &str, data: &str);
}
impl ProgressSender {
    pub fn noop(id: impl Into<String>) -> Self {
        Self {
            tool_use_id: id.into(),
            callback: None,
        }
    }
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            tool_use_id: id.into(),
            callback: None,
        }
    }
    pub fn with_callback(id: impl Into<String>, cb: Arc<dyn ProgressCallback>) -> Self {
        Self {
            tool_use_id: id.into(),
            callback: Some(cb),
        }
    }
    pub fn send(&self, data: &str) {
        if let Some(ref cb) = self.callback {
            cb.on_progress(&self.tool_use_id, data);
        }
    }
    pub fn send_blob(&self, _: &[u8]) {}
    pub fn tool_use_id(&self) -> &str {
        &self.tool_use_id
    }
}

#[derive(Debug, Clone)]
pub enum PermissionDecision {
    Allow {
        decision_reason: Option<String>,
    },
    Deny {
        reason: Option<String>,
        decision_reason: Option<String>,
    },
    Ask {
        message: String,
        decision_reason: Option<String>,
    },
}
impl PermissionDecision {
    pub fn allow() -> Self {
        Self::Allow {
            decision_reason: None,
        }
    }
    pub fn deny(r: impl Into<String>) -> Self {
        Self::Deny {
            reason: Some(r.into()),
            decision_reason: None,
        }
    }
    pub fn ask(m: impl Into<String>) -> Self {
        Self::Ask {
            message: m.into(),
            decision_reason: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ValidationResult {
    Ok,
    Err(String, i32),
}
impl ValidationResult {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
    pub fn is_err(&self) -> bool {
        matches!(self, Self::Err(..))
    }
    pub fn err(m: impl Into<String>, c: i32) -> Self {
        Self::Err(m.into(), c)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolResult {
    pub content: ToolResultContent,
    pub is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_meta: Option<McpMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_messages: Option<Vec<Value>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpMeta {
    #[serde(default)]
    pub server_name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ToolResultBlock>),
}
impl Default for ToolResultContent {
    fn default() -> Self {
        Self::Text(String::new())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: Option<String>,
    pub source: Option<Value>,
}
impl ToolResult {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: ToolResultContent::Text(s.into()),
            is_error: false,
            structured_content: None,
            mcp_meta: None,
            new_messages: None,
        }
    }
    pub fn error_text(s: impl Into<String>) -> Self {
        Self {
            content: ToolResultContent::Text(s.into()),
            is_error: true,
            structured_content: None,
            mcp_meta: None,
            new_messages: None,
        }
    }
}

// ── Unified ToolContext (legacy + v2 fields) ──

#[derive(Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub session_id: String,
    pub turn_no: u32,
    pub sandbox: SandboxSettings,
    pub cancel: CancellationToken,
    pub additional_writable_dirs: Vec<PathBuf>,
    pub snapshot_file: Option<Arc<dyn SnapshotFile>>,
    pub effects: Option<Arc<dyn EffectsCallback>>,
    pub running_tasks: Option<Arc<dyn RunningTasksCallback>>,
    pub dangerously_disable_sandbox: bool,
    pub max_file_read_bytes: usize,
    pub permission_mode: PermissionMode,
    pub config: Arc<crate::context::EngineConfig>,
    pub session: Arc<crate::context::SessionState>,
    pub tool_use_id: String,
    pub agent: Option<crate::session::AgentContext>,
    pub parent_messages: Option<Vec<crate::message::Message>>,
    pub agent_depth: u32,
    pub events_tx: Option<
        tokio::sync::mpsc::UnboundedSender<crate::context::task::BackgroundAgentProgressData>,
    >,
}
impl ToolContext {
    pub fn for_test(cwd: PathBuf) -> Self {
        Self {
            cwd: cwd.clone(),
            session_id: "test".into(),
            turn_no: 0,
            sandbox: Default::default(),
            cancel: CancellationToken::new(),
            additional_writable_dirs: vec![],
            snapshot_file: None,
            effects: None,
            running_tasks: None,
            dangerously_disable_sandbox: true,
            max_file_read_bytes: 0,
            permission_mode: PermissionMode::default(),
            config: Arc::new(crate::context::EngineConfig::defaults_for("test")),
            session: Arc::new(crate::context::SessionState::new(cwd)),
            tool_use_id: String::new(),
            agent: None,
            parent_messages: None,
            agent_depth: 0,
            events_tx: None,
        }
    }
    pub fn from_engine_ctx(cwd: PathBuf, cancel: CancellationToken) -> Self {
        Self {
            cwd: cwd.clone(),
            session_id: String::new(),
            turn_no: 0,
            sandbox: SandboxSettings::default(),
            cancel,
            additional_writable_dirs: vec![],
            snapshot_file: None,
            effects: None,
            running_tasks: None,
            dangerously_disable_sandbox: false,
            max_file_read_bytes: 10 * 1024 * 1024,
            permission_mode: PermissionMode::default(),
            config: Arc::new(crate::context::EngineConfig::defaults_for("unknown")),
            session: Arc::new(crate::context::SessionState::new(cwd)),
            tool_use_id: String::new(),
            agent: None,
            parent_messages: None,
            agent_depth: 0,
            events_tx: None,
        }
    }
}

// ── Unified Tool trait ──

#[async_trait]
pub trait Tool: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self) -> &str {
        ""
    }
    fn input_schema(&self) -> Value;
    async fn prompt(&self, _: &PromptContext) -> String {
        self.prompt_fragment()
    }
    fn prompt_fragment(&self) -> String {
        self.description().to_string()
    }
    fn is_enabled(&self) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_destructive(&self, _: &Value) -> bool {
        false
    }
    fn strict(&self) -> bool {
        false
    }
    fn is_deferred(&self) -> bool {
        false
    }
    fn is_dynamic(&self) -> bool {
        false
    }
    fn is_direct(&self) -> bool {
        false
    }
    fn short_description(&self) -> Option<String> {
        None
    }
    fn permission_match_content(&self, _: &Value) -> Option<String> {
        None
    }
    fn affected_paths(&self, _: &Value) -> Vec<PathBuf> {
        vec![]
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Cancel
    }
    async fn validate_input(&self, _: &Value, _: &ToolContext) -> ValidationResult {
        ValidationResult::Ok
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn call(
        &self,
        i: Value,
        c: ToolContext,
        p: ProgressSender,
    ) -> Result<ToolResult, ToolError>;
}

pub trait ToolRegistry: Send + Sync + 'static {
    fn all(&self) -> Vec<Arc<dyn Tool>>;
    fn find(&self, n: &str) -> Option<Arc<dyn Tool>>;
}
#[derive(Clone)]
pub struct InMemoryToolRegistry {
    pub(crate) tools: Arc<std::sync::RwLock<Vec<Arc<dyn Tool>>>>,
}
impl InMemoryToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: Arc::new(std::sync::RwLock::new(Vec::new())),
        }
    }
    pub fn register(&self, t: Arc<dyn Tool>) {
        self.tools
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(t);
    }
    pub fn get(&self, n: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|t| t.name() == n)
            .cloned()
    }
    pub fn list(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn all(&self) -> Vec<Arc<dyn Tool>> {
        self.list()
    }
    pub fn find(&self, n: &str) -> Option<Arc<dyn Tool>> {
        self.get(n)
    }
    pub fn names(&self) -> Vec<String> {
        self.tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|t| t.name().to_string())
            .collect()
    }
    pub fn len(&self) -> usize {
        self.tools.read().unwrap_or_else(|e| e.into_inner()).len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn has_direct_tool(&self) -> bool {
        self.tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|t| t.is_direct())
    }
}
impl ToolRegistry for InMemoryToolRegistry {
    fn all(&self) -> Vec<Arc<dyn Tool>> {
        self.list()
    }
    fn find(&self, n: &str) -> Option<Arc<dyn Tool>> {
        self.get(n)
    }
}
impl Default for InMemoryToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
pub fn extract_tool_name(i: &Value) -> Option<&str> {
    i.get("tool").and_then(|v| v.as_str())
}

#[async_trait]
pub trait SecondaryLlm: Send + Sync {
    async fn extract_with_prompt(&self, p: &str, c: &str) -> Result<String, String>;
}

#[cfg(test)]
mod tests {
    use super::*;
    struct F;
    #[async_trait]
    impl Tool for F {
        fn name(&self) -> &str {
            "f"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({})
        }
        fn is_read_only(&self, _: &Value) -> bool {
            true
        }
        fn is_concurrency_safe(&self, _: &Value) -> bool {
            true
        }
        async fn call(
            &self,
            _: Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("ok"))
        }
    }
    #[test]
    fn empty() {
        assert!(InMemoryToolRegistry::new().all().is_empty());
    }
    #[test]
    fn find() {
        let r = InMemoryToolRegistry::new();
        r.register(Arc::new(F));
        assert!(r.find("f").is_some());
    }
}
