//! `BudgetPolicy` — what a turn is allowed to spend, and how much context it
//! may carry.
//!
//! Three judgements, all of them constants before this: a cumulative token
//! ceiling read straight off settings, an output-volume target with three
//! magic numbers deciding when to stop asking for more, and a compaction
//! threshold the scene returned as a pair of numbers with no ceiling above
//! them. The last one is the gap that matters: `TokenBudget` said when to
//! start compacting and never said how large a context was too large, so
//! there was no configuration that could answer "never send a request bigger
//! than this".
//!
//! # Two ceilings that look alike and are not
//!
//! `Spending` is about the bill: input plus output, accumulated across the
//! turn, checked against what the provider actually reported. `OutputTarget`
//! is about the opposite problem — the model stopping short of the volume of
//! work that was asked for. One ends a turn that has spent too much, the
//! other prolongs a turn that has produced too little. They stay separate
//! methods because merging them would let "write more" and "you have spent
//! enough" argue with each other inside one verdict.
//!
//! `ContextBudget` is neither: it is about the size of a single request, not
//! the cost of the turn.

use crate::interface::scene::TokenBudget;

/// What the turn has spent so far, as the provider reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spend {
    /// Input plus output tokens across every call in this turn.
    pub total_tokens: u64,
}

/// Whether the turn may keep spending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spending {
    WithinBudget,
    /// Close enough to say so. The turn continues.
    Warn {
        reminder: String,
        /// The ceiling this was measured against, for the record the engine
        /// keeps of budget enforcement.
        limit: u64,
    },
    /// The turn ends, reporting `budget_exceeded`.
    Exhausted { limit: u64 },
}

/// Progress toward an output-volume target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputProgress {
    /// Output tokens accumulated across the turn.
    pub accumulated: u64,
    pub target: u64,
    /// How many times the model has already been asked to keep going.
    pub continuations: u32,
    /// Output tokens from the call that just returned.
    pub this_delta: u64,
    /// Output tokens from the call before it.
    pub last_delta: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputTarget {
    /// Enough, or not worth asking again. The turn ends normally.
    Reached,
    /// Say this and go around again.
    KeepGoing { nudge: String },
}

/// How large a single request's context may get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    /// Compact once the context passes this. `0` disables compaction.
    pub compact_threshold: usize,
    /// Rounds compaction keeps.
    pub compact_keep_recent: usize,
    /// A ceiling compaction is not allowed to leave the context above. `None`
    /// is what the engine has always done — the threshold said when to start
    /// compacting and nothing said when to stop trying.
    pub hard_cap: Option<usize>,
}

/// What a turn may spend and how much it may carry.
pub trait BudgetPolicy: Send + Sync {
    /// After a model call, against the usage the provider reported.
    fn on_usage(&self, _spend: &Spend) -> Spending {
        Spending::WithinBudget
    }

    /// When the model has stopped and a volume of output was asked for.
    fn on_output_target(&self, _progress: &OutputProgress) -> OutputTarget {
        OutputTarget::Reached
    }

    /// What the scene asked for, and what the deployment will actually allow.
    fn context_budget(&self, scene: &TokenBudget) -> ContextBudget {
        ContextBudget {
            compact_threshold: scene.compact_threshold,
            compact_keep_recent: scene.compact_keep_recent,
            hard_cap: None,
        }
    }
}

/// The engine's own numbers.
pub struct EngineBudget {
    /// `settings.execution.max_budget_tokens`. `None` is unlimited, which is
    /// the default.
    pub max_total_tokens: Option<u64>,
}

impl EngineBudget {
    pub fn new(max_total_tokens: Option<u64>) -> Self {
        Self { max_total_tokens }
    }
}

impl BudgetPolicy for EngineBudget {
    fn on_usage(&self, spend: &Spend) -> Spending {
        let Some(budget) = self.max_total_tokens else {
            return Spending::WithinBudget;
        };
        if spend.total_tokens >= budget {
            return Spending::Exhausted { limit: budget };
        }
        if spend.total_tokens >= budget * 9 / 10 {
            return Spending::Warn {
                reminder: "<system-reminder>\nOutput token budget nearly exhausted. \
                           Keep your response concise and wrap up.\n</system-reminder>"
                    .into(),
                limit: budget,
            };
        }
        Spending::WithinBudget
    }

    fn on_output_target(&self, p: &OutputProgress) -> OutputTarget {
        // Three numbers with no configuration entry, now at least in one
        // place: 90% of the target counts as met, and three consecutive
        // sub-500-token calls count as the model having run out of things to
        // say rather than needing another nudge.
        let met = (p.target as f64 * 0.9) as u64;
        let diminishing = p.continuations >= 3 && p.this_delta < 500 && p.last_delta < 500;
        if diminishing || p.accumulated >= met {
            return OutputTarget::Reached;
        }
        let remaining = p.target.saturating_sub(p.accumulated);
        OutputTarget::KeepGoing {
            nudge: format!(
                "\
<system-reminder>
Continue working. Used {}/{} output tokens ({} remaining).
</system-reminder>",
                p.accumulated, p.target, remaining
            ),
        }
    }
}

