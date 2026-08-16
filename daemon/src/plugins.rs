//! The plugin subsystem, behind one type the rest of the daemon talks to.
//!
//! Two implementations of the same API are selected by the `plugins` feature.
//! With the feature off, `plugin`/`plugin-host` (and everything they pull in)
//! are not compiled at all, and every call here answers "nothing installed" —
//! so the rest of the daemon needs no conditional compilation of its own, and
//! a locked-down build is a build-flag decision rather than a code path
//! anyone has to remember to take.
//!
//! Build the locked-down artifact with
//! `cargo build -p daemon --no-default-features`. Not `--workspace`: cargo
//! unifies features across the graph, so any other member enabling `plugins`
//! turns it back on.

use crate::config::DaemonPaths;
use std::collections::HashMap;
use std::sync::Arc;

/// What `daemon.doctor` reports, so "does this binary have plugins" is
/// answerable from the running process rather than from release notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginStatus {
    /// The subsystem was not compiled into this binary.
    CompiledOut,
    /// Compiled in and active.
    Enabled,
    /// Compiled in, switched off by configuration.
    DisabledByPolicy,
}

impl PluginStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CompiledOut => "compiled-out",
            Self::Enabled => "enabled",
            Self::DisabledByPolicy => "disabled-by-policy",
        }
    }
}

/// Error returned by every management call in a build without plugins.
pub const DISABLED_MESSAGE: &str = "plugins are not available in this build";

#[cfg(feature = "plugins")]
mod imp {
    use super::*;
    use plugin_host::InstalledPlugins;
    use runtime::agent_tool::AgentTypeDefinition;
    use runtime::plugin_host::PluginHost;
    use std::sync::RwLock;

    pub struct PluginSubsystem {
        paths: Arc<dyn DaemonPaths>,
        /// The directory `${workspace}` in a capability declaration resolves
        /// against.
        ///
        /// Held here, and nowhere else, because it decides how much of the
        /// filesystem a plugin was granted. Taking it from the caller at each
        /// load let startup and post-install reloads anchor to different
        /// directories, so the same declaration meant different things
        /// depending on which path had run.
        workspace: std::path::PathBuf,
        /// One engine for the whole process — it owns the compiler and its
        /// code cache, so building one per plugin would pay that repeatedly
        /// for no isolation benefit. Isolation comes from the per-call
        /// store, not from the engine.
        ///
        /// `None` when wasmtime could not be initialised at all, which
        /// leaves MCP payloads working and WASM ones absent rather than
        /// taking the daemon down.
        engine: Option<wasm_host::WasmEngine>,
        /// Fault records, kept here so they outlive the component instances
        /// a refresh rebuilds — otherwise a plugin that disabled itself came
        /// back the moment the user touched a different one.
        health: Arc<wasm_host::health::HealthRegistry>,
        /// The enabled set, refreshed by install/uninstall/enable/disable.
        /// Sessions pick this up when they are created; already-running
        /// sessions keep what they were built with, same hot-swap semantics
        /// as `config.setProvider` / `mcp.addServer`.
        /// A plain lock, not an async one: every access is a short read or
        /// swap of an `Arc` with no `.await` held across it, and keeping the
        /// accessors synchronous is what lets `SessionPool::new` — which is
        /// not async — build its agent-type catalog from them.
        active: RwLock<Arc<InstalledPlugins>>,
        enabled: bool,
        /// The `settings.plugins` policy this pool started with. Held rather
        /// than re-read, so a refresh mid-session cannot quietly widen what
        /// is allowed to load.
        policy: base::interface::settings::PluginsConfig,
    }

    impl PluginSubsystem {
        pub fn new(
            paths: Arc<dyn DaemonPaths>,
            workspace: std::path::PathBuf,
            policy: base::interface::settings::PluginsConfig,
        ) -> Self {
            let enabled = policy.enabled;
            let active = Arc::new(InstalledPlugins::new(if enabled {
                active_plugins(paths.as_ref(), &policy)
            } else {
                Vec::new()
            }));
            let engine = match wasm_host::WasmEngine::new() {
                Ok(e) => Some(e),
                Err(e) => {
                    tracing::warn!(error = %e, "wasmtime unavailable; WASM plugins will not load");
                    None
                }
            };
            Self {
                paths,
                workspace,
                engine,
                health: wasm_host::health::HealthRegistry::new(),
                policy,
                active: RwLock::new(active),
                enabled,
            }
        }

        /// Compile and interrogate every installed component.
        ///
        /// Split out of `new` because it is async and slow, and because the
        /// daemon has an async startup phase to do it in. Until this has
        /// run, plugins contribute their manifests but no tools — so it is
        /// awaited before the daemon serves, not after.
        pub async fn load_components(&self) {
            let (Some(engine), true) = (&self.engine, self.enabled) else {
                return;
            };
            let mut refreshed =
                InstalledPlugins::new(active_plugins(self.paths.as_ref(), &self.policy));
            refreshed
                .load_components_with_health(engine, &self.workspace, &self.health)
                .await;
            *self.active.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(refreshed);
        }

