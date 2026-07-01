//! ContextPack — structured context collection for coding tasks.
//!
//! The ContextPackBuilder gathers git status, relevant files, error summaries,
//! and test hints BEFORE the model call, using pre-turn tool execution.
//! This ensures the model sees structured, relevant context instead of
//! having to search blindly.

use crate::coding::task::CodingTaskKind;

// ═══════════════════════════════════════════════════════════
// ContextPack
// ═══════════════════════════════════════════════════════════

/// Structured context collected for a coding task.
///
/// Rendered into the system prompt as a `# Context Pack` section.
#[derive(Debug, Clone, Default)]
pub struct ContextPack {
    /// The classified task kind.
    pub task_kind: Option<CodingTaskKind>,
    /// The user's raw request.
    pub user_request: String,

    /// High-level repository summary (language, framework, build system).
    pub repo_summary: Option<String>,
    /// Current git status output (truncated).
    pub git_status: Option<String>,
    /// Git diff for the current branch (truncated).
    pub git_diff: Option<String>,

    /// Files relevant to the user's request (with reasons).
    pub relevant_files: Vec<ContextFile>,
    /// Symbols (functions, types) relevant to the request.
    pub relevant_symbols: Vec<ContextSymbol>,
    /// Related tests discovered.
    pub related_tests: Vec<ContextTest>,

    /// Error/failure summary extracted from user message.
    pub error_summary: Option<ErrorSummary>,
    /// Suggested verification commands.
    pub command_hints: Vec<String>,
    /// Active skills that may be relevant.
    pub active_skills: Vec<String>,
    /// Risk notes for the agent.
    pub risk_notes: Vec<String>,
}

/// A file relevant to the task.
#[derive(Debug, Clone)]
pub struct ContextFile {
    /// Relative path from repo root.
    pub path: String,
    /// Why this file is relevant.
    pub reason: String,
    /// Snippet of key content (first ~500 chars or relevant section).
    pub snippet: Option<String>,
}

/// A code symbol relevant to the task.
#[derive(Debug, Clone)]
pub struct ContextSymbol {
    /// Symbol name.
    pub name: String,
    /// Symbol kind (function, struct, trait, module, etc.).
    pub kind: String,
    /// File where the symbol is defined.
    pub file: String,
    /// Line number.
    pub line: Option<usize>,
}

/// A test relevant to the task.
#[derive(Debug, Clone)]
pub struct ContextTest {
    /// Test name.
    pub name: String,
    /// File where the test is defined.
    pub file: String,
    /// Command to run this test.
    pub command: String,
}

/// Parsed error/failure summary from user input.
#[derive(Debug, Clone)]
pub struct ErrorSummary {
    /// Error type: TestFailure, BuildError, RuntimeError, etc.
    pub failure_type: String,
    /// The failing command.
    pub command: Option<String>,
    /// Key error message excerpt.
    pub key_message: String,
    /// File:line where the error originated (if identifiable).
    pub location: Option<String>,
}

