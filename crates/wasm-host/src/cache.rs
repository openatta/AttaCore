//! Ahead-of-time compilation cache for plugin components.
//!
//! Compiling a component is the expensive part of loading a plugin;
//! `Component::serialize` produces a machine-code artifact that skips it.
//! An artifact is only usable by a compatible wasmtime build, so the key
//! combines the component's content hash with wasmtime's own compatibility
//! hash — which accounts for more than the version number (optimization
//! level and module-version strategy change it too). A key that only tracked
//! the version would let a rebuild with different codegen settings read back
//! an artifact it cannot use.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// AOT artifacts for one plugin version, stored beside its extracted files.
pub struct AotCache {
    dir: PathBuf,
    /// Identifies the wasmtime build + configuration that produces artifacts
    /// this cache may hand back — see [`crate::WasmEngine::compat_key`].
    compat_key: u64,
}

impl AotCache {
    /// `plugin_version_dir` is the plugin's extracted directory
    /// (`plugins/cache/<name>/<version>/`); artifacts land in `.aot/` inside
    /// it, so uninstalling the version takes its artifacts with it.
    pub fn new(plugin_version_dir: &Path, compat_key: u64) -> Self {
        Self {
            dir: plugin_version_dir.join(".aot"),
            compat_key,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Cache file for `component_bytes` under this engine configuration.
    ///
    /// The engine key is in the *filename* rather than checked after loading:
    /// an upgraded or reconfigured host then simply misses, instead of
    /// reading an artifact it would have to reject, and the orphaned files
    /// are visible rather than silently shadowing.
    pub fn artifact_path(&self, component_bytes: &[u8]) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(component_bytes);
        let digest = hex::encode(hasher.finalize());
        self.dir
            .join(format!("{:016x}-{}.cwasm", self.compat_key, &digest[..32]))
    }

    pub fn read(&self, path: &Path) -> Option<Vec<u8>> {
        std::fs::read(path).ok()
    }

    /// Best-effort: a cache that can't be written (read-only install, full
    /// disk) costs startup time, not correctness, so a failure is logged and
    /// swallowed rather than propagated.
    pub fn write(&self, path: &Path, artifact: &[u8]) {
        if let Err(e) =
            std::fs::create_dir_all(&self.dir).and_then(|_| std::fs::write(path, artifact))
        {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not write the AOT artifact; the component will be recompiled next time"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: u64 = 0xabcd_1234;

    #[test]
    fn the_key_changes_with_the_component_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AotCache::new(dir.path(), KEY);
        assert_ne!(cache.artifact_path(b"one"), cache.artifact_path(b"two"));
        assert_eq!(cache.artifact_path(b"one"), cache.artifact_path(b"one"));
    }

    /// An artifact produced by a different wasmtime build or codegen
    /// configuration must not be mistaken for a hit — the two are not
    /// interchangeable, and the key in the filename is what keeps them apart.
    #[test]
    fn the_key_changes_with_the_engine_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let a = AotCache::new(dir.path(), KEY);
        let b = AotCache::new(dir.path(), KEY + 1);
        assert_ne!(a.artifact_path(b"same"), b.artifact_path(b"same"));
    }

    #[test]
    fn artifacts_live_under_the_plugin_version_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AotCache::new(dir.path(), KEY);
        assert_eq!(cache.dir(), dir.path().join(".aot"));
        assert!(cache
            .artifact_path(b"x")
            .starts_with(dir.path().join(".aot")));
        assert!(cache
            .artifact_path(b"x")
            .to_str()
            .unwrap()
            .ends_with(".cwasm"));
    }

    #[test]
    fn a_missing_artifact_is_a_miss_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AotCache::new(dir.path(), KEY);
        assert!(cache.read(&cache.artifact_path(b"x")).is_none());
    }

    #[test]
    fn a_written_artifact_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AotCache::new(dir.path(), KEY);
        let path = cache.artifact_path(b"x");
        cache.write(&path, b"artifact-bytes");
        assert_eq!(cache.read(&path).as_deref(), Some(&b"artifact-bytes"[..]));
    }

    /// An unwritable cache slows the next load down; it must not break it.
    #[test]
    fn an_unwritable_cache_is_survivable() {
        let cache = AotCache::new(Path::new("/dev/null/nope"), KEY);
        let path = cache.artifact_path(b"x");
        cache.write(&path, b"bytes");
        assert!(cache.read(&path).is_none());
    }
}
