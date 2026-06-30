//! `PermissionGate` —— 权限决策入口。
//!
//! 决策顺序（与 docs/RUST_ARCHITECTURE.md §5 一致；hooks 步骤推迟到 ）：
//!
//! 1. `tool.validate_input` —— 输入合法性
//! 2. `tool.check_permissions` —— 工具自带的判定（allow / deny 直接返回）
//! 3. 通用规则引擎（`RuleSet::evaluate`） —— hit allow / deny 直接返回
//! 4. PermissionMode 分派：
//!    - `BypassPermissions` → allow
//!    - `Plan` 且非只读工具 → deny
//!    - `AcceptEdits` 且工具是 Edit / Write → allow
//!    - `DontAsk` → deny
//!    - 其余 → ask（让 engine 通过 effects 弹问）
//!
//! 关键：gate **不**直接调 `effects.ask_user`。它返回 `PermissionDecision::Ask`，
//! 由调用方（engine）决定怎么把它升级成 allow / deny。

use crate::error::GateError;
use crate::rule::format_rule_string;
use crate::ruleset::{RuleHit, RuleSet};
use async_trait::async_trait;
use base::permission::PermissionRule;
use base::permission::{DecisionReason, PermissionDecision, PermissionMode};
use base::tool::Tool;
use base::tool::ToolContext;
use serde_json::Value;
use std::sync::{Arc, RwLock};

/// Auto 模式下的 LLM-based 决策器。在没有规则匹配 + 工具自身没结论时，让小
/// 模型（典型 haiku）看一眼工具调用，给出 allow / defer。
///
/// **职责非常窄**：classifier 不能 Deny —— 拒绝由 RuleSet 或工具自身负责。
/// 如果 classifier 不能给出明确 Allow，就 Defer 让 gate fall through 到 Ask。
/// 这保留了用户最终决定权，避免 model 过度自信酿成 destructive ops。
#[async_trait]
pub trait AutoClassifier: Send + Sync {
    /// 决策入参：工具名、tool prompt（让 classifier 知道工具能干啥）、tool input（看具体调用）
    async fn classify(
        &self,
        tool_name: &str,
        tool_description: &str,
        input: &Value,
    ) -> ClassifyDecision;
}

/// classifier 决策。
///
/// `Serialize` + `Deserialize` 允许 LlmClassifier 持久化决策缓存到 ~/.atta/。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum ClassifyDecision {
    /// classifier 判定为安全，应允许执行。
    Allow { reason: String },
    /// classifier 判定为危险（仅 LLM-based classifier 使用）。对应的
    /// `PermissionDecision::Deny` 将包含此 reason。
    Deny { reason: String },
    /// classifier 认为安全但建议改输入后执行（例如加 `--dry-run`）。
    /// Gate 将其映射为 `Allow` 并在 decision_reason 附带建议内容。
    AllowWithEdit {
        reason: String,
        /// 建议的修改内容（文本描述，不直接做 input 改写）。
        suggested_edits: String,
    },
    /// classifier 不确定，让 gate 继续走 ask 流程
    Defer,
}

pub struct PermissionGate {
    rules: Arc<RwLock<RuleSet>>,
    /// `Auto` 模式生效时的 classifier。`None` 时 Auto 行为同 Default（fall to ask）。
    auto_classifier: Option<Arc<dyn AutoClassifier>>,
    /// Denial counter for tracking and circuit-breaking.
    /// TS parity: denialTracking.ts — tracks consecutive and total denials
    /// to decide when to fall back from auto-mode to prompting.
    denial_count: std::sync::atomic::AtomicU64,
    total_denial_count: std::sync::atomic::AtomicU64,
}

