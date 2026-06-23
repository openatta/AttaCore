//! `AskUserQuestionTool` —— 让模型主动向用户提问。
//!
//! 模型显式调用此工具以向用户提问。工具返回格式化的 JSON 结构，
//! TUI/权限层渲染交互式对话框。

use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult, ValidationResult};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AskUserQuestionInput {
    /// The question to put to the user
    pub question: String,
    /// Short label for the question (UI header, max 12 chars)
    #[serde(default)]
    pub header: Option<String>,
    /// Multiple-choice options. If empty, free-form text answer.
    #[serde(default)]
    pub options: Vec<AskUserOption>}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AskUserOption {
    /// Short key (1-5 chars; what the user types)
    pub key: String,
    /// Human-readable label
    pub label: String}

pub struct AskUserQuestionTool;

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str {
        "AskUserQuestion"
    }

    fn description(&self) -> &str {
        "Ask the user a question with multiple-choice options."
    }

    /// **P3f **: deferred -- only Bash/Read/Edit/ToolSearch 4 eager.
    /// Other tools activated via ToolSearch, saving ~13KB tools schema.
    fn is_deferred(&self) -> bool {
        true
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(AskUserQuestionInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/ask_user.prompt.md").to_string()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<AskUserQuestionInput>(input.clone()) {
            Ok(p) if p.question.trim().is_empty() => {
                ValidationResult::err("question must not be empty", 1)
            }
            Ok(p) if p.options.iter().any(|o| o.key.trim().is_empty()) => {
                ValidationResult::err("option key must not be empty", 2)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 3)}
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
        let input: AskUserQuestionInput = serde_json::from_value(input)?;
        // Return the question as structured content — the TUI/permission layer
        // renders the interactive dialog.
        let json_str = serde_json::to_string_pretty(&json!({
            "question": input.question,
            "header": input.header,
            "options": input.options.iter().map(|o| json!({"key": o.key, "label": o.label})).collect::<Vec<_>>()}))
        .unwrap_or_else(|_| "{}".into());
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(json_str),
            is_error: false,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn validates_empty_question() {
        let tool = AskUserQuestionTool;
        let r = tool
            .validate_input(
                &json!({"question": "  "}),
                &base::tool::ToolContext::for_test("/tmp".into()),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn validates_empty_option_key() {
        let tool = AskUserQuestionTool;
        let r = tool
            .validate_input(
                &json!({"question": "ok?", "options": [{"key": "", "label": "yes"}]}),
                &base::tool::ToolContext::for_test("/tmp".into()),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[test]
    fn name_matches_ts() {
        assert_eq!(AskUserQuestionTool.name(), "AskUserQuestion");
    }
}
