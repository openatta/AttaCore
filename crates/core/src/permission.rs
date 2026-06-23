//! 权限相关类型 —— `PermissionDecision` 是权限系统**唯一**的判定结果。
//!
//! 见 docs/RUST_ARCHITECTURE.md §3.5 与 docs/DATA_FORMATS.md §B.5。

use serde::{Deserialize, Serialize};

/// 权限模式。
///
/// `auto` 仅在启用 transcript 分类器时有意义。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    #[default]
    Default,
    Plan,
    AcceptEdits,
    BypassPermissions,
    /// 启用 auto-mode 分类器才有意义
    Auto,
    DontAsk,
    /// **P2 **: Bubble permission requests up to parent agent in team/coordinator
    /// mode. Instead of prompting the user directly, the permission request is
    /// forwarded to the parent agent for decision. TS parity: bubble mode.
    Bubble,
    /// **YOLO mode**: aggressive auto-approval for power users. Automatically
    /// allows known-safe operations (read, git, ls, etc.) using the YoloClassifier.
    /// Feature-gated — falls back to Default behavior when the feature is disabled.
    Yolo,
}

impl PermissionMode {
    /// True for read-only / non-mutating permission modes (Default, Plan).
    pub fn is_safe_default(self) -> bool {
        matches!(self, Self::Default)
    }

    /// Whether the user can set this mode via settings.json / CLI.
    /// TS parity: `EXTERNAL_PERMISSION_MODES` in `types/permissions.ts`.
    /// `Auto` and `Bubble` are program-only — they are set by the runtime
    /// (classifier activation, team coordinator) and rejected from user config.
    pub fn is_user_settable(self) -> bool {
        !matches!(self, Self::Auto | Self::Bubble)
    }
}

/// 权限闸的统一返回类型。Tool::check_permissions / 规则引擎 / Hook / mode 决策
/// 都返回它。**没有**单独的 `PermissionResult` / `Decision` 别名 —— 不要造。
#[derive(Debug, Clone)]
pub enum PermissionDecision {
    Allow {
        /// 改写后的输入（如把路径重定向到 worktree）；None 表示不改
        updated_input: Option<serde_json::Value>,
        decision_reason: Option<DecisionReason>,
    },
    Deny {
        message: String,
        decision_reason: DecisionReason,
    },
    Ask {
        message: String,
        decision_reason: Option<DecisionReason>,
    },
}

impl PermissionDecision {
    /// Permission decision: allow this tool call.
    pub fn allow() -> Self {
        Self::Allow {
            updated_input: None,
            decision_reason: None,
        }
    }
    /// Permission decision: deny with a user-visible reason.
    pub fn deny(msg: impl Into<String>, reason: DecisionReason) -> Self {
        Self::Deny {
            message: msg.into(),
            decision_reason: reason,
        }
    }
    /// True if this decision is `Allow`.
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow { .. })
    }
    /// True if this decision is `Deny`.
    pub fn is_deny(&self) -> bool {
        matches!(self, Self::Deny { .. })
    }
    /// True if this decision is `Ask` (interactive prompt required).
    pub fn is_ask(&self) -> bool {
        matches!(self, Self::Ask { .. })
    }
}

#[derive(Debug, Clone)]
pub enum DecisionReason {
    Rule(PermissionRule),
    Mode(PermissionMode),
    Hook {
        hook_name: String,
    },
    Classifier {
        classifier: String,
    },
    SubcommandResults,
    SafetyCheck,
    /// Tool's own check_permissions returned Allow.
    ToolAllowed(String),
    /// Tool's own check_permissions returned Deny.
    ToolDenied(String),
    /// 工具自带的判定（FileEdit 路径白名单等）
    ToolBuiltin(String),
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionRule {
    pub source: RuleSource,
    pub behavior: RuleBehavior,
    pub tool_name: String,
    /// 例：Bash 用 "git push:*"；Read 用 "/etc/**"
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rule_content: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum RuleSource {
    /// `~/.atta/code/settings.json`
    UserSettings,
    /// `<git-root>/.atta/code/settings.json`
    ProjectSettings,
    /// `<git-root>/.atta/code/settings.local.json`
    LocalSettings,
    /// CLI flag (`--permission-mode`, `--allow-tool` 等)
    CliArg,
    /// 会话内通过 `/permissions` 命令设置
    Session,
    /// 来自 attacode-managed-settings
    PolicySettings,
    /// 由命令展开时临时注入
    Command,
}

impl RuleSource {
    /// 优先级：数值越大越优先（CliArg 最高）
    pub fn priority(self) -> u8 {
        match self {
            Self::CliArg => 60,
            Self::Session => 50,
            Self::Command => 45,
            Self::LocalSettings => 40,
            Self::ProjectSettings => 30,
            Self::UserSettings => 20,
            Self::PolicySettings => 10,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum RuleBehavior {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Clone)]
pub enum ValidationResult {
    Ok,
    Err {
        message: String,
        /// 给上层决定要不要退出 / 重试用；模型也能从 toolresult 看到
        error_code: i32,
    },
}

impl ValidationResult {
    /// Validation result: input passes all checks.
    pub fn ok() -> Self {
        Self::Ok
    }
    /// Validation result: failed with a message and code (non-zero).
    pub fn err(msg: impl Into<String>, code: i32) -> Self {
        Self::Err {
            message: msg.into(),
            error_code: code,
        }
    }
    /// True if validation passed.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_priority_ordering() {
        assert!(RuleSource::CliArg.priority() > RuleSource::LocalSettings.priority());
        assert!(RuleSource::LocalSettings.priority() > RuleSource::ProjectSettings.priority());
        assert!(RuleSource::ProjectSettings.priority() > RuleSource::UserSettings.priority());
        assert!(RuleSource::UserSettings.priority() > RuleSource::PolicySettings.priority());
    }

    #[test]
    fn permission_mode_serializes_camelcase() {
        let s = serde_json::to_value(PermissionMode::BypassPermissions).unwrap();
        assert_eq!(s, serde_json::Value::String("bypassPermissions".into()));
        let back: PermissionMode = serde_json::from_value(s).unwrap();
        assert_eq!(back, PermissionMode::BypassPermissions);
    }

    #[test]
    fn rule_roundtrip() {
        let r = PermissionRule {
            source: RuleSource::ProjectSettings,
            behavior: RuleBehavior::Deny,
            tool_name: "Bash".into(),
            rule_content: Some("rm -rf *".into()),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: PermissionRule = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }
}