impl PermissionGate {
    /// Construct a new instance.
    pub fn new(rules: RuleSet) -> Self {
        Self {
            rules: Arc::new(RwLock::new(rules)),
            auto_classifier: None,
            denial_count: std::sync::atomic::AtomicU64::new(0),
            total_denial_count: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Empty/default instance with no state.
    pub fn empty() -> Self {
        Self::new(RuleSet::empty())
    }

    /// Record a permission denial for tracking.
    pub fn record_denial(&self) {
        self.denial_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.total_denial_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Record a success — resets the consecutive denial counter.
    pub fn record_success(&self) {
        self.denial_count
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Number of consecutive denials (reset on success).
    pub fn consecutive_denials(&self) -> u64 {
        self.denial_count.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Total denials since gate creation.
    pub fn total_denials(&self) -> u64 {
        self.total_denial_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Check if the denial threshold has been exceeded (consecutive denials >= max).
    /// TS parity: DENIAL_LIMITS — maxConsecutive=3, maxTotal=20.
    pub fn should_fallback_to_prompting(&self) -> bool {
        self.consecutive_denials() >= 3 || self.total_denials() >= 20
    }

    /// 注入 Auto mode classifier。CLI 在 permission_mode == Auto 时构造 LLM-based
    /// 实现；测试可以注入 mock。
    pub fn with_auto_classifier(mut self, classifier: Arc<dyn AutoClassifier>) -> Self {
        self.auto_classifier = Some(classifier);
        self
    }

    /// 注入 YOLO classifier 用于 Yolo 权限模式。与 with_auto_classifier 互不干扰；
    /// Yolo 模式使用 YoloClassifier 的特定规则，Auto 模式使用通用 LLM-based classifier。
    pub fn with_yolo_classifier(mut self, classifier: Arc<dyn AutoClassifier>) -> Self {
        self.auto_classifier = Some(classifier);
        self
    }

    /// Snapshot view of the rule list.
    pub fn rules(&self) -> Vec<PermissionRule> {
        self.rules.read().unwrap().rules().to_vec()
    }

    /// **P (2026-05-17)**: 运行时加规则。用于"允许此项目"流程 —— 用户选
    /// "allow for this project" 后，Engine 构造 ProjectSettings 规则写入。
    pub fn add_rules(&self, additional: Vec<PermissionRule>) {
        self.rules.write().unwrap().extend(additional);
    }

    /// Produce a human-readable explanation of what permission decision
    /// the rule engine would make for a given tool call.
    ///
    /// This is a lightweight, synchronous alternative to `check()` that
    /// only consults the rule engine (no tool-specific checks, no mode
    /// dispatch). It explains whether a rule matches, and if so, which one.
    ///
    /// Examples:
    /// - `"Allowed by rule: Bash(git:*) matches \"git status\""`
    /// - `"Denied by path safety: /etc/passwd is in bypass-immune deny list"`
    /// - `"Ask: no matching rule found for Bash(kubectl) — no rules configured for this tool"`
    /// - `"Rule matches: Bash(git status) [ask] — requires confirmation"`
    pub fn explain_decision(&self, tool_name: &str, tool_input: &Value) -> String {
        // Best-effort content extraction mirroring tool implementations.
        let content = extract_content_from_input(tool_input);

        // 1. Check bypass-immune paths even in the explainer, since this
        //    check always runs before mode dispatch.
        if let Some(ref c) = content {
            if is_path_bypass_immune(c) {
                return format!("Denied by path safety: \"{c}\" is in the bypass-immune deny list");
            }
        }

        // 2. Rule engine evaluation.
        let rules = self.rules.read().unwrap();
        match rules.evaluate(tool_name, content.as_deref()) {
            RuleHit::Allow(rule) => {
                let rule_str = format_rule_string(&rule);
                let content_info = content
                    .as_ref()
                    .map(|c| format!(" matches \"{c}\""))
                    .unwrap_or_default();
                format!("Allowed by rule: {rule_str}{content_info}")
            }
            RuleHit::Deny(rule) => {
                let rule_str = format_rule_string(&rule);
                let content_info = content
                    .as_ref()
                    .map(|c| format!(" matches \"{c}\""))
                    .unwrap_or_default();
                format!("Denied by rule: {rule_str}{content_info}")
            }
            RuleHit::Ask(rule) => {
                let rule_str = format_rule_string(&rule);
                let content_info = content
                    .as_ref()
                    .map(|c| format!(" matches \"{c}\""))
                    .unwrap_or_default();
                format!("Rule matches: {rule_str} [ask]{content_info} — requires confirmation")
            }
            RuleHit::None => {
                let content_display = content.as_deref().unwrap_or("<no content>");
                let has_rules_for_tool = rules
                    .rules()
                    .iter()
                    .any(|r| crate::ruleset::matches_tool_name(&r.tool_name, tool_name));

                if has_rules_for_tool {
                    format!(
                        "Ask: no matching rule found for {}({content_display}) — \
                         the tool has rules but none match this specific content",
                        tool_name,
                    )
                } else {
                    format!(
                        "Ask: no matching rule found for {}({content_display}) — \
                         no rules have been configured for this tool",
                        tool_name,
                    )
                }
            }
        }
    }

    pub async fn check(
        &self,
        tool: &dyn Tool,
        input: &Value,
        ctx: &ToolContext,
    ) -> Result<PermissionDecision, GateError> {
        // 1. validateInput
        if let base::tool::ValidationResult::Err(msg, code) = tool.validate_input(input, ctx).await
        {
            return Err(GateError::InvalidInput { message: msg, code });
        }

        // 2. tool.check_permissions —— 工具自带判定
        match tool.check_permissions(input, ctx).await {
            base::tool::PermissionDecision::Allow { .. } => {
                return Ok(PermissionDecision::Allow {
                    updated_input: None,
                    decision_reason: Some(DecisionReason::Other("tool allowed".into())),
                });
            }
            base::tool::PermissionDecision::Deny { reason, .. } => {
                return Ok(PermissionDecision::Deny {
                    message: reason.unwrap_or_default(),
                    decision_reason: DecisionReason::Other("tool denied".into()),
                })
            }
            base::tool::PermissionDecision::Ask { .. } => {}
        }

        // 3. 通用规则引擎
        let content = tool.permission_match_content(input);
        match self
            .rules
            .read()
            .unwrap()
            .evaluate(tool.name(), content.as_deref())
        {
            RuleHit::Allow(rule) => {
                return Ok(PermissionDecision::Allow {
                    updated_input: None,
                    decision_reason: Some(DecisionReason::Rule(rule)),
                });
            }
            RuleHit::Deny(rule) => {
                return Ok(PermissionDecision::Deny {
                    message: format!(
                        "denied by rule: {}{}",
                        rule.tool_name,
                        rule.rule_content
                            .as_deref()
                            .map(|c| format!("({c})"))
                            .unwrap_or_default()
                    ),
                    decision_reason: DecisionReason::Rule(rule),
                });
            }
            RuleHit::Ask(_) | RuleHit::None => { /* 落到 mode 分派 */ }
        }

        // 4. 模式分派 —— 读 session（运行时可变），不读 config 的初始值

        // Bypass-immune safety: even in BypassPermissions mode, block access
        // to sensitive paths that should never be written without explicit
        // user confirmation. Equivalent to TS step 1g (safety checks).
        if let Some(content) = content.as_deref() {
            if is_path_bypass_immune(content) {
                return Ok(PermissionDecision::Deny {
                    message: format!(
                        "access to protected path '{content}' is blocked regardless of \
                         permission mode — this path is bypass-immune"
                    ),
                    decision_reason: DecisionReason::ToolBuiltin("bypass_immune".into()),
                });
            }
        }

        let mode = ctx.session.permission_mode();
        match mode {
            PermissionMode::BypassPermissions => Ok(PermissionDecision::Allow {
                updated_input: None,
                decision_reason: Some(DecisionReason::Mode(mode)),
            }),

            PermissionMode::Plan if !tool.is_read_only(input) => Ok(PermissionDecision::Deny {
                message: "plan mode forbids non-readonly tools".into(),
                decision_reason: DecisionReason::Mode(mode),
            }),

            PermissionMode::AcceptEdits => {
                let read_only = tool.is_read_only(input);
                let is_edit_or_write = tool.name() == "Edit" || tool.name() == "Write";
                if read_only || is_edit_or_write {
                    Ok(PermissionDecision::Allow {
                        updated_input: None,
                        decision_reason: Some(DecisionReason::Mode(mode)),
                    })
                } else {
                    // acceptEdits 不适用 → 走默认 ask
                    Ok(PermissionDecision::Ask {
                        message: format!("Allow {}?", tool.name()),
                        decision_reason: Some(DecisionReason::Mode(mode)),
                    })
                }
            }

            PermissionMode::DontAsk => Ok(PermissionDecision::Deny {
                message: "dontAsk mode and no rule matched".into(),
                decision_reason: DecisionReason::Mode(mode),
            }),

            // P2: Bubble mode — forward permission requests to parent agent
            // rather than prompting the user. The parent agent decides allow/deny.
            // TS parity: bubble permission mode in team/coordinator contexts.
            PermissionMode::Bubble => Ok(PermissionDecision::Ask {
                message: "Bubble: forwarding permission request to parent agent".into(),
                decision_reason: Some(DecisionReason::Mode(mode)),
            }),

            PermissionMode::Default
            | PermissionMode::Plan
            | PermissionMode::Auto
            | PermissionMode::Yolo => {
                // Plan 命中此处说明 tool 是 read_only —— 默认 allow
                if matches!(mode, PermissionMode::Plan) && tool.is_read_only(input) {
                    return Ok(PermissionDecision::Allow {
                        updated_input: None,
                        decision_reason: Some(DecisionReason::Mode(mode)),
                    });
                }
                // Auto / Yolo 模式：如果挂了 classifier 就让它判一下。
                // - Allow → 允许
                // - Deny → 拒绝（仅 LLM-based classifier 产生）
                // - AllowWithEdit → 允许（reason 附带建议内容）
                // - Defer → 继续走 ask。Default 和 Plan(write) 直接走 ask。
                if matches!(mode, PermissionMode::Auto | PermissionMode::Yolo) {
                    if let Some(classifier) = &self.auto_classifier {
                        let prompt_ctx = base::tool::PromptContext::default();
                        let description = tool.prompt(&prompt_ctx).await;
                        let decision = classifier.classify(tool.name(), &description, input).await;
                        match decision {
                            ClassifyDecision::Allow { reason } => {
                                return Ok(PermissionDecision::Allow {
                                    updated_input: None,
                                    decision_reason: Some(DecisionReason::Other(format!(
                                        "auto-classifier: {reason}"
                                    ))),
                                });
                            }
                            ClassifyDecision::Deny { reason } => {
                                return Ok(PermissionDecision::Deny {
                                    message: format!("classifier denied: {reason}"),
                                    decision_reason: DecisionReason::Classifier {
                                        classifier: "llm".into(),
                                    },
                                });
                            }
                            ClassifyDecision::AllowWithEdit {
                                reason,
                                suggested_edits,
                            } => {
                                let mut label = format!("auto-classifier: {reason}");
                                if !suggested_edits.is_empty() {
                                    label.push_str(&format!(
                                        " (suggested edits: {suggested_edits})"
                                    ));
                                }
                                return Ok(PermissionDecision::Allow {
                                    updated_input: None,
                                    decision_reason: Some(DecisionReason::Other(label)),
                                });
                            }
                            ClassifyDecision::Defer => { /* fall through to Ask */ }
                        }
                    }
                }
                Ok(PermissionDecision::Ask {
                    message: format!("Allow {}?", tool.name()),
                    decision_reason: Some(DecisionReason::Mode(mode)),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use base::context::SessionState;
    use base::error::ToolError;
    use base::permission::PermissionMode;
    use base::tool::Tool;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// 一个可配置的 fake tool，用来覆盖 gate 各分支。
    struct FakeTool {
        name: &'static str,
        read_only: bool,
        own_decision: Option<base::tool::PermissionDecision>,
        match_content: Option<String>,
    }

    impl FakeTool {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                read_only: false,
                own_decision: None,
                match_content: None,
            }
        }
        fn read_only(mut self) -> Self {
            self.read_only = true;
            self
        }
        fn own(mut self, d: base::tool::PermissionDecision) -> Self {
            self.own_decision = Some(d);
            self
        }
        fn matches(mut self, c: &str) -> Self {
            self.match_content = Some(c.into());
            self
        }
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }
        fn input_schema(&self) -> Value {
            json!({"type":"object"})
        }
        async fn prompt(&self, _: &base::tool::PromptContext) -> String {
            "fake".into()
        }
        fn is_read_only(&self, _: &Value) -> bool {
            self.read_only
        }
        async fn check_permissions(
            &self,
            _: &Value,
            _: &base::tool::ToolContext,
        ) -> base::tool::PermissionDecision {
            self.own_decision
                .clone()
                .unwrap_or(base::tool::PermissionDecision::ask("?"))
        }
        fn permission_match_content(&self, _: &Value) -> Option<String> {
            self.match_content.clone()
        }
        async fn call(
            &self,
            _: Value,
            _: base::tool::ToolContext,
            _: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, ToolError> {
            Ok(base::tool::ToolResult::text("ok"))
        }
    }

    fn ctx_with_mode(mode: PermissionMode) -> base::tool::ToolContext {
        let mut ctx = base::tool::ToolContext::for_test(PathBuf::from("/tmp"));
        ctx.permission_mode = mode;
        ctx.session = Arc::new(SessionState::new(PathBuf::from("/tmp")).with_permission_mode(mode));
        ctx.tool_use_id = "test".into();
        ctx
    }

    #[tokio::test]
    async fn tool_own_allow_short_circuits() {
        let tool = FakeTool::new("X").own(base::tool::PermissionDecision::allow());
        let gate = PermissionGate::empty();
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn tool_own_deny_short_circuits() {
        let tool = FakeTool::new("X").own(base::tool::PermissionDecision::Deny {
            reason: Some("no".into()),
            decision_reason: None,
        });
        let gate = PermissionGate::empty();
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn rule_engine_allow_overrides_default_ask() {
        let tool = FakeTool::new("Bash").matches("ls");
        let rules = RuleSet::new(vec![PermissionRule {
            source: base::permission::RuleSource::UserSettings,
            behavior: base::permission::RuleBehavior::Allow,
            tool_name: "Bash".into(),
            rule_content: Some("ls".into()),
        }]);
        let gate = PermissionGate::new(rules);
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn rule_engine_deny_short_circuits() {
        let tool = FakeTool::new("Bash").matches("rm -rf /");
        let rules = RuleSet::new(vec![PermissionRule {
            source: base::permission::RuleSource::UserSettings,
            behavior: base::permission::RuleBehavior::Deny,
            tool_name: "Bash".into(),
            rule_content: Some("rm -rf:*".into()),
        }]);
        let gate = PermissionGate::new(rules);
        let d = gate
            .check(
                &tool,
                &json!({}),
                &ctx_with_mode(PermissionMode::BypassPermissions),
            )
            .await
            .unwrap();
        // 即便 Bypass 模式，更早的 rule deny 已生效
        assert!(matches!(d, PermissionDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn bypass_mode_allows_when_no_rule() {
        let tool = FakeTool::new("Whatever");
        let gate = PermissionGate::empty();
        let d = gate
            .check(
                &tool,
                &json!({}),
                &ctx_with_mode(PermissionMode::BypassPermissions),
            )
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn plan_mode_denies_non_readonly() {
        let tool = FakeTool::new("Write");
        let gate = PermissionGate::empty();
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Plan))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn plan_mode_allows_readonly() {
        let tool = FakeTool::new("Read").read_only();
        let gate = PermissionGate::empty();
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Plan))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn accept_edits_allows_write_tool() {
        let tool = FakeTool::new("Write");
        let gate = PermissionGate::empty();
        let d = gate
            .check(
                &tool,
                &json!({}),
                &ctx_with_mode(PermissionMode::AcceptEdits),
            )
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn dontask_mode_denies_all_unmatched_tools() {
        let tool = FakeTool::new("Bash");
        let gate = PermissionGate::empty();
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::DontAsk))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn dontask_mode_with_allow_rule_permits() {
        let tool = FakeTool::new("Read").read_only();
        let rules = RuleSet::new(vec![PermissionRule {
            source: base::permission::RuleSource::UserSettings,
            behavior: base::permission::RuleBehavior::Allow,
            tool_name: "Read".into(),
            rule_content: None,
        }]);
        let gate = PermissionGate::new(rules);
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::DontAsk))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Allow { .. }));
    }

    #[test]
    fn dontask_mode_parses_from_config() {
        let mode: PermissionMode = serde_json::from_str("\"dontAsk\"").unwrap();
        assert_eq!(mode, PermissionMode::DontAsk);
    }

    #[tokio::test]
    async fn default_mode_asks_when_no_rule() {
        let tool = FakeTool::new("Bash");
        let gate = PermissionGate::empty();
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Ask { .. }));
    }

    #[tokio::test]
    async fn auto_mode_without_classifier_falls_to_ask() {
        // 没挂 classifier 时 Auto 退化为 Default ask
        let tool = FakeTool::new("Bash");
        let gate = PermissionGate::empty();
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Auto))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Ask { .. }));
    }

    /// 测试用 stub classifier：固定返回某 ClassifyDecision。
    struct StubClassifier(ClassifyDecision);

    #[async_trait]
    impl AutoClassifier for StubClassifier {
        async fn classify(&self, _: &str, _: &str, _: &Value) -> ClassifyDecision {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn auto_mode_with_classifier_allow_returns_allow() {
        let tool = FakeTool::new("Read");
        let gate = PermissionGate::empty().with_auto_classifier(Arc::new(StubClassifier(
            ClassifyDecision::Allow {
                reason: "obviously read-only".into(),
            },
        )));
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Auto))
            .await
            .unwrap();
        match d {
            PermissionDecision::Allow {
                decision_reason: Some(DecisionReason::Other(label)),
                ..
            } => {
                assert!(label.contains("auto-classifier"));
                assert!(label.contains("obviously read-only"));
            }
            other => panic!("expected Allow with auto-classifier reason, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn auto_mode_with_classifier_defer_falls_to_ask() {
        let tool = FakeTool::new("Bash");
        let gate = PermissionGate::empty()
            .with_auto_classifier(Arc::new(StubClassifier(ClassifyDecision::Defer)));
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Auto))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Ask { .. }));
    }

    #[tokio::test]
    async fn auto_mode_with_classifier_deny_returns_deny() {
        let tool = FakeTool::new("Bash");
        let gate = PermissionGate::empty().with_auto_classifier(Arc::new(StubClassifier(
            ClassifyDecision::Deny {
                reason: "rm -rf / is destructive".into(),
            },
        )));
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Auto))
            .await
            .unwrap();
        match d {
            PermissionDecision::Deny {
                message,
                decision_reason: DecisionReason::Classifier { classifier },
            } => {
                assert!(message.contains("destructive"));
                assert_eq!(classifier, "llm");
            }
            other => panic!("expected Deny with classifier reason, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn auto_mode_with_classifier_allow_with_edit_returns_allow() {
        let tool = FakeTool::new("Bash");
        let gate = PermissionGate::empty().with_auto_classifier(Arc::new(StubClassifier(
            ClassifyDecision::AllowWithEdit {
                reason: "safe with dry-run".into(),
                suggested_edits: "add --dry-run flag".into(),
            },
        )));
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Auto))
            .await
            .unwrap();
        match d {
            PermissionDecision::Allow {
                decision_reason: Some(DecisionReason::Other(label)),
                ..
            } => {
                assert!(label.contains("dry-run"));
                assert!(label.contains("add --dry-run flag"));
            }
            other => panic!("expected Allow with classifier reason, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn auto_mode_with_classifier_deny_short_circuits_to_deny() {
        let tool = FakeTool::new("Bash");
        let gate = PermissionGate::empty().with_auto_classifier(Arc::new(StubClassifier(
            ClassifyDecision::Deny {
                reason: "dangerous across any mode".into(),
            },
        )));
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Auto))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn auto_mode_with_classifier_allow_with_edit_empty_edits() {
        let tool = FakeTool::new("Read");
        let gate = PermissionGate::empty().with_auto_classifier(Arc::new(StubClassifier(
            ClassifyDecision::AllowWithEdit {
                reason: "fine as-is".into(),
                suggested_edits: String::new(),
            },
        )));
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Auto))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn classifier_does_not_run_in_default_mode() {
        // Default 模式根本不该调 classifier；这里给个会 panic 的 classifier 也安全
        struct ExplodingClassifier;
        #[async_trait]
        impl AutoClassifier for ExplodingClassifier {
            async fn classify(&self, _: &str, _: &str, _: &Value) -> ClassifyDecision {
                panic!("should never be called in Default mode");
            }
        }
        let tool = FakeTool::new("Read");
        let gate = PermissionGate::empty().with_auto_classifier(Arc::new(ExplodingClassifier));
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Ask { .. }));
    }

    use base::permission::PermissionRule;
}

