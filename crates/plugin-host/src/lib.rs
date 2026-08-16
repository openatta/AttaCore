//! Binds installed plugins to the engine's [`PluginHost`] seam.
//!
//! This crate is the only place that knows both halves: `plugin`'s on-disk
//! manifests on one side, `runtime`'s engine types on the other. Keeping the
//! translation here is what lets `runtime` stay free of any plugin dependency
//! and lets the whole subsystem be an optional dependency of the host — see
//! `runtime::plugin_host`.

use runtime::agent_tool::{AgentTypeDefinition, AgentTypeSource};
use runtime::plugin_host::PluginHost;
use std::path::Path;
use std::sync::Arc;

/// Every enabled plugin this daemon discovered, presented to the engine as
/// one host.
pub struct InstalledPlugins {
    plugins: Vec<plugin::manifest::Plugin>,
}

impl InstalledPlugins {
    pub fn new(plugins: Vec<plugin::manifest::Plugin>) -> Self {
        Self { plugins }
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    pub fn names(&self) -> Vec<&str> {
        self.plugins
            .iter()
            .map(|p| p.manifest.plugin.name.as_str())
            .collect()
    }
}

impl PluginHost for InstalledPlugins {
    fn tools(&self) -> Vec<Arc<dyn base::tool::Tool>> {
        Vec::new()
    }

    /// Keyed `{plugin}-mcp-{declared name}` so two plugins declaring a
    /// server each don't collide when the host merges these into its own
    /// configured set.
    ///
    /// Only `kind = "native"` payloads produce a config here. A `dsh` payload
    /// names a JS entry module rather than a server, and reaches the host as
    /// an `atta-dsh-bridge` invocation — see that bridge's own docs.
    fn mcp_servers(&self) -> Vec<(String, serde_json::Value)> {
        let mut out = Vec::new();
        for p in &self.plugins {
            for server in &p.manifest.mcp {
                let Some(rel) = server.config.as_ref() else {
                    continue;
                };
                let path = p.path(rel);
                let key = format!("{}-mcp-{}", p.name(), server.name);
                match std::fs::read_to_string(&path) {
                    Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
                        Ok(v) => out.push((key, v)),
                        Err(e) => tracing::warn!(
                            plugin = %p.name(),
                            path = %path.display(),
                            error = %e,
                            "plugin MCP server config is not valid JSON, skipping"
                        ),
                    },
                    Err(e) => tracing::warn!(
                        plugin = %p.name(),
                        path = %path.display(),
                        error = %e,
                        "failed to read plugin MCP server config, skipping"
                    ),
                }
            }
        }
        out
    }

    fn hook_configs(&self) -> Vec<(hooks::config::HookEvent, hooks::config::HookConfig)> {
        Vec::new()
    }

    fn scenes(&self) -> Vec<Arc<dyn base::interface::scene::AgentScene>> {
        Vec::new()
    }

    fn agent_types(&self) -> Vec<AgentTypeDefinition> {
        self.plugins
            .iter()
            .flat_map(|p| {
                p.manifest
                    .agent
                    .iter()
                    .filter_map(|def| agent_def_to_type(def, &p.root))
            })
            .collect()
    }
}

