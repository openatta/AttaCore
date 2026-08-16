//! Binds installed plugins to the engine's [`PluginHost`] seam.
//!
//! This crate is the only place that knows both halves: `plugin`'s on-disk
//! manifests on one side, `runtime`'s engine types on the other. Keeping the
//! translation here is what lets `runtime` stay free of any plugin dependency
//! and lets the whole subsystem be an optional dependency of the host — see
//! `runtime::plugin_host`.

pub mod events;
pub mod scene;

pub use events::WasmEvents;
pub use scene::PluginScene;

use runtime::agent_tool::{AgentTypeDefinition, AgentTypeSource};
use runtime::plugin_host::PluginHost;
use std::path::Path;
use std::sync::Arc;
use wasm_host::{ResolvedCapabilities, WasmEngine, WasmToolAdapter};

/// Every enabled plugin this daemon discovered, presented to the engine as
/// one host.
pub struct InstalledPlugins {
    plugins: Vec<plugin::manifest::Plugin>,
    /// Adapters for the tools each plugin's components export. Empty until
    /// [`load_components`](Self::load_components) has run — a manifest says
    /// what a plugin claims, only the component says what it provides.
    tools: Vec<Arc<dyn base::tool::Tool>>,
    /// Scenes plugins own, built once at load. Constructed alongside the
    /// tools because a plugin's own scene lists its own tools in
    /// `extra_tools`, and that list is only known after the components have
    /// been asked.
    scenes: Vec<Arc<dyn base::interface::scene::AgentScene>>,
    /// Loaded components by plugin name, so the event executor can find the
    /// one a `HookConfig::Wasm` entry names.
    instances: std::collections::HashMap<String, Arc<wasm_host::PluginInstance>>,
}

impl InstalledPlugins {
    pub fn new(plugins: Vec<plugin::manifest::Plugin>) -> Self {
        Self {
            plugins,
            tools: Vec::new(),
            scenes: Vec::new(),
            instances: std::collections::HashMap::new(),
        }
    }

    /// Compile, link and interrogate every declared WASM component.
    ///
    /// Separate from construction because it is async and slow — components
    /// are compiled here (or read from the AOT cache) and asked for their
    /// tool list. A plugin that fails any of these steps is dropped with a
    /// warning and the others still load: one broken package in a
    /// marketplace install must not cost the user the rest.
    pub async fn load_components(&mut self, engine: &WasmEngine, workspace: &Path) {
        let mut tools: Vec<Arc<dyn base::tool::Tool>> = Vec::new();
        let mut scenes: Vec<Arc<dyn base::interface::scene::AgentScene>> = Vec::new();
        let mut instances = std::collections::HashMap::new();

        for p in &self.plugins {
            let mut own_tools: Vec<Arc<dyn base::tool::Tool>> = Vec::new();
            for payload in &p.manifest.wasm {
                match load_payload(engine, p, payload, workspace).await {
                    Ok((instance, mut loaded)) => {
                        own_tools.append(&mut loaded);
                        instances.insert(p.name().to_string(), instance);
                    }
                    Err(e) => tracing::warn!(
                        plugin = %p.name(),
                        component = %payload.component.display(),
                        error = %e,
                        "plugin component could not be loaded; skipping it"
                    ),
                }
            }
            // A plugin's own scene gets its own tools unconditionally: it is
            // the plugin's scene, so being able to name a tool it shipped is
            // not a privilege, it is the point.
            if let Some(scene) = crate::scene::PluginScene::from_plugin(p, own_tools.clone()) {
                scenes.push(Arc::new(scene));
            }
            tools.append(&mut own_tools);
        }

        self.tools = tools;
        self.scenes = scenes;
        self.instances = instances;
    }



