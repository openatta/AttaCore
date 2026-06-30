//! `RuleSet` —— 规则集合 + 匹配引擎。
//!
//! 匹配选优（key 元组比较，max wins）：
//! 1. **特异性**（specificity）：有 content 的比无 content 的更特异；
//!    longer pattern length = 更特异
//! 2. **来源优先级**：CliArg > Session > Local > Project > User > Policy
//! 3. **行为优先级**：Deny > Ask > Allow（同 source / 同特异性时 Deny 最强）
//!
//! 不做"默认拒"语义；规则不命中就返回 None，由 mode 分派定夺。

use crate::dangerous::{detect_dangerous_rules, log_dangerous_warnings};
use crate::shadow::{detect_shadowed_rules, log_shadow_warnings};
use base::permission::{PermissionMode, PermissionRule, RuleBehavior};

/// Tools that are considered safe (read-only, no side effects).
///
/// These are auto-allowed even in strict modes (DontAsk, Plan) and are the
/// only tools auto-allowed in BypassPermissions without path safety checks.
pub const SAFE_TOOLS: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "LSP",
    "TaskList",
    "TaskGet",
    "ToolSearch",
];

/// Result of the safe-tool / mode-based quick check via `RuleSet::check`.
///
/// - `Allowed` — the tool is auto-allowed by the safe-tool allowlist or mode policy.
/// - `Deferred` — needs further evaluation by rules and standard permission flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckResult {
    Allowed,
    Deferred,
}

/// 规则匹配结果。
pub enum RuleHit {
    Allow(PermissionRule),
    Deny(PermissionRule),
    Ask(PermissionRule),
    /// 没有规则命中
    None,
}

#[derive(Default, Clone)]
pub struct RuleSet {
    rules: Vec<PermissionRule>,
}

impl RuleSet {
    /// Empty/default instance with no state.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Construct a new instance.
    ///
    /// Detects shadowed rules and dangerous allow rules, logging warnings
    /// for each finding.
    pub fn new(rules: Vec<PermissionRule>) -> Self {
        let shadows = detect_shadowed_rules(&rules);
        if !shadows.is_empty() {
            log_shadow_warnings(&shadows);
        }
        let dangers = detect_dangerous_rules(&rules);
        if !dangers.is_empty() {
            log_dangerous_warnings(&dangers);
        }
        Self { rules }
    }

    /// Append a new entry.
    pub fn add(&mut self, r: PermissionRule) {
        self.rules.push(r);
    }

