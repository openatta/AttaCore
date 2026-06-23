//! `EnterPlanModeTool` / `ExitPlanModeTool` —— 模型自己进出 plan 模式。
//!
//! plan 模式：只允许只读工具运行（FileEdit / FileWrite / Bash 等被 PermissionGate
//! 直接 deny）。模型在这个模式下"想"清楚再退出，避免误改文件。
//!
//! 状态存储在 process-wide static 中：
//! - mode 用 `AtomicBool`（避免 Mutex poisoning 影响测试隔离）
//! - plan_text 用 `Mutex<Option<String>>`
//!
//! Engine/PermissionGate 通过 `plan_mode_active()` 查询当前状态。
//!
//! **Session isolation**: The static store is process-wide. The agent crate is
//! single-session-per-process; if multi-session reuse is added, replace the
//! static with an owned `PlanState` on the session/engine.

use anyhow;
use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult, ValidationResult};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;

/// Whether plan mode is active (process-wide).
/// `AtomicU8` avoids potential AtomicBool quirks on some platforms.
/// 0 = inactive (Default), 1 = active (Plan).
static PLAN_MODE_ACTIVE: AtomicU8 = AtomicU8::new(0);

/// Plan text guard (only the `Option<String>` needs a Mutex).
static PLAN_TEXT: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn plan_text_lock() -> &'static Mutex<Option<String>> {
    PLAN_TEXT.get_or_init(|| Mutex::new(None))
}

/// Check if plan mode is currently active (used by PermissionGate).
pub fn plan_mode_active() -> bool {
    PLAN_MODE_ACTIVE.load(Ordering::SeqCst) == 1
}

