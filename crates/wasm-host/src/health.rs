//! Tracking whether a plugin is still worth calling.
//!
//! Per-call isolation means a fault costs one call rather than the process,
//! which is what makes it safe to *keep* calling a plugin that just failed.
//! It does not make it useful. A component that traps on every invocation
//! turns each of the model's attempts into an error result, and the model
//! will keep trying — so something has to decide the plugin is broken and
//! stop asking.
//!
//! Only faults count. A timeout or a cancellation says something about the
//! work or the user, not about the plugin's soundness: a plugin doing
//! something genuinely slow would otherwise disable itself for being used
//! as intended.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Consecutive faults before a plugin is set aside.
///
/// Three rather than one, because a fault can come from an input the plugin
/// mishandles rather than from the plugin being broken, and a single bad
/// argument should not cost the user the tool. Three in a row is no longer
/// an argument problem.
pub const FAULT_LIMIT: u32 = 3;

/// A plugin's fault record. Cheap to share; every call updates it.
#[derive(Debug, Default)]
pub struct Health {
    consecutive_faults: AtomicU32,
    total_faults: AtomicU32,
}

impl Health {
    pub fn new() -> Self {
        Self::default()
    }

    /// A call returned normally — whatever it returned. An error *result* is
    /// still the plugin working: it ran, and it had something to say.
    pub fn record_success(&self) {
        self.consecutive_faults.store(0, Ordering::Relaxed);
    }

    /// The component trapped, or could not be run at all.
    pub fn record_fault(&self) {
        self.consecutive_faults.fetch_add(1, Ordering::Relaxed);
        self.total_faults.fetch_add(1, Ordering::Relaxed);
    }

    /// A call that ended without the plugin's participation. Neither a
    /// success nor a fault: the plugin is not implicated either way, so the
    /// streak is left exactly as it was.
    pub fn record_abandoned(&self) {}

    pub fn consecutive_faults(&self) -> u32 {
        self.consecutive_faults.load(Ordering::Relaxed)
    }

    pub fn total_faults(&self) -> u32 {
        self.total_faults.load(Ordering::Relaxed)
    }

    /// Has this plugin failed often enough in a row to stop calling it?
    pub fn is_broken(&self) -> bool {
        self.consecutive_faults() >= FAULT_LIMIT
    }
}

/// Fault records kept per plugin name, outliving the instances themselves.
///
/// A record on the instance is lost whenever the instance is rebuilt, and
/// rebuilding happens on every install, uninstall, enable and disable — so a
/// plugin that had disabled itself came back the moment the user touched any
/// *other* plugin. The registry is what makes "disabled after three
/// consecutive faults" survive a refresh.
#[derive(Default)]
pub struct HealthRegistry {
    by_plugin: std::sync::Mutex<std::collections::HashMap<String, Arc<Health>>>,
}

impl HealthRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The record for `plugin`, creating it the first time it is asked for.
    pub fn for_plugin(&self, plugin: &str) -> Arc<Health> {
        self.lock()
            .entry(plugin.to_string())
            .or_insert_with(|| Arc::new(Health::new()))
            .clone()
    }

    /// Forget a plugin's record — for an uninstall, where the user has said
    /// something about the plugin that a fault streak should not outlive.
    pub fn forget(&self, plugin: &str) {
        self.lock().remove(plugin);
    }

    pub fn is_broken(&self, plugin: &str) -> bool {
        self.lock().get(plugin).is_some_and(|h| h.is_broken())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, Arc<Health>>> {
        self.by_plugin.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_plugin_is_healthy() {
        let h = Health::new();
        assert!(!h.is_broken());
        assert_eq!(h.consecutive_faults(), 0);
    }

    #[test]
    fn faults_short_of_the_limit_do_not_disable_it() {
        let h = Health::new();
        for _ in 0..FAULT_LIMIT - 1 {
            h.record_fault();
        }
        assert!(
            !h.is_broken(),
            "one bad argument should not cost the user the tool"
        );
    }

    #[test]
    fn consecutive_faults_reaching_the_limit_disable_it() {
        let h = Health::new();
        for _ in 0..FAULT_LIMIT {
            h.record_fault();
        }
        assert!(h.is_broken());
    }

    /// The streak is what matters. A plugin that fails occasionally and
    /// recovers is being used at its edges, not broken.
    #[test]
    fn a_success_clears_the_streak_but_not_the_history() {
        let h = Health::new();
        h.record_fault();
        h.record_fault();
        h.record_success();
        h.record_fault();

        assert!(!h.is_broken());
        assert_eq!(h.consecutive_faults(), 1);
        assert_eq!(h.total_faults(), 3, "the history is still worth reporting");
    }

    /// Otherwise a plugin doing something genuinely slow, or one the user
    /// interrupted three times, would disable itself for working as
    /// intended.
    #[test]
    fn timeouts_and_cancellations_do_not_count_against_it() {
        let h = Health::new();
        for _ in 0..FAULT_LIMIT * 2 {
            h.record_abandoned();
        }
        assert!(!h.is_broken());
        assert_eq!(h.consecutive_faults(), 0);
    }

    /// An error result is the plugin answering. Only a fault is the plugin
    /// failing to answer.
    #[test]
    fn an_error_result_is_a_success_for_health_purposes() {
        let h = Health::new();
        for _ in 0..FAULT_LIMIT * 2 {
            h.record_success();
        }
        assert!(!h.is_broken());
    }

    /// The whole point of the registry: a record on the instance is lost on
    /// every rebuild, and rebuilding happens whenever the user touches any
    /// plugin at all.
    #[test]
    fn a_record_survives_being_looked_up_again() {
        let reg = HealthRegistry::new();
        for _ in 0..FAULT_LIMIT {
            reg.for_plugin("broken").record_fault();
        }
        assert!(reg.is_broken("broken"));
        // What a reload does: ask for the record again for a fresh instance.
        assert!(reg.for_plugin("broken").is_broken());
    }

    #[test]
    fn records_do_not_bleed_between_plugins() {
        let reg = HealthRegistry::new();
        for _ in 0..FAULT_LIMIT {
            reg.for_plugin("broken").record_fault();
        }
        assert!(!reg.is_broken("healthy"));
        assert!(!reg.for_plugin("healthy").is_broken());
    }

    /// Uninstalling is the user saying something about the plugin; a fault
    /// streak should not outlive that.
    #[test]
    fn forgetting_a_plugin_clears_its_record() {
        let reg = HealthRegistry::new();
        for _ in 0..FAULT_LIMIT {
            reg.for_plugin("broken").record_fault();
        }
        reg.forget("broken");
        assert!(!reg.is_broken("broken"));
    }

    #[test]
    fn an_unknown_plugin_is_not_broken() {
        assert!(!HealthRegistry::new().is_broken("never-seen"));
    }
}
