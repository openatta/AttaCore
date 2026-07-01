//! Model tier escalation — decides whether to upgrade from the default
//! tier based on task risk, user signals, and runtime feedback.
//!
//! # Architecture
//!
//! Three-layer decision:
//! 1. **Force Rules** — unconditional upgrade (security, crash, retry)
//! 2. **Risk Scoring** — weighted signals, threshold-gated upgrade
//! 3. **Runtime Feedback** — previous-turn failure escalates
//!
//! The evaluator consumes `CodingSignals` (extracted from user message +
//! turn context), not raw strings. This keeps the scoring logic clean
//! and allows the signal extraction to evolve independently.

use crate::coding::task::CodingTaskKind;
use crate::coding::tier::{ModelTier, TierRuntimePolicy};

// ═══════════════════════════════════════════════════════════
// CodingSignals
// ═══════════════════════════════════════════════════════════

/// Structured signals extracted from user message and turn context.
///
/// These are the inputs to the RiskEvaluator. How they are extracted
/// (keyword, AST, LSP, LLM classifier) is the responsibility of
/// `SignalExtractor`, not the evaluator.
#[derive(Debug, Clone, Default)]
pub struct CodingSignals {
    pub has_error_log: bool,
    pub has_stack_trace: bool,
    pub touches_security: bool,
    pub touches_public_api: bool,
    pub touches_concurrency: bool,
    pub touches_deps: bool,
    pub large_context: bool,
    pub production_grade_request: bool,
    pub multi_file_change: bool,
}

/// Trait for extracting CodingSignals from a user message.
///
/// Phase 1 implementation: keyword heuristics.
/// Future: AST/LSP analysis, diff analysis, LLM classifier.
pub trait SignalExtractor: Send + Sync {
    fn extract(&self, user_message: &str) -> CodingSignals;
}

/// Keyword-based signal extractor.
#[derive(Debug, Clone, Default)]
pub struct KeywordSignalExtractor;

impl SignalExtractor for KeywordSignalExtractor {
    fn extract(&self, user_message: &str) -> CodingSignals {
        let msg = user_message.trim();
        if msg.is_empty() {
            return CodingSignals::default();
        }
        let pl = msg.to_lowercase();

        CodingSignals {
            has_error_log: check_error_log(&pl),
            has_stack_trace: check_stack_trace(&pl),
            touches_security: check_security(&pl),
            touches_public_api: check_public_api(&pl),
            touches_concurrency: check_concurrency(&pl),
            touches_deps: check_deps(&pl),
            large_context: check_large_context(&pl),
            production_grade_request: check_production_grade(&pl),
            multi_file_change: check_multi_file(&pl),
        }
    }
}

// ═══════════════════════════════════════════════════════════
// Signal detection helpers (keyword-based, bilingual)
// ═══════════════════════════════════════════════════════════

fn check_error_log(pl: &str) -> bool {
    let markers = [
        "error:",
        "error ",
        "failed",
        "failure",
        "exception",
        "panic",
        "错误",
        "失败",
        "异常",
        "报错",
    ];
    markers.iter().any(|m| pl.contains(m))
}

fn check_stack_trace(pl: &str) -> bool {
    let markers = [
        "stack trace",
        "backtrace",
        "panicked at",
        "sigsegv",
        "segfault",
        "traceback",
        "coredump",
        "signal:",
    ];
    markers.iter().any(|m| pl.contains(m))
}

fn check_security(pl: &str) -> bool {
    let markers = [
        "auth",
        "login",
        "logout",
        "token",
        "jwt",
        "oauth",
        "encrypt",
        "decrypt",
        "password",
        "secret",
        "credential",
        "sql injection",
        "xss",
        "csrf",
        "rce",
        "path traversal",
        "permission",
        "role",
        "access control",
        "sandbox",
        "认证",
        "登录",
        "密码",
        "加密",
        "权限",
        "安全",
    ];
    markers.iter().any(|m| pl.contains(m))
}

