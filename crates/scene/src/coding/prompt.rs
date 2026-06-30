//! TaskProfile and PromptProfile — task-specific prompt templates and execution policies.
//!
//! Each `CodingTaskKind` has a built-in default `TaskProfile` that specifies:
//! - Which model profile to use (→ strong/normal/lite)
//! - Which tool policy (read_only / read_write / read_write_test)
//! - Whether verification is required
//! - The prompt fragment that specializes the system prompt for this task.

use crate::coding::task::CodingTaskKind;

// ═══════════════════════════════════════════════════════════
// TaskProfile
// ═══════════════════════════════════════════════════════════

/// Execution profile for a coding task kind.
///
/// Maps task kind → model selection + tool permissions + verification requirements.
#[derive(Debug, Clone)]
pub struct TaskProfile {
    /// Task kind name (e.g. "explain", "debug") — config key.
    pub kind: String,
    /// Model profile id to use (references ModelProfileRegistry).
    /// None = use CodingSceneConfig.default_model_profile.
    pub model_profile: Option<String>,
    /// Tool policy: "read_only", "read_write", "read_write_test".
    pub tool_policy: String,
    /// Verification policy: "none", "suggested", "required", "review_only".
    pub verification_policy: String,
    /// Prompt profile id (references a PromptProfile).
    /// None = use the built-in prompt for this task kind.
    pub prompt_profile: Option<String>,
    /// Whether this task requires a plan before editing.
    pub require_plan: bool,
    /// Whether this task requires a review after editing.
    pub require_review: bool,
    /// Whether verification is required for task completion.
    pub require_verification: bool,
}

impl TaskProfile {
    /// Built-in default profile for each task kind.
    pub fn builtin(kind: CodingTaskKind) -> Self {
        match kind {
            CodingTaskKind::Explain => Self {
                kind: "explain".into(),
                model_profile: Some("lite".into()),
                tool_policy: "read_only".into(),
                verification_policy: "none".into(),
                prompt_profile: None,
                require_plan: false,
                require_review: false,
                require_verification: false,
            },
            CodingTaskKind::Search => Self {
                kind: "search".into(),
                model_profile: Some("lite".into()),
                tool_policy: "read_only".into(),
                verification_policy: "none".into(),
                prompt_profile: None,
                require_plan: false,
                require_review: false,
                require_verification: false,
            },
            CodingTaskKind::Generate => Self {
                kind: "generate".into(),
                model_profile: Some("normal".into()),
                tool_policy: "read_write".into(),
                verification_policy: "none".into(),
                prompt_profile: None,
                require_plan: false,
                require_review: false,
                require_verification: false,
            },
            CodingTaskKind::Modify => Self {
                kind: "modify".into(),
                model_profile: Some("normal".into()),
                tool_policy: "read_write".into(),
                verification_policy: "suggested".into(),
                prompt_profile: None,
                require_plan: false,
                require_review: false,
                require_verification: false,
            },
            CodingTaskKind::Debug => Self {
                kind: "debug".into(),
                model_profile: Some("strong".into()),
                tool_policy: "read_write_test".into(),
                verification_policy: "required".into(),
                prompt_profile: None,
                require_plan: false,
                require_review: false,
                require_verification: true,
            },
            CodingTaskKind::Review => Self {
                kind: "review".into(),
                model_profile: Some("normal".into()),
                tool_policy: "read_only".into(),
                verification_policy: "review_only".into(),
                prompt_profile: None,
                require_plan: false,
                require_review: false,
                require_verification: false,
            },
            CodingTaskKind::Refactor => Self {
                kind: "refactor".into(),
                model_profile: Some("strong".into()),
                tool_policy: "read_write_test".into(),
                verification_policy: "required".into(),
                prompt_profile: None,
                require_plan: false,
                require_review: false,
                require_verification: true,
            },
            CodingTaskKind::Document => Self {
                kind: "document".into(),
                model_profile: Some("lite".into()),
                tool_policy: "read_only".into(),
                verification_policy: "none".into(),
                prompt_profile: None,
                require_plan: false,
                require_review: false,
                require_verification: false,
            },
            CodingTaskKind::Plan => Self {
                kind: "plan".into(),
                model_profile: Some("normal".into()),
                tool_policy: "read_only".into(),
                verification_policy: "none".into(),
                prompt_profile: None,
                require_plan: false,
                require_review: false,
                require_verification: false,
            },
        }
    }

    /// Whether this task profile requires verification.
    pub fn needs_verification(&self) -> bool {
        self.require_verification || self.verification_policy == "required"
    }

    /// Whether this task is read-only.
    pub fn is_read_only(&self) -> bool {
        self.tool_policy == "read_only"
    }
}

// ═══════════════════════════════════════════════════════════
// PromptProfile
// ═══════════════════════════════════════════════════════════

