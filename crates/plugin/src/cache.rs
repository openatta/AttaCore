//! Plugin cache — versioned storage for downloaded plugins.
//!
//! TS parity: claude-code's versioned plugin cache at `~/.claude/plugins/cache/`.
//!
//! Layout:
//! ```text
//! ~/.atta/code/plugins/cache/
//! ├── {name}/
//! │   ├── {version}/
//! │   │   ├── plugin.toml
//! │   │   ├── SKILL.md (or skills/)
//! │   │   └── ... (extracted archive contents)
//! │   └── latest -> {version}/  (symlink)
//! └── registry.json  (cached index)
//! ```

use crate::manifest::PluginError;
use std::path::PathBuf;

/// Manages the plugin cache directory.
pub struct PluginCache {
    pub(crate) root: PathBuf,
}

impl PluginCache {
    /// Create a cache rooted at `~/.atta/code/plugins/cache/`.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Ensure the cache directory exists.
    pub fn ensure_dirs(&self) -> Result<(), PluginError> {
        std::fs::create_dir_all(&self.root).map_err(|e| {
            PluginError::Io(std::io::Error::other(format!(
                "failed to create cache dir: {e}"
            )))
        })?;
        Ok(())
    }

    /// Get the versioned directory for a plugin.
    pub fn version_dir(&self, name: &str, version: &str) -> PathBuf {
        self.root.join(name).join(version)
    }

    /// Check if a plugin version is already cached.
    pub fn is_cached(&self, name: &str, version: &str) -> bool {
        let dir = self.version_dir(name, version);
        dir.join("plugin.toml").is_file()
    }

    /// List cached versions of a plugin.
    pub fn list_versions(&self, name: &str) -> Vec<String> {
        let dir = self.root.join(name);
        if !dir.is_dir() {
            return Vec::new();
        }
        let mut versions = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if let Some(v) = path.file_name().and_then(|s| s.to_str()) {
                        if v != "latest" {
                            versions.push(v.to_string());
                        }
                    }
                }
            }
        }
        versions.sort();
        // Sort by version string (simple lexicographic)
        versions.reverse();
        versions
    }

    /// Get the path for a cached plugin's extracted directory.
    /// Returns the expanded plugin manifest path.
    pub fn cached_manifest(&self, name: &str, version: &str) -> Option<PathBuf> {
        let dir = self.version_dir(name, version);
        let manifest_path = dir.join("plugin.toml");
        if manifest_path.is_file() {
            Some(manifest_path)
        } else {
            None
        }
    }

    /// Remove a cached plugin version.
    pub fn remove_version(&self, name: &str, version: &str) -> Result<(), PluginError> {
        let dir = self.version_dir(name, version);
        if dir.is_dir() {
            std::fs::remove_dir_all(&dir).map_err(|e| {
                PluginError::Io(std::io::Error::other(format!(
                    "failed to remove cached plugin: {e}"
                )))
            })?;
        }
        // If no more versions remain, remove the plugin directory
        let plugin_dir = self.root.join(name);
        if plugin_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&plugin_dir) {
                if entries.flatten().count() == 0 {
                    let _ = std::fs::remove_dir(&plugin_dir);
                }
            }
        }
        Ok(())
    }

    /// Store cached registry index.
    pub fn cache_registry_index(&self, index_json: &str) -> Result<(), PluginError> {
        let path = self.root.join("registry.json");
        std::fs::write(&path, index_json).map_err(|e| {
            PluginError::Io(std::io::Error::other(format!(
                "failed to cache registry index: {e}"
            )))
        })?;
        Ok(())
    }

    /// Load cached registry index.
    pub fn load_registry_index(&self) -> Option<String> {
        let path = self.root.join("registry.json");
        std::fs::read_to_string(&path).ok()
    }

    /// Total number of cached plugin versions.
    pub fn total_cached(&self) -> usize {
        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && path.file_name() != Some(std::ffi::OsStr::new("registry.json"))
                {
                    if let Ok(versions) = std::fs::read_dir(&path) {
                        count += versions
                            .flatten()
                            .filter(|e| e.path().is_dir() && e.file_name() != "latest")
                            .count();
                    }
                }
            }
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn empty_cache_has_no_versions() {
        let tmp = TempDir::new().unwrap();
        let cache = PluginCache::new(tmp.path().join("plugins").join("cache"));
        assert_eq!(cache.list_versions("test-plugin"), Vec::<String>::new());
        assert_eq!(cache.total_cached(), 0);
    }

    #[test]
    fn is_cached_detects_installed_plugin() {
        let tmp = TempDir::new().unwrap();
        let cache = PluginCache::new(tmp.path().join("plugins").join("cache"));
        let dir = cache.version_dir("my-plugin", "1.0.0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.toml"),
            "[plugin]\nname = \"my-plugin\"\nversion = \"1.0.0\"",
        )
        .unwrap();
        assert!(cache.is_cached("my-plugin", "1.0.0"));
        assert!(!cache.is_cached("my-plugin", "2.0.0"));
    }
}
