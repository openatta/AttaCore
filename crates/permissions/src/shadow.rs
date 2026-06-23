//! Shadowed rule detection for permission rules.
//!
//! A rule is "shadowed" when a higher-priority rule with an equal or broader
//! match pattern exists before it in the rules list. The shadowed rule will
//! never be reached by the matching engine, making it dead weight that may
//! confuse users who added it expecting it to take effect.
//!
//! TS parity: shadowedRuleDetection.ts — warns when a rule is silently
//! overridden by a higher-priority rule.

use crate::rule::format_rule_string;
use crate::ruleset::{matches_content, matches_tool_name};
use base::permission::{PermissionRule, RuleBehavior};

/// Describes one shadowed rule relationship.
///
/// A `shadowing` rule (higher priority, earlier in the list) completely
/// covers the same match space as `shadowed` (lower priority, later),
/// meaning `shadowed` will never be reached.
#[derive(Debug, Clone)]
pub struct ShadowedRule {
    /// The rule that is being overridden (silently ignored).
    pub shadowed: PermissionRule,
    /// The higher-priority rule that shadows it.
    pub shadowing: PermissionRule,
    /// Index of the shadowed rule in the original rules list (0-based).
    pub shadowed_index: usize,
    /// Index of the shadowing rule in the original rules list (0-based).
    pub shadowing_index: usize,
}

/// Check a ruleset for shadowed rules.
///
/// Iterates rules from highest to lowest positional priority (index 0
/// considered highest priority). For each rule at index `j`, checks
/// if any higher-priority rule (index < j) has a pattern that fully
/// contains its pattern. Returns all shadow relationships found.
///
/// # Shadow detection rules
///
/// | Higher (shadowing) | Lower (shadowed) | Detected? |
/// |---|---|---|
/// | `ToolName` | `ToolName(content)` | Yes — tool-only matches all content |
/// | `ToolName(*)` | `ToolName(foo)` | Yes — wildcard matches any content |
/// | `ToolName` (pos 0) | `ToolName` (pos 5) | Yes — duplicate, earlier wins |
/// | `ToolName(git:*)` | `ToolName(git status)` | Yes — prefix glob covers specific |
/// | `Read(/etc/**)` | `Read(/etc/passwd)` | Yes — path glob covers specific |
/// | `ToolName(git status)` | `ToolName(git:*)` | No — specific does not shadow broader |
pub fn detect_shadowed_rules(rules: &[PermissionRule]) -> Vec<ShadowedRule> {
    let mut result: Vec<ShadowedRule> = Vec::new();

    // Track which rules have already been identified as shadowed to avoid
    // reporting the same shadowed rule multiple times.
    let mut reported: Vec<bool> = vec![false; rules.len()];

    // (j, lower) is the lower-priority (candidate shadowed) rule;
    // (i, higher) is the higher-priority (candidate shadowing) rule.
    for (j, lower) in rules.iter().enumerate() {
        for (i, higher) in rules.iter().enumerate().take(j) {
            if reported[j] {
                break;
            }

            // Must operate on the same tool (or MCP scope where broader
            // scope shadows narrower)
            if !matches_tool_name(&higher.tool_name, &lower.tool_name) {
                continue;
            }

            if is_shadowed_by(higher, lower) {
                result.push(ShadowedRule {
                    shadowed: lower.clone(),
                    shadowing: higher.clone(),
                    shadowed_index: j,
                    shadowing_index: i,
                });
                reported[j] = true;
                // Break: first (closest) shadowing rule is the most relevant
                break;
            }
        }
    }

    result
}

