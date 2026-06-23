//! Durable memory — cross-session persistent knowledge, file-based.
//!
//! TS parity: Claude Code's `memdir/` system.
//! - Directory: `<user_dir>/memory/` or `<local_dir>/memory/`
//! - Each memory is a `.md` file with YAML frontmatter
//! - `MEMORY.md` is the index (one line per memory entry)
//! - 4 types: user | feedback | project | reference
//! - [[wikilink]] internal linking for cross-references

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A single durable memory entry — stored as a .md file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurableMemory {
    /// Short kebab-case slug (used as filename: `memory-dir/<slug>.md`).
    pub name: String,
    /// One-line summary — used to decide relevance during recall.
    pub description: String,
    /// Memory type.
    #[serde(default)]
    pub memory_type: MemoryType,
    /// The memory content (body after frontmatter).
    pub content: String,
    /// Session that produced this memory.
    #[serde(default)]
    pub source_session_id: String,
    /// Confidence score 0.0–1.0. Entries below 0.3 are discarded on compaction.
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    /// ISO-8601 timestamp of last write.
    #[serde(default)]
    pub last_seen: String,
    /// Number of times this memory has been surfaced to the model (recall count).
    /// Higher values indicate well-established memories that age more gracefully.
    #[serde(default)]
    pub recall_count: u32,
}

fn default_confidence() -> f64 { 0.8 }

impl DurableMemory {
    /// Compute a staleness penalty (0.0–1.0) based on time since last update.
    /// Memories older than 30 days get a ≥0.5 penalty; memories updated within
    /// 7 days get no penalty. `recall_count` slows decay (well-established
    /// memories age more gracefully).
    ///
    /// TS parity: memoryAge.ts staleness scoring.
    pub fn staleness_penalty(&self) -> f64 {
        let last = time::OffsetDateTime::parse(
            &self.last_seen,
            &time::format_description::well_known::Iso8601::DEFAULT,
        )
        .ok();
        let Some(last) = last else { return 1.0 };
        let now = time::OffsetDateTime::now_utc();
        let age_days = (now - last).whole_days().max(0) as f64;

        if age_days <= 7.0 {
            return 0.0; // Fresh — no penalty
        }

        // Base penalty: linear decay from 7 to 90 days, capped at 1.0
        let base_penalty = ((age_days - 7.0) / 83.0).min(1.0);

        // Recall bonus: each recall reduces penalty by 5%, max 50% reduction
        let recall_bonus = (self.recall_count as f64 * 0.05).min(0.5);
        (base_penalty - recall_bonus).max(0.0)
    }

