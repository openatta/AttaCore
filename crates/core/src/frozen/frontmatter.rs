//! YAML frontmatter parsing for SKILL.md files and memory entries.

use std::path::Path;

use super::skill::{SkillEntry, SkillSource};

/// 把 `---\n<yaml>\n---\n<body>` 切成 (Some(yaml), body)；不带 frontmatter 时
/// 返回 (None, content)。
pub fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    let bytes = content.as_bytes();
    if !content.starts_with("---") {
        return (None, content);
    }
    // 第一个 `---` 后紧跟换行
    let after_first = &content[3..];
    let after_first = after_first.strip_prefix('\n').unwrap_or(after_first);
    // 在剩余内容里找下一个 `\n---` 行
    let Some(rel) = find_closing_marker(after_first) else {
        return (None, content);
    };
    let yaml = &after_first[..rel];
    let after_close = &after_first[rel..];
    // 跳过 `---` 后的换行
    let after_close = after_close.strip_prefix("---").unwrap_or(after_close);
    let after_close = after_close.strip_prefix('\n').unwrap_or(after_close);
    let _ = bytes;
    (Some(yaml), after_close)
}

/// 找 `\n---` 之后是 \n 或 EOF 的位置；返回相对 yaml 起点的 byte offset。
fn find_closing_marker(s: &str) -> Option<usize> {
    let mut search_from = 0usize;
    while let Some(rel) = s[search_from..].find("\n---") {
        let abs = search_from + rel;
        let after = abs + 4; // `\n---` 4 字节
        let next = s.as_bytes().get(after);
        if next.is_none() || matches!(next, Some(b'\n') | Some(b' ') | Some(b'\r')) {
            // `\n---\n` 命中；返回 abs+1（跳过开头那个 `\n`）
            return Some(abs + 1);
        }
        search_from = abs + 1;
    }
    None
}

fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2 {
        let first = s.as_bytes()[0];
        let last = s.as_bytes()[s.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Parse a YAML inline list value: `[a, b, c]` or bare `a, b, c`.
pub(crate) fn parse_yaml_list(raw: &str) -> Vec<String> {
    let s = raw.trim();
    if s.starts_with('[') && s.ends_with(']') {
        let inner = &s[1..s.len() - 1];
        inner
            .split(',')
            .map(|item| strip_quotes(item.trim()).to_string())
            .filter(|item| !item.is_empty())
            .collect()
    } else if s.contains(',') {
        s.split(',')
            .map(|item| strip_quotes(item.trim()).to_string())
            .filter(|item| !item.is_empty())
            .collect()
    } else {
        vec![s.to_string()]
    }
}

/// Parse SKILL.md: extract YAML frontmatter (`---` delimited `key: value` lines),
/// extracting name / description / when_to_use and extended fields.
///
/// Handles common YAML scalars: bare strings, quoted strings, and inline lists
/// (`[a, b, c]`). Multi-line values (`|`, `>`) and nested maps are NOT supported.
///
/// Frontmatter missing: description from first markdown line; name from dir name.
/// description still empty -> skill is considered invalid, return None.
pub fn parse_skill_file(
    content: &str,
    dir_name: String,
    path: &Path,
    source: SkillSource,
) -> Option<SkillEntry> {
    let (front, body) = split_frontmatter(content);
    let mut entry = SkillEntry {
        name: dir_name.clone(),
        source,
        path: path.to_path_buf(),
        ..Default::default()
    };
    if let Some(yaml) = front {
        for line in yaml.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            let key = k.trim();
            let raw_value = v.trim();
            if raw_value.is_empty() {
                continue;
            }
            let value = strip_quotes(raw_value);
            if value.is_empty() {
                continue;
            }
            match key {
                "name" => entry.name = value.to_string(),
                "description" => entry.description = value.to_string(),
                "when_to_use" | "whenToUse" => entry.when_to_use = Some(value.to_string()),
                "argument_hint" | "argumentHint" => entry.argument_hint = Some(value.to_string()),
                "allowed_tools" | "allowedTools" => {
                    entry.allowed_tools = Some(parse_yaml_list(value));
                }
                "allowed-tools" => {
                    entry.allowed_tools = Some(parse_yaml_list(value));
                }
                "model" => entry.model = Some(value.to_string()),
                "context" => entry.context = Some(value.to_string()),
                "disable_model_invocation" | "disableModelInvocation" => {
                    entry.disable_model_invocation = matches!(
                        value.to_lowercase().as_str(),
                        "true" | "yes" | "1"
                    );
                }
                "user_invocable" | "userInvocable" => {
                    entry.user_invocable = !matches!(
                        value.to_lowercase().as_str(),
                        "false" | "no" | "0"
                    );
                }
                "paths" => {
                    entry.paths = Some(parse_yaml_list(value));
                }
                "version" => entry.version = Some(value.to_string()),
                "files" => {
                    entry.files = Some(parse_yaml_list(value));
                }
                "hooks" => {
                    entry.hooks = Some(parse_yaml_list(value));
                }
                _ => {}
            }
        }
    }
    if entry.description.is_empty() {
        // fallback: first non-empty body line (strip # heading prefix)
        for line in body.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let stripped = trimmed.trim_start_matches('#').trim();
            if !stripped.is_empty() {
                entry.description = stripped.to_string();
            }
            break;
        }
    }
    if entry.description.is_empty() {
        return None;
    }
    Some(entry)
}