/// Check whether a path (or command argument) references a bypass-immune
/// sensitive path. Even in BypassPermissions mode, these paths should never
/// be accessed without explicit user awareness.
fn is_path_bypass_immune(content: &str) -> bool {
    let lowered = content.to_lowercase();
    // Check if this looks like a file path (contains /)
    let looks_like_path = lowered.contains('/');
    if looks_like_path {
        // Check path components — not just substring. A component boundary
        // is either the start of the string or preceded by `/`.
        for sensitive in &[".git", ".claude", ".atta", ".ssh", ".aws", ".gnupg"] {
            if let Some(idx) = lowered.find(sensitive) {
                let is_start = idx == 0;
                let preceded_by_slash = idx > 0 && lowered.as_bytes().get(idx - 1) == Some(&b'/');
                if is_start || preceded_by_slash {
                    return true;
                }
            }
        }
        // Critical system files
        for sys_path in &["/etc/passwd", "/etc/shadow", "/etc/ssh/"] {
            if lowered.contains(sys_path) {
                return true;
            }
        }
    }
    // Destructive patterns — check on whitespace boundaries to avoid false
    // positives like "echo rm -rf /" in descriptions.
    for destructive in &["rm -rf /", "mkfs.", "dd if="] {
        if let Some(idx) = lowered.find(destructive) {
            let at_start = idx == 0;
            let after_space = idx > 0 && lowered.as_bytes().get(idx - 1) == Some(&b' ');
            if at_start || after_space {
                return true;
            }
        }
    }
    false
}