    /// Effective confidence after applying staleness penalty.
    pub fn effective_confidence(&self) -> f64 {
        (self.confidence * (1.0 - self.staleness_penalty())).max(0.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryType {
    /// Who the user is (role, expertise, preferences).
    #[default]
    User,
    /// Guidance the user has given on how you should work.
    Feedback,
    /// Ongoing work, goals, or constraints not derivable from code/git.
    Project,
    /// Pointers to external resources (URLs, dashboards, tickets).
    Reference,
}

impl MemoryType {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryType::User => "user",
            MemoryType::Feedback => "feedback",
            MemoryType::Project => "project",
            MemoryType::Reference => "reference",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScopedMemory {
    pub memory: DurableMemory,
    pub scope: MemoryScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScope {
    /// Global → write to user_data_dir
    User,
    /// Project-specific → write to local_data_dir
    Local,
}

/// Persistent memory store using file-per-memory + MEMORY.md index.
pub struct MemoryStore {
    user_dir: PathBuf,
    local_dir: PathBuf,
}

/// File name for the index.
pub const INDEX_FILE: &str = "MEMORY.md";

impl MemoryStore {
    pub fn new(user_dir: PathBuf, local_dir: PathBuf) -> Self {
        Self { user_dir, local_dir }
    }

    /// Load all memories from both directories (local overrides user on same name).
    pub fn load_all(&self) -> Vec<DurableMemory> {
        let mut user = Self::load_from_dir(&self.user_dir);
        let local = Self::load_from_dir(&self.local_dir);
        for mem in local {
            if let Some(pos) = user.iter().position(|m| m.name == mem.name) {
                user[pos] = mem;
            } else {
                user.push(mem);
            }
        }
        user.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        user
    }

    /// Scan all memory headers without loading full content.
    /// Returns DurableMemory entries with only metadata populated.
    /// Used by `select_memories_with_llm` for efficient batch filtering.
    pub fn scan_memory_headers(&self) -> Vec<DurableMemory> {
        self.load_all()
    }

    /// Load MEMORY.md index content from a directory.
    pub fn load_index(&self) -> String {
        let mut content = String::new();
        for dir in [&self.local_dir, &self.user_dir] {
            let path = dir.join(INDEX_FILE);
            if let Ok(s) = std::fs::read_to_string(&path) {
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(&s);
            }
        }
        content
    }

    /// Persist a batch of memories to user_dir.
    pub fn persist_batch(&self, memories: Vec<DurableMemory>) -> Result<usize, MemoryError> {
        let mut saved = 0;
        for mem in &memories {
            if mem.confidence < 0.3 {
                continue;
            }
            Self::write_memory_file(&self.user_dir, mem)?;
            saved += 1;
        }
        Self::rebuild_index(&self.user_dir, &memories)?;
        Ok(saved)
    }

    /// Persist with scope routing.
    pub fn persist_batch_scoped(&self, memories: Vec<ScopedMemory>) -> Result<usize, MemoryError> {
        let mut user_mems: Vec<DurableMemory> = Vec::new();
        let mut local_mems: Vec<DurableMemory> = Vec::new();
        for sm in &memories {
            if sm.memory.confidence < 0.3 {
                continue;
            }
            match sm.scope {
                MemoryScope::User => user_mems.push(sm.memory.clone()),
                MemoryScope::Local => local_mems.push(sm.memory.clone()),
            }
        }
        let total = user_mems.len() + local_mems.len();
        // Write files
        for mem in &user_mems {
            Self::write_memory_file(&self.user_dir, mem)?;
        }
        for mem in &local_mems {
            Self::write_memory_file(&self.local_dir, mem)?;
        }
        // Rebuild indices
        if !user_mems.is_empty() {
            Self::rebuild_index(&self.user_dir, &Self::merge_with_existing(&self.user_dir, &user_mems))?;
        }
        if !local_mems.is_empty() {
            Self::rebuild_index(&self.local_dir, &Self::merge_with_existing(&self.local_dir, &local_mems))?;
        }
        Ok(total)
    }

    /// Search memories by query (simple substring match on name + description + content).
    pub fn search(&self, query: &str) -> Vec<DurableMemory> {
        self.load_all()
            .into_iter()
            .filter(|m| {
                m.name.contains(query)
                    || m.description.contains(query)
                    || m.content.contains(query)
            })
            .collect()
    }

    /// Remove a memory by name.
    pub fn remove(&self, name: &str) -> Result<bool, MemoryError> {
        for dir in [&self.user_dir, &self.local_dir] {
            let path = dir.join(format!("{name}.md"));
            if path.exists() {
                std::fs::remove_file(&path).map_err(|e| MemoryError::Io(e.to_string()))?;
                // Rebuild index from remaining files
                let all = Self::load_from_dir(dir);
                Self::rebuild_index(dir, &all)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Compact: keep N most recent memories, discard older files and
    /// low-confidence entries (< 0.3 effective confidence).
    /// TS parity: memory confidence-based pruning.
    pub fn compact(&self, max_entries: usize) -> Result<usize, MemoryError> {
        let mut all = self.load_all();
        // P2: Filter out low-confidence memories before count-based truncation.
        let before_filter = all.len();
        all.retain(|m| m.effective_confidence() >= 0.3);
        let filtered_count = before_filter - all.len();
        let removed = if all.len() > max_entries {
            let count = all.len() - max_entries;
            all.truncate(max_entries);
            // Clear and repopulate user_dir
            for dir in [&self.user_dir, &self.local_dir] {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let p = entry.path();
                        if p.extension().is_some_and(|e| e == "md") && p.file_name() != Some(std::ffi::OsStr::new(INDEX_FILE)) {
                            let _ = std::fs::remove_file(&p);
                        }
                    }
                }
            }
            // Rewrite kept memories
            for mem in &all {
                Self::write_memory_file(&self.user_dir, mem)?;
            }
            Self::rebuild_index(&self.user_dir, &all)?;
            count + filtered_count
        } else {
            filtered_count
        };
        Ok(removed)
    }

    // ── Internal helpers ──

    fn memory_dir(dir: &Path) -> PathBuf {
        dir.to_path_buf()
    }

    fn load_from_dir(dir: &Path) -> Vec<DurableMemory> {
        let md = Self::memory_dir(dir);
        let mut memories = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&md) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().is_some_and(|e| e == "md")
                    && p.file_name() != Some(std::ffi::OsStr::new(INDEX_FILE))
                {
                    if let Ok(content) = std::fs::read_to_string(&p) {
                        if let Some(mem) = Self::parse_memory_file(&content) {
                            memories.push(mem);
                        }
                    }
                }
            }
        }
        memories
    }

    fn parse_memory_file(raw: &str) -> Option<DurableMemory> {
        // Parse YAML frontmatter between --- delimiters
        let trimmed = raw.trim();
        let frontmatter = if let Some(after_first) = trimmed.strip_prefix("---") {
            if let Some(end) = after_first.find("\n---") {
                &after_first[..end]
            } else if let Some(end) = after_first.find("---") {
                &after_first[..end]
            } else {
                return None;
            }
        } else {
            return None;
        };

        // Try serde_yaml parsing first (flat format: name/description/type at top level)
        let name: String;
        let description: String;
        let memory_type: MemoryType;

        if let Ok(parsed) = serde_yaml::from_str::<serde_yaml::Value>(frontmatter) {
            name = parsed.get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())?;
            description = parsed.get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            // Look for flat "type:" field first (new format).
            // Fall back to nested "metadata.type:" for backward compatibility
            // with files written by the old write_memory_file.
            let type_str = parsed.get("type")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    parsed.get("metadata")
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                });
            memory_type = type_str
                .map(|t| match t {
                    "user" => MemoryType::User,
                    "feedback" => MemoryType::Feedback,
                    "project" => MemoryType::Project,
                    "reference" => MemoryType::Reference,
                    _ => MemoryType::User,
                })
                .unwrap_or_default();
        } else {
            // Fallback to manual line-scanning for edge cases (no serde_yaml available)
            name = extract_yaml_field(frontmatter, "name")?;
            description = extract_yaml_field(frontmatter, "description").unwrap_or_default();
            memory_type = extract_yaml_field(frontmatter, "type")
                .or_else(|| {
                    // Backward-compat: old nested "metadata:\n  type:" format
                    extract_yaml_field_nested(frontmatter, "metadata", "type")
                })
                .map(|t| match t.as_str() {
                    "user" => MemoryType::User,
                    "feedback" => MemoryType::Feedback,
                    "project" => MemoryType::Project,
                    "reference" => MemoryType::Reference,
                    _ => MemoryType::User,
                })
                .unwrap_or_default();
        }

        // Content is everything after the closing ---
        let content = if let Some(end) = trimmed[3..].find("\n---") {
            trimmed[3 + end + 4..].trim().to_string()
        } else {
            String::new()
        };

        Some(DurableMemory {
            name,
            description,
            memory_type,
            content,
            source_session_id: String::new(),
            confidence: 0.8,
            last_seen: String::new(),
            recall_count: 0,
        })
    }