fn check_public_api(pl: &str) -> bool {
    let markers = [
        "public api",
        "pub fn",
        "breaking change",
        "interface",
        "api break",
        "deprecated",
        "backward compat",
        "公开接口",
        "对外 api",
        "破坏性变更",
    ];
    markers.iter().any(|m| pl.contains(m))
}

fn check_concurrency(pl: &str) -> bool {
    let markers = [
        "concurrent",
        "race condition",
        "deadlock",
        "livelock",
        "async",
        "await",
        "tokio",
        "goroutine",
        "thread",
        "mutex",
        "rwlock",
        "lock",
        "atomic",
        "unsafe",
        "raw pointer",
        "lifetime",
        "borrow",
        "memory leak",
        "use after free",
        "double free",
        "并发",
        "死锁",
        "内存泄漏",
        "生命周期",
    ];
    markers.iter().any(|m| pl.contains(m))
}

fn check_deps(pl: &str) -> bool {
    let markers = [
        "dependency",
        "dependencies",
        "cargo.toml",
        "package.json",
        "go.mod",
        "requirements.txt",
        "gemfile",
        "lockfile",
        "cve",
        "license",
        "breaking change",
        "version conflict",
        "npm install",
        "cargo update",
        "pip install",
        "upgrade",
        "bump version",
        "deprecat",
        "依赖",
        "升级",
        "版本",
    ];
    markers.iter().any(|m| pl.contains(m))
}

fn check_large_context(pl: &str) -> bool {
    let markers = [
        "across files",
        "multiple modules",
        "整个项目",
        "全局",
        "all files",
        "every file",
        "整个模块",
    ];
    markers.iter().any(|m| pl.contains(m))
}

fn check_production_grade(pl: &str) -> bool {
    let markers = [
        "production",
        "生产级",
        "不要出错",
        "完整修复",
        "彻底",
        "高可靠",
        "仔细",
        "严谨",
        "thorough",
        "careful",
        "robust",
        "reliable",
    ];
    markers.iter().any(|m| pl.contains(m))
}

fn check_multi_file(pl: &str) -> bool {
    let markers = [
        "multiple files",
        "several files",
        "across modules",
        "跨文件",
        "多个文件",
        "多个模块",
        "多处",
        "all callers",
        "all usages",
        "every reference",
    ];
    markers.iter().any(|m| pl.contains(m))
}

// ═══════════════════════════════════════════════════════════
// EscalationInput
// ═══════════════════════════════════════════════════════════

/// Input to the escalation evaluator — combines static signals
/// from the user message with runtime feedback from previous turns.
#[derive(Debug, Clone)]
pub struct EscalationInput {
    pub base_tier: ModelTier,
    pub task_kind: CodingTaskKind,
    pub signals: CodingSignals,
    /// Runtime feedback signals (from previous turn trace).
    pub previous_attempt_failed: bool,
    pub last_verification_failed: bool,
    pub hook_rejected_count: u32,
    pub changed_files_estimate: usize,
}

// ═══════════════════════════════════════════════════════════
// TierDecision
// ═══════════════════════════════════════════════════════════

/// The result of escalation evaluation.
#[derive(Debug, Clone)]
pub struct TierDecision {
    pub base_tier: ModelTier,
    pub final_tier: ModelTier,
    pub score: u32,
    pub reasons: Vec<String>,
    /// If set, a force rule triggered the upgrade (takes precedence over scoring).
    pub force_reason: Option<String>,
    /// The runtime policy to apply for this tier.
    pub runtime_policy: TierRuntimePolicy,
}

impl TierDecision {
    /// Whether the tier was upgraded from the base.
    pub fn was_upgraded(&self) -> bool {
        self.final_tier > self.base_tier
    }