        pub fn status(&self) -> PluginStatus {
            if self.enabled {
                PluginStatus::Enabled
            } else {
                PluginStatus::DisabledByPolicy
            }
        }

        /// The policy in force, for diagnostics.
        pub fn policy(&self) -> &base::interface::settings::PluginsConfig {
            &self.policy
        }

        pub fn host(&self) -> Option<Arc<dyn PluginHost>> {
            let active = self.read();
            if active.is_empty() {
                return None;
            }
            Some(active as Arc<dyn PluginHost>)
        }

        pub fn agent_types(&self) -> Vec<AgentTypeDefinition> {
            self.read().agent_types()
        }

        pub fn scenes(&self) -> Vec<Arc<dyn base::interface::scene::AgentScene>> {
            use runtime::plugin_host::PluginHost;
            self.read().scenes()
        }

        pub fn mcp_servers(&self) -> HashMap<String, serde_json::Value> {
            self.read().mcp_servers().into_iter().collect()
        }

        pub fn count(&self) -> usize {
            self.read().names().len()
        }

        /// What `name` will contribute, as JSON. `null` when the plugin is
        /// not installed; an `error` field when its declarations are
        /// themselves inadmissible.
        pub fn disclosure(&self, name: &str) -> serde_json::Value {
            match self.read().disclose(name) {
                None => serde_json::Value::Null,
                Some(Err(e)) => serde_json::json!({"error": e.to_string()}),
                Some(Ok(d)) => serde_json::json!({
                    "plugin": d.plugin,
                    "version": d.version,
                    "capabilities": d.capabilities,
                    "events": d.events,
                    "scene": d.scene,
                    "mcp_servers": d.mcp_servers,
                    "model_visible": d.model_visible.iter().map(|v| serde_json::json!({
                        "origin": v.origin,
                        "text": v.text,
                    })).collect::<Vec<_>>(),
                    "inert": d.is_inert(),
                }),
            }
        }

        fn read(&self) -> Arc<InstalledPlugins> {
            self.active
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        /// Every plugin on disk with its enable state — including the
        /// disabled ones, which a management UI has to show in order to turn
        /// them back on.
        pub fn list(&self) -> Vec<serde_json::Value> {
            let (global, scene) = tier_dirs(self.paths.as_ref());
            let global_state = plugin::state::EnableState::new(global.clone());
            let scene_state = plugin::state::EnableState::new(scene.clone());
            plugin::discover_plugins(&global, &scene)
                .iter()
                .map(|p| {
                    let name = &p.manifest.plugin.name;
                    serde_json::json!({
                        "name": name,
                        "version": p.manifest.plugin.version,
                        "description": p.manifest.plugin.description,
                        "enabled": plugin::state::resolve_enabled(name, &global_state, &scene_state),
                    })
                })
                .collect()
        }

        pub async fn install(
            &self,
            name: &str,
            version: &str,
            download_url: &str,
            checksum: Option<&str>,
            scope: &str,
        ) -> Result<serde_json::Value, String> {
            let commands = self.commands_for(scope)?;
            let source = plugin::marketplace::PluginSource {
                download_url: download_url.to_string(),
                checksum: checksum.map(str::to_string),
                version: version.to_string(),
            };
            let result = commands
                .install_source(name, &source)
                .await
                .map_err(|e| e.to_string())?;

            // Ahead of the refresh, and fatal: a plugin whose components
            // cannot be compiled is one that will fail to load every time,
            // and discovering that at install — where the user is standing
            // right here — beats discovering it in the middle of a session.
            if let Err(e) = self.precompile(name).await {
                let _ = commands.uninstall(name, Some(version)).await;
                return Err(format!(
                    "installed but could not be compiled, so it was removed: {e:#}"
                ));
            }

            self.refresh().await;
            Ok(serde_json::json!({
                "success": result.success,
                "message": result.message,
                // What the plugin will contribute, for the caller to show
                // before anyone relies on it. Text reaching the model is the
                // one thing the sandbox cannot check, so it travels back
                // with the install rather than waiting to be asked for.
                "disclosure": self.disclosure(name),
            }))
        }

        pub async fn uninstall(
            &self,
            name: &str,
            version: Option<&str>,
            scope: &str,
        ) -> Result<serde_json::Value, String> {
            let commands = self.commands_for(scope)?;
            let result = commands
                .uninstall(name, version)
                .await
                .map_err(|e| e.to_string())?;
            self.refresh().await;
            Ok(serde_json::json!({
                "success": result.success,
                "message": result.message,
            }))
        }

        pub async fn set_enabled(
            &self,
            name: &str,
            enabled: bool,
            scope: &str,
        ) -> Result<serde_json::Value, String> {
            let state = plugin::state::EnableState::new(self.tier_root(scope)?);
            state
                .set_enabled(name, enabled)
                .map_err(|e| e.to_string())?;
            self.refresh().await;
            Ok(serde_json::json!({"name": name, "enabled": enabled, "scope": scope}))
        }

        /// Re-read the installed set after an install/uninstall/enable/
        /// disable. Components are reloaded too, since the point of the
        /// refresh is usually that they changed.
        pub async fn refresh(&self) {
            if !self.enabled {
                return;
            }
            self.load_components().await;
        }

        /// Compile the components of every installed version of `name`.
        ///
        /// Versions rather than "the current one" because install leaves the
        /// others in place, and which one resolves can change under a
        /// downgrade — an uncompiled sibling would then fail to load in a
        /// build that cannot compile.
        async fn precompile(&self, name: &str) -> anyhow::Result<()> {
            let (global, scene) = tier_dirs(self.paths.as_ref());
            for tier in [global, scene] {
                let cache = plugin::cache::PluginCache::new(tier.join("cache"));
                for version in cache.list_versions(name) {
                    let dir = cache.version_dir(name, &version);
                    if dir.join("plugin.toml").is_file() {
                        plugin_host::precompile_plugin(&dir).await?;
                    }
                }
            }
            Ok(())
        }

        fn tier_root(&self, scope: &str) -> Result<std::path::PathBuf, String> {
            let (global, scene) = tier_dirs(self.paths.as_ref());
            match scope {
                "global" => Ok(global),
                "scene" => Ok(scene),
                other => Err(format!(
                    "invalid scope '{other}' — expected 'global' or 'scene'"
                )),
            }
        }

        fn commands_for(&self, scope: &str) -> Result<plugin::cli::PluginCommands, String> {
            let cache = plugin::cache::PluginCache::new(self.tier_root(scope)?.join("cache"));
            Ok(plugin::cli::PluginCommands::new(cache, None))
        }
    }