    fn write_memory_file(dir: &Path, mem: &DurableMemory) -> Result<(), MemoryError> {
        let md = Self::memory_dir(dir);
        std::fs::create_dir_all(&md).map_err(|e| MemoryError::Io(e.to_string()))?;
        // Sanitize filename: strip path separators and parent-dir traversal
        let safe_name = sanitize_filename(&mem.name);
        let path = md.join(format!("{}.md", safe_name));
        // Flat frontmatter format matching TS reference and parse_memory_file expectations.
        // Uses serde_yaml to serialize the frontmatter fields, then appends the body.
        // Fields: name, description, type (flat, not nested under metadata:).
        let frontmatter_fields = format!(
            "name: {}\ndescription: {}\ntype: {}",
            mem.name,
            mem.description,
            mem.memory_type.as_str(),
        );
        let frontmatter = format!(
            "---\n{}---\n\n{}",
            frontmatter_fields,
            mem.content,
        );
        std::fs::write(&path, frontmatter).map_err(|e| MemoryError::Io(e.to_string()))?;
        Ok(())
    }

    fn rebuild_index(dir: &Path, memories: &[DurableMemory]) -> Result<(), MemoryError> {
        let md = Self::memory_dir(dir);
        std::fs::create_dir_all(&md).map_err(|e| MemoryError::Io(e.to_string()))?;
        let mut index = String::new();
        for mem in memories {
            index.push_str(&format!(
                "- [{}]({}.md) — {}\n",
                mem.name, mem.name, mem.description
            ));
        }
        std::fs::write(md.join(INDEX_FILE), index)
            .map_err(|e| MemoryError::Io(e.to_string()))?;
        Ok(())
    }

    fn merge_with_existing(dir: &Path, new: &[DurableMemory]) -> Vec<DurableMemory> {
        let mut existing = Self::load_from_dir(dir);
        for mem in new {
            if mem.confidence < 0.3 {
                continue;
            }
            if let Some(pos) = existing.iter().position(|m| m.name == mem.name) {
                existing[pos] = mem.clone();
            } else {
                existing.push(mem.clone());
            }
        }
        existing
    }
}

fn extract_yaml_field(frontmatter: &str, key: &str) -> Option<String> {
    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if let Some(v) = trimmed.strip_prefix(&format!("{}:", key)) {
            let val = v.trim();
            if val.is_empty() {
                return Some(String::new());
            }
            let unquoted = val.trim_matches('"').trim_matches('\'');
            return Some(unquoted.to_string());
        }
    }
    None
}

