//! Hook 进程 stdin / stdout 协议。见 docs/DATA_FORMATS.md §B.4。
//!
//! - **stdin**：CLI 写一行 JSON `HookInput`
//! - **stdout**：hook 写一行 JSON `HookResponse`（不合法 → 视为 default {continue: true}）

use serde::{Deserialize, Serialize};

/// CLI 喂给 hook 子进程的 payload。字段集随事件变化，所有字段都 optional。
#[derive(Debug, Clone, Serialize)]
pub struct HookInput {
    pub hook_event_name: String, // "PreToolUse" / "PostToolUse" / ...
    pub session_id: String,
    pub cwd: String,
    pub permission_mode: String,

    // PreToolUse / PostToolUse
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,

    // PostToolUse only
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,

    // UserPromptSubmit
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_prompt: Option<String>,
}

/// hook 写回的响应。所有字段都 optional；缺省视为"什么都不干"。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HookResponse {
    /// 是否继续这一 turn。false → engine 中止整个 turn。
    #[serde(default, rename = "continue")]
    pub r#continue: Option<bool>,

    /// 改写权限决定（仅 PreToolUse 有意义）。
    #[serde(default)]
    pub decision: Option<HookDecision>,

    /// 给用户 / 模型看的解释。
    #[serde(default)]
    pub message: Option<String>,

    /// 改写工具入参（仅 PreToolUse；hook 想 redirect 路径等用）
    #[serde(default)]
    pub updated_input: Option<serde_json::Value>,

    /// 不把 hook 的 stdout 进 transcript（仅日志）
    #[serde(default)]
    pub suppress_output: Option<bool>,

    /// P2: 异步唤醒。hook 返回 true 表示请求在后台工作完成时被重新执行。
    /// 需要 hook 配置中 `async_rewake: true` 配合。
    #[serde(default)]
    pub rewake: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookDecision {
    Approve,
    Block,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_full_response() {
        let v = json!({
            "continue": true,
            "decision": "approve",
            "message": "ok",
            "updated_input": {"file_path": "/safe/path"},
            "suppress_output": false
        });
        let r: HookResponse = serde_json::from_value(v).unwrap();
        assert_eq!(r.r#continue, Some(true));
        assert_eq!(r.decision, Some(HookDecision::Approve));
        assert_eq!(r.message.as_deref(), Some("ok"));
        assert!(r.updated_input.is_some());
    }

    #[test]
    fn decodes_block_decision() {
        let v = json!({"decision": "block", "message": "no way"});
        let r: HookResponse = serde_json::from_value(v).unwrap();
        assert_eq!(r.decision, Some(HookDecision::Block));
    }

    #[test]
    fn empty_object_is_default_response() {
        let v = json!({});
        let r: HookResponse = serde_json::from_value(v).unwrap();
        assert!(r.r#continue.is_none());
        assert!(r.decision.is_none());
    }

    #[test]
    fn input_serializes_with_tool_fields() {
        let i = HookInput {
            hook_event_name: "PreToolUse".into(),
            session_id: "sess1".into(),
            cwd: "/tmp".into(),
            permission_mode: "default".into(),
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "ls"})),
            tool_use_id: Some("toolu_01".into()),
            tool_result: None,
            is_error: None,
            user_prompt: None,
        };
        let v = serde_json::to_value(&i).unwrap();
        assert_eq!(v["hook_event_name"], "PreToolUse");
        assert_eq!(v["tool_name"], "Bash");
        assert!(v.get("tool_result").is_none());
    }
}