/// Check if `higher` (earlier, higher positional priority) shadows
/// `lower` (later, lower positional priority).
///
/// Returns `true` when the higher-priority rule's pattern fully covers
/// the match space of the lower-priority rule.
pub(crate) fn is_shadowed_by(higher: &PermissionRule, lower: &PermissionRule) -> bool {
    match (&higher.rule_content, &lower.rule_content) {
        // Both tool-only: earlier tool-only rule shadows later tool-only rule
        (None, None) => true,

        // Higher is tool-only, lower has content:
        // tool-only rule matches ALL content for that tool, so it shadows any specific content
        (None, Some(_)) => true,

        // Higher has content, lower is tool-only:
        // specific content does NOT shadow the broader tool-only rule
        (Some(_), None) => false,

        // Both have content: check if higher-priority content pattern covers
        // the lower-priority content.
        (Some(hc), Some(lc)) => {
            // Exact same content string → exact duplicate
            if hc == lc {
                return true;
            }
            // Check if higher-priority pattern matches the lower-priority content
            // pattern treated as a content string. This catches:
            //   `*`       matches `git status`      (wildcard shadows specific)
            //   `git:*`   matches `git status`      (prefix covers specific)
            //   `/etc/**` matches `/etc/passwd`     (path glob covers specific)
            //
            // This is not a perfect containment check for arbitrary globs
            // (undecidable in general), but catches the practical cases.
            matches_content(hc, lc)
        }
    }
}

/// Format a single shadow warning for human-readable display.
///
/// Returns a string like:
/// ```text
/// Permission rule shadowed: "Bash(git:*)  [allow]" at line 5 is shadowed by "Bash(*)  [deny]" at line 2 (higher priority)
/// ```
pub fn format_shadow_warning(shadow: &ShadowedRule) -> String {
    let shadowed_str = format_rule_string(&shadow.shadowed);
    let shadowing_str = format_rule_string(&shadow.shadowing);

    format!(
        "Permission rule shadowed: \"{0}  [{3}]\" at line {4} is shadowed by \"{1}  [{2}]\" at line {5} (higher priority)",
        shadowed_str,
        shadowing_str,
        behavior_label(shadow.shadowing.behavior),
        behavior_label(shadow.shadowed.behavior),
        shadow.shadowed_index + 1,
        shadow.shadowing_index + 1,
    )
}

/// Format and log all shadow warnings via `tracing::warn!`.
///
/// Each shadow is logged as a separate warning so individual shadows
/// are visible in filtered log output.
pub fn log_shadow_warnings(shadows: &[ShadowedRule]) {
    for s in shadows {
        tracing::warn!("{}", format_shadow_warning(s));
    }
}