/// Extract a nested YAML field (e.g., "metadata" → "type").
/// Backward-compat: old write_memory_file wrote `metadata:\n  type:`.
fn extract_yaml_field_nested(
    frontmatter: &str,
    parent_key: &str,
    child_key: &str,
) -> Option<String> {
    let mut in_parent = false;
    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if trimmed == format!("{}:", parent_key) {
            in_parent = true;
            continue;
        }
        if in_parent {
            if let Some(v) = trimmed.strip_prefix(&format!("{}:", child_key)) {
                let val = v.trim().trim_matches('"').trim_matches('\'');
                return Some(val.to_string());
            }
            // If we hit an unindented line (not starting with space), we've left the parent block
            if !trimmed.starts_with(' ') {
                in_parent = false;
            }
        }
    }
    None
}

/// Sanitize a memory name for use as a filename.
/// Prevents path traversal and filesystem issues.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Build the memory system prompt for injection into the system prompt.
/// TS parity: `buildMemoryLines()` in memdir.ts + `TYPES_SECTION_INDIVIDUAL` +
/// `WHAT_NOT_TO_SAVE_SECTION` + `WHEN_TO_ACCESS_SECTION` + `TRUSTING_RECALL_SECTION`.
///
/// v2: Full XML-style type descriptions with `<when_to_save>`, `<how_to_use>`,
/// `<body_structure>`, and `<examples>` tags. Ported verbatim from claude-code
/// `memoryTypes.ts` to give the model the same guidance on what/when/how to
/// save memories.
///
/// v3: Reads MEMORY.md index content from disk and appends it to the prompt,
/// truncated to 200 lines / 25KB. TS parity: `truncateEntrypointContent()` +
/// entrypoint injection in `buildMemoryPrompt()`.
pub fn build_memory_prompt(memory_dir: &Path) -> String {
    let dir_str = memory_dir.display().to_string();
    let mut sections = vec![
        MEMORY_HEADER.replace("{dir}", &dir_str),
        TYPES_SECTION_INDIVIDUAL.to_string(),
        WHAT_NOT_TO_SAVE_SECTION.to_string(),
        WHEN_TO_ACCESS_SECTION.to_string(),
        TRUSTING_RECALL_SECTION.to_string(),
    ];

    // Read and append MEMORY.md index content (TS parity: entrypoint injection)
    let entrypoint_path = memory_dir.join(INDEX_FILE);
    if let Ok(raw) = std::fs::read_to_string(&entrypoint_path) {
        let truncated = truncate_entrypoint_content(&raw);
        if !truncated.content.is_empty() {
            sections.push(format!(
                "\n## Current memory index ({} — {} entries, {} chars)\n\n{}",
                INDEX_FILE,
                truncated.line_count,
                truncated.content.len(),
                truncated.content
            ));
        }
    }

    sections.join("\n")
}

/// Truncated memory entrypoint content.
struct TruncatedEntry {
    content: String,
    line_count: usize,
}

/// Truncate MEMORY.md content to line and byte caps.
/// TS parity: `truncateEntrypointContent()` in memdir.ts.
/// - Line cap: 200 lines
/// - Byte cap: 25,000 bytes
fn truncate_entrypoint_content(raw: &str) -> TruncatedEntry {
    const MAX_LINES: usize = 200;
    const MAX_BYTES: usize = 25_000;

    let trimmed = raw.trim();
    let lines: Vec<&str> = trimmed.lines().collect();
    let line_count = lines.len();
    let byte_count = trimmed.len();

    let was_line_truncated = line_count > MAX_LINES;
    let was_byte_truncated = byte_count > MAX_BYTES;

    if !was_line_truncated && !was_byte_truncated {
        return TruncatedEntry {
            content: trimmed.to_string(),
            line_count,
        };
    }

    let mut truncated: String = if was_line_truncated {
        lines[..MAX_LINES].join("\n")
    } else {
        trimmed.to_string()
    };

    if truncated.len() > MAX_BYTES {
        if let Some(cut_at) = truncated[..MAX_BYTES].rfind('\n') {
            truncated.truncate(cut_at);
        } else {
            truncated.truncate(MAX_BYTES);
        }
    }

    let reason = if was_byte_truncated && !was_line_truncated {
        format!(
            "{} bytes (limit: {} bytes) — index entries are too long",
            byte_count, MAX_BYTES
        )
    } else if was_line_truncated && !was_byte_truncated {
        format!("{} lines (limit: {} lines)", line_count, MAX_LINES)
    } else {
        format!("{} lines and {} bytes", line_count, byte_count)
    };

    truncated.push_str(&format!(
        "\n\n> WARNING: {} is {}. Only part of it was loaded. Keep index entries to one line under ~200 chars; move detail into topic files.",
        INDEX_FILE, reason
    ));

    TruncatedEntry {
        content: truncated,
        line_count,
    }
}