/// Convert a plugin-declared `AgentDef` into a runtime [`AgentTypeDefinition`],
/// reading its system prompt file relative to the plugin's root.
///
/// Returns `None` (with a `tracing::warn!`) when the prompt file can't be
/// read, so one bad definition doesn't break agent type resolution for
/// everyone else.
///
/// The result carries [`AgentTypeSource::Plugin`], which is what clamps its
/// `permission_mode` / `max_turns` overrides — a definition that arrived over
/// the network must not be able to hand its sub-agent more than the session
/// already had.
pub fn agent_def_to_type(
    def: &plugin::manifest::AgentDef,
    plugin_root: &Path,
) -> Option<AgentTypeDefinition> {
    let prompt_path = plugin_root.join(&def.prompt);
    let system_prompt = match std::fs::read_to_string(&prompt_path) {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!(
                agent = %def.name,
                path = %prompt_path.display(),
                error = %e,
                "plugin agent type: failed to read system prompt, skipping"
            );
            return None;
        }
    };

    let permission_mode = def.permission_mode.as_deref().and_then(|raw| {
        let parsed = runtime::agent_tool::parse_permission_mode(raw);
        if parsed.is_none() {
            tracing::warn!(
                agent = %def.name,
                permission_mode = %raw,
                "plugin agent type: unrecognized permission_mode, ignoring"
            );
        }
        parsed
    });

    Some(AgentTypeDefinition {
        name: def.name.clone(),
        source: AgentTypeSource::Plugin,
        description: def.description.clone(),
        allowed_tools: def.allowed_tools.clone(),
        disallowed_tools: def.disallowed_tools.clone(),
        model: def.model.clone(),
        permission_mode,
        effort: def.effort.clone(),
        max_turns: def.max_turns,
        scene: def.scene.clone(),
        system_prompt,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "[plugin]\nname = \"p\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n";

    fn load(root: &Path, body: &str) -> plugin::manifest::Plugin {
        std::fs::write(root.join("plugin.toml"), format!("{HEAD}{body}")).unwrap();
        plugin::manifest::Plugin::load(root, &root.join("plugin.toml")).unwrap()
    }

    fn agent_body(prompt_rel: &str) -> String {
        format!(
            r#"
[[agent]]
name = "helper"
description = "A plugin-declared agent"
prompt = "{prompt_rel}"
allowed_tools = ["Read"]
disallowed_tools = ["Bash"]
model = "claude-opus-5"
permission_mode = "plan"
effort = "high"
max_turns = 30
scene = "plugin:p"
"#
        )
    }

    /// Every field the manifest offers has to arrive in the engine type. A
    /// field that parses but is never mapped is worse than one that doesn't
    /// exist: the author sees it accepted and assumes it took effect.
    #[test]
    fn every_declared_agent_field_reaches_the_engine_type() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("prompt.md"), "You are a plugin agent.").unwrap();
        let p = load(dir.path(), &agent_body("prompt.md"));

        let types = InstalledPlugins::new(vec![p]).agent_types();
        assert_eq!(types.len(), 1);
        let t = &types[0];

        assert_eq!(t.name, "helper");
        assert_eq!(t.system_prompt, "You are a plugin agent.");
        assert_eq!(t.allowed_tools, ["Read"]);
        assert_eq!(t.disallowed_tools, ["Bash"]);
        assert_eq!(t.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(
            t.permission_mode,
            Some(base::interface::settings::PermissionMode::Plan)
        );
        assert_eq!(t.effort.as_deref(), Some("high"));
        assert_eq!(t.max_turns, Some(30));
        assert_eq!(t.scene.as_deref(), Some("plugin:p"));
        assert_eq!(
            t.source,
            AgentTypeSource::Plugin,
            "provenance is what clamps this definition's permission overrides"
        );
    }

    #[test]
    fn an_unreadable_prompt_skips_that_definition_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let p = load(dir.path(), &agent_body("does-not-exist.md"));
        assert!(InstalledPlugins::new(vec![p]).agent_types().is_empty());
    }

    /// An unrecognized mode is dropped, not guessed at — and dropping it
    /// leaves the sub-agent on the session's inherited mode, which is the
    /// safe direction.
    #[test]
    fn an_unrecognized_permission_mode_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("prompt.md"), "body").unwrap();
        let p = load(
            dir.path(),
            r#"
[[agent]]
name = "helper"
description = "d"
prompt = "prompt.md"
permission_mode = "make-me-root"
"#,
        );
        let types = InstalledPlugins::new(vec![p]).agent_types();
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].permission_mode, None);
    }

    #[test]
    fn native_mcp_payloads_are_namespaced_and_dsh_ones_are_not_servers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("mcp")).unwrap();
        std::fs::write(
            dir.path().join("mcp/server.json"),
            r#"{"command":"echo","args":[]}"#,
        )
        .unwrap();
        let p = load(
            dir.path(),
            r#"
[[mcp]]
name = "github"
kind = "native"
config = "mcp/server.json"

[[mcp]]
name = "pr-helper"
kind = "dsh"
entry = "dist/index.js"
"#,
        );

        let servers = InstalledPlugins::new(vec![p]).mcp_servers();
        assert_eq!(
            servers.len(),
            1,
            "a dsh payload names a JS module, not an MCP server"
        );
        assert_eq!(servers[0].0, "p-mcp-github");
        assert_eq!(servers[0].1["command"], "echo");
    }

    /// A host with nothing installed must be indistinguishable from having no
    /// host at all — that equivalence is what makes the subsystem removable.
    #[test]
    fn an_empty_host_contributes_nothing() {
        let host = InstalledPlugins::new(Vec::new());
        assert!(host.is_empty());
        assert!(host.agent_types().is_empty());
        assert!(host.mcp_servers().is_empty());
        assert!(host.tools().is_empty());
        assert!(host.scenes().is_empty());
        assert!(host.hook_configs().is_empty());
    }

    /// The manifest names events as strings because `plugin` has no hooks
    /// dependency. This is the check that the two lists have not drifted.
    #[test]
    fn every_subscribable_event_name_resolves_to_a_real_hook_event() {
        for name in plugin::manifest::SUBSCRIBABLE_EVENTS {
            let parsed: Result<hooks::config::HookEvent, _> =
                serde_json::from_value(serde_json::Value::String((*name).to_string()));
            assert!(parsed.is_ok(), "`{name}` is not a real HookEvent");
        }
    }
}
