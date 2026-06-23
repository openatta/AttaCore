//! 规则字符串与 `PermissionRule` 之间的双向转换。
//!
//! grammar（见 docs/DATA_FORMATS.md §B.5）：
//! ```text
//! RULE      ::= TOOL ( '(' CONTENT ')' )?
//! TOOL      ::= IDENTIFIER ( '__' IDENTIFIER )*
//! CONTENT   ::= 任意字符（不含未转义的 `)`）
//! ```
//!
//! 例：
//! - `Bash`                       —— 任何 Bash 调用
//! - `Bash(git status)`           —— 命令精确匹配 `git status`
//! - `Bash(git log:*)`            —— 命令以 `git log` 开头
//! - `Read(/etc/**)`              —— 文件路径 glob `/etc/**`
//! - `mcp__github`                —— github MCP server 的任意工具

use crate::error::ParseRuleError;
use base::permission::{PermissionRule, RuleBehavior, RuleSource};

/// 解析单条规则字符串。
pub fn parse_rule_string(
    s: &str,
    source: RuleSource,
    behavior: RuleBehavior,
) -> Result<PermissionRule, ParseRuleError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ParseRuleError::Empty);
    }

    if let Some(open_idx) = s.find('(') {
        if !s.ends_with(')') {
            return Err(ParseRuleError::Unbalanced(s.to_string()));
        }
        let tool_name = s[..open_idx].trim().to_string();
        if tool_name.is_empty() {
            return Err(ParseRuleError::Malformed(s.to_string()));
        }
        let content = s[open_idx + 1..s.len() - 1].to_string();
        return Ok(PermissionRule {
            source,
            behavior,
            tool_name,
            rule_content: Some(content),
        });
    }

    Ok(PermissionRule {
        source,
        behavior,
        tool_name: s.to_string(),
        rule_content: None,
    })
}

/// `PermissionRule` → 规则字符串。
pub fn format_rule_string(r: &PermissionRule) -> String {
    match &r.rule_content {
        Some(c) => format!("{}({})", r.tool_name, c),
        None => r.tool_name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<PermissionRule, ParseRuleError> {
        parse_rule_string(s, RuleSource::UserSettings, RuleBehavior::Allow)
    }

    #[test]
    fn parses_tool_only() {
        let r = parse("Bash").unwrap();
        assert_eq!(r.tool_name, "Bash");
        assert!(r.rule_content.is_none());
    }

    #[test]
    fn parses_tool_with_content() {
        let r = parse("Bash(git status)").unwrap();
        assert_eq!(r.tool_name, "Bash");
        assert_eq!(r.rule_content, Some("git status".into()));
    }

    #[test]
    fn parses_prefix_glob() {
        let r = parse("Bash(git push:*)").unwrap();
        assert_eq!(r.rule_content, Some("git push:*".into()));
    }

    #[test]
    fn parses_path_glob() {
        let r = parse("Read(/etc/**)").unwrap();
        assert_eq!(r.tool_name, "Read");
        assert_eq!(r.rule_content, Some("/etc/**".into()));
    }

    #[test]
    fn parses_mcp_prefix() {
        let r = parse("mcp__github").unwrap();
        assert_eq!(r.tool_name, "mcp__github");
        assert!(r.rule_content.is_none());

        let r = parse("mcp__github__create_issue").unwrap();
        assert_eq!(r.tool_name, "mcp__github__create_issue");
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(parse("").unwrap_err(), ParseRuleError::Empty);
        assert_eq!(parse("   ").unwrap_err(), ParseRuleError::Empty);
    }

    #[test]
    fn rejects_unbalanced() {
        assert!(matches!(
            parse("Bash(git status").unwrap_err(),
            ParseRuleError::Unbalanced(_)
        ));
    }

    #[test]
    fn rejects_no_tool() {
        assert!(matches!(
            parse("(git status)").unwrap_err(),
            ParseRuleError::Malformed(_)
        ));
    }

    #[test]
    fn format_roundtrip() {
        for s in &[
            "Bash",
            "Bash(git status)",
            "Bash(git push:*)",
            "Read(/etc/**)",
            "mcp__github__create_issue",
        ] {
            let r = parse(s).unwrap();
            assert_eq!(format_rule_string(&r), *s);
        }
    }

    #[test]
    fn trims_whitespace() {
        let r = parse("  Bash(git status)  ").unwrap();
        assert_eq!(r.tool_name, "Bash");
        assert_eq!(r.rule_content, Some("git status".into()));
    }
}