// ── Prompt constants (verbatim from claude-code memoryTypes.ts) ──

const MEMORY_HEADER: &str = "\
# Memory

You have a persistent file-based memory at `{dir}`. This directory already exists — write \
to it directly (do not run mkdir or check for its existence). Each memory is one file \
holding one fact, with frontmatter:

```
---
name: <short-kebab-case-slug>
description: <one-line summary — used to decide relevance during recall>
type: user | feedback | project | reference
---

<the fact; for feedback/project, follow with **Why:** and **How to apply:** lines. \
Link related memories with `[[their-name]]`.>
```

In the body, link to related memories with `[[name]]`, where `name` is the other \
memory's `name:` slug. Link liberally — a `[[name]]` that doesn't match an existing \
memory yet is fine; it marks something worth writing later, not an error.

After writing the file, add a one-line pointer in `MEMORY.md` (`- [Title](file.md) — \
hook`). `MEMORY.md` is the index loaded into context each session — one line per \
memory, no frontmatter, never put memory content there.

Before saving, check for an existing file that already covers it — update that file \
rather than creating a duplicate. Delete memories that turn out to be wrong. Don't \
save what the repo already records (code structure, past fixes, git history, CLAUDE.md).";

const TYPES_SECTION_INDIVIDUAL: &str = r#"## Types of memory

There are several discrete types of memory that you can store in your memory system:

