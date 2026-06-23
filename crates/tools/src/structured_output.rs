use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StructuredOutputInput {
    /// Any data to return as structured output. All properties are echoed back.
    #[serde(flatten)]
    pub payload: Value}

pub struct StructuredOutputTool;

#[async_trait]
impl Tool for StructuredOutputTool {
    fn name(&self) -> &str {
        "StructuredOutput"
    }

    fn description(&self) -> &str {
        "Return structured output in the requested format"
    }

    fn is_deferred(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(StructuredOutputInput)).expect("schema")
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/structured_output.prompt.md").to_string()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    async fn validate_input(&self, _: &Value, _: &ToolContext) -> ValidationResult {
        ValidationResult::Ok
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }

    async fn call(
        &self,
        input: Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(
                "Structured output provided successfully".into(),
            ),
            is_error: false,
            structured_content: Some(json!({"structured_output": input})),
            mcp_meta: None,
            new_messages: Some(vec![])})
    }
}
