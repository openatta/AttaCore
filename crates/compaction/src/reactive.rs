//! Reactive compaction — proactive context compression before the budget is exhausted.
//!
//! TS parity: proactive compact in claude-code's turn loop. Instead of waiting
//! until the token budget is critically low, reactive compaction predicts when
//! compaction will be needed and fires early (typically a MicroCompact) to avoid
//! stalling the user with a last-minute big compaction.
//!
//! v2: Multi-level thresholds (auto/warn/error/block) and circuit breaker
//! (MAX_CONSECUTIVE_FAILURES = 3). TS parity: autoCompact.ts.

use crate::grouping::estimate_tokens;
use base::interface::model::ModelMessage;

/// Buffer tokens subtracted from the effective context window to compute
/// the auto-compaction threshold. TS parity: AUTOCOMPACT_BUFFER_TOKENS = 13_000.
const AUTOCOMPACT_BUFFER_TOKENS: usize = 13_000;
/// Additional buffer beyond auto threshold for warning level.
const WARNING_BUFFER_TOKENS: usize = 20_000;
/// Additional buffer beyond auto threshold for error level.
const ERROR_BUFFER_TOKENS: usize = 20_000;
/// Buffer from context window for blocking limit.
const BLOCK_BUFFER_TOKENS: usize = 3_000;
/// Maximum consecutive compaction failures before opening the circuit breaker.
const MAX_CONSECUTIVE_FAILURES: usize = 3;

/// Configuration for reactive (proactive) compaction.
#[derive(Debug, Clone)]
pub struct ReactiveCompactConfig {
    /// Whether reactive compaction is enabled.
    pub enabled: bool,
    /// Trigger reactive compact when remaining tokens falls below this value.
    /// Default: 50000.
    pub trigger_at_remaining_tokens: usize,
}

impl Default for ReactiveCompactConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger_at_remaining_tokens: 50_000,
        }
    }
}

/// Multi-level threshold state for compaction.
/// TS parity: calculateTokenWarningState in autoCompact.ts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionLevel {
    /// Token usage is well within budget.
    Normal,
    /// Token usage crossed the auto-compaction threshold (window - 13K).
    Auto,
    /// Token usage is high enough to warn the user (auto - 20K).
    Warning,
    /// Token usage is critically high (auto - 20K).
    Error,
    /// Token usage is at blocking level — no new API calls until compact.
    Blocking,
}

/// State tracking for compaction across turns.
/// TS parity: AutoCompactTrackingState in autoCompact.ts.
#[derive(Debug, Clone, Default)]
pub struct CompactionState {
    /// Number of consecutive compaction failures.
    pub consecutive_failures: usize,
    /// Turn number of the last compaction attempt.
    pub last_compact_turn: usize,
    /// Whether the circuit breaker is open (skip further compaction attempts).
    pub circuit_open: bool,
}

impl CompactionState {
    /// Record a successful compaction. Resets the failure counter and closes the circuit.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.circuit_open = false;
    }

    /// Record a failed compaction. Increments the failure counter; opens the circuit
    /// after MAX_CONSECUTIVE_FAILURES consecutive failures.
    pub fn record_failure(&mut self) {
        self.consecutive_failures += 1;
        if self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            self.circuit_open = true;
        }
    }
}

/// Calculate multi-level compaction thresholds from the effective context window.
/// TS parity: autoCompact.ts threshold constants.
///
/// Returns (auto, warn, error, block) — all as absolute token counts.
pub fn calculate_thresholds(context_window: usize) -> (usize, usize, usize, usize) {
    let auto = context_window.saturating_sub(AUTOCOMPACT_BUFFER_TOKENS);
    let warn = auto.saturating_sub(WARNING_BUFFER_TOKENS);
    let error = auto.saturating_sub(ERROR_BUFFER_TOKENS);
    let block = context_window.saturating_sub(BLOCK_BUFFER_TOKENS);
    (auto, warn, error, block)
}

/// Determine the current compaction level based on token usage.
/// Escalation order (TS parity): Normal → Warning → Auto → Blocking.
/// Warning and Error are the same buffer offset in TS, so they map to the same level.
pub fn compaction_level(current_tokens: usize, context_window: usize) -> CompactionLevel {
    let (auto, warn, _error, block) = calculate_thresholds(context_window);
    // Check from highest to lowest: Blocking > Auto > Warning/Error > Normal
    if current_tokens >= block {
        CompactionLevel::Blocking
    } else if current_tokens >= auto {
        CompactionLevel::Auto
    } else if current_tokens >= warn {
        // Both Warning and Error use the same 20K buffer offset from auto threshold
        CompactionLevel::Warning
    } else {
        CompactionLevel::Normal
    }
}

