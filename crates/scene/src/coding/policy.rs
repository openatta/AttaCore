//! PolicyHook — in-process, Rust-native hooks for enforcing engineering rules.
//!
//! Unlike the external hook system (command/Prompt/HTTP/Agent hooks in `hooks/`),
//! PolicyHooks are Rust trait implementations that evaluate rules at critical
//! execution points. They run BEFORE external hooks.
//!
//! # Built-in hooks (Phase 4)
//!
//! - `CompletionVerificationHook` — blocks task completion if verification is
//!   required but not performed or failed.
//! - `DiffSummaryHook` — requires a summary of changes before task completion.
//! - `DangerousCommandHook` — blocks or requires approval for high-risk commands.

use std::fmt;

// ═══════════════════════════════════════════════════════════
// PolicyHookPoint
// ═══════════════════════════════════════════════════════════

/// Execution points where policy hooks are evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyHookPoint {
    /// Before a model API call is made.
    BeforeModelCall,
    /// Before a tool is executed.
    BeforeToolCall,
    /// After a tool has executed.
    AfterToolCall,
    /// Before a file write operation.
    BeforeFileWrite,
    /// After a file write operation.
    AfterFileWrite,
    /// Before a shell command execution.
    BeforeCommandExec,
    /// After a shell command execution.
    AfterCommandExec,
    /// After verification has been attempted.
    AfterVerification,
    /// Before the task is marked complete.
    BeforeTaskComplete,
}

// ═══════════════════════════════════════════════════════════
// HookDecision
// ═══════════════════════════════════════════════════════════

/// The decision made by a policy hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    /// Allow the action to proceed.
    Allow,
    /// Deny the action outright with a reason.
    Deny { reason: String },
    /// Require user approval before proceeding.
    RequireUserApproval { reason: String },
    /// Require the agent to take remedial actions before continuing.
    RequireRemediation {
        /// Human-readable explanation of what's needed.
        message: String,
        /// Specific actions the agent must take.
        required_actions: Vec<String>,
    },
}

impl HookDecision {
    /// Whether this decision blocks the action.
    pub fn is_blocked(&self) -> bool {
        matches!(
            self,
            HookDecision::Deny { .. }
                | HookDecision::RequireUserApproval { .. }
                | HookDecision::RequireRemediation { .. }
        )
    }

    /// Whether this decision allows the action.
    pub fn is_allowed(&self) -> bool {
        matches!(self, HookDecision::Allow)
    }
}

// ═══════════════════════════════════════════════════════════
// PolicyContext
// ═══════════════════════════════════════════════════════════

/// Context passed to policy hooks for evaluation.
#[derive(Debug, Clone, Default)]
pub struct PolicyContext {
    /// The hook point being evaluated.
    pub hook_point: Option<PolicyHookPoint>,
    /// Files that have been changed in this turn.
    pub changed_files: Vec<String>,
    /// Commands that have been executed in this turn.
    pub executed_commands: Vec<String>,
    /// Whether verification has been performed.
    pub has_verification: bool,
    /// Whether verification passed (if performed).
    pub verification_passed: bool,
    /// The current tool name (for BeforeToolCall/AfterToolCall).
    pub tool_name: Option<String>,
    /// The current tool input (for BeforeToolCall).
    pub tool_input: Option<String>,
    /// The current command (for BeforeCommandExec/AfterCommandExec).
    pub command: Option<String>,
    /// v2.1.0: Whether the current tier requires verification (from escalation).
    pub tier_requires_verification: bool,
}

// ═══════════════════════════════════════════════════════════
// PolicyHook trait
// ═══════════════════════════════════════════════════════════

/// Trait for in-process policy hooks.
///
/// Implementations evaluate rules at specific `PolicyHookPoint`s and
/// return a `HookDecision`.
pub trait PolicyHook: Send + Sync + fmt::Debug {
    /// Unique hook identifier.
    fn id(&self) -> &str;

    /// The hook point this hook evaluates at.
    fn hook_point(&self) -> PolicyHookPoint;

    /// Human-readable description.
    fn description(&self) -> &str;

    /// Evaluate the hook with the given context.
    fn evaluate(&self, ctx: &PolicyContext) -> HookDecision;
}

// ═══════════════════════════════════════════════════════════
// Built-in hooks
// ═══════════════════════════════════════════════════════════