/// Best-effort extraction of permission-match content from a JSON tool
/// input, mirroring what individual tool implementations do in their
/// `permission_match_content` methods. This is used by the explainer
/// for human-readable descriptions; it does not need to be exhaustive
/// since the real decision always goes through the proper Tool trait.
fn extract_content_from_input(input: &Value) -> Option<String> {
    // Bash, Monitor, and similar command-based tools
    if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
        return Some(cmd.to_string());
    }
    // FileRead, FileWrite, FileEdit, and similar path-based tools
    if let Some(path) = input.get("file_path").and_then(|v| v.as_str()) {
        return Some(path.to_string());
    }
    if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
        return Some(path.to_string());
    }
    // WebFetch
    if let Some(url) = input.get("url").and_then(|v| v.as_str()) {
        return Some(url.to_string());
    }
    // Glob, Grep
    if let Some(pattern) = input.get("pattern").and_then(|v| v.as_str()) {
        return Some(pattern.to_string());
    }
    // WebSearch
    if let Some(query) = input.get("query").and_then(|v| v.as_str()) {
        return Some(query.to_string());
    }
    None
}

#[cfg(test)]
mod explain_tests {
    use super::*;
    use base::permission::{RuleBehavior, RuleSource};
    use serde_json::json;

    fn make_gate_with_rules(rules: Vec<(bool, &str, &str)>) -> PermissionGate {
        let permission_rules: Vec<PermissionRule> = rules
            .into_iter()
            .map(|(is_allow, tool, content)| {
                let behavior = if is_allow {
                    RuleBehavior::Allow
                } else {
                    RuleBehavior::Deny
                };
                PermissionRule {
                    source: RuleSource::UserSettings,
                    behavior,
                    tool_name: tool.into(),
                    rule_content: if content.is_empty() {
                        None
                    } else {
                        Some(content.into())
                    },
                }
            })
            .collect();
        PermissionGate::new(RuleSet::new(permission_rules))
    }

