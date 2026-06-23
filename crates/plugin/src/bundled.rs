//! Built-in plugins shipped with AttaCore.
//!
//! These plugins are registered at startup by `init_builtin_plugins()` and are
//! always available. They use `Plugin::from_manifest()` so no disk files are
//! required.
//!
//! # Plugins
//!
//! | Plugin | Description |
//! |--------|-------------|
//! | `plugin-hello` | Minimal example registering a `/hello` slash command |
//! | `plugin-mcp-tools` | Bundles common MCP server configurations |

use crate::manifest::{Plugin, PluginManifest, PluginMeta};

/// Return all built-in plugins.
pub fn builtin_plugins() -> Vec<Plugin> {
    vec![plugin_hello(), plugin_mcp_tools()]
}

/// `plugin-hello` — a minimal plugin that registers a `/hello` slash command.
///
/// The slash command prompt is built from the skill's fallback body
/// (name + description), so no disk files are needed.
fn plugin_hello() -> Plugin {
    let mut slash_commands = std::collections::HashMap::new();
    slash_commands.insert("/hello".to_string(), "_inline".to_string());

    let manifest = PluginManifest {
        plugin: PluginMeta {
            name: "plugin-hello".into(),
            version: "0.1.0".into(),
            description: "A minimal example plugin that registers a /hello slash command".into(),
            author: "AttaCore".into(),
            homepage: String::new(),
        },
        skills: Default::default(),
        slash_commands,
        mcp: Default::default(),
        hooks: Default::default(),
        agents: Vec::new(),
        output_styles: Vec::new(),
        conditional_skills: Vec::new(),
    };
    Plugin::from_manifest(manifest)
}

/// `plugin-mcp-tools` — bundles common MCP server configurations.
///
/// Provides configurations for FileSystem and WebSearch MCP servers.
/// Users can invoke these by installing the plugin, which wires the
/// MCP servers into the runtime directly.
fn plugin_mcp_tools() -> Plugin {
    // FileSystem + WebSearch MCP server configs bundled for the plugin.
    let mcp_servers: Vec<String> = vec![
        "mcp_configs/filesystem.json".to_string(),
        "mcp_configs/websearch.json".to_string(),
    ];

    let manifest = PluginManifest {
        plugin: PluginMeta {
            name: "plugin-mcp-tools".into(),
            version: "0.1.0".into(),
            description:
                "Bundles common MCP server configurations (filesystem, web search)".into(),
            author: "AttaCore".into(),
            homepage: String::new(),
        },
        skills: Default::default(),
        slash_commands: std::collections::HashMap::new(),
        mcp: crate::manifest::McpSection {
            servers: mcp_servers,
        },
        hooks: Default::default(),
        agents: Vec::new(),
        output_styles: Vec::new(),
        conditional_skills: Vec::new(),
    };
    Plugin::from_manifest(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_plugins_are_valid() {
        let plugins = builtin_plugins();
        assert_eq!(plugins.len(), 2);

        // plugin-hello
        let hello = &plugins[0];
        assert_eq!(hello.manifest.plugin.name, "plugin-hello");
        assert_eq!(hello.manifest.slash_commands.len(), 1);
        assert!(hello.manifest.slash_commands.contains_key("/hello"));

        // plugin-mcp-tools
        let mcp_tools = &plugins[1];
        assert_eq!(mcp_tools.manifest.plugin.name, "plugin-mcp-tools");
        assert_eq!(mcp_tools.manifest.mcp.servers.len(), 2);
    }

    #[test]
    fn plugin_hello_has_inline_content_path() {
        let hello = plugin_hello();
        assert!(
            hello.root.to_string_lossy().starts_with("(builtin:"),
            "Builtin plugin should have synthetic root path"
        );
    }

    #[test]
    fn plugin_mcp_tools_has_mcp_servers() {
        let mcp_tools = plugin_mcp_tools();
        assert_eq!(mcp_tools.manifest.mcp.servers[0], "mcp_configs/filesystem.json");
        assert_eq!(mcp_tools.manifest.mcp.servers[1], "mcp_configs/websearch.json");
    }
}