    /// Append all entries from `rs`.
    pub fn extend<I: IntoIterator<Item = PermissionRule>>(&mut self, rs: I) {
        self.rules.extend(rs);
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Read-only view of the rule list.
    pub fn rules(&self) -> &[PermissionRule] {
        &self.rules
    }

    /// 给定工具名 + 可选匹配内容，返回最优命中。
    ///
    /// `content` 是 `Tool::permission_match_content(input)` 的结果（如 Bash 命令、
    /// Read 文件路径）。`None` 表示工具没提供匹配内容；此时只按工具名（无 content
    /// 限定的）规则匹配。
    pub fn evaluate(&self, tool_name: &str, content: Option<&str>) -> RuleHit {
        let mut best: Option<(&PermissionRule, MatchScore)> = None;

        for r in &self.rules {
            if !matches_tool_name(&r.tool_name, tool_name) {
                continue;
            }
            let specificity = match (&r.rule_content, content) {
                (None, _) => 0, // 仅工具名
                (Some(rc), Some(c)) => {
                    if !matches_content(rc, c) {
                        continue;
                    }
                    // 长度作为特异性的近似指标
                    rc.len() as i32
                }
                (Some(_), None) => continue, // 规则要 content，工具没给
            };

            let score = MatchScore {
                specificity,
                source_priority: r.source.priority() as i32,
                behavior_rank: behavior_rank(r.behavior),
            };

            if best.is_none() || score > best.as_ref().unwrap().1 {
                best = Some((r, score));
            }
        }

        match best {
            None => RuleHit::None,
            Some((r, _)) => match r.behavior {
                RuleBehavior::Allow => RuleHit::Allow(r.clone()),
                RuleBehavior::Deny => RuleHit::Deny(r.clone()),
                RuleBehavior::Ask => RuleHit::Ask(r.clone()),
            },
        }
    }

    /// 基于安全工具白名单与权限模式的快速决策。
    ///
    /// - **BypassPermissions**: 安全工具直接允许；非安全工具仍返回 `Deferred`，
    ///   由调用方应用路径安全检查后再决定。
    /// - **DontAsk / Plan**: 安全工具直接允许（只读操作不应被严格模式误杀）。
    /// - 其它模式: 一律 `Deferred`，走标准规则匹配与模式分派流程。
    pub fn check(
        &self,
        tool_name: &str,
        _content: Option<&str>,
        mode: PermissionMode,
    ) -> CheckResult {
        match mode {
            PermissionMode::BypassPermissions => {
                // 安全工具绕过
                if SAFE_TOOLS.contains(&tool_name) {
                    return CheckResult::Allowed;
                }
                // 非安全工具：调用方应继续执行路径安全检查
                CheckResult::Deferred
            }
            PermissionMode::DontAsk | PermissionMode::Plan => {
                // 严格模式下安全工具自动放行
                if SAFE_TOOLS.contains(&tool_name) {
                    return CheckResult::Allowed;
                }
                CheckResult::Deferred
            }
            _ => CheckResult::Deferred,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct MatchScore {
    specificity: i32,
    source_priority: i32,
    behavior_rank: i32,
}

fn behavior_rank(b: RuleBehavior) -> i32 {
    match b {
        RuleBehavior::Deny => 2,
        RuleBehavior::Ask => 1,
        RuleBehavior::Allow => 0,
    }
}

pub(crate) fn matches_tool_name(rule: &str, actual: &str) -> bool {
    if rule == actual {
        return true;
    }
    // MCP server 前缀匹配："mcp__github" → "mcp__github__create_issue"
    if let Some(prefix) = rule.strip_prefix("mcp__") {
        if let Some(actual_after) = actual.strip_prefix("mcp__") {
            return actual_after == prefix || actual_after.starts_with(&format!("{prefix}__"));
        }
    }
    false
}

pub(crate) fn matches_content(pattern: &str, content: &str) -> bool {
    // 1. `prefix:*` —— 命令以 prefix 开头（exactly 等于 prefix 或 prefix + 空格 + ...）
    if let Some(prefix) = pattern.strip_suffix(":*") {
        return content == prefix || content.starts_with(&format!("{prefix} "));
    }
    // 2. shell glob —— `*` / `?` / `[...]`
    if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
        if let Ok(g) = globset::Glob::new(pattern) {
            return g.compile_matcher().is_match(content);
        }
        return false;
    }
    // 3. 精确匹配
    pattern == content
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::permission::{RuleBehavior, RuleSource};

    fn rule(
        tool: &str,
        content: Option<&str>,
        behavior: RuleBehavior,
        source: RuleSource,
    ) -> PermissionRule {
        PermissionRule {
            source,
            behavior,
            tool_name: tool.into(),
            rule_content: content.map(|s| s.into()),
        }
    }

    #[test]
    fn name_only_rule_matches_tool() {
        let rs = RuleSet::new(vec![rule(
            "Bash",
            None,
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert!(matches!(rs.evaluate("Bash", Some("ls")), RuleHit::Allow(_)));
        assert!(matches!(rs.evaluate("Read", None), RuleHit::None));
    }

    #[test]
    fn exact_content_match() {
        let rs = RuleSet::new(vec![rule(
            "Bash",
            Some("git status"),
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert!(matches!(
            rs.evaluate("Bash", Some("git status")),
            RuleHit::Allow(_)
        ));
        assert!(matches!(
            rs.evaluate("Bash", Some("git push")),
            RuleHit::None
        ));
    }

    #[test]
    fn prefix_glob_match() {
        let rs = RuleSet::new(vec![rule(
            "Bash",
            Some("git log:*"),
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert!(matches!(
            rs.evaluate("Bash", Some("git log")),
            RuleHit::Allow(_)
        ));
        assert!(matches!(
            rs.evaluate("Bash", Some("git log --oneline")),
            RuleHit::Allow(_)
        ));
        assert!(matches!(
            rs.evaluate("Bash", Some("git logger")),
            RuleHit::None
        ));
    }

    #[test]
    fn path_glob_match() {
        let rs = RuleSet::new(vec![rule(
            "Read",
            Some("/etc/**"),
            RuleBehavior::Deny,
            RuleSource::UserSettings,
        )]);
        assert!(matches!(
            rs.evaluate("Read", Some("/etc/passwd")),
            RuleHit::Deny(_)
        ));
        assert!(matches!(
            rs.evaluate("Read", Some("/etc/some/sub/file")),
            RuleHit::Deny(_)
        ));
        assert!(matches!(
            rs.evaluate("Read", Some("/var/log")),
            RuleHit::None
        ));
    }

    #[test]
    fn mcp_prefix_match() {
        let rs = RuleSet::new(vec![rule(
            "mcp__github",
            None,
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert!(matches!(
            rs.evaluate("mcp__github__create_issue", None),
            RuleHit::Allow(_)
        ));
        assert!(matches!(
            rs.evaluate("mcp__slack__post_msg", None),
            RuleHit::None
        ));
    }

    #[test]
    fn deny_beats_allow_at_same_specificity_and_source() {
        let rs = RuleSet::new(vec![
            rule(
                "Bash",
                Some("git status"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
            rule(
                "Bash",
                Some("git status"),
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
        ]);
        assert!(matches!(
            rs.evaluate("Bash", Some("git status")),
            RuleHit::Deny(_)
        ));
    }

    #[test]
    fn higher_priority_source_overrides_lower() {
        // user 拒，cli 准 → CliArg 优先级更高
        let rs = RuleSet::new(vec![
            rule(
                "Bash",
                Some("git push"),
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
            rule(
                "Bash",
                Some("git push"),
                RuleBehavior::Allow,
                RuleSource::CliArg,
            ),
        ]);
        assert!(matches!(
            rs.evaluate("Bash", Some("git push")),
            RuleHit::Allow(_)
        ));
    }

    #[test]
    fn more_specific_rule_wins_over_less_specific() {
        let rs = RuleSet::new(vec![
            // 名字级 deny
            rule("Bash", None, RuleBehavior::Deny, RuleSource::UserSettings),
            // 具体内容 allow
            rule(
                "Bash",
                Some("git status"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
        ]);
        // git status → 具体规则胜
        assert!(matches!(
            rs.evaluate("Bash", Some("git status")),
            RuleHit::Allow(_)
        ));
        // 任意其它命令 → 名字级 deny
        assert!(matches!(
            rs.evaluate("Bash", Some("rm -rf /")),
            RuleHit::Deny(_)
        ));
    }

    #[test]
    fn rule_with_content_skipped_when_tool_provides_no_content() {
        let rs = RuleSet::new(vec![rule(
            "AgentTool",
            Some("review"),
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert!(matches!(rs.evaluate("AgentTool", None), RuleHit::None));
    }

    #[test]
    fn empty_ruleset_yields_none() {
        let rs = RuleSet::empty();
        assert!(matches!(rs.evaluate("Bash", Some("ls")), RuleHit::None));
    }

    #[test]
    fn safe_tools_allowed_in_bypass_mode() {
        let rs = RuleSet::empty();
        for &tool in SAFE_TOOLS {
            assert_eq!(
                rs.check(tool, None, PermissionMode::BypassPermissions),
                CheckResult::Allowed,
                "safe tool {tool} should be Allowed in BypassPermissions"
            );
        }
    }

    #[test]
    fn unsafe_tools_deferred_in_bypass_mode() {
        let rs = RuleSet::empty();
        assert_eq!(
            rs.check("Bash", None, PermissionMode::BypassPermissions),
            CheckResult::Deferred,
            "Bash should be Deferred in BypassPermissions (needs path safety)"
        );
        assert_eq!(
            rs.check("Edit", None, PermissionMode::BypassPermissions),
            CheckResult::Deferred
        );
        assert_eq!(
            rs.check("Write", None, PermissionMode::BypassPermissions),
            CheckResult::Deferred
        );
    }

    #[test]
    fn safe_tools_allowed_in_strict_modes() {
        let rs = RuleSet::empty();
        for &tool in SAFE_TOOLS {
            assert_eq!(
                rs.check(tool, None, PermissionMode::DontAsk),
                CheckResult::Allowed,
                "safe tool {tool} should be Allowed in DontAsk"
            );
            assert_eq!(
                rs.check(tool, None, PermissionMode::Plan),
                CheckResult::Allowed,
                "safe tool {tool} should be Allowed in Plan"
            );
        }
    }

    #[test]
    fn unsafe_tools_deferred_in_strict_modes() {
        let rs = RuleSet::empty();
        assert_eq!(
            rs.check("Bash", None, PermissionMode::DontAsk),
            CheckResult::Deferred
        );
        assert_eq!(
            rs.check("Bash", None, PermissionMode::Plan),
            CheckResult::Deferred
        );
    }

    #[test]
    fn check_defers_in_default_mode() {
        let rs = RuleSet::empty();
        assert_eq!(
            rs.check("Read", None, PermissionMode::Default),
            CheckResult::Deferred,
            "even safe tools should be Deferred in Default mode"
        );
        assert_eq!(
            rs.check("Bash", None, PermissionMode::Default),
            CheckResult::Deferred
        );
    }

    #[test]
    fn safe_tools_list_contains_expected_tools() {
        assert!(SAFE_TOOLS.contains(&"Read"));
        assert!(SAFE_TOOLS.contains(&"Glob"));
        assert!(SAFE_TOOLS.contains(&"Grep"));
        assert!(SAFE_TOOLS.contains(&"LSP"));
        assert!(SAFE_TOOLS.contains(&"TaskList"));
        assert!(SAFE_TOOLS.contains(&"TaskGet"));
        assert!(SAFE_TOOLS.contains(&"ToolSearch"));
        assert_eq!(SAFE_TOOLS.len(), 7);
    }
}
