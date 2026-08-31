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

    /// Every plugin with a record, and what that record says.
    pub fn snapshot(&self) -> Vec<PluginFaults> {
        let mut out: Vec<PluginFaults> = self
            .lock()
            .iter()
            .map(|(plugin, h)| PluginFaults {
                plugin: plugin.clone(),
                consecutive_faults: h.consecutive_faults(),
                total_faults: h.total_faults(),
                broken: h.is_broken(),
            })
            .collect();
        out.sort_by(|a, b| a.plugin.cmp(&b.plugin));
        out
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, Arc<Health>>> {
        self.by_plugin.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One plugin's record, flattened for reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginFaults {
    pub plugin: String,
    pub consecutive_faults: u32,
    pub total_faults: u32,
    pub broken: bool,
}

/// The fault records as a health check.
///
/// Setting a plugin aside is a decision this crate was already making and
/// nobody outside could see: the tool stops being offered, the model stops
/// asking for it, and whoever installed it gets no signal that any of that
/// happened. Reported, it becomes something an operator can act on — the
/// plugin's author has a bug, or this component does not work on this
/// machine.
///
/// Degraded rather than failing when a plugin is broken: the engine is
/// working, and its answer to "should I keep calling this" is the one the
/// fault limit was designed to give.
pub struct PluginBreakers {
    registry: Arc<HealthRegistry>,
}

impl PluginBreakers {
    pub fn new(registry: Arc<HealthRegistry>) -> Self {
        Self { registry }
    }
}

impl base::interface::health::HealthCheck for PluginBreakers {
    fn name(&self) -> &str {
        "plugins.breakers"
    }

    fn check(&self) -> base::interface::health::CheckResult {
        use base::interface::health::CheckResult;

        let records = self.registry.snapshot();
        let broken: Vec<&PluginFaults> = records.iter().filter(|r| r.broken).collect();
        let details = serde_json::json!({
            "plugins": records
                .iter()
                .map(|r| serde_json::json!({
                    "plugin": r.plugin,
                    "consecutive_faults": r.consecutive_faults,
                    "total_faults": r.total_faults,
                    "broken": r.broken,
                }))
                .collect::<Vec<_>>(),
            "fault_limit": FAULT_LIMIT,
        });

        if broken.is_empty() {
            CheckResult::ok(format!("{} plugin(s) called, none set aside", records.len()))
                .with_details(details)
        } else {
            let names: Vec<&str> = broken.iter().map(|r| r.plugin.as_str()).collect();
            CheckResult::degraded(format!(
                "set aside after {FAULT_LIMIT} consecutive faults: {}",
                names.join(", ")
            ))
            .with_details(details)
        }
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

    #[test]
    fn the_health_check_is_quiet_until_something_is_actually_set_aside() {
        use base::interface::health::{HealthCheck, HealthStatus};

        let reg = HealthRegistry::new();
        reg.for_plugin("fine").record_success();
        reg.for_plugin("wobbly").record_fault();

        let result = PluginBreakers::new(reg.clone()).check();
        assert_eq!(result.status, HealthStatus::Ok);
        assert_eq!(result.details["plugins"].as_array().unwrap().len(), 2);

        for _ in 0..FAULT_LIMIT {
            reg.for_plugin("wobbly").record_fault();
        }
        let result = PluginBreakers::new(reg).check();
        assert_eq!(result.status, HealthStatus::Degraded);
        assert!(
            result.summary.contains("wobbly"),
            "the operator needs the name, not a count: {}",
            result.summary
        );
    }
}