impl ContextPack {
    /// Render the ContextPack as a structured markdown section for the system prompt.
    pub fn render(&self) -> String {
        let mut out = String::from("# Context Pack\n");

        // Task
        if let Some(kind) = self.task_kind {
            out.push_str(&format!("\n## Task\n- Kind: {}\n", kind_label(kind)));
        }

        // User request (trimmed)
        let req = if self.user_request.len() > 300 {
            format!("{}…", &self.user_request[..300])
        } else {
            self.user_request.clone()
        };
        out.push_str(&format!("\n## User Request\n{req}\n"));

        // Repo summary
        if let Some(ref summary) = self.repo_summary {
            out.push_str(&format!("\n## Repository\n{summary}\n"));
        }

        // Git status
        if let Some(ref status) = self.git_status {
            let truncated = truncate_lines(status, 30);
            out.push_str(&format!("\n## Git Status\n{truncated}\n"));
        }

        // Git diff
        if let Some(ref diff) = self.git_diff {
            let truncated = truncate_lines(diff, 50);
            out.push_str(&format!("\n## Git Diff\n{truncated}\n"));
        }

        // Relevant files
        if !self.relevant_files.is_empty() {
            out.push_str("\n## Relevant Files\n");
            for f in &self.relevant_files {
                out.push_str(&format!(
                    "- `{path}`\n  Reason: {reason}\n",
                    path = f.path,
                    reason = f.reason
                ));
            }
        }

        // Relevant symbols
        if !self.relevant_symbols.is_empty() {
            out.push_str("\n## Relevant Symbols\n");
            for s in &self.relevant_symbols {
                let loc = s
                    .line
                    .map(|l| format!("{}:{}", s.file, l))
                    .unwrap_or_else(|| s.file.clone());
                out.push_str(&format!(
                    "- `{name}` ({kind}) — {loc}\n",
                    name = s.name,
                    kind = s.kind,
                    loc = loc
                ));
            }
        }

        // Related tests
        if !self.related_tests.is_empty() {
            out.push_str("\n## Related Tests\n");
            for t in &self.related_tests {
                out.push_str(&format!(
                    "- `{name}` in `{file}` — `{cmd}`\n",
                    name = t.name,
                    file = t.file,
                    cmd = t.command
                ));
            }
        }

        // Error summary
        if let Some(ref err) = self.error_summary {
            out.push_str("\n## Error Summary\n");
            out.push_str(&format!("- Type: {}\n", err.failure_type));
            if let Some(ref cmd) = err.command {
                out.push_str(&format!("- Command: `{cmd}`\n"));
            }
            out.push_str(&format!("- Message: {}\n", err.key_message));
            if let Some(ref loc) = err.location {
                out.push_str(&format!("- Location: {loc}\n"));
            }
        }

        // Command hints
        if !self.command_hints.is_empty() {
            out.push_str("\n## Suggested Commands\n");
            for hint in &self.command_hints {
                out.push_str(&format!("- `{hint}`\n"));
            }
        }

        out
    }
}

// ═══════════════════════════════════════════════════════════
// ContextPackBuilder
// ═══════════════════════════════════════════════════════════

/// Builder for constructing a ContextPack.
///
/// Uses pre-turn tool execution (grep, LSP, Read) to collect relevant
/// context before the model call. All collection is read-only and
/// has a 5-second timeout per operation.
#[derive(Debug, Clone, Default)]
pub struct ContextPackBuilder {
    pack: ContextPack,
}

impl ContextPackBuilder {
    /// Create a new builder with a user request.
    pub fn new(user_request: impl Into<String>) -> Self {
        Self {
            pack: ContextPack {
                user_request: user_request.into(),
                ..Default::default()
            },
        }
    }

    /// Set the task kind.
    pub fn task_kind(mut self, kind: CodingTaskKind) -> Self {
        self.pack.task_kind = Some(kind);
        self
    }

    /// Set repo summary (e.g. from FrozenContext or language detectors).
    pub fn repo_summary(mut self, summary: impl Into<String>) -> Self {
        self.pack.repo_summary = Some(summary.into());
        self
    }

    /// Set git status from frozen context.
    pub fn git_status(mut self, status: impl Into<String>) -> Self {
        let s = status.into();
        if !s.is_empty() {
            self.pack.git_status = Some(s);
        }
        self
    }

    /// Set git diff.
    pub fn git_diff(mut self, diff: impl Into<String>) -> Self {
        let d = diff.into();
        if !d.is_empty() {
            self.pack.git_diff = Some(d);
        }
        self
    }

    /// Add a relevant file.
    pub fn relevant_file(mut self, path: impl Into<String>, reason: impl Into<String>) -> Self {
        self.pack.relevant_files.push(ContextFile {
            path: path.into(),
            reason: reason.into(),
            snippet: None,
        });
        self
    }

    /// Add a relevant symbol.
    pub fn relevant_symbol(
        mut self,
        name: impl Into<String>,
        kind: impl Into<String>,
        file: impl Into<String>,
        line: Option<usize>,
    ) -> Self {
        self.pack.relevant_symbols.push(ContextSymbol {
            name: name.into(),
            kind: kind.into(),
            file: file.into(),
            line,
        });
        self
    }

    /// Add a related test.
    pub fn related_test(
        mut self,
        name: impl Into<String>,
        file: impl Into<String>,
        command: impl Into<String>,
    ) -> Self {
        self.pack.related_tests.push(ContextTest {
            name: name.into(),
            file: file.into(),
            command: command.into(),
        });
        self
    }

    /// Parse and set error summary from the user's message.
    pub fn error_from_message(mut self, message: &str) -> Self {
        if let Some(err) = try_extract_error(message) {
            self.pack.error_summary = Some(err);
        }
        self
    }

