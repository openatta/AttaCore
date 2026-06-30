//! Verification loop — ensures coding tasks produce verified results.
//!
//! For Debug, Refactor, and other high-risk tasks, the agent must verify
//! its work before claiming completion. This module defines:
//! - `VerificationPolicy` — when and how to verify
//! - `VerificationRecord` — what was verified and the result
//! - `CodingLoopState` — the verification state machine
//!
//! The verification loop is triggered by the Agent after a turn completes,
//! if the scene's `VerificationPolicy::required_level` is > `None`.

// CodingTaskKind used by callers of VerificationPolicy::from_kind

// ═══════════════════════════════════════════════════════════
// VerificationLevel
// ═══════════════════════════════════════════════════════════

/// How thoroughly to verify a task's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VerificationLevel {
    /// No verification needed (explain, search, document, plan).
    None = 0,
    /// Agent should self-check its diff for obvious issues.
    DiffSelfCheck = 1,
    /// Run static analysis (linter, formatter, type-checker).
    StaticCheck = 2,
    /// Run the specific tests related to the change.
    TargetedTest = 3,
    /// Run the full test suite.
    FullTest = 4,
    /// Run the full CI-equivalent checks (build + test + lint).
    CiEquivalent = 5,
}

// ═══════════════════════════════════════════════════════════
// VerificationPolicy
// ═══════════════════════════════════════════════════════════

/// Policy for verifying coding task results.
#[derive(Debug, Clone)]
pub struct VerificationPolicy {
    /// Minimum verification level required before task completion.
    pub required_level: VerificationLevel,
    /// If true, the agent CANNOT claim completion when verification fails.
    pub block_completion_on_failure: bool,
    /// If true, the agent may explain why verification is unavailable.
    pub allow_explain_if_unavailable: bool,
    /// Maximum number of repair iterations (diagnose → fix → verify again).
    pub max_repair_iterations: u32,
}

impl Default for VerificationPolicy {
    fn default() -> Self {
        Self {
            required_level: VerificationLevel::None,
            block_completion_on_failure: false,
            allow_explain_if_unavailable: true,
            max_repair_iterations: 3,
        }
    }
}

impl VerificationPolicy {
    /// No verification at all.
    pub fn none() -> Self {
        Self::default()
    }

    /// Basic diff self-check — the agent reviews its own changes.
    pub fn diff_self_check() -> Self {
        Self {
            required_level: VerificationLevel::DiffSelfCheck,
            block_completion_on_failure: false,
            allow_explain_if_unavailable: true,
            max_repair_iterations: 0,
        }
    }

    /// Run targeted tests related to the change.
    pub fn targeted_test() -> Self {
        Self {
            required_level: VerificationLevel::TargetedTest,
            block_completion_on_failure: true,
            allow_explain_if_unavailable: true,
            max_repair_iterations: 3,
        }
    }

    /// Full verification required — test suite must pass.
    pub fn required() -> Self {
        Self {
            required_level: VerificationLevel::FullTest,
            block_completion_on_failure: true,
            allow_explain_if_unavailable: false,
            max_repair_iterations: 3,
        }
    }

    /// Build from a task profile's verification_policy string.
    pub fn from_profile(policy_str: &str) -> Self {
        match policy_str {
            "required" => Self::required(),
            "suggested" => Self::targeted_test(),
            "review_only" => Self {
                required_level: VerificationLevel::DiffSelfCheck,
                block_completion_on_failure: false,
                allow_explain_if_unavailable: true,
                max_repair_iterations: 0,
            },
            _ => Self::none(),
        }
    }

    /// Whether any verification is needed at all.
    pub fn is_enabled(&self) -> bool {
        self.required_level > VerificationLevel::None
    }
}

// ═══════════════════════════════════════════════════════════
// VerificationRecord
// ═══════════════════════════════════════════════════════════

