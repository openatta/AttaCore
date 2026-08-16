//! Hook 配置（来自 settings.json `hooks` 字段）。
//!
//! 四种变体都会执行（`HookRunner::run_one` 逐一分发）：`command` 与 `http`
//! 自足；`prompt` 与 `agent` 需要宿主注入 `PromptHookExecutor` /
//! `AgentHookExecutor`，没注入时该 hook 被 Skip 并附原因，不是静默失败。

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
    /// Call an installed plugin's WASM component.
    ///
    /// The one hook backend a downloaded package may use. It exists so that
    /// a plugin can take part in the engine's lifecycle without the host
    /// gaining a *new* place to call out from: this is a fifth executor
    /// behind the dispatcher that already runs, not a new call site in the
    /// turn loop.
    ///
    /// Which events a plugin may subscribe to is a whitelist enforced when
    /// its manifest is parsed (`plugin::manifest::SUBSCRIBABLE_EVENTS`), and
    /// the decisions it may return are narrower than a local hook's — see
    /// `WasmHookExecutor`.
    Wasm {
        /// Installed plugin name; the executor resolves it to a component.
        plugin: String,
        #[serde(default)]
        timeout: Option<u64>,
    },
}

/// Hook 事件枚举。命名使用 PascalCase。
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
    /// An MCP server's tool result contains an elicitation URL (`mcp://` or
    /// `elicitation://`) — the server is asking for user attention (e.g. an
    /// out-of-band authorization link). Fired by
    /// `hooks::HookRunner::run_elicitation` from the MCP adapter's tool-call
    /// path (`crates/mcp/src/adapter.rs`).
    Elicitation,
    /// The response half of an elicitation started by `Elicitation` above.
    /// See `HookRunner::run_elicitation_result`'s doc comment: no host-side
    /// response round-trip exists in this codebase yet, so this event has no
    /// production caller today.
    ElicitationResult,
    /// Settings/config change.
    ///
    /// **Not fired today, and the reason is structural.** The only thing that
    /// mutates settings at runtime is the daemon's `config.reload` /
    /// `config.setProvider` RPC, which operates at the *pool* level — above
    /// any individual session, and therefore above the only place a
    /// `HookRunner` exists (one per session, built from that session's merged
    /// settings). Firing it would mean either fanning out to every live
    /// session from the pool, or giving the pool a hook runner of its own
    /// with no session to attach it to.
    ///
    /// Listed here rather than deleted because the RPC surface for it exists
    /// and a hook for "the daemon was reconfigured under me" is genuinely
    /// useful; it is a wiring gap with a known shape, not a stray variant.
    /// Configuring a hook for it parses fine and simply never runs.
    ConfigChange,
    /// Git worktree lifecycle.
    WorktreeCreate,
    WorktreeRemove,
    /// AGENTS.md loaded.
    InstructionsLoaded,
    /// Working directory changed.
    CwdChanged,
    /// File change detected (watch mode).
    FileChanged,
    /// **P1 **: Fired after each model API sampling completes (streaming
    /// response fully consumed). Hook receives model output for real-time
    /// audit, logging, or modification.
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