    /// The two plugin-tier roots (plain `plugins/`, not `plugins/cache/` —
    /// see `plugin::cache`): global, shared across scenes, and this daemon's
    /// own scene.
    fn tier_dirs(paths: &dyn DaemonPaths) -> (std::path::PathBuf, std::path::PathBuf) {
        (
            paths.global_root().join("plugins"),
            paths.config_root().join("plugins"),
        )
    }

    fn active_plugins(
        paths: &dyn DaemonPaths,
        policy: &base::interface::settings::PluginsConfig,
    ) -> Vec<plugin::manifest::Plugin> {
        let (global, scene) = tier_dirs(paths);
        let global_state = plugin::state::EnableState::new(global.clone());
        let scene_state = plugin::state::EnableState::new(scene.clone());
        plugin::discover_plugins(&global, &scene)
            .into_iter()
            .filter(|p| {
                let name = &p.manifest.plugin.name;
                if !policy.permits(name) {
                    tracing::info!(plugin = %name, "not permitted by settings.plugins; skipping");
                    return false;
                }
                plugin::state::resolve_enabled(name, &global_state, &scene_state)
            })
            .collect()
    }
}

#[cfg(not(feature = "plugins"))]
mod imp {
    use super::*;
    use runtime::agent_tool::AgentTypeDefinition;
    use runtime::plugin_host::PluginHost;

    pub struct PluginSubsystem;

    impl PluginSubsystem {
        pub fn new(
            _paths: Arc<dyn DaemonPaths>,
            _workspace: std::path::PathBuf,
            _policy: base::interface::settings::PluginsConfig,
        ) -> Self {
            Self
        }
        pub fn status(&self) -> PluginStatus {
            PluginStatus::CompiledOut
        }
        pub fn host(&self) -> Option<Arc<dyn PluginHost>> {
            None
        }
        pub fn agent_types(&self) -> Vec<AgentTypeDefinition> {
            Vec::new()
        }
        pub fn scenes(&self) -> Vec<Arc<dyn base::interface::scene::AgentScene>> {
            Vec::new()
        }
        pub fn mcp_servers(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }
        pub fn count(&self) -> usize {
            0
        }
        pub fn list(&self) -> Vec<serde_json::Value> {
            Vec::new()
        }
        pub fn disclosure(&self, _name: &str) -> serde_json::Value {
            serde_json::Value::Null
        }
        pub async fn install(
            &self,
            _name: &str,
            _version: &str,
            _download_url: &str,
            _checksum: Option<&str>,
            _scope: &str,
        ) -> Result<serde_json::Value, String> {
            Err(DISABLED_MESSAGE.to_string())
        }
        pub async fn uninstall(
            &self,
            _name: &str,
            _version: Option<&str>,
            _scope: &str,
        ) -> Result<serde_json::Value, String> {
            Err(DISABLED_MESSAGE.to_string())
        }
        pub async fn set_enabled(
            &self,
            _name: &str,
            _enabled: bool,
            _scope: &str,
        ) -> Result<serde_json::Value, String> {
            Err(DISABLED_MESSAGE.to_string())
        }
        pub async fn refresh(&self) {}
        pub async fn load_components(&self) {}
    }
}

pub use imp::PluginSubsystem;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_strings_are_the_ones_doctor_reports() {
        assert_eq!(PluginStatus::CompiledOut.as_str(), "compiled-out");
        assert_eq!(PluginStatus::Enabled.as_str(), "enabled");
        assert_eq!(
            PluginStatus::DisabledByPolicy.as_str(),
            "disabled-by-policy"
        );
    }
}
