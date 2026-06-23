//! `ScheduleWakeupTool` —— 在 /loop dynamic 模式下安排一次未来唤醒。
//!
//! 模型调用此工具设定一个定时器，超时后被重新唤起，并传入指定的 prompt。

use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ToolResultContent, ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

const MIN_DELAY_SECONDS: u64 = 60;
const MAX_DELAY_SECONDS: u64 = 3600;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScheduleWakeupInput {
    /// Delay in seconds before the wakeup (clamped to [60, 3600])
    #[serde(rename = "delaySeconds")]
    pub delay_seconds: u64,
    /// Short human-readable reason for this scheduled wakeup
    pub reason: String,
    /// The prompt to use when re-invoking the model
    pub prompt: String,
}

pub struct ScheduleWakeupTool;

#[async_trait]
impl Tool for ScheduleWakeupTool {
    fn name(&self) -> &str {
        "ScheduleWakeup"
    }

    fn description(&self) -> &str {
        "Schedule a delayed wakeup for /loop dynamic mode"
    }

    fn is_deferred(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ScheduleWakeupInput)).expect("schema")
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/schedule_wakeup.prompt.md").to_string()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<ScheduleWakeupInput>(input.clone()) {
            Ok(p) if p.delay_seconds < MIN_DELAY_SECONDS || p.delay_seconds > MAX_DELAY_SECONDS => {
                ValidationResult::err(
                    format!("delaySeconds must be between {MIN_DELAY_SECONDS} and {MAX_DELAY_SECONDS}"),
                    1,
                )
            }
            Ok(p) if p.reason.trim().is_empty() => {
                ValidationResult::err("reason must not be empty", 2)
            }
            Ok(p) if p.prompt.trim().is_empty() => {
                ValidationResult::err("prompt must not be empty", 3)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 4),
        }
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
        let input: ScheduleWakeupInput = serde_json::from_value(input)?;
        let delay = input.delay_seconds.clamp(MIN_DELAY_SECONDS, MAX_DELAY_SECONDS);
        Ok(ToolResult {
            content: ToolResultContent::Text(format!(
                "Scheduled wakeup in {}s: {}",
                delay, input.reason
            )),
            is_error: false,
            structured_content: Some(json!({
                "delay_seconds": delay,
                "reason": input.reason,
                "prompt": input.prompt,
            })),
            mcp_meta: None,
            new_messages: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> ToolContext {
        ToolContext::for_test("/tmp".into())
    }

    #[tokio::test]
    async fn name_matches() {
        assert_eq!(ScheduleWakeupTool.name(), "ScheduleWakeup");
    }

    #[tokio::test]
    async fn is_deferred() {
        assert!(ScheduleWakeupTool.is_deferred());
    }

    #[tokio::test]
    async fn is_concurrency_safe() {
        assert!(ScheduleWakeupTool.is_concurrency_safe(&Value::Null));
    }

    #[tokio::test]
    async fn is_read_only() {
        assert!(ScheduleWakeupTool.is_read_only(&Value::Null));
    }

    #[tokio::test]
    async fn validates_delay_too_low() {
        let tool = ScheduleWakeupTool;
        let r = tool
            .validate_input(
                &json!({"delaySeconds": 10, "reason": "test", "prompt": "continue"}),
                &test_ctx(),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn validates_delay_too_high() {
        let tool = ScheduleWakeupTool;
        let r = tool
            .validate_input(
                &json!({"delaySeconds": 9999, "reason": "test", "prompt": "continue"}),
                &test_ctx(),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn validates_empty_reason() {
        let tool = ScheduleWakeupTool;
        let r = tool
            .validate_input(
                &json!({"delaySeconds": 120, "reason": "  ", "prompt": "continue"}),
                &test_ctx(),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn validates_empty_prompt() {
        let tool = ScheduleWakeupTool;
        let r = tool
            .validate_input(
                &json!({"delaySeconds": 120, "reason": "test", "prompt": "  "}),
                &test_ctx(),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn validates_invalid_json() {
        let tool = ScheduleWakeupTool;
        let r = tool
            .validate_input(
                &json!({"delaySeconds": "not-a-number", "reason": "test", "prompt": "continue"}),
                &test_ctx(),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn valid_input_passes_validation() {
        let tool = ScheduleWakeupTool;
        let r = tool
            .validate_input(
                &json!({"delaySeconds": 120, "reason": "test reason", "prompt": "continue working"}),
                &test_ctx(),
            )
            .await;
        assert!(matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn check_permissions_always_allows() {
        let tool = ScheduleWakeupTool;
        let r = tool
            .check_permissions(&Value::Null, &test_ctx())
            .await;
        assert!(matches!(r, PermissionDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn call_returns_structured_confirmation() {
        let tool = ScheduleWakeupTool;
        let result = tool
            .call(
                json!({"delaySeconds": 300, "reason": "check CI status", "prompt": "continue checking"}),
                test_ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should succeed");

        assert!(!result.is_error);

        // Check text content
        if let ToolResultContent::Text(ref text) = result.content {
            assert!(text.contains("Scheduled wakeup in 300s"));
            assert!(text.contains("check CI status"));
        } else {
            panic!("expected Text content");
        }

        // Check structured content
        let structured = result.structured_content.expect("structured_content should be present");
        assert_eq!(structured["delay_seconds"], 300);
        assert_eq!(structured["reason"], "check CI status");
        assert_eq!(structured["prompt"], "continue checking");
    }

    #[tokio::test]
    async fn call_clamps_delay() {
        let tool = ScheduleWakeupTool;
        let result = tool
            .call(
                json!({"delaySeconds": 9999, "reason": "too high", "prompt": "continue"}),
                test_ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should succeed");

        let structured = result.structured_content.expect("structured_content should be present");
        assert_eq!(structured["delay_seconds"], MAX_DELAY_SECONDS);
    }

    #[tokio::test]
    async fn call_handles_minimum() {
        let tool = ScheduleWakeupTool;
        let result = tool
            .call(
                json!({"delaySeconds": 10, "reason": "too low", "prompt": "continue"}),
                test_ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should succeed");

        let structured = result.structured_content.expect("structured_content should be present");
        assert_eq!(structured["delay_seconds"], MIN_DELAY_SECONDS);
    }

    #[test]
    fn input_schema_contains_camel_case_field() {
        let tool = ScheduleWakeupTool;
        let schema = tool.input_schema();
        // The schema should reference delaySeconds somewhere
        let schema_str = serde_json::to_string(&schema).unwrap();
        assert!(schema_str.contains("delaySeconds"), "schema should contain camelCase field 'delaySeconds'");
    }
}