    /// Short summary for logging / trace display.
    pub fn summary(&self) -> String {
        if !self.was_upgraded() {
            return format!("{} (base, score={})", self.final_tier.as_str(), self.score);
        }
        let reason = self.force_reason.as_deref().unwrap_or("scoring threshold");
        format!(
            "{} → {} ({}: {}), score={}",
            self.base_tier.as_str(),
            self.final_tier.as_str(),
            if self.force_reason.is_some() {
                "force"
            } else {
                "score"
            },
            reason,
            self.score,
        )
    }
}

// ═══════════════════════════════════════════════════════════
// EscalationConfig
// ═══════════════════════════════════════════════════════════

/// Configuration for the escalation system.
#[derive(Debug, Clone)]
pub struct EscalationConfig {
    /// Master switch. Default: true.
    pub enabled: bool,
    /// Whether force rules can override explicit user model_profile settings.
    pub allow_force_override: bool,

    // Thresholds
    pub lite_to_normal_score: u32,
    pub lite_to_strong_score: u32,
    pub normal_to_strong_score: u32,
    /// Extra: normal → strong when score >= this AND changed_files >= 3.
    pub normal_multi_file_score: u32,

    // Weights
    pub weight_has_error_log: u32,
    pub weight_has_stack_trace: u32,
    pub weight_previous_attempt_failed: u32,
    pub weight_last_verification_failed: u32,
    pub weight_touches_security: u32,
    pub weight_touches_public_api: u32,
    pub weight_touches_concurrency: u32,
    pub weight_multi_file_change: u32,
    pub weight_touches_deps: u32,
    pub weight_hook_rejected: u32,
    pub weight_large_context: u32,
    pub weight_production_grade_request: u32,
}

impl Default for EscalationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_force_override: true,
            lite_to_normal_score: 2,
            lite_to_strong_score: 5,
            normal_to_strong_score: 4,
            normal_multi_file_score: 3,
            weight_has_error_log: 3,
            weight_has_stack_trace: 3,
            weight_previous_attempt_failed: 3,
            weight_last_verification_failed: 3,
            weight_touches_security: 3,
            weight_touches_public_api: 2,
            weight_touches_concurrency: 2,
            weight_multi_file_change: 2,
            weight_touches_deps: 2,
            weight_hook_rejected: 2,
            weight_large_context: 1,
            weight_production_grade_request: 1,
        }
    }
}

// ═══════════════════════════════════════════════════════════
// RiskEvaluator
// ═══════════════════════════════════════════════════════════

/// The escalation decision engine.
///
/// Evaluates `EscalationInput` → `TierDecision` using the three-layer
/// rule system.
#[derive(Debug, Clone)]
pub struct RiskEvaluator {
    config: EscalationConfig,
}

impl RiskEvaluator {
    pub fn new(config: EscalationConfig) -> Self {
        Self { config }
    }

    /// Evaluate escalation for a given input.
    pub fn evaluate(&self, input: &EscalationInput) -> TierDecision {
        if !self.config.enabled {
            return self.no_escalation(input);
        }

        // ── Layer 1: Force Rules ──
        if let Some(reason) = self.check_force_rules(input) {
            return TierDecision {
                base_tier: input.base_tier,
                final_tier: ModelTier::Strong,
                score: 0, // score is irrelevant when force rule triggers
                reasons: vec![reason.clone()],
                force_reason: Some(reason),
                runtime_policy: TierRuntimePolicy::for_tier(ModelTier::Strong),
            };
        }

        // ── Layer 2: Risk Scoring ──
        let (score, reasons) = self.compute_score(input);
        let final_tier = self.apply_threshold(input.base_tier, score, input.changed_files_estimate);

        TierDecision {
            base_tier: input.base_tier,
            final_tier,
            score,
            reasons,
            force_reason: None,
            runtime_policy: TierRuntimePolicy::for_tier(final_tier),
        }
    }

