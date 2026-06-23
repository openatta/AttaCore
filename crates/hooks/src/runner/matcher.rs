//! Hook `if` pattern matching utilities.
//!
//! Supports:
//! - `Bash` — exact tool name match
//! - `Bash(<content>)` — tool name + content (prefix:*, glob, exact)
//! - Other tool names (Read, Edit, mcp__*) — same rules

use crate::payload::HookInput;

/// Check whether a hook's `if` pattern matches the given input.
pub(super) fn if_matches(pattern: &str, input: &HookInput) -> bool {
    let pat = pattern.trim();
    let Some(tool_name) = input.tool_name.as_deref() else {
        return false;
    };

    let (rule_tool, rule_content) = match pat.find('(') {
        Some(i) if pat.ends_with(')') => (
            pat[..i].trim().to_string(),
            Some(pat[i + 1..pat.len() - 1].to_string()),
        ),
        _ => (pat.to_string(), None),
    };

    if !match_tool_name(&rule_tool, tool_name) {
        return false;
    }

    let Some(rc) = rule_content else {
        return true;
    };
    // 取 input 中的"匹配内容"：Bash 用 command；Read/Edit/Write 用 file_path
    let content = input
        .tool_input
        .as_ref()
        .and_then(|v| {
            v.get("command")
                .or_else(|| v.get("file_path"))
                .or_else(|| v.get("path"))
                .or_else(|| v.get("url"))
        })
        .and_then(|v| v.as_str());
    let Some(content) = content else {
        return false;
    };
    match_content(&rc, content)
}

fn match_tool_name(rule: &str, actual: &str) -> bool {
    if rule == actual {
        return true;
    }
    if let Some(prefix) = rule.strip_prefix("mcp__") {
        if let Some(after) = actual.strip_prefix("mcp__") {
            return after == prefix || after.starts_with(&format!("{prefix}__"));
        }
    }
    false
}

fn match_content(pattern: &str, content: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix(":*") {
        return content == prefix || content.starts_with(&format!("{prefix} "));
    }
    if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
        if let Ok(g) = globset::Glob::new(pattern) {
            return g.compile_matcher().is_match(content);
        }
        return false;
    }
    pattern == content
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::HookInput;
    use serde_json::json;

    fn make_input(tool_name: &str, command: Option<&str>) -> HookInput {
        HookInput {
            hook_event_name: "PreToolUse".into(),
            session_id: "test".into(),
            cwd: "/tmp".into(),
            permission_mode: "default".into(),
            tool_name: Some(tool_name.into()),
            tool_input: command.map(|c| json!({"command": c})),
            tool_use_id: Some("toolu_01".into()),
            tool_result: None,
            is_error: None,
            user_prompt: None,
        }
    }

    // -- match_tool_name --

    #[test]
    fn match_tool_name_exact() {
        assert!(match_tool_name("Bash", "Bash"));
        assert!(!match_tool_name("Bash", "Read"));
    }

    #[test]
    fn match_tool_name_mcp_prefix() {
        assert!(match_tool_name("mcp__github", "mcp__github"));
        assert!(match_tool_name("mcp__github", "mcp__github__list_issues"));
        assert!(!match_tool_name("mcp__github", "Bash"));
        assert!(!match_tool_name("mcp__github", "mcp__slack"));
    }

    // -- match_content --

    #[test]
    fn match_content_prefix_wildcard() {
        assert!(match_content("git push:*", "git push origin main"));
        assert!(match_content("git push:*", "git push"));
        assert!(!match_content("git push:*", "git pull"));
    }

    #[test]
    fn match_content_glob() {
        assert!(match_content("*.rs", "main.rs"));
        assert!(match_content("src/**/*.rs", "src/main.rs"));
        assert!(!match_content("*.rs", "main.py"));
    }

    #[test]
    fn match_content_exact() {
        assert!(match_content("ls", "ls"));
        assert!(!match_content("ls", "ls -la"));
    }

    // -- if_matches --

    #[test]
    fn if_matches_tool_name_only() {
        let input = make_input("Bash", Some("ls"));
        assert!(if_matches("Bash", &input));
        assert!(!if_matches("Read", &input));
    }

    #[test]
    fn if_matches_tool_name_with_content() {
        let input = make_input("Bash", Some("git push origin main"));
        assert!(if_matches("Bash(git push:*)", &input));
        assert!(!if_matches("Bash(ls)", &input));
    }

    #[test]
    fn if_matches_no_tool_name() {
        let input = HookInput {
            tool_name: None,
            ..make_input("Bash", Some("ls"))
        };
        assert!(!if_matches("Bash", &input));
    }

    #[test]
    fn if_matches_no_tool_input() {
        let input = make_input("Bash", None);
        assert!(if_matches("Bash", &input)); // tool name matches, no content filter needed
        assert!(!if_matches("Bash(ls)", &input)); // content filter fails without input
    }

    #[test]
    fn if_matches_mcp_tool() {
        let input = make_input("mcp__github__list_issues", None);
        assert!(if_matches("mcp__github", &input));
    }
}
