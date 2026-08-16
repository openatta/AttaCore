//! Ahead-of-time compilation cache for plugin components.
//!
//! Compiling a component is the expensive part of loading a plugin;
//! `Component::serialize` produces a machine-code artifact that skips it.
//!
//! The key is the component's content hash and nothing else. Compatibility
//! is not the key's job: `Component::deserialize` validates the artifact's
//! own header and rejects one built by an incompatible toolchain, so putting
//! a compatibility fingerprint in the filename would only duplicate a check
//! that already happens — and it cannot be done anyway, because the function
//! that computes wasmtime's compatibility hash exists only in a build that
//! links the compiler. The whole point of the split is that the process
//! doing the loading may not have one.
//!
//! What that buys: the compiler and the runtime agree on where an artifact
//! goes without having to agree on anything else, and an artifact from the
//! wrong toolchain produces wasmtime's own rejection — which names the real
//! problem — instead of a silent cache miss.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// AOT artifacts for one plugin version, stored beside its extracted files.
pub struct AotCache {
    dir: PathBuf,
}

impl AotCache {
    /// `plugin_version_dir` is the plugin's extracted directory
    /// (`plugins/cache/<name>/<version>/`); artifacts land in `.aot/` inside
    /// it, so uninstalling the version takes its artifacts with it.
    pub fn new(plugin_version_dir: &Path) -> Self {
        Self {
            dir: plugin_version_dir.join(".aot"),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Cache file for `component_bytes`.
    ///
    /// Content-addressed, so a rebuilt plugin never reads back the previous
    /// build's machine code, and the compiler and the runtime land on the
    /// same filename without sharing anything but this function.
    pub fn artifact_path(&self, component_bytes: &[u8]) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(component_bytes);
        let digest = hex::encode(hasher.finalize());
        self.dir.join(format!("{}.cwasm", &digest[..32]))
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

    #[test]
    fn the_key_changes_with_the_component_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AotCache::new(dir.path());
        assert_ne!(cache.artifact_path(b"one"), cache.artifact_path(b"two"));
        assert_eq!(cache.artifact_path(b"one"), cache.artifact_path(b"one"));
    }

    /// The compiler and the runtime are separate binaries sharing only this
    /// function. If they disagreed on the filename, nothing would ever load
    /// in a build that cannot compile a replacement.
    #[test]
    fn the_same_bytes_always_land_on_the_same_filename() {
        let dir = tempfile::tempdir().unwrap();
        let a = AotCache::new(dir.path());
        let b = AotCache::new(dir.path());
        assert_eq!(a.artifact_path(b"same"), b.artifact_path(b"same"));
    }

    #[test]
    fn artifacts_live_under_the_plugin_version_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AotCache::new(dir.path());
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
        let cache = AotCache::new(dir.path());
        assert!(cache.read(&cache.artifact_path(b"x")).is_none());
    }

    #[test]
    fn a_written_artifact_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AotCache::new(dir.path());
        let path = cache.artifact_path(b"x");
        cache.write(&path, b"artifact-bytes");
        assert_eq!(cache.read(&path).as_deref(), Some(&b"artifact-bytes"[..]));
    }

    /// An unwritable cache slows the next load down; it must not break it.
    #[test]
    fn an_unwritable_cache_is_survivable() {
        let cache = AotCache::new(Path::new("/dev/null/nope"));
        let path = cache.artifact_path(b"x");
        cache.write(&path, b"bytes");
        assert!(cache.read(&path).is_none());
    }
}
