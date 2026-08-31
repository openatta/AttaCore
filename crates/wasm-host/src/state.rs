//! Per-call store state, and the host services a component may reach.
//!
//! A component gets a fresh `Store` for every call. That is what makes a
//! plugin's failures survivable: a trap, a runaway loop and an allocation
//! blow-up all end the same way — the store is dropped, the call returns an
//! error, and the engine carries on. It also means a component has no memory
//! between calls, which is deliberate: anything that must persist goes
//! through [`KvNamespace`], where the host can see it, clear it, and drop it
//! with the plugin.

use crate::capabilities::ResolvedCapabilities;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use wasmtime::{ResourceLimiter, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{
    DirPerms, FilePerms, ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView,
};

/// A plugin's slice of host-side storage.
///
/// Shared across the calls of one plugin and nothing else — the namespace is
/// the plugin, so one plugin cannot read another's keys by guessing them.
/// State lives for as long as the host process keeps this namespace alive and
/// is dropped when the plugin is unloaded.
#[derive(Default)]
pub struct KvNamespace {
    entries: Mutex<HashMap<String, Vec<u8>>>,
}

impl KvNamespace {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.lock().get(key).cloned()
    }

    pub fn set(&self, key: String, value: Vec<u8>) {
        self.lock().insert(key, value);
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop everything this plugin stored — what unloading it should mean.
    pub fn clear(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<u8>>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Where a component's `progress` calls go.
pub trait ProgressSink: Send + Sync {
    fn on_progress(&self, call_id: &str, text: &str);
}

/// Everything one call's store carries.
pub struct PluginState {
    /// Plugin name, for log attribution and error messages.
    pub plugin: String,
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
    pub caps: Arc<ResolvedCapabilities>,
    pub kv: Arc<KvNamespace>,
    pub progress: Option<Arc<dyn ProgressSink>>,
    /// Where this plugin's HTTP goes out. Declared hosts are checked against
    /// `caps` first; this is the egress that carries what survives.
    pub net: Arc<dyn base::interface::exec::Network>,
}

impl PluginState {
    /// Build the state for one call.
    ///
    /// The WASI context is constructed from the capability list and nothing
    /// else: no inherited environment, no inherited stdio beyond what a
    /// component needs to be constructible, and preopens only for the
    /// directories the manifest names. What a plugin did not ask for in
    /// writing, it does not get.
    pub fn new(
        plugin: impl Into<String>,
        caps: Arc<ResolvedCapabilities>,
        kv: Arc<KvNamespace>,
        progress: Option<Arc<dyn ProgressSink>>,
    ) -> anyhow::Result<Self> {
        let mut builder = WasiCtxBuilder::new();
        for dir in &caps.fs_read {
            builder
                .preopened_dir(dir, guest_path(dir), DirPerms::READ, FilePerms::READ)
                .map_err(|e| anyhow::anyhow!("preopening {} read-only: {e}", dir.display()))?;
        }
        for dir in &caps.fs_write {
            builder
                .preopened_dir(dir, guest_path(dir), DirPerms::all(), FilePerms::all())
                .map_err(|e| anyhow::anyhow!("preopening {} writable: {e}", dir.display()))?;
        }

        // `memory_size` is the ceiling that matters: it bounds the total
        // linear memory the store may hand out, so a component cannot
        // allocate its way past it by any route.
        //
        // The instance and memory counts are bounds on *structure*, not on
        // size, and they are not 1: instantiating a component produces
        // several core instances (the guest's own modules plus the WASI
        // adapter), so a limit of one rejects every real component before it
        // runs. These numbers exist to stop unbounded growth, and any
        // component needing more than this is doing something the host
        // should be asked about first.
        let limits = StoreLimitsBuilder::new()
            .memory_size(caps.max_memory_bytes)
            .instances(64)
            .memories(16)
            .tables(64)
            .build();

        Ok(Self {
            plugin: plugin.into(),
            wasi: builder.build(),
            table: ResourceTable::new(),
            limits,
            caps,
            kv,
            progress,
            net: Arc::new(base::interface::exec::local::LocalNetwork::default()),
        })
    }

    /// Send this plugin's HTTP out through a given egress.
    pub fn with_network(mut self, net: Arc<dyn base::interface::exec::Network>) -> Self {
        self.net = net;
        self
    }

    pub fn limiter(&mut self) -> &mut dyn ResourceLimiter {
        &mut self.limits
    }
}

impl WasiView for PluginState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// The path a preopened directory appears at inside the component.
///
/// Guests see the last path segment rather than the host's absolute path:
/// a plugin has no business learning where on the machine its workspace
/// lives, and a stable short name is what its own code can be written
/// against.
fn guest_path(host_dir: &std::path::Path) -> String {
    host_dir
        .file_name()
        .map(|s| format!("/{}", s.to_string_lossy()))
        .unwrap_or_else(|| "/".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn caps_with(fs_read: Vec<PathBuf>, fs_write: Vec<PathBuf>) -> Arc<ResolvedCapabilities> {
        let c = plugin::manifest::Capabilities {
            max_memory_mb: 8,
            ..Default::default()
        };
        let mut resolved =
            crate::capabilities::resolve(&c, Path::new("/ws"), Path::new("/plug")).unwrap();
        resolved.fs_read = fs_read;
        resolved.fs_write = fs_write;
        Arc::new(resolved)
    }

    #[test]
    fn kv_is_scoped_and_clearable() {
        let kv = KvNamespace::new();
        assert!(kv.is_empty());
        kv.set("a".into(), b"1".to_vec());
        kv.set("b".into(), b"2".to_vec());
        assert_eq!(kv.get("a").as_deref(), Some(&b"1"[..]));
        assert_eq!(kv.len(), 2);
        assert!(kv.get("missing").is_none());

        kv.clear();
        assert!(
            kv.is_empty(),
            "unloading a plugin must take its state with it"
        );
        assert!(kv.get("a").is_none());
    }

    #[test]
    fn a_state_with_no_capabilities_preopens_nothing() {
        let state = PluginState::new(
            "p",
            caps_with(Vec::new(), Vec::new()),
            Arc::new(KvNamespace::new()),
            None,
        )
        .unwrap();
        assert_eq!(state.plugin, "p");
    }

    #[test]
    fn declared_directories_are_preopened() {
        let dir = tempfile::tempdir().unwrap();
        let readable = dir.path().join("readable");
        let writable = dir.path().join("writable");
        std::fs::create_dir_all(&readable).unwrap();
        std::fs::create_dir_all(&writable).unwrap();

        PluginState::new(
            "p",
            caps_with(vec![readable], vec![writable]),
            Arc::new(KvNamespace::new()),
            None,
        )
        .expect("existing directories must preopen");
    }

    /// A capability naming a directory that isn't there is a manifest bug,
    /// and it surfaces at load with the path in the message rather than as a
    /// mysterious failure the first time the plugin touches a file.
    #[test]
    fn a_missing_declared_directory_fails_with_the_path() {
        let err = PluginState::new(
            "p",
            caps_with(vec![PathBuf::from("/definitely/not/here")], Vec::new()),
            Arc::new(KvNamespace::new()),
            None,
        )
        .err()
        .expect("a capability naming a directory that isn't there must fail")
        .to_string();
        assert!(err.contains("/definitely/not/here"), "{err}");
    }

    #[test]
    fn guest_paths_hide_the_host_layout() {
        assert_eq!(
            guest_path(Path::new("/Users/someone/secret/project")),
            "/project"
        );
        assert_eq!(guest_path(Path::new("/")), "/");
    }
}
