//! The process-wide wasmtime engine and component loading.
//!
//! One `Engine` per process, shared by every plugin: it owns the compiler and
//! the code cache, and building one per plugin would pay that cost repeatedly
//! for no isolation benefit — isolation comes from the per-call `Store`, not
//! from the engine.

use crate::cache::AotCache;
use anyhow::{anyhow, Context, Result};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use wasmtime::component::Component;
use wasmtime::{Config, Engine};

/// wasmtime has its own error type rather than re-exporting `anyhow`'s, so
/// it needs flattening before `anyhow`'s context combinators apply.
fn wasm_err(e: wasmtime::Error) -> anyhow::Error {
    anyhow!("{e}")
}

/// Shared wasmtime engine, configured once for every plugin this process runs.
#[derive(Clone)]
pub struct WasmEngine {
    engine: Engine,
    compat_key: u64,
    /// Keeps the epoch ticker alive for as long as any clone of this engine
    /// exists. The ticker is what makes a deadline mean anything: without it
    /// the epoch never advances, guests never reach a yield point, and a
    /// spinning plugin blocks the thread it is running on with no way out.
    _ticker: Arc<EpochTicker>,
}

/// Advances the engine's epoch on a fixed interval.
///
/// A dedicated thread rather than a timer task: it has to keep running even
/// when every async worker is occupied — and "every worker is occupied" is
/// precisely the situation a runaway guest creates.
struct EpochTicker {
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl EpochTicker {
    fn start(engine: &Engine) -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let engine = engine.weak();
        let flag = stop.clone();
        std::thread::Builder::new()
            .name("atta-plugin-epoch".into())
            .spawn(move || {
                while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(crate::instance::EPOCH_TICK);
                    match engine.upgrade() {
                        Some(engine) => engine.increment_epoch(),
                        None => break,
                    }
                }
            })
            .expect("spawning the epoch ticker");
        Self { stop }
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl WasmEngine {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        // Timeouts and cancellation are wall-clock questions — "has the user
        // pressed Ctrl-C", "has this call outlived its budget" — so the
        // interruption mechanism is epochs, not fuel. Fuel counts
        // instructions, which answers a different question.
        config.epoch_interruption(true);
        let engine = Engine::new(&config)
            .map_err(wasm_err)
            .context("failed to construct the wasmtime engine")?;
        let compat_key = compat_key(&engine);
        let ticker = Arc::new(EpochTicker::start(&engine));
        Ok(Self {
            engine,
            compat_key,
            _ticker: ticker,
        })
    }

    /// Identifies this engine's build *and* configuration for AOT caching.
    ///
    /// wasmtime exposes the compatibility input as an opaque `Hash` rather
    /// than a version string, and deliberately: codegen settings change what
    /// artifacts are usable just as much as a version bump does.
    pub fn compat_key(&self) -> u64 {
        self.compat_key
    }

    pub fn inner(&self) -> &Engine {
        &self.engine
    }

    /// Load a component, compiling it only when the AOT cache misses.
    ///
    /// `plugin_version_dir` is the plugin's extracted directory, which is
    /// where the artifact is cached (see [`AotCache`]).
    pub fn load(&self, component_path: &Path, plugin_version_dir: &Path) -> Result<ComponentHandle> {
        let bytes = std::fs::read(component_path)
            .with_context(|| format!("reading component {}", component_path.display()))?;

        let cache = AotCache::new(plugin_version_dir, self.compat_key);
        let artifact_path = cache.artifact_path(&bytes);

        if let Some(artifact) = cache.read(&artifact_path) {
            // SAFETY: the artifact is one this process wrote, in a directory
            // the daemon owns, keyed by the exact wasmtime build now running.
            // `deserialize` still validates its own header, so a corrupted or
            // foreign file is rejected rather than executed — which is why a
            // failure here is demoted to a recompile instead of an error.
            match unsafe { Component::deserialize(&self.engine, &artifact) } {
                Ok(component) => {
                    return Ok(ComponentHandle {
                        component: Arc::new(component),
                        from_cache: true,
                    });
                }
                Err(e) => tracing::warn!(
                    path = %artifact_path.display(),
                    error = %e,
                    "cached AOT artifact was rejected; recompiling"
                ),
            }
        }

        let component = Component::new(&self.engine, &bytes)
            .map_err(wasm_err)
            .with_context(|| format!("compiling component {}", component_path.display()))?;
        match component.serialize() {
            Ok(artifact) => cache.write(&artifact_path, &artifact),
            Err(e) => tracing::warn!(error = %e, "component could not be serialized for caching"),
        }

        Ok(ComponentHandle {
            component: Arc::new(component),
            from_cache: false,
        })
    }
}

