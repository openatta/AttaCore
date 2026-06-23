//! Skill manager — runtime loading, listing, and reloading of skills.
//!
//! v2: delegates YAML frontmatter parsing to `base::frozen::frontmatter::parse_skill_file`
//! for TS parity (all 15+ fields). Uses `base::frozen::skill::SkillEntry` as the canonical
//! parsed type; `SkillInfo` is the runtime wrapper with cached description for prompt assembly.

use base::frozen::frontmatter::parse_skill_file;
use base::frozen::skill::{SkillEntry, SkillSource as FrozenSkillSource};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// Metadata for a loaded skill at runtime.
#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub source: SkillSource,
    pub path: PathBuf,
    // -- Extended fields (TS parity) --
    /// Restrict tools this skill can invoke (whitelist of tool names)
    pub allowed_tools: Option<Vec<String>>,
    /// Override model for this skill
    pub model: Option<String>,
    /// Execution context: "fork" (sub-agent in worktree) or "inline" (default)
    pub context: Option<String>,
    /// Hint shown to user for arguments (e.g. "commit message")
    pub argument_hint: Option<String>,
    /// Glob patterns for conditional activation
    pub paths: Option<Vec<String>>,
    /// If true, model cannot invoke this skill directly (only user via slash)
    pub disable_model_invocation: bool,
    /// If false, skill is hidden from user-facing slash command list
    pub user_invocable: bool,
    /// Version string
    pub version: Option<String>,
}

impl From<SkillEntry> for SkillInfo {
    fn from(e: SkillEntry) -> Self {
        SkillInfo {
            name: e.name,
            description: e.description,
            source: SkillSource::from(e.source),
            path: e.path,
            allowed_tools: e.allowed_tools,
            model: e.model,
            context: e.context,
            argument_hint: e.argument_hint,
            paths: e.paths,
            disable_model_invocation: e.disable_model_invocation,
            user_invocable: e.user_invocable,
            version: e.version,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    User,
    Project,
    Plugin,
}

impl SkillSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            SkillSource::User => "user",
            SkillSource::Project => "project",
            SkillSource::Plugin => "plugin",
        }
    }
}

impl From<FrozenSkillSource> for SkillSource {
    fn from(s: FrozenSkillSource) -> Self {
        match s {
            FrozenSkillSource::User => SkillSource::User,
            FrozenSkillSource::Project => SkillSource::Project,
            FrozenSkillSource::Plugin => SkillSource::Plugin,
        }
    }
}

/// Manages loaded skills at runtime. Skills are .md files with YAML frontmatter.
pub struct SkillManager {
    skills: RwLock<HashMap<String, SkillInfo>>,
    watcher: RwLock<Option<crate::watcher::SkillWatcher>>,
}

impl SkillManager {
    pub fn new() -> Self {
        Self {
            skills: RwLock::new(HashMap::new()),
            watcher: RwLock::new(None),
        }
    }