/// Hook: Blocks task completion if verification is required but not performed or failed.
#[derive(Debug, Clone)]
pub struct CompletionVerificationHook;

impl PolicyHook for CompletionVerificationHook {
    fn id(&self) -> &str {
        "completion-verification"
    }
    fn hook_point(&self) -> PolicyHookPoint {
        PolicyHookPoint::BeforeTaskComplete
    }
    fn description(&self) -> &str {
        "Blocks task completion if verification was required but not performed or failed"
    }
    fn evaluate(&self, ctx: &PolicyContext) -> HookDecision {
        // Determine if verification is required: either the tier demands it
        // or the task profile explicitly requires it.
        let requires_verification = ctx.tier_requires_verification;
        if !requires_verification && !ctx.has_verification {
            // No verification needed and none performed — allow
            return HookDecision::Allow;
        }
        if requires_verification && !ctx.has_verification {
            return HookDecision::Deny {
                reason:
                    "Verification is required for this task tier but no verification was performed."
                        .into(),
            };
        }
        if !ctx.verification_passed {
            return HookDecision::Deny {
                reason: "Verification failed. Fix the issues and verify again before completing."
                    .into(),
            };
        }
        HookDecision::Allow
    }
}

/// Hook: Requires a change summary when files have been modified.
#[derive(Debug, Clone)]
pub struct DiffSummaryHook;

impl PolicyHook for DiffSummaryHook {
    fn id(&self) -> &str {
        "diff-summary"
    }
    fn hook_point(&self) -> PolicyHookPoint {
        PolicyHookPoint::BeforeTaskComplete
    }
    fn description(&self) -> &str {
        "Requires a summary of changes before task completion when files were modified"
    }
    fn evaluate(&self, ctx: &PolicyContext) -> HookDecision {
        if ctx.changed_files.is_empty() {
            return HookDecision::Allow;
        }
        // When files were changed, ensure the agent's final output includes:
        // 1. What files were changed
        // 2. What was changed and why
        // 3. Whether verification was performed
        // The hook itself can't check the output text — it signals
        // the requirement and the agent layer enforces it.
        HookDecision::RequireRemediation {
            message: format!(
                "Files were modified: {}. The final response must include: \
                 1) which files changed, 2) what changed and why, 3) verification result.",
                ctx.changed_files.join(", ")
            ),
            required_actions: vec![
                "Summarize changes (files + rationale)".into(),
                "Report verification result".into(),
            ],
        }
    }
}

/// Hook: Blocks or requires approval for dangerous commands.
#[derive(Debug, Clone)]
pub struct DangerousCommandHook;

impl PolicyHook for DangerousCommandHook {
    fn id(&self) -> &str {
        "dangerous-command"
    }
    fn hook_point(&self) -> PolicyHookPoint {
        PolicyHookPoint::BeforeCommandExec
    }
    fn description(&self) -> &str {
        "Blocks or requires user approval for high-risk shell commands"
    }
    fn evaluate(&self, ctx: &PolicyContext) -> HookDecision {
        let cmd = match &ctx.command {
            Some(c) => c.as_str(),
            None => return HookDecision::Allow,
        };

        // Deny: explicitly destructive and irreversible
        let deny_patterns = [
            "rm -rf /",
            "rm -rf --no-preserve-root",
            "dd if=",
            "mkfs.",
            ":(){ :|:& };:", // fork bomb
        ];
        for pattern in &deny_patterns {
            if cmd.contains(pattern) {
                return HookDecision::Deny {
                    reason: format!(
                        "Command '{}' is explicitly blocked as dangerous and irreversible.",
                        truncate_cmd(cmd)
                    ),
                };
            }
        }

        // Require user approval: high-risk but potentially valid
        let require_approval_patterns = [
            "git reset --hard",
            "git clean -fd",
            "git clean -fdx",
            "git push --force",
            "git push -f",
            "rm -rf",
            "rm -r",
            "chmod 777",
            "chmod -R 777",
            "curl ",
            "wget ",
            "| sh",
            "| bash",
            "sudo ",
            "DROP ",
            "DELETE FROM",
            "TRUNCATE",
        ];
        for pattern in &require_approval_patterns {
            if cmd.contains(pattern) {
                return HookDecision::RequireUserApproval {
                    reason: format!(
                        "Command '{}' is high-risk and requires user approval.",
                        truncate_cmd(cmd)
                    ),
                };
            }
        }

        HookDecision::Allow
    }
}