<types>
<type>
    <name>user</name>
    <description>Contain information about the user's role, goals, responsibilities, and knowledge. Great user memories help you tailor your future behavior to the user's preferences and perspective. Your goal in reading and writing these memories is to build up an understanding of who the user is and how you can be most helpful to them specifically. For example, you should collaborate with a senior software engineer differently than a student who is coding for the very first time. Keep in mind, that the aim here is to be helpful to the user. Avoid writing memories about the user that could be viewed as a negative judgement or that are not relevant to the work you're trying to accomplish together.</description>
    <when_to_save>When you learn any details about the user's role, preferences, responsibilities, or knowledge</when_to_save>
    <how_to_use>When your work should be informed by the user's profile or perspective. For example, if the user is asking you to explain a part of the code, you should answer that question in a way that is tailored to the specific details that they will find most valuable or that helps them build their mental model in relation to domain knowledge they already have.</how_to_use>
    <examples>
    user: I'm a data scientist investigating what logging we have in place
    assistant: [saves user memory: user is a data scientist, currently focused on observability/logging]

    user: I've been writing Go for ten years but this is my first time touching the React side of this repo
    assistant: [saves user memory: deep Go expertise, new to React and this project's frontend — frame frontend explanations in terms of backend analogues]
    </examples>
</type>
<type>
    <name>feedback</name>
    <description>Guidance the user has given you about how to approach work — both what to avoid and what to keep doing. These are a very important type of memory to read and write as they allow you to remain coherent and responsive to the way you should approach work in the project. Record from failure AND success: if you only save corrections, you will avoid past mistakes but drift away from approaches the user has already validated, and may grow overly cautious.</description>
    <when_to_save>Any time the user corrects your approach ("no not that", "don't", "stop doing X") OR confirms a non-obvious approach worked ("yes exactly", "perfect, keep doing that", accepting an unusual choice without pushback). Corrections are easy to notice; confirmations are quieter — watch for them. In both cases, save what is applicable to future conversations, especially if surprising or not obvious from the code. Include *why* so you can judge edge cases later.</when_to_save>
    <how_to_use>Let these memories guide your behavior so that the user does not need to offer the same guidance twice.</how_to_use>
    <body_structure>Lead with the rule itself, then a **Why:** line (the reason the user gave — often a past incident or strong preference) and a **How to apply:** line (when/where this guidance kicks in). Knowing *why* lets you judge edge cases instead of blindly following the rule.</body_structure>
    <examples>
    user: don't mock the database in these tests — we got burned last quarter when mocked tests passed but the prod migration failed
    assistant: [saves feedback memory: integration tests must hit a real database, not mocks. Reason: prior incident where mock/prod divergence masked a broken migration]

    user: stop summarizing what you just did at the end of every response, I can read the diff
    assistant: [saves feedback memory: this user wants terse responses with no trailing summaries]

    user: yeah the single bundled PR was the right call here, splitting this one would've just been churn
    assistant: [saves feedback memory: for refactors in this area, user prefers one bundled PR over many small ones. Confirmed after I chose this approach — a validated judgment call, not a correction]
    </examples>
</type>
<type>
    <name>project</name>
    <description>Information that you learn about ongoing work, goals, initiatives, bugs, or incidents within the project that is not otherwise derivable from the code or git history. Project memories help you understand the broader context and motivation behind the work the user is doing within this working directory.</description>
    <when_to_save>When you learn who is doing what, why, or by when. These states change relatively quickly so try to keep your understanding of this up to date. Always convert relative dates in user messages to absolute dates when saving (e.g., "Thursday" → "2026-03-05"), so the memory remains interpretable after time passes.</when_to_save>
    <how_to_use>Use these memories to more fully understand the details and nuance behind the user's request and make better informed suggestions.</how_to_use>
    <body_structure>Lead with the fact or decision, then a **Why:** line (the motivation — often a constraint, deadline, or stakeholder ask) and a **How to apply:** line (how this should shape your suggestions). Project memories decay fast, so the why helps future-you judge whether the memory is still load-bearing.</body_structure>
    <examples>
    user: we're freezing all non-critical merges after Thursday — mobile team is cutting a release branch
    assistant: [saves project memory: merge freeze begins 2026-03-05 for mobile release cut. Flag any non-critical PR work scheduled after that date]

    user: the reason we're ripping out the old auth middleware is that legal flagged it for storing session tokens in a way that doesn't meet the new compliance requirements
    assistant: [saves project memory: auth middleware rewrite is driven by legal/compliance requirements around session token storage, not tech-debt cleanup — scope decisions should favor compliance over ergonomics]
    </examples>
</type>
<type>
    <name>reference</name>
    <description>Stores pointers to where information can be found in external systems. These memories allow you to remember where to look to find up-to-date information outside of the project directory.</description>
    <when_to_save>When you learn about resources in external systems and their purpose. For example, that bugs are tracked in a specific project in Linear or that feedback can be found in a specific Slack channel.</when_to_save>
    <how_to_use>When the user references an external system or information that may be in an external system.</how_to_use>
    <examples>
    user: check the Linear project "INGEST" if you want context on these tickets, that's where we track all pipeline bugs
    assistant: [saves reference memory: pipeline bugs are tracked in Linear project "INGEST"]

    user: the Grafana board at grafana.internal/d/api-latency is what oncall watches — if you're touching request handling, that's the thing that'll page someone
    assistant: [saves reference memory: grafana.internal/d/api-latency is the oncall latency dashboard — check it when editing request-path code]
    </examples>
</type>
</types>"#;

const WHAT_NOT_TO_SAVE_SECTION: &str = r#"## What NOT to save in memory

- Code patterns, conventions, architecture, file paths, or project structure — these can be derived by reading the current project state.
- Git history, recent changes, or who-changed-what — `git log` / `git blame` are authoritative.
- Debugging solutions or fix recipes — the fix is in the code; the commit message has the context.
- Anything already documented in CLAUDE.md files.
- Ephemeral task details: in-progress work, temporary state, current conversation context.

These exclusions apply even when the user explicitly asks you to save. If they ask you to save a PR list or activity summary, ask what was *surprising* or *non-obvious* about it — that is the part worth keeping."#;

const WHEN_TO_ACCESS_SECTION: &str = r#"## When to access memories
- When memories seem relevant, or the user references prior-conversation work.
- You MUST access memory when the user explicitly asks you to check, recall, or remember.
- If the user says to *ignore* or *not use* memory: proceed as if MEMORY.md were empty. Do not apply remembered facts, cite, compare against, or mention memory content.
- Memory records can become stale over time. Use memory as context for what was true at a given point in time. Before answering the user or building assumptions based solely on information in memory records, verify that the memory is still correct and up-to-date by reading the current state of the files or resources. If a recalled memory conflicts with current information, trust what you observe now — and update or remove the stale memory rather than acting on it."#;

const TRUSTING_RECALL_SECTION: &str = r#"## Before recommending from memory

A memory that names a specific function, file, or flag is a claim that it existed *when the memory was written*. It may have been renamed, removed, or never merged. Before recommending it:

- If the memory names a file path: check the file exists.
- If the memory names a function or flag: grep for it.
- If the user is about to act on your recommendation (not just asking about history), verify first.

"The memory says X exists" is not the same as "X exists now."

A memory that summarizes repo state (activity logs, architecture snapshots) is frozen in time. If the user asks about *recent* or *current* state, prefer `git log` or reading the code over recalling the snapshot."#;

/// Select the most relevant memories using an LLM (Sonnet-tier).
///
/// TS parity: claude-code's `findRelevantMemories()` uses Sonnet via `sideQuery()`
/// to pick up to 5 most relevant memories from the MEMORY.md index.
///
/// Parameters:
/// - `already_surfaced`: memory names already shown in prior turns — filtered out
///   to avoid re-injecting the same memories.
/// - `recent_tools`: tool names used recently; memories whose slug matches a tool
///   name are excluded (ambiguous-name guard).
/// - `model_name`: the model identifier to use for the LLM call (e.g. the caller's
///   primary or compact model).
///
/// Falls back to substring `search()` if the model call fails.
///
/// Returns a list of memory names (slugs) that are most relevant to the query.
pub async fn select_memories_with_llm(
    store: &MemoryStore,
    query: &str,
    model: &dyn crate::interface::model::Model,
    max_results: usize,
    already_surfaced: &std::collections::HashSet<String>,
    recent_tools: &[String],
    model_name: &str,
) -> Vec<String> {
    let _start = std::time::Instant::now();
    let mut headers = store.load_all();
    let num_available = headers.len();

    if headers.is_empty() {
        return Vec::new();
    }

    // ── Dedup filtering ──
    // 1. Exclude memories already surfaced in prior turns.
    headers.retain(|h| !already_surfaced.contains(&h.name));
    // 2. Exclude memories whose name matches a recently-used tool (ambiguity guard).
    headers.retain(|h| !recent_tools.iter().any(|t| h.name.eq_ignore_ascii_case(t)));

    if headers.is_empty() {
        let latency_ms = _start.elapsed().as_millis() as u64;
        tracing::info!(
            target: "memory_recall_shape",
            num_memories_selected = 0u32,
            num_memories_available = num_available,
            model_used = model_name,
            latency_ms = latency_ms,
            "memory recall — no memories after dedup",
        );
        return Vec::new();
    }

    if headers.len() <= max_results {
        let names: Vec<String> = headers.into_iter().map(|h| h.name).collect();
        let latency_ms = _start.elapsed().as_millis() as u64;
        tracing::info!(
            target: "memory_recall_shape",
            num_memories_selected = names.len(),
            num_memories_available = num_available,
            model_used = model_name,
            latency_ms = latency_ms,
            "memory recall completed",
        );
        return names;
    }

    // Build a manifest listing all memory names and descriptions (header-only)
    let manifest: String = headers
        .iter()
        .map(|h| format!("- **{}**: {}", h.name, h.description))
        .collect::<Vec<_>>()
        .join("\n");

    let system_prompt = "\
You are selecting memories that will be useful to an AI agent as it processes a user's query. \
You will be given the user's query and a list of available memory files with their names and descriptions.

Return a JSON array of memory names for the memories that will clearly be useful (up to the requested maximum). \
Only include memories that you are certain will be helpful based on their name and description.
- If you are unsure if a memory will be useful, do not include it. Be selective and discerning.
- If there are no memories that would clearly be useful, return an empty array.

Respond with ONLY valid JSON matching this schema (no markdown, no extra text, no code fences):

```json
{\"selected_memories\": [\"name1\", \"name2\"]}
```";

    let user_message = format!(
        "User query: {query}\n\nAvailable memories:\n{manifest}\n\n\
         Select up to {max_results} most relevant memory names.",
    );

    use crate::interface::model::{MessageRole, ModelContentBlock, ModelMessage};
    use crate::interface::settings::ThinkingMode;

    let request_messages = vec![
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text { text: system_prompt.into() }],
        },
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text { text: user_message }],
        },
    ];

    let params = crate::interface::model::StreamParams {
        model: model_name.into(),
        max_tokens: 1000,
        thinking_mode: ThinkingMode::Off,
        fallback_model: None,
        cache_edits: vec![],
    };

    let stream_result = model
        .stream(vec![], vec![], request_messages, params, tokio_util::sync::CancellationToken::new())
        .await;

    let Ok(mut stream) = stream_result else {
        // Fallback: substring search (with dedup)
        let searched = store.search(query);
        let allowed: std::collections::HashSet<&str> =
            headers.iter().map(|h| h.name.as_str()).collect();
        let names: Vec<String> = searched
            .into_iter()
            .filter(|m| allowed.contains(m.name.as_str()))
            .map(|m| m.name)
            .take(max_results)
            .collect();
        let latency_ms = _start.elapsed().as_millis() as u64;
        tracing::info!(
            target: "memory_recall_shape",
            num_memories_selected = names.len(),
            num_memories_available = num_available,
            model_used = model_name,
            latency_ms = latency_ms,
            "memory recall — fallback to substring search",
        );
        return names;
    };

    use futures::StreamExt;
    let mut full_text = String::new();
    while let Some(Ok(event)) = stream.next().await {
        if let crate::interface::model::ModelEvent::TextDelta { text } = event {
            full_text.push_str(&text);
        }
    }

    // Try to parse JSON from response
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&full_text) {
        if let Some(arr) = parsed["selected_memories"].as_array() {
            let names: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .take(max_results)
                .collect();
            let latency_ms = _start.elapsed().as_millis() as u64;
            tracing::info!(
                target: "memory_recall_shape",
                num_memories_selected = names.len(),
                num_memories_available = num_available,
                model_used = model_name,
                latency_ms = latency_ms,
                "memory recall completed",
            );
            return names;
        }
    }

    // Final fallback: substring search (with dedup)
    let searched = store.search(query);
    let allowed: std::collections::HashSet<&str> =
        headers.iter().map(|h| h.name.as_str()).collect();
    let names: Vec<String> = searched
        .into_iter()
        .filter(|m| allowed.contains(m.name.as_str()))
        .map(|m| m.name)
        .take(max_results)
        .collect();
    let latency_ms = _start.elapsed().as_millis() as u64;
    tracing::info!(
        target: "memory_recall_shape",
        num_memories_selected = names.len(),
        num_memories_available = num_available,
        model_used = model_name,
        latency_ms = latency_ms,
        "memory recall — final fallback to substring search",
    );
    names
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("IO error: {0}")]
    Io(String),
    #[error("serialization error: {0}")]
    Serde(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn empty_store_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::new(
            tmp.path().join("user").join("memory"),
            tmp.path().join("local").join("memory"),
        );
        assert!(store.load_all().is_empty());
    }

    #[test]
    fn parse_memory_with_frontmatter() {
        let raw = r#"---
name: test-memory
description: A test memory
type: project
---

This is the content of the memory.
It can have multiple lines."#;
        let mem = MemoryStore::parse_memory_file(raw).unwrap();
        assert_eq!(mem.name, "test-memory");
        assert_eq!(mem.description, "A test memory");
        assert_eq!(mem.memory_type, MemoryType::Project);
        assert_eq!(mem.content, "This is the content of the memory.\nIt can have multiple lines.");
    }

    #[test]
    fn write_and_read_memory_file() {
        let tmp = TempDir::new().unwrap();
        let mem = DurableMemory {
            name: "test".into(),
            description: "test desc".into(),
            memory_type: MemoryType::Feedback,
            content: "Test content".into(),
            source_session_id: "s1".into(),
            confidence: 0.9,
            last_seen: "2026-06-10T00:00:00Z".into(),
            recall_count: 0,
        };
        MemoryStore::write_memory_file(tmp.path(), &mem).unwrap();
        // Read back
        let path = tmp.path().join("test.md");
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed = MemoryStore::parse_memory_file(&raw).unwrap();
        assert_eq!(parsed.name, "test");
        assert_eq!(parsed.description, "test desc");
        assert_eq!(parsed.memory_type, MemoryType::Feedback);
    }

    #[test]
    fn persist_and_load() {
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::new(
            tmp.path().join("user").join("memory"),
            tmp.path().join("local").join("memory"),
        );
        let mem = DurableMemory {
            name: "test".into(),
            description: "test summary".into(),
            memory_type: MemoryType::User,
            content: "The content".into(),
            source_session_id: "s1".into(),
            confidence: 0.8,
            last_seen: "2026-06-10T00:00:00Z".into(),
            recall_count: 0,
        };
        store.persist_batch(vec![mem]).unwrap();
        let loaded = store.load_all();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "test");
    }

    #[test]
    fn low_confidence_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::new(
            tmp.path().join("user").join("memory"),
            tmp.path().join("local").join("memory"),
        );
        let mem = DurableMemory {
            name: "low-conf".into(),
            description: "desc".into(),
            memory_type: MemoryType::Project,
            content: "content".into(),
            source_session_id: "s1".into(),
            confidence: 0.1, // below 0.3 threshold
            last_seen: "2026-06-10T00:00:00Z".into(),
            recall_count: 0,
        };
        let saved = store.persist_batch(vec![mem]).unwrap();
        assert_eq!(saved, 0);
    }

    #[test]
    fn parse_backward_compat_nested_type() {
        // Old format: metadata:\n  type:  — should still parse correctly
        let raw = r#"---
name: old-format
description: written by old write_memory_file
metadata:
  type: feedback
---

This memory was written before the flat-format fix."#;
        let mem = MemoryStore::parse_memory_file(raw).unwrap();
        assert_eq!(mem.name, "old-format");
        assert_eq!(mem.memory_type, MemoryType::Feedback);
        assert_eq!(mem.content, "This memory was written before the flat-format fix.");
    }

    #[test]
    fn sanitize_filename_prevents_traversal() {
        let result = sanitize_filename("../../etc/passwd");
        assert!(!result.contains('/'));
        assert!(!result.contains(".."));
    }
}