    /// Load skills from a directory (user skills: ~/.atta/code/skills/).
    /// Each .md file is a skill; filename (without .md) is the skill name.
    pub fn load_dir(&self, dir: &Path, source: SkillSource) -> std::io::Result<usize> {
        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("md") {
                    if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                        let info = Self::load_skill_at_path(&path, name, source);
                        if let Some(info) = info {
                            self.skills.write().unwrap().insert(info.name.clone(), info);
                            count += 1;
                        }
                    }
                }
            }
        }
        Ok(count)
    }

    /// Load skills from a directory using SKILL.md subdirectory format.
    /// TS parity: claude-code's `loadSkillsDir.ts` supports `skill-name/SKILL.md`.
    /// Each subdirectory containing a SKILL.md becomes a skill.
    pub fn load_dir_subdirs(&self, dir: &Path, source: SkillSource) -> std::io::Result<usize> {
        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let subdir = entry.path();
                if subdir.is_dir() {
                    let skill_md = subdir.join("SKILL.md");
                    if skill_md.is_file() {
                        let dir_name = subdir
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("unknown")
                            .to_string();
                        let info = Self::load_skill_at_path(&skill_md, &dir_name, source);
                        if let Some(info) = info {
                            self.skills.write().unwrap().insert(info.name.clone(), info);
                            count += 1;
                        }
                    }
                } else if subdir.extension().and_then(|s| s.to_str()) == Some("md") {
                    // Legacy flat format
                    if let Some(name) = subdir.file_stem().and_then(|s| s.to_str()) {
                        let info = Self::load_skill_at_path(&subdir, name, source);
                        if let Some(info) = info {
                            self.skills.write().unwrap().insert(info.name.clone(), info);
                            count += 1;
                        }
                    }
                }
            }
        }
        Ok(count)
    }

    /// Register a bundled (in-memory) skill. Used for built-in skills.
    /// Disk-loaded skills with the same name take priority (skip).
    pub fn register_bundled(&self, entry: SkillEntry) {
        let mut skills = self.skills.write().unwrap();
        let name = entry.name.clone();
        // Disk skills take priority — only register if not already loaded
        skills.entry(name).or_insert_with(|| SkillInfo::from(entry));
    }

    /// Discover skills by walking up directory tree from given file paths.
    /// TS parity: `discoverSkillDirsForPaths()` in loadSkillsDir.ts.
    /// For each path, walks up to find `skills/` directories
    /// containing SKILL.md files relevant to the path.
    pub fn discover_for_paths(&self, paths: &[PathBuf]) -> Vec<SkillInfo> {
        if paths.is_empty() {
            return Vec::new();
        }
        let mut discovered = Vec::new();
        let mut seen_dirs = HashSet::new();
        for path in paths {
            let mut current = if path.is_dir() {
                path.clone()
            } else {
                path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."))
            };
            // Walk up to filesystem root
            loop {
                let candidate = current.join("skills");
                if candidate.is_dir() && seen_dirs.insert(candidate.clone()) {
                        if let Ok(entries) = std::fs::read_dir(&candidate) {
                            for entry in entries.flatten() {
                                let p = entry.path();
                                if p.is_dir() {
                                    let skill_md = p.join("SKILL.md");
                                    if skill_md.is_file() {
                                        let dir_name = p
                                            .file_name()
                                            .and_then(|s| s.to_str())
                                            .unwrap_or("unknown")
                                            .to_string();
                                        let info =
                                            Self::load_skill_at_path(&skill_md, &dir_name, SkillSource::Project);
                                        if let Some(info) = info {
                                            discovered.push(info);
                                        }
                                    }
                                }
                            }
                        }
                }
                match current.parent() {
                    Some(parent) if parent != current => current = parent.to_path_buf(),
                    _ => break,
                }
            }
        }
        discovered
    }

    /// List all loaded skills.
    pub fn list(&self) -> Vec<SkillInfo> {
        self.skills.read().unwrap().values().cloned().collect()
    }

    /// Get a skill by name.
    pub fn get(&self, name: &str) -> Option<SkillInfo> {
        self.skills.read().unwrap().get(name).cloned()
    }

    /// Get the full content of a skill file (for prompt expansion at invocation time).
    pub fn get_skill_content(&self, name: &str) -> Option<String> {
        let info = self.get(name)?;
        std::fs::read_to_string(&info.path).ok()
    }

    /// Generate a prompt for a skill invocation with arguments.
    /// Substitutes `{args}` placeholder in the skill content.
    pub fn expand_skill(&self, name: &str, args: &str) -> Option<String> {
        let content = self.get_skill_content(name)?;
        let expanded = content.replace("{args}", args);
        Some(expanded)
    }

    /// Activate skills whose `paths` patterns match any of the given `file_paths`.
    ///
    /// For each loaded skill with a non-empty `paths` field, checks whether any
    /// of the supplied file paths match the gitignore-style glob patterns.
    /// Returns the matching skills so the caller can inject them into context.
    ///
    /// Pattern rules (same as `.gitignore`):
    /// - `*`  matches any sequence of characters except `/`
    /// - `**` matches any sequence including `/`
    /// - `?`  matches any single character except `/`
    /// - Leading `/` anchors the pattern to the root of the path
    /// - Trailing `/` is ignored for file-path matching
    pub fn activate_conditional_skills_for_paths(
        &self,
        file_paths: &[PathBuf],
    ) -> Vec<SkillInfo> {
        if file_paths.is_empty() {
            return Vec::new();
        }

        let skills = self.skills.read().unwrap();
        let mut activated = Vec::new();

        'skill: for info in skills.values() {
            let Some(patterns) = &info.paths else { continue };
            if patterns.is_empty() {
                continue;
            }

            // Build a GlobSet once per skill using GlobSetBuilder.
            let mut gb = globset::GlobSetBuilder::new();
            for p in patterns {
                // Strip leading '/' for anchored gitignore patterns; globset
                // handles the rest with literal_separator = true.
                let trimmed = p.strip_prefix('/').unwrap_or(p);
                if let Ok(glob) = globset::GlobBuilder::new(trimmed)
                    .literal_separator(true)
                    .build()
                {
                    gb.add(glob);
                }
            }
            let Ok(glob_set) = gb.build() else { continue };

            for fp in file_paths {
                let p_str = fp.to_string_lossy();
                if glob_set.is_match(p_str.as_ref()) {
                    activated.push(info.clone());
                    continue 'skill;
                }
            }
        }

        activated
    }

    /// Build a prompt block listing all loaded skills with their descriptions.
    pub fn build_skills_prompt(&self) -> Option<String> {
        let skills = self.list();
        if skills.is_empty() {
            return None;
        }
        let mut prompt = String::from("## Available Skills\n\n");
        for s in &skills {
            prompt.push_str(&format!("- **{}**: {}\n", s.name, s.description));
        }
        Some(prompt)
    }

    /// Parse a SKILL.md file at the given path, using the core frontmatter parser.
    fn load_skill_at_path(path: &Path, dir_name: &str, source: SkillSource) -> Option<SkillInfo> {
        let content = std::fs::read_to_string(path).ok()?;
        let frozen_source = match source {
            SkillSource::User => FrozenSkillSource::User,
            SkillSource::Project => FrozenSkillSource::Project,
            SkillSource::Plugin => FrozenSkillSource::Plugin,
        };
        let entry = parse_skill_file(&content, dir_name.to_string(), path, frozen_source)?;
        Some(SkillInfo::from(entry))
    }

    // ── File-watching support ──

    /// Enable file watching on the given directories.
    ///
    /// Starts a background [`SkillWatcher`](crate::watcher::SkillWatcher) that
    /// monitors the provided paths for SKILL.md / *.md changes. Use
    /// [`check_for_changes`](Self::check_for_changes) periodically to apply
    /// any pending reloads.
    ///
    /// Can be called multiple times — only the most recent set of watch paths
    /// is active; previous watcher is replaced.
    pub fn enable_watching(&self, paths: &[std::path::PathBuf]) -> Result<(), String> {
        let mut watcher = crate::watcher::SkillWatcher::new();
        watcher.watch_skills(paths)?;
        *self.watcher.write().unwrap() = Some(watcher);
        Ok(())
    }

    /// Poll the file watcher and reload any skills whose files have changed.
    ///
    /// This is a lightweight call (essentially a Mutex lock + iteration) safe
    /// to run at the start of each turn. Returns the number of skills reloaded.
    pub fn check_for_changes(&self) -> usize {
        let guard = self.watcher.read().unwrap();
        match guard.as_ref() {
            Some(w) => w.check_and_reload(self),
            None => 0,
        }
    }

    /// Register MCP-derived skills for a server's tools.
    ///
    /// Each MCP tool becomes a skill named `mcp__{server}__{tool}` that
    /// users can invoke via slash command. Skills are registered with
    /// `user_invocable: true` so they appear in `/skills`.
    ///
    /// Returns the number of skills that were actually registered (not
    /// counting duplicates already present from disk).
    pub fn register_mcp_skills(
        &self,
        server_name: &str,
        tools: &[base::interface::model::ToolDef],
    ) -> usize {
        let entries = crate::mcp_builder::build_skills_from_mcp(server_name, tools);
        let mut count = 0;
        let mut skills = self.skills.write().unwrap();
        for entry in entries {
            let name = entry.name.clone();
            if let std::collections::hash_map::Entry::Vacant(e) = skills.entry(name) {
                e.insert(SkillInfo::from(entry));
                count += 1;
            }
        }
        count
    }

    /// Reload a single skill from its file path.
    ///
    /// Handles both subdirectory format (`skills/<name>/SKILL.md`) and flat
    /// format (`<name>.md`). If the file no longer exists on disk, the skill
    /// is removed from the cache. Returns an error if the path is not a valid
    /// skill file or parsing fails.
    pub fn reload_skill(&self, path: &Path) -> Result<(), String> {
        if !path.exists() {
            let mut skills = self.skills.write().unwrap();
            skills.retain(|_, info| info.path != path);
            return Ok(());
        }

        // Determine skill name from path
        let (name, _file_name) = if path.file_name().and_then(|s| s.to_str()) == Some("SKILL.md")
        {
            // Subdirectory format: skills/<name>/SKILL.md
            let name = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .ok_or_else(|| format!("Cannot determine skill name: {}", path.display()))?
                .to_string();
            (name, "SKILL.md".to_string())
        } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
            // Flat format: <name>.md
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| format!("Cannot determine skill name: {}", path.display()))?
                .to_string();
            let fname = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown.md")
                .to_string();
            (name, fname)
        } else {
            return Err(format!("Not a skill file: {}", path.display()));
        };

        // Determine source from existing entry, default to User
        let source = {
            let skills = self.skills.read().unwrap();
            skills
                .get(&name)
                .map(|s| s.source)
                .unwrap_or(SkillSource::User)
        };
        let frozen_source = match source {
            SkillSource::User => FrozenSkillSource::User,
            SkillSource::Project => FrozenSkillSource::Project,
            SkillSource::Plugin => FrozenSkillSource::Plugin,
        };

        let content =
            std::fs::read_to_string(path).map_err(|e| format!("Failed to read skill: {e}"))?;
        let entry = parse_skill_file(&content, name.clone(), path, frozen_source).ok_or_else(
            || format!("Failed to parse skill file: {}", path.display()),
        )?;

        let mut skills = self.skills.write().unwrap();
        skills.insert(name, SkillInfo::from(entry));
        Ok(())
    }
}