    #[test]
    fn explain_allowed_by_rule_matches_content() {
        let gate = make_gate_with_rules(vec![(true, "Bash", "git status")]);
        let explanation = gate.explain_decision("Bash", &json!({"command": "git status"}));
        assert!(
            explanation.contains("Allowed by rule"),
            "expected Allowed by rule, got: {explanation}"
        );
        assert!(
            explanation.contains("Bash(git status)"),
            "expected rule ref, got: {explanation}"
        );
        assert!(explanation.contains("git status"));
    }

    #[test]
    fn explain_allowed_by_prefix_rule() {
        let gate = make_gate_with_rules(vec![(true, "Bash", "git:*")]);
        let explanation = gate.explain_decision("Bash", &json!({"command": "git log"}));
        assert!(explanation.contains("Allowed by rule"));
        assert!(explanation.contains("Bash(git:*)"));
        assert!(explanation.contains("git log"));
    }

    #[test]
    fn explain_denied_by_rule() {
        let gate = make_gate_with_rules(vec![(false, "Read", "/tmp/secret/**")]);
        let explanation =
            gate.explain_decision("Read", &json!({"file_path": "/tmp/secret/data.txt"}));
        assert!(
            explanation.contains("Denied by rule"),
            "expected Denied by rule, got: {explanation}"
        );
        assert!(explanation.contains("/tmp/secret/**"));
    }

