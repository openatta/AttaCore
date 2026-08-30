//! `.atta/rules/` discovery — a lightweight, lazy index surfaced in the
//! system prompt so the model knows rule documents exist without eagerly
//! loading their content.
//!
//! Rules stay intentionally lazy (Read-tool-on-demand, not auto-injected in
//! full). This module only
//! closes the "orphaned rule file" gap: a `.md` file sitting in `.atta/rules/`
//! that nobody referenced from `AGENTS.md` used to be invisible to the model.
//! Discovery still only reads each file's first line, never full content.

use crate::settings::PathSettings;
use std::path::{Path, PathBuf};

/// One discovered rule document.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleEntry {
    /// Filename stem (e.g. "security" for `security.md`).
    pub name: String,
    /// First non-empty line of the file, `#`/whitespace-stripped — same
    /// "fallback to first body line" convention as skills. Capped at
    /// `MAX_DESCRIPTION_CHARS` regardless of how verbose the line is.
    pub description: String,
    /// Absolute path — always resolvable by the Read tool regardless of
    /// which tier (project/scene/global) the file came from.
    pub path: PathBuf,
}

const MAX_DESCRIPTION_CHARS: usize = 150;

/// Scan `.atta/rules/` across all three tiers and return one entry per
/// unique filename stem — project overrides scene overrides global, same
/// precedence as skills/agents (`crates/core/src/frozen/skill.rs`,
/// `crates/runtime/src/agent_tool.rs::merge_agent_types`). Sorted by name
/// for stable output.
pub fn discover_rules(paths: &PathSettings) -> Vec<RuleEntry> {
    let project_dir = paths.project_root().join(".atta").join("rules");
    let scene_dir = paths.user_data_dir.join("rules");
    let global_dir = paths.global_data_dir.join("rules");

    let mut by_name: std::collections::HashMap<String, RuleEntry> =
        std::collections::HashMap::new();
    for dir in [&global_dir, &scene_dir, &project_dir] {
        for entry in scan_rules_dir(dir) {
            by_name.insert(entry.name.clone(), entry);
        }
    }
    let mut entries: Vec<RuleEntry> = by_name.into_values().collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

fn scan_rules_dir(dir: &Path) -> Vec<RuleEntry> {
    let mut out = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return out; // directory doesn't exist yet
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let description = std::fs::read_to_string(&path)
            .ok()
            .and_then(|content| first_line_description(&content))
            .unwrap_or_else(|| "(no description)".to_string());
        out.push(RuleEntry {
            name: name.to_string(),
            description,
            path,
        });
    }
    out
}

fn first_line_description(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let stripped = trimmed.trim_start_matches('#').trim();
        if stripped.is_empty() {
            continue;
        }
        let truncated: String = stripped.chars().take(MAX_DESCRIPTION_CHARS).collect();
        return Some(if stripped.chars().count() > MAX_DESCRIPTION_CHARS {
            format!("{truncated}...")
        } else {
            truncated
        });
    }
    None
}

/// Render the "Available Rules" system prompt block. Returns `None` when no
/// rule documents exist anywhere — keeps token cost at zero for sessions
/// that don't use this feature at all, matching the "宽进严出" philosophy
/// used elsewhere in the config system (unused config = zero runtime cost).
pub fn build_rules_prompt(paths: &PathSettings) -> Option<String> {
    let entries = discover_rules(paths);
    if entries.is_empty() {
        return None;
    }
    let mut lines = vec![
        "## Available Rules".to_string(),
        String::new(),
        "The following rule documents exist under `.atta/rules/`. They are NOT \
         auto-loaded — use the Read tool to load one when it's relevant to the \
         current task."
            .to_string(),
        String::new(),
    ];
    for e in &entries {
        lines.push(format!("- `{}`: {}", e.path.display(), e.description));
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::PathSettings;
    use tempfile::TempDir;

    fn write_rule(dir: &Path, filename: &str, content: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(filename), content).unwrap();
    }

    fn paths_for(project: &Path, scene: &Path, global: &Path) -> PathSettings {
        PathSettings {
            user_data_dir: scene.to_path_buf(),
            global_data_dir: global.to_path_buf(),
            local_data_dir: project.join(".atta"),
            scope: "code".into(),
        }
    }

    #[test]
    fn discover_rules_empty_when_no_dirs_exist() {
        let root = TempDir::new().unwrap();
        let paths = paths_for(
            &root.path().join("project"),
            &root.path().join("scene"),
            &root.path().join("global"),
        );
        assert!(discover_rules(&paths).is_empty());
        assert!(build_rules_prompt(&paths).is_none());
    }

    #[test]
    fn discover_rules_reads_first_line_as_description() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("project");
        write_rule(
            &project.join(".atta").join("rules"),
            "security.md",
            "# Security guidelines\n\nBody text here.",
        );
        let paths = paths_for(
            &project,
            &root.path().join("scene"),
            &root.path().join("global"),
        );
        let entries = discover_rules(&paths);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "security");
        assert_eq!(entries[0].description, "Security guidelines");
    }

    #[test]
    fn discover_rules_project_overrides_scene_overrides_global() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("project");
        let scene = root.path().join("scene");
        let global = root.path().join("global");
        write_rule(&global.join("rules"), "testing.md", "from global");
        write_rule(&scene.join("rules"), "testing.md", "from scene");
        write_rule(
            &project.join(".atta").join("rules"),
            "testing.md",
            "from project",
        );

        let paths = paths_for(&project, &scene, &global);
        let entries = discover_rules(&paths);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].description, "from project");
    }

    #[test]
    fn discover_rules_keeps_distinct_names_from_different_tiers() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("project");
        let global = root.path().join("global");
        write_rule(&global.join("rules"), "architecture.md", "global rule");
        write_rule(
            &project.join(".atta").join("rules"),
            "security.md",
            "project rule",
        );

        let paths = paths_for(&project, &root.path().join("scene"), &global);
        let entries = discover_rules(&paths);
        assert_eq!(entries.len(), 2);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"architecture"));
        assert!(names.contains(&"security"));
    }

    #[test]
    fn build_rules_prompt_lists_filenames_and_descriptions() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("project");
        write_rule(
            &project.join(".atta").join("rules"),
            "testing.md",
            "Testing conventions",
        );
        let paths = paths_for(
            &project,
            &root.path().join("scene"),
            &root.path().join("global"),
        );
        let text = build_rules_prompt(&paths).expect("expected a rules block");
        assert!(text.contains("Available Rules"));
        assert!(text.contains("testing.md"));
        assert!(text.contains("Testing conventions"));
        assert!(text.contains("Read tool"));
    }

    #[test]
    fn first_line_description_truncates_long_lines() {
        let long = "a".repeat(300);
        let desc = first_line_description(&long).unwrap();
        assert!(desc.ends_with("..."));
        assert!(desc.chars().count() <= MAX_DESCRIPTION_CHARS + 3);
    }

    #[test]
    fn first_line_description_skips_blank_lines_and_bare_hashes() {
        let content = "\n\n#  \n# Real Title\nbody";
        assert_eq!(
            first_line_description(content).as_deref(),
            Some("Real Title")
        );
    }
}