fn behavior_label(b: RuleBehavior) -> &'static str {
    match b {
        RuleBehavior::Allow => "allow",
        RuleBehavior::Deny => "deny",
        RuleBehavior::Ask => "ask",
    }
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
    fn tool_only_shadows_tool_with_content() {
        let rules = vec![
            rule("Bash", None, RuleBehavior::Deny, RuleSource::UserSettings),
            rule(
                "Bash",
                Some("git status"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
        ];
        let shadows = detect_shadowed_rules(&rules);
        assert_eq!(shadows.len(), 1);
        assert_eq!(shadows[0].shadowing_index, 0);
        assert_eq!(shadows[0].shadowed_index, 1);
        assert!(shadows[0].shadowing.rule_content.is_none());
    }

    #[test]
    fn exact_content_duplicate_shadow() {
        let rules = vec![
            rule(
                "Bash",
                Some("git status"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
            rule(
                "Bash",
                Some("git status"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
        ];
        let shadows = detect_shadowed_rules(&rules);
        assert_eq!(shadows.len(), 1);
        assert_eq!(shadows[0].shadowing_index, 0);
        assert_eq!(shadows[0].shadowed_index, 1);
    }

    #[test]
    fn tool_only_duplicate_shadow() {
        let rules = vec![
            rule("Bash", None, RuleBehavior::Deny, RuleSource::UserSettings),
            rule("Bash", None, RuleBehavior::Allow, RuleSource::UserSettings),
        ];
        let shadows = detect_shadowed_rules(&rules);
        assert_eq!(shadows.len(), 1);
        assert_eq!(shadows[0].shadowing_index, 0);
        assert_eq!(shadows[0].shadowed_index, 1);
    }

    #[test]
    fn wildcard_content_shadows_specific() {
        let rules = vec![
            rule(
                "Bash",
                Some("*"),
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
            rule(
                "Bash",
                Some("git status"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
        ];
        let shadows = detect_shadowed_rules(&rules);
        assert_eq!(shadows.len(), 1);
        assert_eq!(shadows[0].shadowing_index, 0);
        assert_eq!(shadows[0].shadowed_index, 1);
    }

    #[test]
    fn prefix_wildcard_shadows_specific() {
        let rules = vec![
            rule(
                "Bash",
                Some("git:*"),
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
            rule(
                "Bash",
                Some("git status"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
        ];
        let shadows = detect_shadowed_rules(&rules);
        assert_eq!(shadows.len(), 1);
        assert_eq!(shadows[0].shadowing_index, 0);
        assert_eq!(shadows[0].shadowed_index, 1);
    }

    #[test]
    fn path_glob_shadows_specific() {
        let rules = vec![
            rule(
                "Read",
                Some("/etc/**"),
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
            rule(
                "Read",
                Some("/etc/passwd"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
        ];
        let shadows = detect_shadowed_rules(&rules);
        assert_eq!(shadows.len(), 1);
        assert_eq!(shadows[0].shadowing_index, 0);
        assert_eq!(shadows[0].shadowed_index, 1);
    }

    #[test]
    fn specific_content_does_not_shadow_broader() {
        let rules = vec![
            rule(
                "Bash",
                Some("git status"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
            rule(
                "Bash",
                Some("git:*"),
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
        ];
        let shadows = detect_shadowed_rules(&rules);
        assert!(shadows.is_empty(), "specific should not shadow broader");
    }

    #[test]
    fn different_tools_no_shadow() {
        let rules = vec![
            rule("Bash", None, RuleBehavior::Deny, RuleSource::UserSettings),
            rule("Read", None, RuleBehavior::Allow, RuleSource::UserSettings),
        ];
        let shadows = detect_shadowed_rules(&rules);
        assert!(shadows.is_empty());
    }

    #[test]
    fn mcp_server_shadows_specific_tool() {
        let rules = vec![
            rule(
                "mcp__github",
                None,
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
            rule(
                "mcp__github__create_issue",
                None,
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
        ];
        let shadows = detect_shadowed_rules(&rules);
        assert_eq!(shadows.len(), 1);
        assert_eq!(shadows[0].shadowing_index, 0);
        assert_eq!(shadows[0].shadowed_index, 1);
    }

    #[test]
    fn mcp_specific_does_not_shadow_server() {
        let rules = vec![
            rule(
                "mcp__github__create_issue",
                None,
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
            rule(
                "mcp__github",
                None,
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
        ];
        // The specific tool at higher priority does NOT shadow the broader MCP server rule
        let shadows = detect_shadowed_rules(&rules);
        assert!(
            shadows.is_empty(),
            "specific MCP should not shadow broader MCP"
        );
    }

    #[test]
    fn multiple_shadows_detected() {
        let rules = vec![
            rule("Bash", None, RuleBehavior::Deny, RuleSource::UserSettings),
            rule(
                "Bash",
                Some("git status"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
            rule(
                "Bash",
                Some("git push"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
        ];
        let shadows = detect_shadowed_rules(&rules);
        assert_eq!(shadows.len(), 2);
        // Both specific rules shadowed by same tool-only deny
        assert_eq!(shadows[0].shadowing_index, 0);
        assert_eq!(shadows[0].shadowed_index, 1);
        assert_eq!(shadows[1].shadowing_index, 0);
        assert_eq!(shadows[1].shadowed_index, 2);
    }

    #[test]
    fn chain_shadow_reports_highest_priority() {
        // Three rules for same tool: each shadows the ones after it.
        // The algorithm reports the highest-priority (lowest index)
        // shadowing rule, not the closest one.
        let rules = vec![
            rule("Bash", None, RuleBehavior::Deny, RuleSource::UserSettings),
            rule(
                "Bash",
                Some("git:*"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
            rule(
                "Bash",
                Some("git status"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
        ];
        let shadows = detect_shadowed_rules(&rules);
        // git:* is shadowed by Bash (tool-only) — 1 shadow
        // git status is ALSO shadowed by Bash (tool-only) — 1 shadow
        assert_eq!(shadows.len(), 2);
        // First shadow: Bash → git:*
        assert_eq!(shadows[0].shadowing_index, 0);
        assert_eq!(shadows[0].shadowed_index, 1);
        // Second shadow: Bash (highest priority) → git status
        assert_eq!(shadows[1].shadowing_index, 0);
        assert_eq!(shadows[1].shadowed_index, 2);
    }

    #[test]
    fn no_shadow_when_no_match() {
        let rules = vec![
            rule(
                "Bash",
                Some("git:*"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
            rule(
                "Read",
                Some("/etc/**"),
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
        ];
        let shadows = detect_shadowed_rules(&rules);
        assert!(shadows.is_empty());
    }

    #[test]
    fn empty_rules_produce_no_shadows() {
        let rules: Vec<PermissionRule> = vec![];
        let shadows = detect_shadowed_rules(&rules);
        assert!(shadows.is_empty());
    }

    #[test]
    fn single_rule_produces_no_shadows() {
        let rules = vec![rule(
            "Bash",
            None,
            RuleBehavior::Deny,
            RuleSource::UserSettings,
        )];
        let shadows = detect_shadowed_rules(&rules);
        assert!(shadows.is_empty());
    }

    #[test]
    fn content_of_one_tool_does_not_shadow_another_tool() {
        // Even though "git status" ≠ "/etc/passwd" would suggest no shadow,
        // the different tool name already prevents any shadow
        let rules = vec![
            rule(
                "Read",
                Some("/etc/**"),
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
            rule(
                "Bash",
                Some("/etc/passwd"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
        ];
        let shadows = detect_shadowed_rules(&rules);
        assert!(shadows.is_empty());
    }

    #[test]
    fn format_shadow_warning_output() {
        let shadow = ShadowedRule {
            shadowed: PermissionRule {
                source: RuleSource::UserSettings,
                behavior: RuleBehavior::Allow,
                tool_name: "Bash".into(),
                rule_content: Some("git status".into()),
            },
            shadowing: PermissionRule {
                source: RuleSource::UserSettings,
                behavior: RuleBehavior::Deny,
                tool_name: "Bash".into(),
                rule_content: None,
            },
            shadowed_index: 4,
            shadowing_index: 1,
        };
        let msg = format_shadow_warning(&shadow);
        assert!(
            msg.contains("Permission rule shadowed:"),
            "should contain header: {msg}"
        );
        assert!(
            msg.contains("Bash(git status)"),
            "should contain rule: {msg}"
        );
        assert!(msg.contains("Bash"), "should contain shadowing rule: {msg}");
        assert!(msg.contains("[allow]"), "should contain behavior: {msg}");
        assert!(msg.contains("[deny]"), "should contain behavior: {msg}");
        assert!(msg.contains("at line 5"), "should contain line: {msg}");
        assert!(msg.contains("at line 2"), "should contain line: {msg}");
        assert!(
            msg.contains("higher priority"),
            "should contain suffix: {msg}"
        );
    }

    #[test]
    fn format_shadow_warning_tool_only() {
        let shadow = ShadowedRule {
            shadowed: PermissionRule {
                source: RuleSource::LocalSettings,
                behavior: RuleBehavior::Ask,
                tool_name: "mcp__github".into(),
                rule_content: None,
            },
            shadowing: PermissionRule {
                source: RuleSource::CliArg,
                behavior: RuleBehavior::Deny,
                tool_name: "mcp__github".into(),
                rule_content: None,
            },
            shadowed_index: 2,
            shadowing_index: 0,
        };
        let msg = format_shadow_warning(&shadow);
        assert!(msg.contains("mcp__github"));
        assert!(msg.contains("[ask]"));
        assert!(msg.contains("[deny]"));
        assert!(msg.contains("at line 3"));
        assert!(msg.contains("at line 1"));
    }

    #[test]
    fn prefix_pattern_does_not_shadow_non_matching_content() {
        // git:* should NOT shadow rm commands
        let rules = vec![
            rule(
                "Bash",
                Some("git:*"),
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
            rule(
                "Bash",
                Some("rm:*"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
        ];
        let shadows = detect_shadowed_rules(&rules);
        assert!(shadows.is_empty(), "git:* should not shadow rm:*");
    }
}