/// Extract a simple YAML key:value from a one-level frontmatter block.
/// Handles `name: my-memory` and `description: "one line"`.
pub(crate) fn extract_yaml_field(yaml: &str, key: &str) -> Option<String> {
    for line in yaml.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(key) {
            let rest = rest.trim_start().strip_prefix(':')?.trim_start();
            if rest.is_empty() {
                return None;
            }
            return Some(strip_quotes(rest).to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_split_basic() {
        let s = "---\nname: foo\ndescription: bar\n---\nbody here\n";
        let (front, body) = split_frontmatter(s);
        assert_eq!(front, Some("name: foo\ndescription: bar\n"));
        assert_eq!(body, "body here\n");
    }

    #[test]
    fn frontmatter_split_no_frontmatter_returns_none() {
        let s = "no frontmatter here\nbody";
        let (front, body) = split_frontmatter(s);
        assert_eq!(front, None);
        assert_eq!(body, s);
    }

    #[test]
    fn frontmatter_split_unclosed_returns_none() {
        let s = "---\nname: foo\nno closing marker";
        let (front, body) = split_frontmatter(s);
        assert_eq!(front, None);
        assert_eq!(body, s);
    }

    #[test]
    fn parse_skill_extracts_all_three_fields() {
        let content = "---\nname: summarize-pr\ndescription: Summarize a PR\nwhen_to_use: When user asks for PR summary\n---\nbody\n";
        let entry = parse_skill_file(
            content,
            "summarize-pr".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::User,
        )
        .unwrap();
        assert_eq!(entry.name, "summarize-pr");
        assert_eq!(entry.description, "Summarize a PR");
        assert_eq!(
            entry.when_to_use.as_deref(),
            Some("When user asks for PR summary")
        );
        assert_eq!(entry.source, SkillSource::User);
    }

    #[test]
    fn parse_skill_strips_quotes() {
        let content = "---\nname: foo\ndescription: \"with quotes\"\n---\nbody";
        let entry = parse_skill_file(
            content,
            "foo".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::User,
        )
        .unwrap();
        assert_eq!(entry.description, "with quotes");
    }

    #[test]
    fn parse_skill_falls_back_to_first_body_line() {
        let content = "---\nname: foo\n---\n# Heading\n\nactual body";
        let entry = parse_skill_file(
            content,
            "foo".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::Project,
        )
        .unwrap();
        // 取 `# Heading` -> 剥 # 后 `Heading`
        assert_eq!(entry.description, "Heading");
    }

    #[test]
    fn parse_skill_returns_none_when_no_description_anywhere() {
        let content = "---\nname: foo\n---\n\n\n";
        let entry = parse_skill_file(
            content,
            "foo".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::User,
        );
        assert!(entry.is_none());
    }

    #[test]
    fn parse_skill_uses_dir_name_when_no_name_in_frontmatter() {
        let content = "---\ndescription: A skill\n---\nbody";
        let entry = parse_skill_file(
            content,
            "my-skill".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::User,
        )
        .unwrap();
        assert_eq!(entry.name, "my-skill");
    }
}