    /// Scene ids this host contributes, for the caller that has to register
    /// and later withdraw them.
    pub fn scene_ids(&self) -> Vec<String> {
        self.scenes.iter().map(|s| s.id().to_string()).collect()
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
        self.tools.clone()
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

    /// Only from plugins whose components actually loaded: a manifest that
    /// subscribes to an event but whose component failed to compile would
    /// otherwise register a hook that can never answer, and every event it
    /// claimed would pay a dispatch for nothing.
    fn hook_configs(&self) -> Vec<(hooks::config::HookEvent, hooks::config::HookConfig)> {
        self.plugins
            .iter()
            .filter(|p| self.instances.contains_key(p.name()))
            .flat_map(crate::events::hook_configs_for)
            .collect()
    }

    fn scenes(&self) -> Vec<Arc<dyn base::interface::scene::AgentScene>> {
        self.scenes.clone()
    }

    fn hook_executor(&self) -> Option<Arc<dyn hooks::runner::WasmHookExecutor>> {
        if self.instances.is_empty() {
            return None;
        }
        Some(Arc::new(crate::events::WasmEvents::new(
            self.instances.clone(),
        )))
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

/// Load one `[[wasm]]` payload and adapt each tool it exports.
async fn load_payload(
    engine: &WasmEngine,
    plugin: &plugin::manifest::Plugin,
    payload: &plugin::manifest::WasmPayload,
    workspace: &Path,
) -> anyhow::Result<(Arc<wasm_host::PluginInstance>, Vec<Arc<dyn base::tool::Tool>>)> {
    let caps = Arc::new(ResolvedCapabilities::resolve(
        &payload.capabilities,
        workspace,
        &plugin.root,
    )?);
    let component = engine.load(&plugin.path(&payload.component), &plugin.root)?;
    let instance = Arc::new(wasm_host::PluginInstance::link(
        engine,
        &component,
        plugin.name().to_string(),
        caps,
    )?);

    // Nothing has cancelled anything yet — this runs at load, before any
    // session exists to withdraw the request.
    let cancel = tokio_util::sync::CancellationToken::new();
    let defs = instance
        .list_tools(&cancel)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let tools = defs
        .iter()
        .map(|def| {
            Arc::new(WasmToolAdapter::new(instance.clone(), plugin.name(), def))
                as Arc<dyn base::tool::Tool>
        })
        .collect();
    Ok((instance, tools))
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

#[cfg(test)]
mod component_tests {
    use super::*;

    fn fixture_component() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/wasm_echo_plugin/target/wasm32-wasip2/release/wasm_echo_plugin.wasm")
    }

    /// The whole point of the seam: a manifest on disk turns into tools the
    /// engine can register, named the way permission rules expect.
    #[tokio::test]
    async fn a_declared_component_becomes_registered_tools() {
        let component = fixture_component();
        if !component.exists() {
            // The fixture is built by wasm-host's own integration test; this
            // one does not build it, because a unit test suite should not
            // shell out to cargo.
            eprintln!("skipping: build the fixture first (cargo test -p wasm-host)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(&component, dir.path().join("echo.wasm")).unwrap();
        std::fs::write(
            dir.path().join("plugin.toml"),
            r#"
[plugin]
name = "echo-plugin"
version = "1.0.0"
api_version = "0.1"

[[wasm]]
component = "echo.wasm"
"#,
        )
        .unwrap();
        let p =
            plugin::manifest::Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();

        let mut host = InstalledPlugins::new(vec![p]);
        assert!(
            host.tools().is_empty(),
            "a manifest alone says nothing about what a component provides"
        );

        let engine = WasmEngine::new().unwrap();
        host.load_components(&engine, dir.path()).await;

        let names: Vec<String> = host.tools().iter().map(|t| t.name().to_string()).collect();
        assert!(
            names.contains(&"plugin__echo-plugin__echo".to_string()),
            "{names:?}"
        );
        assert!(
            host.tools().iter().all(|t| t.is_deferred()),
            "plugin tools default to deferred so their schemas cost nothing per turn"
        );
    }

    /// One unloadable component must not cost the user the plugins that work.
    #[tokio::test]
    async fn a_broken_component_is_skipped_and_the_rest_still_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.wasm"), b"not a component").unwrap();
        std::fs::write(
            dir.path().join("plugin.toml"),
            r#"
[plugin]
name = "broken"
version = "1.0.0"
api_version = "0.1"

[[wasm]]
component = "broken.wasm"
"#,
        )
        .unwrap();
        let p =
            plugin::manifest::Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();

        let mut host = InstalledPlugins::new(vec![p]);
        let engine = WasmEngine::new().unwrap();
        host.load_components(&engine, dir.path()).await;

        assert!(host.tools().is_empty());
        assert_eq!(
            host.names(),
            ["broken"],
            "the plugin is still known; only its tools are missing"
        );
    }
}

#[cfg(test)]
mod unload_tests {
    use super::*;

    fn fixture_component() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/wasm_echo_plugin/target/wasm32-wasip2/release/wasm_echo_plugin.wasm")
    }

    fn write_plugin(dir: &Path, component: &std::path::Path) {
        std::fs::copy(component, dir.join("echo.wasm")).unwrap();
        std::fs::write(
            dir.join("plugin.toml"),
            r#"
[plugin]
name = "echo-plugin"
version = "1.0.0"
api_version = "0.1"

[[wasm]]
component = "echo.wasm"
events = ["PreToolUse"]
"#,
        )
        .unwrap();
    }

    /// Unloading is dropping the host: the instances go, and with them the
    /// key-value namespace a plugin accumulated. Nothing has to be
    /// remembered and cleaned up separately, which is the point of keeping
    /// that state on the host side in the first place.
    #[tokio::test]
    async fn replacing_the_host_takes_a_plugins_state_with_it() {
        let component = fixture_component();
        if !component.exists() {
            eprintln!("skipping: build the fixture first (cargo test -p wasm-host)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        write_plugin(dir.path(), &component);
        let p =
            plugin::manifest::Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();
        let engine = WasmEngine::new().unwrap();

        let mut host = InstalledPlugins::new(vec![p.clone()]);
        host.load_components(&engine, dir.path()).await;

        // `remember` reports the value the *previous* call stored, which
        // makes it the tool that can tell whether state survived a reload.
        let remember = |h: &InstalledPlugins| {
            h.tools()
                .into_iter()
                .find(|t| t.name() == "plugin__echo-plugin__remember")
                .expect("the fixture exports `remember`")
        };
        let call = |tool: Arc<dyn base::tool::Tool>, value: &str, root: &Path| {
            let input = serde_json::json!({ "value": value });
            let ctx = base::tool::ToolContext::for_test(root.to_path_buf());
            async move {
                let r = tool
                    .call(input, ctx, base::tool::ProgressSender::noop("t"))
                    .await
                    .expect("the fixture answers");
                match r.content {
                    base::tool::ToolResultContent::Text(s) => s,
                    other => panic!("expected text, got {other:?}"),
                }
            }
        };

        assert_eq!(
            call(remember(&host), "remembered", dir.path()).await,
            "(none)",
            "a freshly loaded plugin has nothing stored"
        );
        assert_eq!(
            call(remember(&host), "again", dir.path()).await,
            "remembered",
            "within one host, state carries between calls"
        );

        // A fresh host over the same plugin is what install/uninstall/enable
        // produces, and it starts with nothing.
        let mut reloaded = InstalledPlugins::new(vec![p]);
        reloaded.load_components(&engine, dir.path()).await;
        assert_eq!(
            call(remember(&reloaded), "after", dir.path()).await,
            "(none)",
            "a reloaded plugin must not inherit the previous one's state"
        );
    }

    /// A plugin whose component failed to load subscribes to nothing, so the
    /// dispatcher does not pay for events that can never be answered.
    #[tokio::test]
    async fn a_plugin_with_no_loaded_component_registers_no_hooks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.wasm"), b"not a component").unwrap();
        std::fs::write(
            dir.path().join("plugin.toml"),
            r#"
[plugin]
name = "broken"
version = "1.0.0"
api_version = "0.1"

[[wasm]]
component = "broken.wasm"
events = ["PreToolUse"]
"#,
        )
        .unwrap();
        let p =
            plugin::manifest::Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();

        let mut host = InstalledPlugins::new(vec![p]);
        host.load_components(&WasmEngine::new().unwrap(), dir.path())
            .await;

        assert!(host.hook_configs().is_empty());
        assert!(host.hook_executor().is_none());
    }
}
