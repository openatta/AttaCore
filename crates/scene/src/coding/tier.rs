//! Model tier and runtime policy — defines the three execution tiers
//! and the strategy changes that accompany a tier upgrade.
//!
//! A tier upgrade is not just a model name change — it also activates
//! stricter verification, planning, and repair requirements.

use crate::coding::task::CodingTaskKind;

// ═══════════════════════════════════════════════════════════
// ModelTier
// ═══════════════════════════════════════════════════════════

/// Three execution tiers for coding tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ModelTier {
    /// Fastest, cheapest — read-only or trivial tasks.
    Lite = 0,
    /// Balanced — general coding tasks.
    Normal = 1,
    /// Most capable — complex, high-risk, or failed-retry tasks.
    Strong = 2,
}

impl ModelTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelTier::Lite => "lite",
            ModelTier::Normal => "normal",
            ModelTier::Strong => "strong",
        }
    }

    /// Default tier for a given task kind.
    pub fn default_for(kind: CodingTaskKind) -> Self {
        match kind {
            CodingTaskKind::Explain => ModelTier::Lite,
            CodingTaskKind::Search => ModelTier::Lite,
            CodingTaskKind::Document => ModelTier::Lite,
            CodingTaskKind::Generate => ModelTier::Normal,
            CodingTaskKind::Modify => ModelTier::Normal,
            CodingTaskKind::Review => ModelTier::Normal,
            CodingTaskKind::Plan => ModelTier::Normal,
            CodingTaskKind::Test => ModelTier::Normal,
            CodingTaskKind::Deps => ModelTier::Normal,
            CodingTaskKind::Debug => ModelTier::Strong,
            CodingTaskKind::Refactor => ModelTier::Strong,
            CodingTaskKind::Perf => ModelTier::Strong,
        }
    }
}

// ═══════════════════════════════════════════════════════════
// TierRuntimePolicy
// ═══════════════════════════════════════════════════════════

/// Execution strategy bound to a model tier.
///
/// When a task is upgraded from normal → strong, the runtime policy
/// is also switched — enabling plan requirements, verification, and
/// more repair iterations.
#[derive(Debug, Clone)]
pub struct TierRuntimePolicy {
    /// Maximum context tokens for this tier.
    pub max_context_tokens: usize,
    /// Whether a plan is required before editing.
    pub require_plan: bool,
    /// Whether a self-review is required after editing.
    pub require_review: bool,
    /// Whether verification (tests/build) is required.
    pub require_verification: bool,
    /// Maximum repair iterations before blocking.
    pub max_repair_iterations: u32,
}

impl TierRuntimePolicy {
    /// Built-in policy for each tier.
    pub fn for_tier(tier: ModelTier) -> Self {
        match tier {
            ModelTier::Lite => Self {
                max_context_tokens: 16_000,
                require_plan: false,
                require_review: false,
                require_verification: false,
                max_repair_iterations: 0,
            },
            ModelTier::Normal => Self {
                max_context_tokens: 64_000,
                require_plan: false,
                require_review: false,
                require_verification: false,
                max_repair_iterations: 1,
            },
            ModelTier::Strong => Self {
                max_context_tokens: 160_000,
                require_plan: true,
                require_review: true,
                require_verification: true,
                max_repair_iterations: 3,
            },
        }
    }
}

// ═══════════════════════════════════════════════════════════
// CompletionRequirements
// ═══════════════════════════════════════════════════════════

/// What must be satisfied before a task can be marked complete.
///
/// Derived from TaskProfile + TierRuntimePolicy. Consumed by
/// CompletionVerificationHook and the Agent's turn-completion logic.
#[derive(Debug, Clone, Default)]
pub struct CompletionRequirements {
    pub require_plan: bool,
    pub require_review: bool,
    pub require_verification: bool,
    pub require_diff_summary: bool,
}

impl CompletionRequirements {
    /// Build from a task profile's policies and the active tier policy.
    pub fn from_policies(task_verification: &str, tier_policy: &TierRuntimePolicy) -> Self {
        let task_requires_verification =
            task_verification == "required" || task_verification == "suggested";
        Self {
            require_plan: tier_policy.require_plan,
            require_review: tier_policy.require_review,
            require_verification: tier_policy.require_verification || task_requires_verification,
            require_diff_summary: tier_policy.require_review,
        }
    }
}

// ═══════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tiers_match_design() {
        assert_eq!(
            ModelTier::default_for(CodingTaskKind::Explain),
            ModelTier::Lite
        );
        assert_eq!(
            ModelTier::default_for(CodingTaskKind::Search),
            ModelTier::Lite
        );
        assert_eq!(
            ModelTier::default_for(CodingTaskKind::Document),
            ModelTier::Lite
        );
        assert_eq!(
            ModelTier::default_for(CodingTaskKind::Modify),
            ModelTier::Normal
        );
        assert_eq!(
            ModelTier::default_for(CodingTaskKind::Debug),
            ModelTier::Strong
        );
        assert_eq!(
            ModelTier::default_for(CodingTaskKind::Refactor),
            ModelTier::Strong
        );
        assert_eq!(
            ModelTier::default_for(CodingTaskKind::Perf),
            ModelTier::Strong
        );
        assert_eq!(
            ModelTier::default_for(CodingTaskKind::Test),
            ModelTier::Normal
        );
        assert_eq!(
            ModelTier::default_for(CodingTaskKind::Deps),
            ModelTier::Normal
        );
    }

    #[test]
    fn tier_ordering() {
        assert!(ModelTier::Strong > ModelTier::Normal);
        assert!(ModelTier::Normal > ModelTier::Lite);
    }

    #[test]
    fn strong_policy_requires_verification() {
        let policy = TierRuntimePolicy::for_tier(ModelTier::Strong);
        assert!(policy.require_verification);
        assert!(policy.require_plan);
        assert_eq!(policy.max_repair_iterations, 3);
    }

    #[test]
    fn lite_policy_is_lenient() {
        let policy = TierRuntimePolicy::for_tier(ModelTier::Lite);
        assert!(!policy.require_verification);
        assert_eq!(policy.max_repair_iterations, 0);
    }

    #[test]
    fn completion_requirements_merge_task_and_tier() {
        let tier = TierRuntimePolicy::for_tier(ModelTier::Strong);
        let req = CompletionRequirements::from_policies("required", &tier);
        assert!(req.require_verification);
        assert!(req.require_plan);
        assert!(req.require_review);
    }
}
