//! Hook 配置（来自 settings.json `hooks` 字段）。
//!
//! 见 docs/DATA_FORMATS.md §B.3 / §B.4。只支持 `command` 变体；
//! `prompt` / `http` / `agent` 三种推迟到 ，配置加载时遇到会 warn + skip。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 单条 hook 配置。`type` 字段做 enum tag。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookConfig {
    /// 跑外部 shell 命令；stdin 给 JSON payload，stdout 解析为 HookResponse。
    Command {
        command: String,
        /// 默认 bash；Windows 上可指定 powershell
        #[serde(default)]
        shell: Option<String>,
        /// 毫秒；不填用 HookRunner 的 default_timeout_ms
        #[serde(default)]
        timeout: Option<u64>,
        /// 权限规则风格的过滤模式，例如 "Bash(git push:*)"。命中才跑。
        #[serde(default, rename = "if")]
        if_pattern: Option<String>,
        /// 仅当工具结果 is_error=true 时才跑（PostToolUse 用）
        #[serde(default)]
        only_on_error: Option<bool>,
        /// 一次性 hook：跑过一次后从 settings 摘掉
        #[serde(default)]
        once: Option<bool>,
        /// P2: 异步唤醒机制。若为 true，hook 可返回 `{rewake: true}`
        /// 请求在后台工作完成时被重新执行。
        #[serde(default)]
        async_rewake: Option<bool>,
    },
    /// 跑 prompt 让小模型评估
    Prompt {
        prompt: String,
        #[serde(default)]
        timeout: Option<u64>,
        #[serde(default)]
        model: Option<String>,
    },
    /// HTTP webhook
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        timeout: Option<u64>,
    },
    /// 跑子 agent
    Agent {
        prompt: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        timeout: Option<u64>,
    },
}

/// Hook 事件枚举。命名使用 PascalCase。
/// TS parity: 28 events from `HOOK_EVENTS` in coreTypes.ts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    TurnStart,
    TurnComplete,
    SessionStart,
    SessionEnd,
    Stop,
    StopFailure,
    UserPromptSubmit,
    PreCompact,
    PostCompact,
    /// Fired when the permission system requires a user decision.
    PermissionRequested,
    /// Fired when a tool call is denied (by user or system).
    PermissionDenied,
    /// Fired during setup / initialization.
    Setup,
    /// **B-4 **: fired when user attention is required (typically a
    /// permission prompt is pending). Hook can play a sound, send a desktop
    /// notification, or buzz a chat channel — output is ignored.
    Notification,
    /// **B-4 **: fired when a sub-agent (`AgentTool`) finishes (success
    /// or error). Lets parent context react — e.g. log the summary to a tracking
    /// system, or alert the user that long-running work is done.
    SubagentStop,
    /// Fired when a sub-agent (`AgentTool`) starts. Hook can log, notify, or
    /// inject context before the sub-agent begins work.
    SubagentStart,
    /// Team member idle notification.
    TeammateIdle,
    /// Task lifecycle events.
    TaskCreated,
    TaskCompleted,
    /// MCP elicitation (URL/tool request from MCP server).
    Elicitation,
    ElicitationResult,
    /// Settings/config change.
    ConfigChange,
    /// Git worktree lifecycle.
    WorktreeCreate,
    WorktreeRemove,
    /// CLAUDE.md / ATTA.md loaded.
    InstructionsLoaded,
    /// Working directory changed.
    CwdChanged,
    /// File change detected (watch mode).
    FileChanged,
    /// **P1 **: Fired after each model API sampling completes (streaming
    /// response fully consumed). Hook receives model output for real-time
    /// audit, logging, or modification. TS parity: postSamplingHooks.ts.
    PostSampling,
}

/// settings.json 的 `hooks` 字段：事件名 → 多个 hook 配置。
pub type HooksSettings = HashMap<HookEvent, Vec<HookConfig>>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_hooks_settings_from_json() {
        let v = json!({
            "PreToolUse": [
                {
                    "type": "command",
                    "command": "./scripts/check.sh",
                    "timeout": 5000,
                    "if": "Bash(git push:*)"
                }
            ],
            "PostToolUse": [],
            "SessionStart": [
                {
                    "type": "command",
                    "command": "echo session-started"
                }
            ]
        });
        let settings: HooksSettings = serde_json::from_value(v).unwrap();
        assert_eq!(settings[&HookEvent::PreToolUse].len(), 1);
        assert_eq!(settings[&HookEvent::SessionStart].len(), 1);
        assert_eq!(settings[&HookEvent::PostToolUse].len(), 0);
    }

    #[test]
    fn command_variant_full_fields() {
        let v = json!({
            "type": "command",
            "command": "x",
            "shell": "bash",
            "timeout": 1000,
            "if": "Bash",
            "only_on_error": true,
            "once": false,
            "async_rewake": true
        });
        let h: HookConfig = serde_json::from_value(v).unwrap();
        match h {
            HookConfig::Command {
                command,
                shell,
                timeout,
                if_pattern,
                only_on_error,
                once,
                async_rewake,
            } => {
                assert_eq!(command, "x");
                assert_eq!(shell.as_deref(), Some("bash"));
                assert_eq!(timeout, Some(1000));
                assert_eq!(if_pattern.as_deref(), Some("Bash"));
                assert_eq!(only_on_error, Some(true));
                assert_eq!(once, Some(false));
                assert_eq!(async_rewake, Some(true));
            }
            _ => panic!("expected Command variant"),
        }
    }

    #[test]
    fn prompt_variant_decodes_but_runner_will_skip() {
        let v = json!({
            "type": "prompt",
            "prompt": "is this safe?",
            "model": "claude-haiku-4-5"
        });
        let h: HookConfig = serde_json::from_value(v).unwrap();
        assert!(matches!(h, HookConfig::Prompt { .. }));
    }
}
