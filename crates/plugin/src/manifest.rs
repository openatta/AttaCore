//! plugin.toml schema + Plugin struct.

use base::frozen::{SkillEntry, SkillSource};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone)]
pub struct PluginManifest {
    pub plugin: PluginMeta,
    #[serde(default)]
    pub skills: SkillsSection,
    /// Map of "/<name>" → relative path to a markdown prompt file
    #[serde(default)]
    pub slash_commands: HashMap<String, String>,
    #[serde(default)]
    pub mcp: McpSection,
    #[serde(default)]
    pub hooks: HooksSection,
    /// Plugin-defined agent types
    #[serde(default)]
    pub agents: Vec<AgentDef>,
    /// Output style .md files shipped with this plugin
    #[serde(default)]
    pub output_styles: Vec<PathBuf>,
    /// Conditional skills activated by file path matching
    #[serde(default)]
    pub conditional_skills: Vec<ConditionalSkill>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PluginMeta {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub homepage: String,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct SkillsSection {
    /// Relative paths to SKILL.md-style files inside this plugin's directory
    #[serde(default)]
    pub include: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct McpSection {
    /// Relative paths to MCP server config JSON files inside this plugin's dir
    #[serde(default)]
    pub servers: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct HooksSection {
    pub pre_tool_use: Option<String>,
    pub post_tool_use: Option<String>,
    pub session_start: Option<String>,
    pub stop: Option<String>,
    pub pre_compact: Option<String>,
    pub post_compact: Option<String>,
    pub user_prompt_submit: Option<String>,
    pub subagent_start: Option<String>,
    pub subagent_stop: Option<String>,
}

/// Plugin-defined agent type. Agents from plugins are discoverable via
/// AgentTool's `subagent_type` parameter.
#[derive(Debug, Deserialize, Clone)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub system_prompt_path: PathBuf,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// Conditional skill: activates when a file matching `path_pattern` (glob)
/// is read or written during a turn.
#[derive(Debug, Deserialize, Clone)]
pub struct ConditionalSkill {
    pub path_pattern: String,
    pub skill_name: String,
    pub description: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("plugin schema: {0}")]
    Schema(String),
    #[error("homograph: {0}")]
    Homograph(String),
}

#[derive(Debug, Clone)]
pub struct SlashEntry {
    pub name: String,
    pub prompt_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Plugin {
    /// Root directory of the plugin
    pub root: PathBuf,
    /// Parsed manifest
    pub manifest: PluginManifest,
    /// Resolved absolute paths to skill files
    pub skill_paths: Vec<PathBuf>,
    /// Resolved slash command entries with absolute prompt paths
    pub slash_entries: Vec<SlashEntry>,
    /// Resolved absolute paths to MCP server config JSON files
    pub mcp_server_paths: Vec<PathBuf>,
}

impl Plugin {
    pub fn load(root: &Path, manifest_path: &Path) -> Result<Self, PluginError> {
        let raw = std::fs::read_to_string(manifest_path)?;
        let manifest: PluginManifest = toml::from_str(&raw)?;
        if manifest.plugin.name.trim().is_empty() {
            return Err(PluginError::Schema("plugin.name must not be empty".into()));
        }

        let skill_paths: Vec<PathBuf> = manifest
            .skills
            .include
            .iter()
            .map(|rel| root.join(rel))
            .filter(|p| p.exists())
            .collect();

        let slash_entries: Vec<SlashEntry> = manifest
            .slash_commands
            .iter()
            .map(|(name, rel)| SlashEntry {
                name: name.clone(),
                prompt_path: root.join(rel),
            })
            .filter(|e| e.prompt_path.exists())
            .collect();

        let mcp_server_paths: Vec<PathBuf> = manifest
            .mcp
            .servers
            .iter()
            .map(|rel| root.join(rel))
            .filter(|p| p.exists())
            .collect();

        Ok(Plugin {
            root: root.to_path_buf(),
            manifest,
            skill_paths,
            slash_entries,
            mcp_server_paths,
        })
    }

    /// Construct a Plugin directly from a manifest (e.g. for built-in plugins
    /// that don't live on disk). Paths that would normally be resolved from
    /// disk are left empty — the caller is responsible for providing
    /// file contents through alternative mechanisms (e.g. in-memory bodies).
    pub fn from_manifest(manifest: PluginManifest) -> Self {
        Plugin {
            root: PathBuf::from(format!("(builtin:{})", manifest.plugin.name)),
            manifest,
            skill_paths: Vec::new(),
            slash_entries: Vec::new(),
            mcp_server_paths: Vec::new(),
        }
    }

    /// Register all plugin components with the runtime.
    ///
    /// - Hooks from `self.manifest.hooks` are registered with the `HookRunner`.
    /// - Slash commands from `self.manifest.slash_commands` are registered via
    ///   the `SlashCommandRegistrar`.
    /// - MCP server configs from `self.manifest.mcp` are connected via `McpManager`.
    /// - Agent types from `self.manifest.agents` are registered with the `AgentRegistry`.
    pub async fn install(
        &self,
        hook_runner: &mut hooks::HookRunner,
        command_registrar: &mut impl SlashCommandRegistrar,
        mcp_manager: &mut mcp::manager::McpManager,
        agent_registry: &mut crate::agent_registry::AgentRegistry,
    ) -> Result<(), PluginError> {
        let name = &self.manifest.plugin.name;
        let root = &self.root;

        // 1. Register hooks
        self.install_hooks(hook_runner, root)?;

        // 2. Register slash commands
        self.install_slash_commands(command_registrar, name, root);

        // 3. Register MCP servers
        self.install_mcp_servers(mcp_manager, name, root).await;

        // 4. Register agent definitions
        self.install_agents(agent_registry);

        tracing::info!(plugin = %name, "Plugin installed");
        Ok(())
    }

    fn install_hooks(
        &self,
        hook_runner: &mut hooks::HookRunner,
        root: &Path,
    ) -> Result<(), PluginError> {
        let h = &self.manifest.hooks;
        if let Some(ref path) = h.pre_tool_use {
            let script_path = root.join(path);
            if script_path.exists() {
                let content = std::fs::read_to_string(&script_path)?;
                hook_runner.register_hook(
                    hooks::config::HookEvent::PreToolUse,
                    hooks::config::HookConfig::Command {
                        command: content,
                        shell: None,
                        timeout: None,
                        if_pattern: None,
                        only_on_error: None,
                        once: Some(true),
                        async_rewake: None,
                    },
                );
            }
        }
        if let Some(ref path) = h.post_tool_use {
            let script_path = root.join(path);
            if script_path.exists() {
                let content = std::fs::read_to_string(&script_path)?;
                hook_runner.register_hook(
                    hooks::config::HookEvent::PostToolUse,
                    hooks::config::HookConfig::Command {
                        command: content,
                        shell: None,
                        timeout: None,
                        if_pattern: None,
                        only_on_error: None,
                        once: Some(true),
                        async_rewake: None,
                    },
                );
            }
        }
        if let Some(ref path) = h.session_start {
            let script_path = root.join(path);
            if script_path.exists() {
                let content = std::fs::read_to_string(&script_path)?;
                hook_runner.register_hook(
                    hooks::config::HookEvent::SessionStart,
                    hooks::config::HookConfig::Command {
                        command: content,
                        shell: None,
                        timeout: None,
                        if_pattern: None,
                        only_on_error: None,
                        once: Some(true),
                        async_rewake: None,
                    },
                );
            }
        }
        if let Some(ref path) = h.stop {
            let script_path = root.join(path);
            if script_path.exists() {
                let content = std::fs::read_to_string(&script_path)?;
                hook_runner.register_hook(
                    hooks::config::HookEvent::Stop,
                    hooks::config::HookConfig::Command {
                        command: content,
                        shell: None,
                        timeout: None,
                        if_pattern: None,
                        only_on_error: None,
                        once: Some(true),
                        async_rewake: None,
                    },
                );
            }
        }
        if let Some(ref path) = h.pre_compact {
            let script_path = root.join(path);
            if script_path.exists() {
                let content = std::fs::read_to_string(&script_path)?;
                hook_runner.register_hook(
                    hooks::config::HookEvent::PreCompact,
                    hooks::config::HookConfig::Command {
                        command: content,
                        shell: None,
                        timeout: None,
                        if_pattern: None,
                        only_on_error: None,
                        once: Some(true),
                        async_rewake: None,
                    },
                );
            }
        }
        if let Some(ref path) = h.post_compact {
            let script_path = root.join(path);
            if script_path.exists() {
                let content = std::fs::read_to_string(&script_path)?;
                hook_runner.register_hook(
                    hooks::config::HookEvent::PostCompact,
                    hooks::config::HookConfig::Command {
                        command: content,
                        shell: None,
                        timeout: None,
                        if_pattern: None,
                        only_on_error: None,
                        once: Some(true),
                        async_rewake: None,
                    },
                );
            }
        }
        if let Some(ref path) = h.user_prompt_submit {
            let script_path = root.join(path);
            if script_path.exists() {
                let content = std::fs::read_to_string(&script_path)?;
                hook_runner.register_hook(
                    hooks::config::HookEvent::UserPromptSubmit,
                    hooks::config::HookConfig::Command {
                        command: content,
                        shell: None,
                        timeout: None,
                        if_pattern: None,
                        only_on_error: None,
                        once: Some(true),
                        async_rewake: None,
                    },
                );
            }
        }
        if let Some(ref path) = h.subagent_start {
            let script_path = root.join(path);
            if script_path.exists() {
                let content = std::fs::read_to_string(&script_path)?;
                hook_runner.register_hook(
                    hooks::config::HookEvent::SubagentStart,
                    hooks::config::HookConfig::Command {
                        command: content,
                        shell: None,
                        timeout: None,
                        if_pattern: None,
                        only_on_error: None,
                        once: Some(true),
                        async_rewake: None,
                    },
                );
            }
        }
        if let Some(ref path) = h.subagent_stop {
            let script_path = root.join(path);
            if script_path.exists() {
                let content = std::fs::read_to_string(&script_path)?;
                hook_runner.register_hook(
                    hooks::config::HookEvent::SubagentStop,
                    hooks::config::HookConfig::Command {
                        command: content,
                        shell: None,
                        timeout: None,
                        if_pattern: None,
                        only_on_error: None,
                        once: Some(true),
                        async_rewake: None,
                    },
                );
            }
        }
        Ok(())
    }

    fn install_slash_commands(
        &self,
        command_registrar: &mut impl SlashCommandRegistrar,
        plugin_name: &str,
        root: &Path,
    ) {
        for (cmd_name, prompt_rel_path) in &self.manifest.slash_commands {
            let path_str = root.to_string_lossy();
            if path_str.starts_with("(builtin:") {
                // Built-in plugins: store body inline via a synthetic path
                // that the command expansion resolves through fallback logic.
                let entry = SkillEntry {
                    name: cmd_name.trim_start_matches('/').to_string(),
                    description: format!("Plugin slash command: {cmd_name}"),
                    source: SkillSource::Plugin,
                    path: PathBuf::from(format!("(plugin:{plugin_name}:{cmd_name})")),
                    user_invocable: true,
                    ..Default::default()
                };
                command_registrar.register_plugin_command(entry);
            } else {
                // Disk plugins: the prompt file is at root / relative_path
                let prompt_path = root.join(prompt_rel_path);
                if prompt_path.exists() {
                    let entry = SkillEntry {
                        name: cmd_name.trim_start_matches('/').to_string(),
                        description: format!("Plugin slash command: {cmd_name}"),
                        source: SkillSource::Plugin,
                        path: prompt_path,
                        user_invocable: true,
                        ..Default::default()
                    };
                    command_registrar.register_plugin_command(entry);
                }
            }
        }
    }

    async fn install_mcp_servers(
        &self,
        mcp_manager: &mut mcp::manager::McpManager,
        plugin_name: &str,
        root: &Path,
    ) {
        for (idx, rel_path) in self.manifest.mcp.servers.iter().enumerate() {
            let config_path = root.join(rel_path);
            match std::fs::read_to_string(&config_path) {
                Ok(json_str) => {
                    match serde_json::from_str::<mcp::config::McpServerConfig>(&json_str) {
                        Ok(config) => {
                            let server_name = format!("{plugin_name}-mcp-{idx}");
                            mcp_manager.add_server(&server_name, &config).await;
                        }
                        Err(e) => {
                            tracing::warn!(
                                plugin = %plugin_name,
                                path = %rel_path,
                                error = %e,
                                "Failed to parse MCP server config for plugin"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        plugin = %plugin_name,
                        path = %rel_path,
                        error = %e,
                        "Failed to read MCP server config for plugin"
                    );
                }
            }
        }
    }

    fn install_agents(&self, agent_registry: &mut crate::agent_registry::AgentRegistry) {
        for def in &self.manifest.agents {
            agent_registry.register(def.clone());
        }
    }
}

/// Trait for registering slash commands discovered from plugins.
///
/// Separated from `CommandRegistry` (in the runtime crate) to avoid
/// a circular dependency: plugin → runtime → plugin.
pub trait SlashCommandRegistrar {
    /// Register a skill entry as a plugin slash command.
    fn register_plugin_command(&mut self, entry: SkillEntry);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_manifest() {
        let toml_str = r#"
[plugin]
name = "test-plugin"
version = "0.1.0"
"#;
        let m: PluginManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(m.plugin.name, "test-plugin");
        assert!(m.slash_commands.is_empty());
    }

    #[test]
    fn parse_full_manifest() {
        let toml_str = r#"
[plugin]
name = "code-review-helper"
version = "1.0.0"
description = "Adds /review"

[skills]
include = ["SKILL.md"]

[slash_commands]
"/review" = "prompts/review.md"
"/diff-summary" = "prompts/diff-summary.md"

[mcp]
servers = ["mcp_server.json"]

[hooks]
pre_tool_use = "hooks/pre.sh"
"#;
        let m: PluginManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(m.skills.include, vec!["SKILL.md"]);
        assert_eq!(m.slash_commands.len(), 2);
        assert!(m.slash_commands.contains_key("/review"));
        assert_eq!(m.mcp.servers, vec!["mcp_server.json"]);
        assert_eq!(m.hooks.pre_tool_use.as_deref(), Some("hooks/pre.sh"));
    }

    #[test]
    fn empty_name_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let m = dir.path().join("plugin.toml");
        std::fs::write(
            &m,
            r#"
[plugin]
name = ""
version = "0.1.0"
"#,
        )
        .unwrap();
        let r = Plugin::load(dir.path(), &m);
        assert!(matches!(r, Err(PluginError::Schema(_))));
    }

    #[test]
    fn load_resolves_existing_paths_only() {
        let dir = tempfile::tempdir().unwrap();
        // Plugin root setup
        std::fs::write(
            dir.path().join("plugin.toml"),
            r#"
[plugin]
name = "p1"
version = "1.0.0"

[skills]
include = ["SKILL.md", "missing.md"]

[slash_commands]
"/exists" = "prompts/exists.md"
"/missing" = "prompts/missing.md"
"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "skill body").unwrap();
        std::fs::create_dir_all(dir.path().join("prompts")).unwrap();
        std::fs::write(dir.path().join("prompts/exists.md"), "prompt").unwrap();

        let p = Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();
        assert_eq!(p.skill_paths.len(), 1);
        assert!(p.skill_paths[0].ends_with("SKILL.md"));
        assert_eq!(p.slash_entries.len(), 1);
        assert_eq!(p.slash_entries[0].name, "/exists");
    }
}