/// A prompt profile — system-level instructions for a specific task kind.
///
/// Injected as an additional system prompt block (Ephemeral cache).
#[derive(Debug, Clone)]
pub struct PromptProfile {
    /// Profile identifier (e.g. "explain", "debug").
    pub id: String,
    /// System prompt fragment for this task role.
    pub system_rules: String,
    /// Output format instruction (appended to the end of the prompt).
    pub output_format: String,
}

impl PromptProfile {
    /// Get the built-in prompt profile for a given task kind.
    pub fn builtin(kind: CodingTaskKind) -> Self {
        match kind {
            CodingTaskKind::Explain => Self {
                id: "explain".into(),
                system_rules: explain_rules(),
                output_format: explain_output(),
            },
            CodingTaskKind::Search => Self {
                id: "search".into(),
                system_rules: search_rules(),
                output_format: search_output(),
            },
            CodingTaskKind::Generate => Self {
                id: "generate".into(),
                system_rules: generate_rules(),
                output_format: generate_output(),
            },
            CodingTaskKind::Modify => Self {
                id: "modify".into(),
                system_rules: modify_rules(),
                output_format: modify_output(),
            },
            CodingTaskKind::Debug => Self {
                id: "debug".into(),
                system_rules: debug_rules(),
                output_format: debug_output(),
            },
            CodingTaskKind::Review => Self {
                id: "review".into(),
                system_rules: review_rules(),
                output_format: review_output(),
            },
            CodingTaskKind::Refactor => Self {
                id: "refactor".into(),
                system_rules: refactor_rules(),
                output_format: refactor_output(),
            },
            CodingTaskKind::Document => Self {
                id: "document".into(),
                system_rules: document_rules(),
                output_format: document_output(),
            },
            CodingTaskKind::Plan => Self {
                id: "plan".into(),
                system_rules: plan_rules(),
                output_format: plan_output(),
            },
        }
    }
}

// ═══════════════════════════════════════════════════════════
// Built-in prompt fragments
// ═══════════════════════════════════════════════════════════

fn explain_rules() -> String {
    "# Task: Explain\n\n\
     You are explaining code, architecture, or behavior. \
     Read the relevant files, understand the context, and provide a clear explanation.\n\n\
     - Start with a one-sentence summary\n\
     - Explain the flow, not just the syntax\n\
     - Mention key dependencies and side effects\n\
     - Never guess — if you're unsure, say so and suggest what to read next"
        .into()
}

fn explain_output() -> String {
    "# Output format\n\
     - Summary (1 sentence)\n\
     - Detailed explanation\n\
     - Key files referenced"
        .into()
}

fn search_rules() -> String {
    "# Task: Search\n\n\
     You are searching the codebase for definitions, usages, or patterns. \
     Use Grep, Glob, and LSP tools to find what the user is looking for.\n\n\
     - Cast a wide net first, then narrow\n\
     - Report exact file paths and line numbers\n\
     - Group results by relevance or location\n\
     - If nothing found, explain what you searched and suggest alternatives"
        .into()
}

fn search_output() -> String {
    "# Output format\n\
     - List of findings with file:line\n\
     - Brief description of each match\n\
     - Summary count"
        .into()
}

fn generate_rules() -> String {
    "# Task: Generate\n\n\
     You are writing new code. Follow the project's existing patterns and conventions.\n\n\
     - Read surrounding files to match style\n\
     - Write idiomatic code for the language/framework\n\
     - Include error handling at system boundaries\n\
     - Don't over-engineer: start simple, add complexity only when needed\n\
     - If tests are expected, write them"
        .into()
}

fn generate_output() -> String {
    "# Output format\n\
     - What you created (file list)\n\
     - Key design decisions\n\
     - Suggested next steps"
        .into()
}

fn modify_rules() -> String {
    "# Task: Modify\n\n\
     You are editing existing code. Make focused, minimal changes.\n\n\
     - Read the file before editing it\n\
     - Change only what's needed — don't refactor unrelated code\n\
     - Match the surrounding code style\n\
     - After editing, verify: run the relevant tests or explain why you can't\n\
     - If the change touches a public API, note the impact"
        .into()
}

fn modify_output() -> String {
    "# Output format\n\
     - What you changed and why\n\
     - Files modified\n\
     - Verification result (or why not verified)"
        .into()
}

fn debug_rules() -> String {
    "# Task: Debug\n\n\
     You are finding and fixing a bug. Follow a disciplined debugging process.\n\n\
     - **Locate first**: read error messages, logs, and relevant code before changing anything\n\
     - **Diagnose**: find the root cause, not just the symptom\n\
     - **Minimal fix**: the smallest change that fixes the root cause\n\
     - **Verify**: run the failing test or command to confirm the fix works\n\
     - **CRITICAL**: If verification fails, diagnose and try again — do NOT claim success without verification\n\
     - If you cannot verify, explain why explicitly"
        .into()
}