/// A ceiling on request size, on top of whatever else a policy decides.
///
/// The second implementation, and the one the missing capability calls for: a
/// deployment that has to guarantee it never sends a request past some size —
/// a provider hard limit, a cost ceiling, a compliance rule — wraps its policy
/// in this. A turn whose context is still above the cap after compaction ends
/// rather than sending the request anyway.
pub struct Capped<P> {
    pub inner: P,
    pub context_hard_cap: usize,
}

impl<P: BudgetPolicy> BudgetPolicy for Capped<P> {
    fn on_usage(&self, spend: &Spend) -> Spending {
        self.inner.on_usage(spend)
    }

    fn on_output_target(&self, progress: &OutputProgress) -> OutputTarget {
        self.inner.on_output_target(progress)
    }

    fn context_budget(&self, scene: &TokenBudget) -> ContextBudget {
        ContextBudget {
            hard_cap: Some(self.context_hard_cap),
            ..self.inner.context_budget(scene)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(budget: u64) -> EngineBudget {
        EngineBudget::new(Some(budget))
    }

    #[test]
    fn the_bill_warns_at_ninety_percent_and_stops_at_the_cap() {
        let p = engine(1_000);
        assert_eq!(
            p.on_usage(&Spend {
                total_tokens: 899
            }),
            Spending::WithinBudget
        );
        assert!(matches!(
            p.on_usage(&Spend {
                total_tokens: 900
            }),
            Spending::Warn { .. }
        ));
        assert_eq!(
            p.on_usage(&Spend {
                total_tokens: 1_000
            }),
            Spending::Exhausted { limit: 1_000 },
            "the check is `>=`; at the cap the turn is over, not one call later"
        );
    }

    #[test]
    fn no_cap_configured_means_no_ceiling() {
        assert_eq!(
            EngineBudget::new(None).on_usage(&Spend {
                total_tokens: u64::MAX
            }),
            Spending::WithinBudget
        );
    }

    fn progress(accumulated: u64, continuations: u32, this: u64, last: u64) -> OutputProgress {
        OutputProgress {
            accumulated,
            target: 100_000,
            continuations,
            this_delta: this,
            last_delta: last,
        }
    }

    #[test]
    fn ninety_percent_of_the_output_target_counts_as_reached() {
        let p = engine(u64::MAX);
        assert!(matches!(
            p.on_output_target(&progress(89_000, 0, 1_000, 0)),
            OutputTarget::KeepGoing { .. }
        ));
        assert_eq!(
            p.on_output_target(&progress(90_000, 0, 1_000, 0)),
            OutputTarget::Reached
        );
    }

    /// The rule that stops the engine nagging a model that has nothing left
    /// to add: three continuations *and* two consecutive small answers.
    #[test]
    fn diminishing_returns_take_all_three_conditions() {
        let p = engine(u64::MAX);
        assert_eq!(
            p.on_output_target(&progress(10_000, 3, 400, 400)),
            OutputTarget::Reached
        );
        assert!(matches!(
            p.on_output_target(&progress(10_000, 2, 400, 400)),
            OutputTarget::KeepGoing { .. },
            ),);
        assert!(matches!(
            p.on_output_target(&progress(10_000, 3, 600, 400)),
            OutputTarget::KeepGoing { .. }
        ));
    }

    /// There is no cap on how many times the model can be asked to continue.
    /// A count alone never ends it — only the count together with two small
    /// answers does.
    #[test]
    fn a_long_run_of_substantial_answers_keeps_going() {
        assert!(matches!(
            engine(u64::MAX).on_output_target(&progress(10_000, 20, 1_000, 1_000)),
            OutputTarget::KeepGoing { .. }
        ));
    }

    #[test]
    fn the_scene_decides_the_threshold_until_a_deployment_caps_it() {
        let scene = TokenBudget {
            compact_threshold: 150_000,
            compact_keep_recent: 4,
        };
        let engine_only = EngineBudget::new(None).context_budget(&scene);
        assert_eq!(engine_only.compact_threshold, 150_000);
        assert_eq!(engine_only.hard_cap, None, "the shape of the gap this fills");

        let capped = Capped {
            inner: EngineBudget::new(None),
            context_hard_cap: 120_000,
        }
        .context_budget(&scene);
        assert_eq!(capped.hard_cap, Some(120_000));
        assert_eq!(
            capped.compact_threshold, 150_000,
            "a ceiling is not a trigger — the scene still says when to compact"
        );
    }
}
