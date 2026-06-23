//! `Permission` trait — tool execution authorization.

use async_trait::async_trait;
use std::path::{Path, PathBuf};

/// Permission check outcome.
#[derive(Debug, Clone)]
pub enum PermissionOutcome {
    /// Allowed.
    Permit,
    /// Denied with a reason.
    Deny { reason: String },
    /// Needs upper-layer decision. Engine emits `AgentEvent::PermissionPrompt`
    /// and waits for `InputMessage::PermissionResponse`.
    Prompt {
        prompt_id: String,
        message: String,
        paths: Vec<PathBuf>,
    },
}

/// Tool execution permission interface.
///
/// Implementations decide whether a tool call is allowed.
/// AttaCode uses `RuleSetPermission` (Allow/Deny/Ask + RuleSet engine).
/// Jiandu uses `CwdPermission` (cwd boundary + callback hook).
#[async_trait]
pub trait Permission: Send + Sync {
    /// Check whether a tool call is permitted.
    async fn check(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        cwd: &Path,
        session_id: &str,
    ) -> PermissionOutcome;
}
