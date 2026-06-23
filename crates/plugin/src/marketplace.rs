//! Plugin marketplace — remote registry and source resolution.
//!
//! TS parity: claude-code's `PluginInstallationManager` + marketplace resolvers.
//! Supports GitHub releases and generic HTTP registries as plugin sources.

use crate::manifest::PluginError;
use async_trait::async_trait;
use serde::Deserialize;

/// A resolved plugin source — where to download the plugin from.
#[derive(Debug, Clone)]
pub struct PluginSource {
    /// Download URL for the plugin archive.
    pub download_url: String,
    /// Expected SHA-256 checksum (hex-encoded).
    pub checksum: Option<String>,
    /// Plugin version string.
    pub version: String,
}

/// Registry entry returned by a marketplace resolver.
#[derive(Debug, Clone, Deserialize)]
pub struct RegistryEntry {
    pub name: String,
    pub version: String,
    pub description: String,
    pub download_url: String,
    pub checksum: Option<String>,
    pub dependencies: Vec<String>,
}

/// Trait for resolving plugin names/versions to download sources.
#[async_trait]
pub trait PluginResolver: Send + Sync {
    /// Fetch the registry index (list of available plugins).
    async fn fetch_index(&self) -> Result<Vec<RegistryEntry>, PluginError>;

    /// Resolve a specific plugin name + version constraint to a source.
    async fn resolve(&self, name: &str, version: &str) -> Result<PluginSource, PluginError>;
}

// ── RegistryResolver (generic HTTP registry) ──

/// A plugin resolver that fetches from an HTTP registry endpoint.
///
/// The registry should expose:
/// - `{registry_url}/index.json` — array of RegistryEntry
/// - `{registry_url}/{name}/{version}/plugin.toml` — plugin manifest
pub struct RegistryResolver {
    registry_url: String,
    client: reqwest::Client,
}

impl RegistryResolver {
    pub fn new(registry_url: String) -> Self {
        Self {
            registry_url: registry_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl PluginResolver for RegistryResolver {
    async fn fetch_index(&self) -> Result<Vec<RegistryEntry>, PluginError> {
        let url = format!("{}/index.json", self.registry_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| PluginError::Io(std::io::Error::other(e.to_string())))?;
        let entries: Vec<RegistryEntry> = resp
            .json()
            .await
            .map_err(|e| PluginError::Schema(format!("invalid index: {e}")))?;
        Ok(entries)
    }

    async fn resolve(&self, name: &str, version: &str) -> Result<PluginSource, PluginError> {
        let entries = self.fetch_index().await?;
        let entry = entries
            .iter()
            .find(|e| e.name == name && e.version == version)
            .ok_or_else(|| {
                PluginError::Schema(format!("plugin {name}@{version} not found in registry"))
            })?;
        Ok(PluginSource {
            download_url: entry.download_url.clone(),
            checksum: entry.checksum.clone(),
            version: entry.version.clone(),
        })
    }
}

// ── Stub resolver (for when marketplace is not configured) ──

/// A no-op resolver for when no marketplace is configured.
pub struct NoopResolver;

#[async_trait]
impl PluginResolver for NoopResolver {
    async fn fetch_index(&self) -> Result<Vec<RegistryEntry>, PluginError> {
        Ok(Vec::new())
    }

    async fn resolve(&self, name: &str, _version: &str) -> Result<PluginSource, PluginError> {
        Err(PluginError::Schema(format!(
            "no marketplace configured — cannot resolve plugin '{name}'"
        )))
    }
}
