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

    /// Temporarily allow `tool_name` without going through the normal
    /// Allow/Deny/Ask flow — used by a skill's `allowed_tools` frontmatter
    /// field while that skill is active. Default no-op: implementations
    /// with no backing rule engine (an always-permit or bubble-to-parent
    /// implementation, say) have nothing meaningful to add a temporary
    /// override on top of — `check` already decides everything for them.
    fn add_temporary_allow(&self, _tool_name: &str) {}

    /// Clear every temporary allow `add_temporary_allow` added. Same
    /// no-op default as above.
    fn clear_temporary_allows(&self) {}

    /// Persist an Allow decision for `tool_name` beyond this single check —
    /// unlike `add_temporary_allow`, this is NOT cleared by
    /// `clear_temporary_allows` and is meant to last for the rest of the
    /// session (and, separately, the caller is responsible for any on-disk
    /// persistence — this method only affects in-memory/live behavior).
    /// Default no-op for implementations without a backing rule engine.
    fn add_persistent_allow(&self, _tool_name: &str, _rule_content: Option<&str>) {}
}