    /// Layer 1: Check if any force rule triggers an unconditional upgrade.
    fn check_force_rules(&self, input: &EscalationInput) -> Option<String> {
        // F1: Task kind is already strong-tier by default
        if matches!(
            input.task_kind,
            CodingTaskKind::Debug | CodingTaskKind::Refactor | CodingTaskKind::Perf
        ) {
            return Some("task requires strong tier by default".into());
        }

        // F2: Stack trace / crash evidence
        if input.signals.has_stack_trace {
            return Some("stack trace or crash evidence detected".into());
        }

        // F3: Security-sensitive change
        if input.signals.touches_security {
            return Some("security-sensitive change detected".into());
        }

        // F4: Concurrency / unsafe code
        if input.signals.touches_concurrency {
            return Some("concurrency or unsafe code detected".into());
        }

        // F5: Normal-tier attempt previously failed
        if input.previous_attempt_failed && input.base_tier == ModelTier::Normal {
            return Some("previous normal-tier attempt failed, escalating".into());
        }

        // F6: Last verification failed
        if input.last_verification_failed {
            return Some("last verification failed, escalating".into());
        }

        // F7: Hook rejected >= 2 times
        if input.hook_rejected_count >= 2 {
            return Some(format!(
                "hook rejected {} times, escalating",
                input.hook_rejected_count
            ));
        }

        None
    }

    /// Layer 2: Compute weighted risk score.
    fn compute_score(&self, input: &EscalationInput) -> (u32, Vec<String>) {
        let mut score = 0u32;
        let mut reasons: Vec<String> = Vec::new();
        let s = &input.signals;
        let cfg = &self.config;

        let mut add = |value: bool, weight: u32, label: &str| {
            if value {
                score += weight;
                reasons.push(label.into());
            }
        };

        add(s.has_error_log, cfg.weight_has_error_log, "error log");
        add(s.has_stack_trace, cfg.weight_has_stack_trace, "stack trace");
        add(
            s.touches_security,
            cfg.weight_touches_security,
            "security-sensitive",
        );
        add(
            s.touches_public_api,
            cfg.weight_touches_public_api,
            "public API change",
        );
        add(
            s.touches_concurrency,
            cfg.weight_touches_concurrency,
            "concurrency/unsafe",
        );
        add(
            s.multi_file_change,
            cfg.weight_multi_file_change,
            "multi-file change",
        );
        add(s.touches_deps, cfg.weight_touches_deps, "dependency change");
        add(s.large_context, cfg.weight_large_context, "large context");
        add(
            s.production_grade_request,
            cfg.weight_production_grade_request,
            "production-grade request",
        );

        // Runtime feedback signals (not from CodingSignals)
        add(
            input.previous_attempt_failed,
            cfg.weight_previous_attempt_failed,
            "previous attempt failed",
        );
        add(
            input.last_verification_failed,
            cfg.weight_last_verification_failed,
            "last verification failed",
        );
        if input.hook_rejected_count >= 1 {
            score += cfg.weight_hook_rejected;
            reasons.push(format!(
                "hook rejected ({} times)",
                input.hook_rejected_count
            ));
        }

        (score, reasons)
    }

    /// Apply tier thresholds to determine the final tier.
    fn apply_threshold(&self, base_tier: ModelTier, score: u32, changed_files: usize) -> ModelTier {
        let cfg = &self.config;
        match base_tier {
            ModelTier::Lite => {
                if score >= cfg.lite_to_strong_score {
                    ModelTier::Strong
                } else if score >= cfg.lite_to_normal_score {
                    ModelTier::Normal
                } else {
                    ModelTier::Lite
                }
            }
            ModelTier::Normal => {
                if score >= cfg.normal_to_strong_score {
                    ModelTier::Strong
                } else if score >= cfg.normal_multi_file_score && changed_files >= 3 {
                    ModelTier::Strong
                } else {
                    ModelTier::Normal
                }
            }
            ModelTier::Strong => ModelTier::Strong, // never downgrade
        }
    }