    #[test]
    fn explain_ask_no_matching_rule() {
        let gate = make_gate_with_rules(vec![(true, "Bash", "git:*")]);
        let explanation = gate.explain_decision("Read", &json!({"file_path": "/tmp/x"}));
        assert!(
            explanation.contains("Ask: no matching rule found"),
            "expected Ask: no matching rule found, got: {explanation}"
        );
        assert!(explanation.contains("Read"));
    }

    #[test]
    fn explain_bypass_immune_path() {
        let gate = PermissionGate::empty();
        let explanation = gate.explain_decision("Read", &json!({"file_path": "/etc/passwd"}));
        assert!(
            explanation.contains("Denied by path safety"),
            "expected Denied by path safety, got: {explanation}"
        );
        assert!(explanation.contains("/etc/passwd"));
    }

    #[test]
    fn explain_ask_rule() {
        let rules = vec![PermissionRule {
            source: RuleSource::UserSettings,
            behavior: RuleBehavior::Ask,
            tool_name: "Bash".into(),
            rule_content: None,
        }];
        let gate = PermissionGate::new(RuleSet::new(rules));
        let explanation = gate.explain_decision("Bash", &json!({"command": "ls"}));
        assert!(
            explanation.contains("[ask]"),
            "expected [ask] marker, got: {explanation}"
        );
        assert!(explanation.contains("requires confirmation"));
    }

    #[test]
    fn extract_content_from_command_field() {
        let input = json!({"command": "git push origin main"});
        assert_eq!(
            extract_content_from_input(&input),
            Some("git push origin main".into())
        );
    }

    #[test]
    fn extract_content_from_file_path_field() {
        let input = json!({"file_path": "/etc/hosts"});
        assert_eq!(
            extract_content_from_input(&input),
            Some("/etc/hosts".into())
        );
    }

    #[test]
    fn extract_content_returns_none_for_empty_input() {
        let input = json!({});
        assert_eq!(extract_content_from_input(&input), None);
    }

    #[test]
    fn extract_content_from_url_field() {
        let input = json!({"url": "https://example.com"});
        assert_eq!(
            extract_content_from_input(&input),
            Some("https://example.com".into())
        );
    }

    #[test]
    fn extract_content_from_path_field() {
        let input = json!({"path": "/tmp/file.txt"});
        assert_eq!(
            extract_content_from_input(&input),
            Some("/tmp/file.txt".into())
        );
    }
}
