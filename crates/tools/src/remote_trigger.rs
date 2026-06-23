//! RemoteTriggerTool — manage remote agent triggers via CCR/ClawPod bridge.
//!
//! This tool provides a stub interface for listing, getting, creating, and running
//! remote agent triggers. Since there is no CCR backend available locally, it
//! returns informational messages directing users to configure via the ClawPod bridge.
//!
//! TS parity: `src/tools/RemoteTriggerTool/RemoteTriggerTool.ts`
//!
//! Actions:
//! - list:   List all configured remote triggers
//! - get:    Get details of a specific trigger
//! - create: Create a new remote trigger
//! - run:    Run/execute a remote trigger immediately

use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoteTriggerInput {
    /// Action to perform: "list" | "get" | "create" | "run"
    pub action: String,

    /// Trigger ID (required for get and run)
    #[serde(default)]
    pub trigger_id: Option<String>,

    /// JSON body for create action
    #[serde(default)]
    pub body: Option<Value>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RemoteTriggerTool;

#[async_trait]
impl Tool for RemoteTriggerTool {
    fn description(&self) -> &str {
        "Manage remote agent triggers (CCR) via ClawPod bridge"
    }

    fn name(&self) -> &str {
        "RemoteTrigger"
    }

    fn is_deferred(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(RemoteTriggerInput)).expect("schema")
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/remote_trigger.prompt.md").to_string()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn is_read_only(&self, input: &Value) -> bool {
        if let Ok(parsed) = serde_json::from_value::<RemoteTriggerInput>(input.clone()) {
            matches!(parsed.action.as_str(), "list" | "get")
        } else {
            false
        }
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let parsed: Result<RemoteTriggerInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) => match p.action.as_str() {
                "list" | "create" | "run" | "get" => {}
                other => {
                    return ValidationResult::err(
                        format!(
                            "Invalid action '{other}'. Must be one of: list, get, create, run"
                        ),
                        1,
                    );
                }
            },
            Err(e) => {
                return ValidationResult::err(format!("Invalid input: {e}"), 2);
            }
        }
        ValidationResult::Ok
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: RemoteTriggerInput = serde_json::from_value(input)?;

        match input.action.as_str() {
            "list" => Ok(ToolResult::text(
                "No remote triggers configured. Configure via ClawPod bridge.",
            )),
            "get" => {
                let id = input.trigger_id.as_deref().unwrap_or("unknown");
                Ok(ToolResult::text(format!("Trigger {id} not found")))
            }
            "create" => Ok(ToolResult::text(
                "Remote trigger creation requires ClawPod bridge connection",
            )),
            "run" => Ok(ToolResult::text(
                "Remote trigger execution requires ClawPod bridge connection",
            )),
            other => Err(ToolError::exec(format!(
                "Unknown action: {other}. Must be one of: list, get, create, run"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    fn ctx(cwd: &Path) -> ToolContext {
        ToolContext::for_test(cwd.to_path_buf())
    }

    #[tokio::test]
    async fn name_is_remote_trigger() {
        let tool = RemoteTriggerTool;
        assert_eq!(tool.name(), "RemoteTrigger");
        assert!(tool.is_deferred());
        assert!(tool.is_concurrency_safe(&Value::Null));
    }

    #[tokio::test]
    async fn is_read_only_for_list_and_get() {
        let tool = RemoteTriggerTool;
        assert!(tool.is_read_only(&json!({"action": "list"})));
        assert!(tool.is_read_only(&json!({"action": "get", "trigger_id": "t1"})));
        assert!(!tool.is_read_only(&json!({"action": "create", "body": {}})));
        assert!(!tool.is_read_only(&json!({"action": "run", "trigger_id": "t1"})));
    }

    #[tokio::test]
    async fn validates_valid_actions() {
        let tool = RemoteTriggerTool;
        for action in &["list", "get", "create", "run"] {
            let r = tool
                .validate_input(&json!({"action": action}), &ctx(Path::new("/tmp")))
                .await;
            assert!(matches!(r, ValidationResult::Ok), "action '{action}' should be valid");
        }
    }

    #[tokio::test]
    async fn rejects_invalid_action() {
        let tool = RemoteTriggerTool;
        let r = tool
            .validate_input(&json!({"action": "delete"}), &ctx(Path::new("/tmp")))
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn rejects_missing_action() {
        let tool = RemoteTriggerTool;
        let r = tool
            .validate_input(&json!({}), &ctx(Path::new("/tmp")))
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn list_returns_info_message() {
        let tool = RemoteTriggerTool;
        let r = tool
            .call(
                json!({"action": "list"}),
                ctx(Path::new("/tmp")),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        match r.content {
            base::tool::ToolResultContent::Text(t) => {
                assert!(t.contains("No remote triggers configured"));
                assert!(t.contains("ClawPod bridge"));
            }
            _ => panic!("expected Text content"),
        }
    }

    #[tokio::test]
    async fn get_returns_not_found() {
        let tool = RemoteTriggerTool;
        let r = tool
            .call(
                json!({"action": "get", "trigger_id": "my-trigger"}),
                ctx(Path::new("/tmp")),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        match r.content {
            base::tool::ToolResultContent::Text(t) => {
                assert!(t.contains("my-trigger"));
                assert!(t.contains("not found"));
            }
            _ => panic!("expected Text content"),
        }
    }

    #[tokio::test]
    async fn create_requires_bridge() {
        let tool = RemoteTriggerTool;
        let r = tool
            .call(
                json!({"action": "create", "body": {"name": "test"}}),
                ctx(Path::new("/tmp")),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        match r.content {
            base::tool::ToolResultContent::Text(t) => {
                assert!(t.contains("Remote trigger creation requires ClawPod bridge connection"));
            }
            _ => panic!("expected Text content"),
        }
    }

    #[tokio::test]
    async fn run_requires_bridge() {
        let tool = RemoteTriggerTool;
        let r = tool
            .call(
                json!({"action": "run", "trigger_id": "my-trigger"}),
                ctx(Path::new("/tmp")),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        match r.content {
            base::tool::ToolResultContent::Text(t) => {
                assert!(t.contains("Remote trigger execution requires ClawPod bridge connection"));
            }
            _ => panic!("expected Text content"),
        }
    }
}