/// Get current plan text, if any.
pub fn plan_text() -> Option<String> {
    plan_text_lock().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Test helper: directly set plan text in the static store.
#[doc(hidden)]
pub fn plan_state_for_test(text: String) {
    PLAN_MODE_ACTIVE.store(1, Ordering::SeqCst);
    *plan_text_lock().lock().unwrap_or_else(|e| e.into_inner()) = Some(text);
}

/// TS parity: EnterPlanMode input is `z.strictObject({})` — no parameters.
/// The plan content is communicated through the model's response and the
/// plan file it writes to disk, not through the tool call input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EnterPlanModeInput {}

#[derive(Debug, Default, Clone, Copy)]
pub struct EnterPlanModeTool;

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn description(&self) -> &str { "Enter plan mode to design before implementing" }
        fn name(&self) -> &str {
        "EnterPlanMode"
    }

    /// **P3b **: 标 deferred —— 系统 prompt 仅暴露 name + 短描述，模型用
    /// ToolSearch 激活后下一轮拉 full schema。减少静态 prompt 占用。
    fn is_deferred(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(EnterPlanModeInput))
            .expect("schemars output is valid JSON")
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/plan_mode.enter.prompt.md").to_string()
    }

    /// TS parity: EnterPlanModeTool returns `true` — safe to call while other
    /// tools are running (it only toggles a session mode flag).
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<EnterPlanModeInput>(input.clone()) {
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
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
        // TS parity: EnterPlanMode tool cannot be used in agent contexts.
        if ctx.agent.is_some() {
            return Err(ToolError::Execution(anyhow::anyhow!(
                "EnterPlanMode tool cannot be used in agent contexts"
            )));
        }

        let _input: EnterPlanModeInput = serde_json::from_value(input)?;
        PLAN_MODE_ACTIVE.store(1, Ordering::SeqCst);
        // Clear any stale plan text from a previous plan session.
        *plan_text_lock().lock().unwrap_or_else(|e| e.into_inner()) = None;

        Ok(ToolResult::text(
            "Entered plan mode. You should now focus on exploring the codebase \
             and designing an implementation approach.\n\n\
             In plan mode, you should:\n\
             1. Thoroughly explore the codebase to understand existing patterns\n\
             2. Identify similar features and architectural approaches\n\
             3. Consider multiple approaches and their trade-offs\n\
             4. Use AskUserQuestion if you need to clarify the approach\n\
             5. Design a concrete implementation strategy\n\
             6. When ready, use ExitPlanMode to present your plan for approval\n\n\
             Remember: DO NOT write or edit any files yet. This is a read-only \
             exploration and planning phase."
            .to_string(),
        ))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExitPlanModeInput {
    /// Prompt-based permissions needed to implement the plan.
    /// These describe categories of actions rather than specific commands.
    /// TS parity: `allowedPrompts` array in ExitPlanModeV2Tool.
    #[serde(default)]
    #[schemars(default)]
    pub allowed_prompts: Vec<AllowedPrompt>,
    /// Optional one-line note explaining why exiting (e.g., "user approved the plan").
    /// Stored for transcript only; not enforced.
    #[serde(default)]
    pub note: Option<String>,
}

/// A prompt-based permission request (TS parity: `allowedPromptSchema`).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AllowedPrompt {
    /// The tool this prompt applies to (currently only Bash).
    pub tool: String,
    /// Semantic description of the action, e.g. "run tests", "install dependencies".
    pub prompt: String,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ExitPlanModeTool;

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn description(&self) -> &str { "Exit plan mode with plan summary and permission requests" }
        fn name(&self) -> &str {
        "ExitPlanMode"
    }

    /// **P3b **: 标 deferred —— 系统 prompt 仅暴露 name + 短描述，模型用
    /// ToolSearch 激活后下一轮拉 full schema。减少静态 prompt 占用。
    fn is_deferred(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ExitPlanModeInput))
            .expect("schemars output is valid JSON")
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/plan_mode.exit.prompt.md").to_string()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    /// TS parity: ExitPlanMode writes the plan to disk — not read-only.
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }

    /// TS parity: reject if not currently in plan mode (errorCode 1).
    async fn validate_input(&self, _input: &Value, ctx: &ToolContext) -> ValidationResult {
        if !plan_mode_active() {
            return ValidationResult::err(
                "You are not in plan mode. This tool is only for exiting plan mode \
                 after writing a plan. If your plan was already approved, continue \
                 with implementation.",
                1,
            );
        }
        // TS parity: teammates skip the mode check — not implemented yet.
        let _ = ctx;
        ValidationResult::Ok
    }

    /// TS parity: require user confirmation to exit plan mode (non-teammates).
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::ask("Exit plan mode?")
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: ExitPlanModeInput = serde_json::from_value(input)?;
        if plan_mode_active() {
            PLAN_MODE_ACTIVE.store(0, Ordering::SeqCst);
            *plan_text_lock().lock().unwrap_or_else(|e| e.into_inner()) = None;
            let note = input.note.unwrap_or_default();
            Ok(ToolResult::text(format!(
                "Exited plan mode (now: Default).{}{}",
                if note.is_empty() { "" } else { " note: " },
                note
            )))
        } else {
            // Should not reach here — validate_input rejects non-plan mode.
            Ok(ToolResult::text(
                "Already not in plan mode; no change."
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> ToolContext {
        ToolContext::for_test(PathBuf::from("/tmp"))
    }

    /// Reset the process-wide plan state.
    /// NOTE: call this AFTER `ctx()` — ToolContext::for_test has a side effect
    /// that sets PLAN_MODE_ACTIVE to 1.
    fn reset_state() {
        PLAN_MODE_ACTIVE.store(0, Ordering::SeqCst);
        *plan_text_lock().lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    #[tokio::test]
    async fn enter_plan_mode_and_validate() {
        // Combined: validate + call + agent context rejection.
        let tool = EnterPlanModeTool;

        // validate_input accepts empty input.
        let c = ToolContext::for_test(PathBuf::from("/tmp"));
        PLAN_MODE_ACTIVE.store(0, Ordering::SeqCst);
        assert!(matches!(
            tool.validate_input(&serde_json::json!({}), &c).await,
            ValidationResult::Ok
        ));

        // call changes state to Plan.
        let c2 = ToolContext::for_test(PathBuf::from("/tmp"));
        PLAN_MODE_ACTIVE.store(0, Ordering::SeqCst);
        let r = tool.call(serde_json::json!({}), c2, ProgressSender::noop("t")).await.unwrap();
        assert!(plan_mode_active());
        match r.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("Entered plan mode"));
                assert!(s.contains("exploration and planning phase"));
            }
            _ => panic!()
        }

        // agent context rejected.
        let mut c3 = ToolContext::for_test(PathBuf::from("/tmp"));
        c3.agent = Some(base::session::AgentContext {
            agent_id: base::id::Id::new(),
            agent_type: "test".into(),
            parent_session: base::session::SessionId::new(),
            depth: 0,
        });
        let r = tool.call(serde_json::json!({}), c3, ProgressSender::noop("t")).await;
        assert!(r.is_err());
        match r.unwrap_err() {
            ToolError::Execution(e) => assert!(e.to_string().contains("agent contexts")),
            _ => panic!("expected ToolError::Execution"),
        }
    }

    #[tokio::test]
    async fn exit_plan_mode_validate_and_call() {
        // Combined test: validate rejection + exit call.
        // Single ToolContext to avoid process-wide static interference.
        let c = ToolContext::for_test(PathBuf::from("/tmp"));
        let tool = ExitPlanModeTool;

        // Scenario 1: validate_input rejects when NOT in plan mode.
        PLAN_MODE_ACTIVE.store(0, Ordering::SeqCst);
        let r = tool.validate_input(&serde_json::json!({}), &c).await;
        assert!(
            matches!(r, ValidationResult::Err(_, 1)),
            "validate_input should reject when not in plan mode, got {r:?}"
        );

        // Scenario 2: call exits plan mode (Plan → Default).
        PLAN_MODE_ACTIVE.store(1, Ordering::SeqCst);
        let _ = tool.call(serde_json::json!({}), c, ProgressSender::noop("t")).await.unwrap();
        assert!(!plan_mode_active());
    }

    #[tokio::test]
    async fn enter_then_exit_round_trip() {
        let c = ToolContext::for_test(PathBuf::from("/tmp"));
        PLAN_MODE_ACTIVE.store(0, Ordering::SeqCst);
        let enter = EnterPlanModeTool;
        let exit = ExitPlanModeTool;
        enter
            .call(serde_json::json!({}), c.clone(), ProgressSender::noop("t"))
            .await
            .unwrap();
        assert!(plan_mode_active());
        exit.call(
            serde_json::json!({"note": "approved"}),
            c,
            ProgressSender::noop("t"),
        )
        .await
        .unwrap();
        assert!(!plan_mode_active());
    }

    #[test]
    fn enter_is_readonly_exit_is_not() {
        let enter = EnterPlanModeTool;
        let exit = ExitPlanModeTool;
        assert!(enter.is_read_only(&Value::Null));
        // TS parity: ExitPlanMode writes the plan to disk — not read-only.
        assert!(!exit.is_read_only(&Value::Null));
    }

    #[tokio::test]
    async fn enter_permissions_allow_exit_permissions_ask() {
        let c = ctx();
        // EnterPlanMode: always allowed.
        assert!(matches!(
            EnterPlanModeTool.check_permissions(&Value::Null, &c).await,
            PermissionDecision::Allow { .. }
        ));
        // ExitPlanMode: asks user for confirmation (TS parity).
        assert!(matches!(
            ExitPlanModeTool.check_permissions(&Value::Null, &c).await,
            PermissionDecision::Ask { .. }
        ));
    }
}