    /// Return a decision that keeps the base tier (escalation disabled).
    fn no_escalation(&self, input: &EscalationInput) -> TierDecision {
        TierDecision {
            base_tier: input.base_tier,
            final_tier: input.base_tier,
            score: 0,
            reasons: vec!["escalation disabled".into()],
            force_reason: None,
            runtime_policy: TierRuntimePolicy::for_tier(input.base_tier),
        }
    }

    /// Access the config (read-only).
    pub fn config(&self) -> &EscalationConfig {
        &self.config
    }
}

impl Default for RiskEvaluator {
    fn default() -> Self {
        Self::new(EscalationConfig::default())
    }
}

// ═══════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn input(base_tier: ModelTier, kind: CodingTaskKind) -> EscalationInput {
        EscalationInput {
            base_tier,
            task_kind: kind,
            signals: CodingSignals::default(),
            previous_attempt_failed: false,
            last_verification_failed: false,
            hook_rejected_count: 0,
            changed_files_estimate: 0,
        }
    }

    #[test]
    fn default_no_escalation() {
        let evaluator = RiskEvaluator::default();
        let inp = input(ModelTier::Normal, CodingTaskKind::Modify);
        let decision = evaluator.evaluate(&inp);
        assert_eq!(decision.final_tier, ModelTier::Normal);
        assert!(!decision.was_upgraded());
    }

    #[test]
    fn force_rule_stack_trace() {
        let evaluator = RiskEvaluator::default();
        let inp = EscalationInput {
            signals: CodingSignals {
                has_stack_trace: true,
                ..Default::default()
            },
            ..input(ModelTier::Normal, CodingTaskKind::Modify)
        };
        let decision = evaluator.evaluate(&inp);
        assert_eq!(decision.final_tier, ModelTier::Strong);
        assert!(decision.force_reason.is_some());
        assert!(decision.reasons[0].contains("stack trace"));
    }

    #[test]
    fn force_rule_security() {
        let evaluator = RiskEvaluator::default();
        let inp = EscalationInput {
            signals: CodingSignals {
                touches_security: true,
                ..Default::default()
            },
            ..input(ModelTier::Normal, CodingTaskKind::Modify)
        };
        let decision = evaluator.evaluate(&inp);
        assert_eq!(decision.final_tier, ModelTier::Strong);
    }

    #[test]
    fn force_rule_previous_failure() {
        let evaluator = RiskEvaluator::default();
        let inp = EscalationInput {
            previous_attempt_failed: true,
            ..input(ModelTier::Normal, CodingTaskKind::Modify)
        };
        let decision = evaluator.evaluate(&inp);
        assert_eq!(decision.final_tier, ModelTier::Strong);
    }

    #[test]
    fn force_rule_debug_already_strong() {
        let evaluator = RiskEvaluator::default();
        let inp = input(ModelTier::Strong, CodingTaskKind::Debug);
        let decision = evaluator.evaluate(&inp);
        assert_eq!(decision.final_tier, ModelTier::Strong);
        // F1 matched: "task requires strong tier by default"
        assert!(decision.force_reason.is_some());
    }

    #[test]
    fn scoring_normal_to_strong() {
        let evaluator = RiskEvaluator::default();
        let inp = EscalationInput {
            signals: CodingSignals {
                has_error_log: true,      // +3
                touches_public_api: true, // +2 → total 5 >= 4 → strong
                ..Default::default()
            },
            ..input(ModelTier::Normal, CodingTaskKind::Modify)
        };
        let decision = evaluator.evaluate(&inp);
        assert_eq!(decision.final_tier, ModelTier::Strong);
        assert!(decision.force_reason.is_none());
        assert!(decision.score >= 4);
    }

    #[test]
    fn scoring_normal_stays_normal() {
        let evaluator = RiskEvaluator::default();
        let inp = EscalationInput {
            signals: CodingSignals {
                large_context: true,            // +1
                production_grade_request: true, // +1 → total 2 < 4 → stays normal
                ..Default::default()
            },
            ..input(ModelTier::Normal, CodingTaskKind::Modify)
        };
        let decision = evaluator.evaluate(&inp);
        assert_eq!(decision.final_tier, ModelTier::Normal);
    }

    #[test]
    fn scoring_lite_to_normal() {
        let evaluator = RiskEvaluator::default();
        let inp = EscalationInput {
            signals: CodingSignals {
                large_context: true,            // +1
                production_grade_request: true, // +1 → total 2 >= 2 → normal
                ..Default::default()
            },
            ..input(ModelTier::Lite, CodingTaskKind::Explain)
        };
        let decision = evaluator.evaluate(&inp);
        assert_eq!(decision.final_tier, ModelTier::Normal);
    }

    #[test]
    fn scoring_multi_file_normal_to_strong() {
        let evaluator = RiskEvaluator::default();
        let inp = EscalationInput {
            signals: CodingSignals {
                multi_file_change: true, // +2
                has_error_log: true,     // +3 → total 5 ≥ 4 → strong
                ..Default::default()
            },
            changed_files_estimate: 3,
            ..input(ModelTier::Normal, CodingTaskKind::Modify)
        };
        let decision = evaluator.evaluate(&inp);
        assert_eq!(decision.final_tier, ModelTier::Strong);
    }

    #[test]
    fn escalation_disabled() {
        let mut config = EscalationConfig::default();
        config.enabled = false;
        let evaluator = RiskEvaluator::new(config);
        let inp = EscalationInput {
            signals: CodingSignals {
                has_stack_trace: true,
                ..Default::default()
            },
            ..input(ModelTier::Normal, CodingTaskKind::Modify)
        };
        let decision = evaluator.evaluate(&inp);
        assert_eq!(decision.final_tier, ModelTier::Normal);
        assert!(!decision.was_upgraded());
    }

    #[test]
    fn strong_never_downgrades() {
        let evaluator = RiskEvaluator::default();
        let inp = input(ModelTier::Strong, CodingTaskKind::Debug);
        let decision = evaluator.evaluate(&inp);
        assert_eq!(decision.final_tier, ModelTier::Strong);
    }

    #[test]
    fn signal_extractor_detects_security() {
        let extractor = KeywordSignalExtractor;
        let signals =
            extractor.extract("fix the authentication token validation error: 401 Unauthorized");
        assert!(signals.touches_security);
        assert!(signals.has_error_log); // "error"
    }

    #[test]
    fn signal_extractor_detects_stack_trace() {
        let extractor = KeywordSignalExtractor;
        let signals = extractor.extract("the app panicked at src/main.rs:42\nstack trace:\n...");
        assert!(signals.has_stack_trace);
        assert!(signals.has_error_log);
    }

    #[test]
    fn signal_extractor_detects_concurrency() {
        let extractor = KeywordSignalExtractor;
        let signals = extractor.extract("there is a deadlock in the async mutex code");
        assert!(signals.touches_concurrency);
    }

    #[test]
    fn signal_extractor_empty_message() {
        let extractor = KeywordSignalExtractor;
        let signals = extractor.extract("");
        assert!(!signals.has_error_log);
        assert!(!signals.touches_security);
    }

    #[test]
    fn tier_decision_summary() {
        let d = TierDecision {
            base_tier: ModelTier::Normal,
            final_tier: ModelTier::Strong,
            score: 5,
            reasons: vec!["security-sensitive".into()],
            force_reason: Some("security-sensitive change detected".into()),
            runtime_policy: TierRuntimePolicy::for_tier(ModelTier::Strong),
        };
        let s = d.summary();
        assert!(s.contains("normal → strong"));
        assert!(s.contains("force"));
    }
}
