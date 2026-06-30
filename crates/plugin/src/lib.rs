//! AttaCore — plugin manifest loader for hooks, skills, and MCP server declarations,
//! plus marketplace integration, dependency resolution, versioned cache, and CLI commands.

pub mod agent_registry;
pub mod bundled;
pub mod cache;
pub mod cli;
pub mod homograph;
pub mod manifest;
pub mod marketplace;
pub mod resolver;

pub use manifest::{Plugin, PluginError, PluginManifest, SlashEntry};

/// Initialize all built-in plugins and install them into the runtime.
///
/// `init_builtin_plugins` is called during daemon startup. It:
/// 1. Creates in-memory `Plugin` instances for each built-in
/// 2. Calls `Plugin::install()` on each with the provided runtime hooks
///
/// Errors during installation are logged; a single plugin failure does
/// not prevent others from being installed.
pub async fn init_builtin_plugins(
    hook_runner: &mut hooks::HookRunner,
    command_registrar: &mut impl manifest::SlashCommandRegistrar,
    mcp_manager: &mut mcp::manager::McpManager,
    agent_registry: &mut agent_registry::AgentRegistry,
) {
    for plugin in bundled::builtin_plugins() {
        let name = plugin.manifest.plugin.name.clone();
        if let Err(e) = plugin
            .install(hook_runner, command_registrar, mcp_manager, agent_registry)
            .await
        {
            tracing::warn!(plugin = %name, error = %e, "Failed to install built-in plugin");
        } else {
            tracing::info!(plugin = %name, "Built-in plugin installed");
        }
    }
}
