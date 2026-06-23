//! Plugin management CLI commands — RPC methods for plugin lifecycle.
//!
//! TS parity: claude-code's `pluginCliCommands.ts`.
//!
//! Commands: install, uninstall, enable, disable, update, list.

use crate::cache::PluginCache;
use crate::manifest::PluginError;
use crate::marketplace::{PluginResolver, PluginSource};
use std::sync::Arc;

/// Result of a plugin CLI operation.
#[derive(Debug, Clone)]
pub struct PluginCommandResult {
    pub success: bool,
    pub message: String,
    pub installed_count: usize,
    pub removed_count: usize,
    pub updated_count: usize,
}

/// Plugin management commands.
pub struct PluginCommands {
    cache: PluginCache,
    resolver: Option<Arc<dyn PluginResolver>>,
}

impl PluginCommands {
    pub fn new(cache: PluginCache, resolver: Option<Arc<dyn PluginResolver>>) -> Self {
        Self { cache, resolver }
    }

    /// Install a plugin from the marketplace.
    pub async fn install(&self, name: &str, version: Option<&str>) -> Result<PluginCommandResult, PluginError> {
        // 1. Check blocklist for known-malicious names (including confusable variants)
        if let Some(blocked) = crate::homograph::check_blocklist(name) {
            return Err(PluginError::Homograph(format!(
                "plugin name '{name}' is blocked (matches blocklist entry '{blocked}')"
            )));
        }

        let Some(ref resolver) = self.resolver else {
            return Err(PluginError::Schema(
                "no marketplace configured — cannot install plugins".into(),
            ));
        };

        // 2. Check for homograph attacks against official plugin names in the registry
        let index = resolver.fetch_index().await?;
        let official_names: Vec<&str> = index.iter().map(|e| e.name.as_str()).collect();
        if let Some(msg) = crate::homograph::check_homograph_name(name, &official_names) {
            return Err(PluginError::Homograph(msg));
        }

        let version = version.unwrap_or("latest");

        // Find the entry in the already-fetched index to avoid a second fetch
        let entry = index.iter().find(|e| {
            e.name == name
                && (version == "latest"
                    || version == "any"
                    || e.version == version)
        }).ok_or_else(|| {
            PluginError::Schema(format!("plugin {name}@{version} not found in registry"))
        })?;

        let source = PluginSource {
            download_url: entry.download_url.clone(),
            checksum: entry.checksum.clone(),
            version: entry.version.clone(),
        };

        // Download and extract the plugin to the cache
        // (Actual download/extraction is a placeholder for now)
        if self.cache.is_cached(name, source.version.as_str()) {
            return Ok(PluginCommandResult {
                success: true,
                message: format!("plugin '{name}' v{} is already installed", source.version),
                installed_count: 0,
                removed_count: 0,
                updated_count: 0,
            });
        }

        // Placeholder: mark as cached by creating the directory
        let dir = self.cache.version_dir(name, &source.version);
        self.cache.ensure_dirs()?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| PluginError::Io(std::io::Error::other(format!("failed to create plugin dir: {e}"))))?;

        Ok(PluginCommandResult {
            success: true,
            message: format!(
                "installed plugin '{name}' v{} from {}",
                source.version, source.download_url
            ),
            installed_count: 1,
            removed_count: 0,
            updated_count: 0,
        })
    }

    /// Uninstall a plugin version.
    pub async fn uninstall(&self, name: &str, version: Option<&str>) -> Result<PluginCommandResult, PluginError> {
        if let Some(version) = version {
            self.cache.remove_version(name, version)?;
            Ok(PluginCommandResult {
                success: true,
                message: format!("removed plugin '{name}' v{version}"),
                installed_count: 0,
                removed_count: 1,
                updated_count: 0,
            })
        } else {
            // Remove all versions
            let versions = self.cache.list_versions(name);
            let mut count = 0;
            for v in &versions {
                self.cache.remove_version(name, v)?;
                count += 1;
            }
            Ok(PluginCommandResult {
                success: true,
                message: format!("removed plugin '{name}' ({count} versions)"),
                installed_count: 0,
                removed_count: count,
                updated_count: 0,
            })
        }
    }

    /// List installed plugins.
    pub fn list(&self) -> Result<Vec<String>, PluginError> {
        let mut result = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.cache.root_path()) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                        if name != "registry.json" {
                            let versions = self.cache.list_versions(name);
                            if versions.is_empty() {
                                result.push(format!("{name} (no versions)"));
                            } else {
                                result.push(format!("{name} ({})", versions.join(", ")));
                            }
                        }
                    }
                }
            }
        }
        Ok(result)
    }

    /// Update a plugin to the latest version.
    pub async fn update(&self, name: &str) -> Result<PluginCommandResult, PluginError> {
        let current_versions = self.cache.list_versions(name);
        let current = current_versions.first().cloned();

        let Some(ref resolver) = self.resolver else {
            return Err(PluginError::Schema(
                "no marketplace configured — cannot update plugins".into(),
            ));
        };

        let source = resolver.resolve(name, "latest").await?;
        let latest_version = source.version.clone();

        if current.as_deref() == Some(&latest_version) {
            return Ok(PluginCommandResult {
                success: true,
                message: format!("plugin '{name}' is already at latest v{latest_version}"),
                installed_count: 0,
                removed_count: 0,
                updated_count: 0,
            });
        }

        // Install latest version
        let dir = self.cache.version_dir(name, &latest_version);
        std::fs::create_dir_all(&dir)
            .map_err(|e| PluginError::Io(std::io::Error::other(format!("failed to create plugin dir: {e}"))))?;

        Ok(PluginCommandResult {
            success: true,
            message: format!(
                "updated plugin '{name}' from {} to v{latest_version}",
                current.as_deref().unwrap_or("not installed")
            ),
            installed_count: 0,
            removed_count: 0,
            updated_count: 1,
        })
    }
}

impl PluginCache {
    /// Expose the root path for listing commands.
    pub fn root_path(&self) -> &std::path::Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn list_empty_cache() {
        let tmp = TempDir::new().unwrap();
        let cache = PluginCache::new(tmp.path().join("plugins").join("cache"));
        let cmds = PluginCommands::new(cache, None);
        let list = cmds.list().unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn uninstall_all_versions() {
        let tmp = TempDir::new().unwrap();
        let cache = PluginCache::new(tmp.path().join("plugins").join("cache"));
        cache.ensure_dirs().unwrap();
        for v in ["1.0.0", "2.0.0"] {
            let dir = cache.version_dir("test-plugin", v);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("plugin.toml"), "[plugin]\nname = \"test-plugin\"").unwrap();
        }
        let cmds = PluginCommands::new(cache, None);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmds.uninstall("test-plugin", None)).unwrap();
        assert_eq!(result.removed_count, 2);
    }
}