/// Hook: Enforces skill-declared rules. Collects hook_rules from all loaded
/// skills and checks whether conditions match and required actions were performed.
#[derive(Clone)]
pub struct SkillRequiredHook {
    /// Skill hook rules to enforce (populated from loaded skills).
    pub rules: Vec<base::frozen::skill::SkillHookRule>,
}

impl SkillRequiredHook {
    /// Create a hook from a list of skill hook rules.
    pub fn new(rules: Vec<base::frozen::skill::SkillHookRule>) -> Self {
        Self { rules }
    }

    /// Evaluate a single rule against the policy context.
    fn evaluate_rule(
        rule: &base::frozen::skill::SkillHookRule,
        ctx: &PolicyContext,
    ) -> HookDecision {
        let condition = &rule.condition;
        let require = &rule.require;

        // Check if the condition matches
        let condition_matches = match &condition.changed_file_ext {
            Some(ext) => ctx.changed_files.iter().any(|f| f.ends_with(ext.as_str())),
            None => !ctx.changed_files.is_empty(), // match any file change
        };

        if !condition_matches {
            return HookDecision::Allow;
        }

        // Check if the required action was performed
        let mut missing_actions: Vec<String> = Vec::new();

        if let Some(ref required_cmd) = require.command_executed {
            let found = ctx
                .executed_commands
                .iter()
                .any(|c| c.contains(required_cmd.as_str()));
            if !found {
                missing_actions.push(format!("Run: `{required_cmd}`"));
            }
        }

        if let Some(ref required_pattern) = require.command_executed_matches {
            let found = ctx
                .executed_commands
                .iter()
                .any(|c| c.contains(required_pattern.as_str()));
            if !found {
                missing_actions.push(format!("Run a command matching: `{required_pattern}`"));
            }
        }

        if missing_actions.is_empty() {
            return HookDecision::Allow;
        }

        HookDecision::RequireRemediation {
            message: format!(
                "Skill rule '{}' requires: {}. Missing actions: {}",
                rule.id,
                rule.description.as_deref().unwrap_or("compliance check"),
                missing_actions.join(", ")
            ),
            required_actions: missing_actions,
        }
    }
}

impl PolicyHook for SkillRequiredHook {
    fn id(&self) -> &str {
        "skill-required"
    }
    fn hook_point(&self) -> PolicyHookPoint {
        PolicyHookPoint::BeforeTaskComplete
    }
    fn description(&self) -> &str {
        "Enforces skill-declared rules (e.g. cargo fmt after .rs changes)"
    }
    fn evaluate(&self, ctx: &PolicyContext) -> HookDecision {
        for rule in &self.rules {
            let decision = Self::evaluate_rule(rule, ctx);
            if decision.is_blocked() {
                return decision;
            }
        }
        HookDecision::Allow
    }
}

impl fmt::Debug for SkillRequiredHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SkillRequiredHook")
            .field("rules_count", &self.rules.len())
            .finish()
    }
}

fn truncate_cmd(cmd: &str) -> String {
    if cmd.len() <= 80 {
        cmd.to_string()
    } else {
        format!("{}…", &cmd[..80])
    }
}

// ═══════════════════════════════════════════════════════════
// PolicyHookRunner
// ═══════════════════════════════════════════════════════════

/// Runs policy hooks in registration order.
///
/// First Deny/Block stops evaluation — remaining hooks are skipped.
#[derive(Debug, Default)]
pub struct PolicyHookRunner {
    hooks: Vec<Box<dyn PolicyHook>>,
}

impl PolicyHookRunner {
    /// Create an empty runner.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a runner with all built-in hooks registered.
    pub fn builtin() -> Self {
        let mut runner = Self::new();
        runner.register(Box::new(CompletionVerificationHook));
        runner.register(Box::new(DiffSummaryHook));
        runner.register(Box::new(DangerousCommandHook));
        runner
    }

    /// Register a hook.
    pub fn register(&mut self, hook: Box<dyn PolicyHook>) {
        self.hooks.push(hook);
    }