fn compat_key(engine: &Engine) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    engine.precompile_compatibility_hash().hash(&mut hasher);
    hasher.finish()
}

/// A loaded component, cheap to clone and share across calls.
#[derive(Clone)]
pub struct ComponentHandle {
    component: Arc<Component>,
    /// Whether this load skipped compilation. Reported by diagnostics; a
    /// plugin that never hits its cache is a symptom worth seeing.
    from_cache: bool,
}

impl ComponentHandle {
    pub fn component(&self) -> &Component {
        &self.component
    }

    pub fn was_cached(&self) -> bool {
        self.from_cache
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest thing that is a valid component.
    fn empty_component() -> Vec<u8> {
        wat::parse_str("(component)").unwrap()
    }

    fn write_component(dir: &Path, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join("plugin.wasm");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn the_engine_is_configured_for_components_and_interruption() {
        // Construction is the assertion: `Engine::new` rejects a `Config`
        // whose options don't agree.
        let engine = WasmEngine::new().expect("engine config must be self-consistent");
        assert_eq!(
            engine.compat_key(),
            WasmEngine::new().unwrap().compat_key(),
            "two engines built the same way must share a cache namespace"
        );
    }

    #[test]
    fn a_component_compiles_then_loads_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_component(dir.path(), &empty_component());
        let engine = WasmEngine::new().unwrap();

        let first = engine.load(&path, dir.path()).unwrap();
        assert!(!first.was_cached(), "the first load has nothing to reuse");

        let second = engine.load(&path, dir.path()).unwrap();
        assert!(
            second.was_cached(),
            "the second load must reuse the artifact the first one wrote"
        );
    }

    /// Editing the component must invalidate its artifact — the cache is
    /// keyed on content precisely so a rebuilt plugin doesn't keep running
    /// the old code.
    #[test]
    fn changing_the_component_misses_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_component(dir.path(), &empty_component());
        let engine = WasmEngine::new().unwrap();
        engine.load(&path, dir.path()).unwrap();

        // A component with a distinct (still empty) inner module, so the
        // bytes differ while remaining valid.
        std::fs::write(
            &path,
            wat::parse_str(r#"(component (core module))"#).unwrap(),
        )
        .unwrap();
        assert!(!engine.load(&path, dir.path()).unwrap().was_cached());
    }

    /// A corrupt artifact must degrade to a recompile, not to a failed load —
    /// and certainly not to executing whatever the bytes happen to be.
    #[test]
    fn a_corrupt_artifact_falls_back_to_compiling() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = empty_component();
        let path = write_component(dir.path(), &bytes);
        let engine = WasmEngine::new().unwrap();
        engine.load(&path, dir.path()).unwrap();

        let artifact = AotCache::new(dir.path(), engine.compat_key()).artifact_path(&bytes);
        std::fs::write(&artifact, b"not a wasmtime artifact").unwrap();

        let handle = engine.load(&path, dir.path()).unwrap();
        assert!(!handle.was_cached());
    }

    #[test]
    fn a_missing_component_file_reports_which_path() {
        let dir = tempfile::tempdir().unwrap();
        let engine = WasmEngine::new().unwrap();
        let err = engine
            .load(&dir.path().join("absent.wasm"), dir.path())
            .err()
            .expect("loading a component that isn't there must fail");
        assert!(err.to_string().contains("absent.wasm"), "{err}");
    }

    #[test]
    fn a_file_that_is_not_a_component_fails_to_compile() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_component(dir.path(), b"definitely not wasm");
        let engine = WasmEngine::new().unwrap();
        assert!(engine.load(&path, dir.path()).is_err());
    }
}