impl Default for SkillManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parse_skill_delegates_to_core_parser() {
        let dir = tempfile::tempdir().unwrap();
        let skill_md = dir.path().join("my-skill.md");
        let mut f = std::fs::File::create(&skill_md).unwrap();
        writeln!(f, "---").unwrap();
        writeln!(f, "description: A test skill for validation").unwrap();
        writeln!(f, "allowed-tools: [Bash, Read]").unwrap();
        writeln!(f, "model: sonnet").unwrap();
        writeln!(f, "argument_hint: file path").unwrap();
        writeln!(f, "---").unwrap();
        writeln!(f, "# Test Skill Body").unwrap();
        drop(f);

        let info = SkillManager::load_skill_at_path(&skill_md, "my-skill", SkillSource::User);
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.name, "my-skill");
        assert_eq!(info.description, "A test skill for validation");
        assert_eq!(info.allowed_tools, Some(vec!["Bash".into(), "Read".into()]));
        assert_eq!(info.model, Some("sonnet".into()));
        assert_eq!(info.argument_hint, Some("file path".into()));
    }

    #[test]
    fn parse_skill_without_description_returns_none_for_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let skill_md = dir.path().join("empty.md");
        std::fs::write(&skill_md, "").unwrap();
        let info = SkillManager::load_skill_at_path(&skill_md, "empty", SkillSource::User);
        assert!(info.is_none());
    }

    #[test]
    fn parse_skill_body_line_is_fallback_description() {
        let dir = tempfile::tempdir().unwrap();
        let skill_md = dir.path().join("body.md");
        std::fs::write(&skill_md, "# A body-only skill").unwrap();
        let info = SkillManager::load_skill_at_path(&skill_md, "body", SkillSource::User);
        assert!(info.is_some());
        assert_eq!(info.unwrap().description, "A body-only skill");
    }

    #[test]
    fn discover_for_paths_finds_skills() {
        let tmp = tempfile::tempdir().unwrap();
        // Create directory structure: project/skills/my-skill/SKILL.md
        let skills_dir = tmp.path().join("skills").join("my-skill");
        std::fs::create_dir_all(&skills_dir).unwrap();
        let mut f = std::fs::File::create(skills_dir.join("SKILL.md")).unwrap();
        writeln!(f, "---").unwrap();
        writeln!(f, "description: Discovered skill").unwrap();
        writeln!(f, "---").unwrap();
        writeln!(f, "Body").unwrap();
        drop(f);

        let mgr = SkillManager::new();
        let discovered = mgr.discover_for_paths(&[tmp.path().to_path_buf()]);
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].name, "my-skill");
    }

    #[test]
    fn register_bundled_respects_disk_priority() {
        let mgr = SkillManager::new();
        // Register a bundled skill
        let bundled = SkillEntry {
            name: "test-skill".into(),
            description: "bundled version".into(),
            source: FrozenSkillSource::User,
            path: PathBuf::from("(bundled:test-skill)"),
            ..Default::default()
        };
        mgr.register_bundled(bundled);
        assert_eq!(mgr.get("test-skill").unwrap().description, "bundled version");

        // Load a disk skill with the same name — should override
        let dir = tempfile::tempdir().unwrap();
        let skill_md = dir.path().join("test-skill.md");
        let mut f = std::fs::File::create(&skill_md).unwrap();
        writeln!(f, "---").unwrap();
        writeln!(f, "description: disk version").unwrap();
        writeln!(f, "---").unwrap();
        writeln!(f, "Body").unwrap();
        drop(f);

        mgr.load_dir(dir.path(), SkillSource::User).unwrap();
        assert_eq!(mgr.get("test-skill").unwrap().description, "disk version");
    }

    #[test]
    fn activate_conditional_skills_matches_paths() {
        let mgr = SkillManager::new();

        // Register a skill with paths patterns
        let skill_with_paths = SkillEntry {
            name: "rust-helper".into(),
            description: "Rust file helper".into(),
            source: FrozenSkillSource::User,
            path: PathBuf::from("(bundled:rust-helper)"),
            paths: Some(vec!["**/*.rs".into(), "**/Cargo.toml".into()]),
            ..Default::default()
        };
        mgr.register_bundled(skill_with_paths);

        // Register a skill without paths — should never activate
        let skill_no_paths = SkillEntry {
            name: "general-helper".into(),
            description: "No path restriction".into(),
            source: FrozenSkillSource::User,
            path: PathBuf::from("(bundled:general-helper)"),
            paths: None,
            ..Default::default()
        };
        mgr.register_bundled(skill_no_paths);

        // Register a skill with empty paths — should never activate
        let skill_empty_paths = SkillEntry {
            name: "empty-paths".into(),
            description: "Empty paths".into(),
            source: FrozenSkillSource::User,
            path: PathBuf::from("(bundled:empty-paths)"),
            paths: Some(Vec::new()),
            ..Default::default()
        };
        mgr.register_bundled(skill_empty_paths);

        // Matching .rs files
        let result = mgr.activate_conditional_skills_for_paths(&[
            PathBuf::from("/tmp/project/src/main.rs"),
        ]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "rust-helper");

        // Matching Cargo.toml files
        let result = mgr.activate_conditional_skills_for_paths(&[
            PathBuf::from("/tmp/project/Cargo.toml"),
        ]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "rust-helper");

        // Non-matching paths
        let result = mgr.activate_conditional_skills_for_paths(&[
            PathBuf::from("/tmp/project/README.md"),
        ]);
        assert_eq!(result.len(), 0);

        // Empty file_paths returns empty
        let result = mgr.activate_conditional_skills_for_paths(&[]);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn activate_conditional_skills_matches_anchored_pattern() {
        let mgr = SkillManager::new();

        let skill = SkillEntry {
            name: "config-helper".into(),
            description: "Root config helper".into(),
            source: FrozenSkillSource::User,
            path: PathBuf::from("(bundled:config-helper)"),
            paths: Some(vec!["/.claude/*".into()]),
            ..Default::default()
        };
        mgr.register_bundled(skill);

        // With anchored pattern /.claude/* (stripped to .claude/*), globset
        // with literal_separator expects .claude as an immediate child of the
        // path root. Paths like /.claude/settings.json match because .claude
        // sits at the root of the path string.
        let result = mgr.activate_conditional_skills_for_paths(&[
            PathBuf::from(".claude/settings.json"),
        ]);
        assert_eq!(result.len(), 1);

        // No match: file not under .claude/
        let result = mgr.activate_conditional_skills_for_paths(&[
            PathBuf::from("/project/src/main.rs"),
        ]);
        assert_eq!(result.len(), 0);

        // Deep path with .claude is not matched by /.claude/* anchoring
        let result = mgr.activate_conditional_skills_for_paths(&[
            PathBuf::from("/project/.claude/settings.json"),
        ]);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn activate_conditional_skills_multiple_matches() {
        let mgr = SkillManager::new();

        let skill_rs = SkillEntry {
            name: "rust-helper".into(),
            description: "Rust helper".into(),
            source: FrozenSkillSource::User,
            path: PathBuf::from("(bundled:rust-helper)"),
            paths: Some(vec!["**/*.rs".into()]),
            ..Default::default()
        };
        let skill_md = SkillEntry {
            name: "md-helper".into(),
            description: "Markdown helper".into(),
            source: FrozenSkillSource::User,
            path: PathBuf::from("(bundled:md-helper)"),
            paths: Some(vec!["**/*.md".into()]),
            ..Default::default()
        };
        mgr.register_bundled(skill_rs);
        mgr.register_bundled(skill_md);

        let result = mgr.activate_conditional_skills_for_paths(&[
            PathBuf::from("/proj/src/lib.rs"),
            PathBuf::from("/proj/README.md"),
        ]);
        assert_eq!(result.len(), 2);
        let names: Vec<&str> = result.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"rust-helper"));
        assert!(names.contains(&"md-helper"));
    }
}