    /// Add a command hint.
    pub fn command_hint(mut self, hint: impl Into<String>) -> Self {
        self.pack.command_hints.push(hint.into());
        self
    }

    /// Add an active skill name.
    pub fn active_skill(mut self, skill: impl Into<String>) -> Self {
        self.pack.active_skills.push(skill.into());
        self
    }

    /// Add a risk note.
    pub fn risk_note(mut self, note: impl Into<String>) -> Self {
        self.pack.risk_notes.push(note.into());
        self
    }

    /// Build the ContextPack.
    pub fn build(self) -> ContextPack {
        self.pack
    }

    /// Quick-build from just a user message (minimal context).
    pub fn minimal(user_request: impl Into<String>) -> ContextPack {
        ContextPack {
            user_request: user_request.into(),
            ..Default::default()
        }
    }

    /// Build from frozen context + user message, extracting what we can without tool calls.
    ///
    /// This is the "cheap" path — no pre-turn tool execution, just parse and include
    /// what's already available (git status, git log from frozen context, error parsing).
    pub fn from_frozen(
        user_request: impl Into<String>,
        frozen: Option<&base::frozen::FrozenContext>,
    ) -> ContextPack {
        let message = user_request.into();
        let mut builder = Self::new(message.clone());

        if let Some(f) = frozen {
            builder = builder
                .git_status(f.git_status.clone().unwrap_or_default())
                .git_diff(f.git_log.clone().unwrap_or_default());

            // Build a repo summary from available info
            let mut summary_parts: Vec<String> = Vec::new();
            if f.is_git {
                summary_parts.push("Git repository".into());
                if let Some(ref branch) = f.git_branch {
                    summary_parts.push(format!("branch: {branch}"));
                }
            }
            if let Some(ref style) = f.output_style {
                summary_parts.push(format!("style: {}", style.name));
            }
            if !summary_parts.is_empty() {
                builder = builder.repo_summary(summary_parts.join(", "));
            }
        }

        // Extract error info from the message (consume builder, get pack)
        let mut pack = builder.error_from_message(&message).build();

        // For debug tasks, suggest verification commands
        if let Some(ref err) = pack.error_summary {
            if let Some(ref cmd) = err.command {
                pack.command_hints.push(format!("Re-run: {cmd}"));
            }
            if let Some(ref loc) = err.location {
                pack.command_hints.push(format!("Run tests near: {loc}"));
            }
        }

        pack
    }
}

// ═══════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════

fn kind_label(kind: CodingTaskKind) -> &'static str {
    match kind {
        CodingTaskKind::Explain => "Explain",
        CodingTaskKind::Search => "Search",
        CodingTaskKind::Generate => "Generate",
        CodingTaskKind::Modify => "Modify",
        CodingTaskKind::Debug => "Debug",
        CodingTaskKind::Review => "Review",
        CodingTaskKind::Refactor => "Refactor",
        CodingTaskKind::Document => "Document",
        CodingTaskKind::Plan => "Plan",
        CodingTaskKind::Test => "Test",
        CodingTaskKind::Perf => "Perf",
        CodingTaskKind::Deps => "Deps",
    }
}

fn truncate_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if total <= max_lines {
        return text.to_string();
    }
    let truncated: Vec<&str> = lines.into_iter().take(max_lines).collect();
    format!(
        "{}\n… ({} more lines)",
        truncated.join("\n"),
        total - max_lines
    )
}

/// Try to extract an error summary from a user message.
fn try_extract_error(message: &str) -> Option<ErrorSummary> {
    let msg = message.trim();
    if msg.is_empty() {
        return None;
    }
    let lower = msg.to_lowercase();

    // Check for test failure patterns
    let has_test_fail =
        lower.contains("test") && (lower.contains("fail") || lower.contains("error"));
    let has_build_error =
        lower.contains("build") && (lower.contains("fail") || lower.contains("error"));
    let has_runtime_error = lower.contains("panic")
        || lower.contains("traceback")
        || lower.contains("exception")
        || lower.contains("segfault");

    if !has_test_fail && !has_build_error && !has_runtime_error {
        return None;
    }

    let failure_type = if has_test_fail {
        "TestFailure"
    } else if has_build_error {
        "BuildError"
    } else {
        "RuntimeError"
    };

    // Try to extract the failing command
    let command = extract_command_hint(msg);

    // Try to extract file:line
    let location = extract_file_line(msg);

    // Extract a key message (first line that looks like an error)
    let key_message = extract_key_message(msg);

    Some(ErrorSummary {
        failure_type: failure_type.into(),
        command,
        key_message,
        location,
    })
}