/// Determine whether reactive compaction should be triggered.
///
/// Returns `true` when the current token usage has consumed enough of the
/// context window that proactive compaction would be beneficial before the
/// next API call. Also checks the circuit breaker.
///
/// `current_tokens` — estimated tokens currently consumed.
/// `context_limit` — effective context window (e.g. 200_000 for most models).
/// `token_usage_velocity` — optional rate of token growth per API call
///   (used to predict when the budget would be exhausted). When `None`,
///   uses a simpler threshold-only check.
/// `state` — optional compaction state for circuit breaker check.
pub fn should_compact_reactively(
    current_tokens: usize,
    context_limit: usize,
    token_usage_velocity: Option<f64>,
) -> bool {
    let trigger = 50_000; // default trigger_at_remaining_tokens
    let remaining = context_limit.saturating_sub(current_tokens);

    // If already below the trigger threshold, compact now.
    if remaining <= trigger {
        return true;
    }

    // If we have velocity data, predict when we'd hit the limit.
    if let Some(velocity) = token_usage_velocity {
        if velocity > 0.0 {
            let predicted_turns_to_exhaustion = remaining as f64 / velocity;
            // If we'll exhaust budget within 3 API calls, compact proactively.
            if predicted_turns_to_exhaustion <= 3.0 {
                return true;
            }
        }
    }

    false
}

/// Determine whether compaction should proceed given the circuit breaker state.
/// Returns false if the circuit is open.
pub fn should_compact_with_state(
    current_tokens: usize,
    context_limit: usize,
    token_usage_velocity: Option<f64>,
    state: &CompactionState,
) -> bool {
    if state.circuit_open {
        return false;
    }
    should_compact_reactively(current_tokens, context_limit, token_usage_velocity)
}

/// Pre-compute the token usage velocity from messages.
/// Rough estimate: rate of token growth per API round.
pub fn estimate_token_velocity(messages: &[ModelMessage]) -> Option<f64> {
    if messages.len() < 4 {
        return None;
    }
    let total = estimate_tokens(messages);
    let per_message = total as f64 / messages.len() as f64;
    Some(per_message * 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_when_remaining_below_threshold() {
        assert!(should_compact_reactively(180_000, 200_000, None));
        assert!(!should_compact_reactively(100_000, 200_000, None));
    }

    #[test]
    fn triggers_early_with_high_velocity() {
        assert!(should_compact_reactively(100_000, 200_000, Some(50_000.0)));
    }

    #[test]
    fn does_not_trigger_with_low_velocity() {
        assert!(!should_compact_reactively(100_000, 200_000, Some(10_000.0)));
    }

    #[test]
    fn default_config_is_enabled() {
        let config = ReactiveCompactConfig::default();
        assert!(config.enabled);
        assert_eq!(config.trigger_at_remaining_tokens, 50_000);
    }

    #[test]
    fn calculate_thresholds_for_200k() {
        let (auto, warn, error, block) = calculate_thresholds(200_000);
        assert_eq!(auto, 187_000); // 200K - 13K
        assert_eq!(warn, 167_000); // auto - 20K
        assert_eq!(error, 167_000); // auto - 20K (same as warn)
        assert_eq!(block, 197_000); // 200K - 3K
    }

    #[test]
    fn compaction_level_escalates() {
        // Thresholds for 200K window: auto=187K, warn=167K, block=197K
        assert_eq!(compaction_level(100_000, 200_000), CompactionLevel::Normal); // < 167K
        assert_eq!(compaction_level(170_000, 200_000), CompactionLevel::Warning); // >= 167K, < 187K
        assert_eq!(compaction_level(190_000, 200_000), CompactionLevel::Auto); // >= 187K, < 197K
        assert_eq!(
            compaction_level(197_001, 200_000),
            CompactionLevel::Blocking
        ); // >= 197K
    }

    #[test]
    fn circuit_breaker_opens_after_3_failures() {
        let mut state = CompactionState::default();
        assert!(!state.circuit_open);
        state.record_failure();
        state.record_failure();
        assert!(!state.circuit_open);
        state.record_failure();
        assert!(state.circuit_open);
    }

    #[test]
    fn success_resets_circuit() {
        let mut state = CompactionState::default();
        state.record_failure();
        state.record_failure();
        state.record_success();
        assert_eq!(state.consecutive_failures, 0);
        assert!(!state.circuit_open);
    }

    #[test]
    fn should_compact_respects_circuit() {
        let mut state = CompactionState::default();
        state.record_failure();
        state.record_failure();
        state.record_failure(); // circuit open
                                // Even though below threshold, circuit blocks
        assert!(!should_compact_with_state(180_000, 200_000, None, &state));
    }
}