    /// Run all hooks for a given hook point.
    ///
    /// Returns the first blocking decision, or Allow if all pass.
    pub fn evaluate(&self, point: PolicyHookPoint, ctx: &PolicyContext) -> HookDecision {
        for hook in &self.hooks {
            if hook.hook_point() == point {
                let decision = hook.evaluate(ctx);
                if decision.is_blocked() {
                    return decision;
                }
            }
        }
        HookDecision::Allow
    }

    /// Check if there are any hooks registered for a point.
    pub fn has_hooks_for(&self, point: PolicyHookPoint) -> bool {
        self.hooks.iter().any(|h| h.hook_point() == point)
    }

    /// Number of registered hooks.
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// Whether no hooks are registered.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }
}

// ═══════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_verification_blocks_when_no_verification() {
        let hook = CompletionVerificationHook;
        let ctx = PolicyContext {
            has_verification: false,
            tier_requires_verification: true,
            ..Default::default()
        };
        assert!(hook.evaluate(&ctx).is_blocked());
    }

    #[test]
    fn completion_verification_blocks_when_failed() {
        let hook = CompletionVerificationHook;
        let ctx = PolicyContext {
            has_verification: true,
            verification_passed: false,
            ..Default::default()
        };
        assert!(hook.evaluate(&ctx).is_blocked());
    }

    #[test]
    fn completion_verification_allows_when_no_tier_requirement() {
        // No verification done but tier doesn't require it → allow
        let hook = CompletionVerificationHook;
        let ctx = PolicyContext {
            has_verification: false,
            tier_requires_verification: false,
            ..Default::default()
        };
        assert!(hook.evaluate(&ctx).is_allowed());
    }

    #[test]
    fn completion_verification_allows_when_passed() {
        let hook = CompletionVerificationHook;
        let ctx = PolicyContext {
            has_verification: true,
            verification_passed: true,
            ..Default::default()
        };
        assert!(hook.evaluate(&ctx).is_allowed());
    }

    #[test]
    fn diff_summary_requires_remediation_when_files_changed() {
        let hook = DiffSummaryHook;
        let ctx = PolicyContext {
            changed_files: vec!["src/main.rs".into()],
            ..Default::default()
        };
        let decision = hook.evaluate(&ctx);
        assert!(decision.is_blocked());
        assert!(matches!(decision, HookDecision::RequireRemediation { .. }));
    }

    #[test]
    fn diff_summary_allows_when_no_changes() {
        let hook = DiffSummaryHook;
        let ctx = PolicyContext::default();
        assert!(hook.evaluate(&ctx).is_allowed());
    }

    #[test]
    fn dangerous_command_denies_rm_rf_root() {
        let hook = DangerousCommandHook;
        let ctx = PolicyContext {
            command: Some("rm -rf /".into()),
            ..Default::default()
        };
        let decision = hook.evaluate(&ctx);
        assert!(matches!(decision, HookDecision::Deny { .. }));
    }

    #[test]
    fn dangerous_command_requires_approval_for_git_push_force() {
        let hook = DangerousCommandHook;
        let ctx = PolicyContext {
            command: Some("git push --force origin main".into()),
            ..Default::default()
        };
        let decision = hook.evaluate(&ctx);
        assert!(matches!(decision, HookDecision::RequireUserApproval { .. }));
    }

    #[test]
    fn dangerous_command_allows_safe_commands() {
        let hook = DangerousCommandHook;
        let ctx = PolicyContext {
            command: Some("cargo test".into()),
            ..Default::default()
        };
        assert!(hook.evaluate(&ctx).is_allowed());
    }

    #[test]
    fn runner_stops_at_first_block() {
        let mut runner = PolicyHookRunner::new();
        runner.register(Box::new(CompletionVerificationHook));
        let ctx = PolicyContext {
            hook_point: Some(PolicyHookPoint::BeforeTaskComplete),
            has_verification: false,
            tier_requires_verification: true,
            ..Default::default()
        };
        let decision = runner.evaluate(PolicyHookPoint::BeforeTaskComplete, &ctx);
        assert!(matches!(decision, HookDecision::Deny { .. }));
    }

    #[test]
    fn runner_allows_when_no_hooks_match() {
        let runner = PolicyHookRunner::builtin();
        let ctx = PolicyContext::default();
        let decision = runner.evaluate(PolicyHookPoint::BeforeModelCall, &ctx);
        assert!(decision.is_allowed());
    }
}