/// The result of a single verification attempt.
#[derive(Debug, Clone)]
pub struct VerificationRecord {
    /// The command that was run (e.g. "cargo test -- test_login").
    pub command: String,
    /// Exit code if the command completed.
    pub exit_code: Option<i32>,
    /// Whether the verification passed.
    pub passed: bool,
    /// First ~500 chars of stdout.
    pub stdout_excerpt: String,
    /// First ~500 chars of stderr.
    pub stderr_excerpt: String,
    /// If failed, a summary of the failure.
    pub failure_summary: Option<String>,
}

impl VerificationRecord {
    /// Create a record from command execution results.
    pub fn new(
        command: impl Into<String>,
        exit_code: Option<i32>,
        stdout: impl Into<String>,
        stderr: impl Into<String>,
    ) -> Self {
        let stdout = stdout.into();
        let stderr = stderr.into();
        let passed = exit_code == Some(0);

        let failure_summary = if !passed {
            let excerpt: String = stderr.lines().take(10).collect::<Vec<_>>().join("\n");
            if excerpt.is_empty() {
                Some(format!("Command exited with code {:?}", exit_code))
            } else {
                Some(truncate_str(&excerpt, 500))
            }
        } else {
            None
        };

        Self {
            command: command.into(),
            exit_code,
            passed,
            stdout_excerpt: truncate_str(&stdout, 500),
            stderr_excerpt: truncate_str(&stderr, 500),
            failure_summary,
        }
    }

    /// Create a skipped record (verification was not run).
    pub fn skipped(reason: impl Into<String>) -> Self {
        Self {
            command: "(skipped)".into(),
            exit_code: None,
            passed: false,
            stdout_excerpt: String::new(),
            stderr_excerpt: reason.into(),
            failure_summary: None,
        }
    }
}

// ═══════════════════════════════════════════════════════════
// CodingLoopState
// ═══════════════════════════════════════════════════════════

/// The state machine for the coding verification loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodingLoopState {
    /// Planning phase — agent designs the approach.
    Plan,
    /// Editing phase — agent makes code changes.
    Edit,
    /// Review phase — agent reviews its own changes.
    Review,
    /// Verification phase — agent runs tests/checks.
    Verify,
    /// Verification failed — agent diagnoses the failure.
    DiagnoseFailure,
    /// Repair phase — agent fixes the issue and will verify again.
    Repair,
    /// Final summary — verification passed, report results.
    Summarize,
    /// Blocked — max repair iterations exceeded, cannot complete.
    Blocked,
    /// Task complete — everything passed.
    Complete,
}

impl CodingLoopState {
    /// Check if this is a terminal state (no more iterations).
    pub fn is_terminal(&self) -> bool {
        matches!(self, CodingLoopState::Complete | CodingLoopState::Blocked)
    }
}

// ═══════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}…", &s[..max_len])
    }
}

// ═══════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_policy_is_disabled() {
        assert!(!VerificationPolicy::none().is_enabled());
    }

    #[test]
    fn required_policy_is_enabled() {
        let p = VerificationPolicy::required();
        assert!(p.is_enabled());
        assert!(p.block_completion_on_failure);
        assert_eq!(p.max_repair_iterations, 3);
    }

    #[test]
    fn from_profile_string() {
        assert!(VerificationPolicy::from_profile("required").is_enabled());
        assert!(!VerificationPolicy::from_profile("none").is_enabled());
        assert!(VerificationPolicy::from_profile("suggested").is_enabled());
    }

    #[test]
    fn verification_record_passed() {
        let record =
            VerificationRecord::new("cargo test", Some(0), "running 5 tests\nall passed", "");
        assert!(record.passed);
        assert!(record.failure_summary.is_none());
    }

    #[test]
    fn verification_record_failed() {
        let record = VerificationRecord::new(
            "cargo test",
            Some(1),
            "",
            "assertion failed at src/test.rs:42",
        );
        assert!(!record.passed);
        assert!(record.failure_summary.is_some());
    }

    #[test]
    fn loop_states_terminal() {
        assert!(CodingLoopState::Complete.is_terminal());
        assert!(CodingLoopState::Blocked.is_terminal());
        assert!(!CodingLoopState::Verify.is_terminal());
        assert!(!CodingLoopState::Repair.is_terminal());
    }
}