fn debug_output() -> String {
    "# Output format\n\
     - Root cause (1-2 sentences)\n\
     - Fix description\n\
     - Files changed\n\
     - Verification result (MANDATORY: include the test/command output)"
        .into()
}

fn review_rules() -> String {
    "# Task: Code Review\n\n\
     You are reviewing code changes. Do NOT edit code — only analyze.\n\n\
     Review for:\n\
     1. **Correctness**: does the change do what it claims?\n\
     2. **Edge cases**: null, empty, error paths, boundary conditions\n\
     3. **Security**: injection, auth bypass, data exposure\n\
     4. **Compatibility**: API breakage, schema changes, dependency impact\n\
     5. **Test gaps**: what should be tested but isn't?\n\n\
     - Be specific: reference exact lines and why they're risky\n\
     - Distinguish severity: critical / important / nice-to-have"
        .into()
}

fn review_output() -> String {
    "# Output format\n\
     - Summary verdict (approve / changes requested)\n\
     - Findings by severity (critical → important → nice-to-have)\n\
     - Each finding: file:line, what's wrong, suggested fix"
        .into()
}

fn refactor_rules() -> String {
    "# Task: Refactor\n\n\
     You are restructuring code without changing behavior.\n\n\
     - **Understand first**: read all callers and callees before moving anything\n\
     - **Preserve behavior**: the refactored code must pass all existing tests\n\
     - **Small steps**: one logical change at a time, verify between steps\n\
     - **Test gate**: run the full test suite (or the relevant subset) before claiming success\n\
     - **CRITICAL**: If tests fail after refactoring, fix the regression — do NOT proceed"
        .into()
}

fn refactor_output() -> String {
    "# Output format\n\
     - What was restructured and why\n\
     - Files changed\n\
     - Before/after comparison (if significant)\n\
     - Test results (MANDATORY)"
        .into()
}

fn document_rules() -> String {
    "# Task: Document\n\n\
     You are writing documentation. Do NOT change code logic.\n\n\
     - Base documentation on the actual code, not assumptions\n\
     - Read the code you're documenting\n\
     - Be concise and accurate\n\
     - Document the WHY, not just the WHAT\n\
     - Never invent features that don't exist"
        .into()
}

fn document_output() -> String {
    "# Output format\n\
     - The documentation content\n\
     - Where it was added/updated"
        .into()
}

fn plan_rules() -> String {
    "# Task: Plan\n\n\
     You are designing a solution before implementation. Do NOT write production code yet.\n\n\
     - Explore the codebase to understand current state\n\
     - Propose architectural options with trade-offs\n\
     - Recommend one approach with reasoning\n\
     - Break down into implementable steps\n\
     - Identify risks and unknowns"
        .into()
}

fn plan_output() -> String {
    "# Output format\n\
     - Recommended approach (1 paragraph)\n\
     - Alternatives considered (with trade-offs)\n\
     - Implementation steps\n\
     - Risks / unknowns"
        .into()
}

// ═══════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_task_kinds_have_builtin_profiles() {
        for kind in CodingTaskKind::all() {
            let tp = TaskProfile::builtin(*kind);
            assert_eq!(tp.kind, kind.as_str());
            assert!(!tp.tool_policy.is_empty());
        }
    }

    #[test]
    fn debug_requires_verification() {
        let tp = TaskProfile::builtin(CodingTaskKind::Debug);
        assert!(tp.needs_verification());
        assert_eq!(tp.model_profile, Some("strong".into()));
    }

    #[test]
    fn explain_is_lite_read_only() {
        let tp = TaskProfile::builtin(CodingTaskKind::Explain);
        assert!(tp.is_read_only());
        assert_eq!(tp.model_profile, Some("lite".into()));
        assert!(!tp.needs_verification());
    }

    #[test]
    fn all_prompt_profiles_exist() {
        for kind in CodingTaskKind::all() {
            let pp = PromptProfile::builtin(*kind);
            assert!(!pp.system_rules.is_empty());
            assert!(!pp.output_format.is_empty());
        }
    }

    #[test]
    fn debug_prompt_mentions_verification() {
        let pp = PromptProfile::builtin(CodingTaskKind::Debug);
        assert!(pp.system_rules.contains("verify"));
        assert!(pp.output_format.contains("Verification"));
    }

    #[test]
    fn review_prompt_mentions_no_edits() {
        let pp = PromptProfile::builtin(CodingTaskKind::Review);
        assert!(pp.system_rules.contains("Do NOT edit"));
    }

    #[test]
    fn refactor_prompt_requires_tests() {
        let pp = PromptProfile::builtin(CodingTaskKind::Refactor);
        assert!(pp.system_rules.contains("test"));
    }
}