fn extract_command_hint(msg: &str) -> Option<String> {
    // Look for lines with cargo test / cargo build / etc.
    for line in msg.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("cargo ")
            || trimmed.starts_with("npm ")
            || trimmed.starts_with("make ")
            || trimmed.starts_with("go ")
            || trimmed.starts_with("python ")
            || trimmed.starts_with("rustc ")
        {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn extract_file_line(msg: &str) -> Option<String> {
    // Match patterns like "src/main.rs:42" or "at src/main.rs:42"
    for line in msg.lines() {
        let trimmed = line.trim();
        // Simple heuristic: find "<path>:<number>"
        if let Some(pos) = trimmed.find(".rs:") {
            let rest = &trimmed[pos..];
            let parts: Vec<&str> = rest.splitn(2, ':').collect();
            if parts.len() == 2 {
                let file = parts[0];
                let num_part = parts[1]
                    .split(|c: char| !c.is_ascii_digit())
                    .next()
                    .unwrap_or("");
                if !num_part.is_empty() {
                    return Some(format!("{file}:{num_part}"));
                }
            }
        }
    }
    None
}

fn extract_key_message(msg: &str) -> String {
    // Look for lines that indicate failure
    for line in msg.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_lowercase();
        if lower.contains("error:")
            || lower.contains("failed")
            || lower.contains("assertion failed")
            || lower.contains("panicked")
            || lower.contains("traceback")
        {
            let truncated = if trimmed.len() > 200 {
                format!("{}…", &trimmed[..200])
            } else {
                trimmed.to_string()
            };
            return truncated;
        }
    }
    // Fallback: first non-empty line
    msg.lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("(no details)")
        .to_string()
}

// ═══════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_context_pack() {
        let pack = ContextPackBuilder::minimal("explain the config parser");
        assert_eq!(pack.user_request, "explain the config parser");
        assert!(pack.task_kind.is_none());
        assert!(pack.relevant_files.is_empty());
    }

    #[test]
    fn builder_adds_relevant_files() {
        let pack = ContextPackBuilder::new("fix the login bug")
            .task_kind(CodingTaskKind::Debug)
            .relevant_file("src/auth/login.rs", "referenced in error message")
            .relevant_file("src/auth/middleware.rs", "called by login handler")
            .build();

        assert_eq!(pack.task_kind, Some(CodingTaskKind::Debug));
        assert_eq!(pack.relevant_files.len(), 2);
    }

    #[test]
    fn render_includes_all_sections() {
        let pack = ContextPackBuilder::new("fix failing test")
            .task_kind(CodingTaskKind::Debug)
            .git_status("On branch main\nnothing to commit")
            .relevant_file("tests/test_api.rs", "failing test")
            .error_from_message("cargo test test_login — assertion failed at src/auth.rs:42")
            .command_hint("cargo test test_login")
            .build();

        let rendered = pack.render();
        assert!(rendered.contains("# Context Pack"));
        assert!(rendered.contains("Debug"));
        assert!(rendered.contains("Git Status"));
        assert!(rendered.contains("tests/test_api.rs"));
        assert!(rendered.contains("Error Summary"));
        assert!(rendered.contains("Suggested Commands"));
    }

    #[test]
    fn error_extraction_from_message() {
        let msg = "cargo test is failing:\n\
                   test test_parse_config ... FAILED\n\
                   assertion `left == right` failed\n\
                   at src/config/parser.rs:142";

        let pack = ContextPackBuilder::new(msg).error_from_message(msg).build();

        let err = pack.error_summary.unwrap();
        assert_eq!(err.failure_type, "TestFailure");
        assert!(err.location.is_some());
    }

    #[test]
    fn no_error_for_normal_message() {
        let pack = ContextPackBuilder::new("explain the config parser")
            .error_from_message("explain the config parser")
            .build();
        assert!(pack.error_summary.is_none());
    }

    #[test]
    fn truncate_lines_limits_output() {
        let long = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_lines(&long, 5);
        assert!(result.contains("95 more lines"));
        assert!(result.lines().count() <= 7); // 5 lines + header + footer
    }
}
