//! Slash command system — intercept `/name args` before the LLM.
//!
//! TS parity: claude-code's `processSlashCommand.tsx` + `commands.ts`.
//! Supports two command types:
//! - **Prompt**: expand skill content, replace user message, continue to LLM
//! - **Local**: execute handler, return result directly, skip LLM
//!
//! Architecture:
//! ```text
//! /simplify main.rs
//!   → parse_slash_command → { name: "simplify", args: "main.rs" }
//!   → CommandRegistry::resolve("simplify")
//!   → Command::Prompt(skill_entry) → expand body + replace {args}
//!   → content = expanded prompt → continue to run_user_turn()
//! ```

use base::frozen::SkillEntry;
use std::collections::HashMap;

// ── Parsed slash command ──

/// Result of parsing a `/name args` string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: String,
    pub args: String,
}

/// Parse a slash command from user input.
/// Returns `None` if the input doesn't start with `/`.
/// TS parity: `parseSlashCommand()` in slashCommandParsing.ts.
pub fn parse_slash_command(input: &str) -> Option<SlashCommand> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let rest = &trimmed[1..]; // strip leading /
    if rest.is_empty() {
        return None; // bare "/" is not a command
    }
    // Split on first whitespace: name vs args
    let (name, args) = if let Some(space_pos) = rest.find(char::is_whitespace) {
        let (n, a) = rest.split_at(space_pos);
        (n.to_string(), a.trim().to_string())
    } else {
        (rest.to_string(), String::new())
    };
    if name.is_empty() {
        return None;
    }
    Some(SlashCommand { name, args })
}

// ── Command types ──

/// The result of executing a local command.
#[derive(Debug, Clone)]
pub struct CommandResult {
    /// Text to display to the user (goes via AgentEvent).
    pub text: String,
    /// Whether to continue to the LLM after this command.
    pub should_query: bool,
}

/// A registered slash command.
pub enum Command {
    /// Prompt command: expand skill content and feed to LLM.
    Prompt { entry: Box<SkillEntry> },
    /// Local command: execute handler, return result, skip LLM.
    Local {
        description: String,
        handler: Box<dyn Fn(&SlashCommand) -> CommandResult + Send + Sync>,
    },
}

impl Command {
    pub fn description(&self) -> &str {
        match self {
            Command::Prompt { entry } => &entry.description,
            Command::Local { description, .. } => description,
        }
    }

    pub fn is_prompt(&self) -> bool {
        matches!(self, Command::Prompt { .. })
    }
}

// ── Command registry ──

/// Registry of all available slash commands, built at session start.
pub struct CommandRegistry {
    commands: HashMap<String, Command>,
}

impl CommandRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            commands: HashMap::new(),
        }
    }

    /// Build a registry from a skill manager (disk + bundled skills).
    /// Each skill becomes a `prompt` command.
    pub fn from_skill_manager(skill_mgr: &::skills::manager::SkillManager) -> Self {
        let mut registry = Self::new();
        for skill in skill_mgr.list() {
            // Only register user-invocable skills as slash commands
            if skill.user_invocable {
                registry.insert_prompt(SkillEntry {
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                    source: match skill.source {
                        ::skills::manager::SkillSource::User => base::frozen::SkillSource::User,
                        ::skills::manager::SkillSource::Project => {
                            base::frozen::SkillSource::Project
                        }
                        ::skills::manager::SkillSource::Plugin => base::frozen::SkillSource::Plugin,
                    },
                    path: skill.path.clone(),
                    allowed_tools: skill.allowed_tools.clone(),
                    model: skill.model.clone(),
                    context: skill.context.clone(),
                    argument_hint: skill.argument_hint.clone(),
                    paths: skill.paths.clone(),
                    disable_model_invocation: skill.disable_model_invocation,
                    user_invocable: skill.user_invocable,
                    version: skill.version.clone(),
                    ..Default::default()
                });
            }
        }
        registry
    }

    /// Insert a prompt command from a SkillEntry.
    pub fn insert_prompt(&mut self, entry: SkillEntry) {
        self.commands.insert(
            entry.name.clone(),
            Command::Prompt {
                entry: Box::new(entry),
            },
        );
    }

    /// Insert a local command with a handler.
    pub fn insert_local(
        &mut self,
        name: &str,
        description: &str,
        handler: Box<dyn Fn(&SlashCommand) -> CommandResult + Send + Sync>,
    ) {
        self.commands.insert(
            name.to_string(),
            Command::Local {
                description: description.to_string(),
                handler,
            },
        );
    }

    /// Look up a command by name.
    pub fn resolve(&self, name: &str) -> Option<&Command> {
        self.commands.get(name)
    }

    /// List all commands (for /help).
    pub fn list(&self) -> Vec<(&str, &str)> {
        let mut entries: Vec<(&str, &str)> = self
            .commands
            .iter()
            .map(|(name, cmd)| (name.as_str(), cmd.description()))
            .collect();
        entries.sort_by_key(|(name, _)| *name);
        entries
    }

    /// Number of registered commands.
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

impl plugin::manifest::SlashCommandRegistrar for CommandRegistry {
    fn register_plugin_command(&mut self, entry: SkillEntry) {
        self.insert_prompt(entry);
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        let mut registry = Self::new();
        // Register built-in local commands
        registry.insert_local(
            "help",
            "List all available slash commands",
            Box::new(|_cmd| {
                // Handler is filled by the caller with registry reference
                CommandResult {
                    text: "Use /help to see available commands".into(),
                    should_query: false,
                }
            }),
        );
        registry.insert_local(
            "skills",
            "List all available skills",
            Box::new(|_cmd| CommandResult {
                text: "Use /skills to see available skills".into(),
                should_query: false,
            }),
        );
        registry.insert_local(
            "clear",
            "Clear the current session context",
            Box::new(|_cmd| CommandResult {
                text: "Session cleared. All messages have been removed.".into(),
                should_query: false,
            }),
        );
        registry.insert_local(
            "compact",
            "Trigger context compaction now",
            Box::new(|_cmd| CommandResult {
                text: "Compaction triggered. Context has been summarized.".into(),
                should_query: true,
            }),
        );
        registry.insert_local(
            "cost",
            "Show session API cost",
            Box::new(|_cmd| CommandResult {
                text: "Cost tracking: use /cost for details".into(),
                should_query: false,
            }),
        );
        registry
    }
}

// ── Skill expansion ──

/// Expand a skill entry for a slash command invocation.
/// Reads the skill body, substitutes {args}, and returns the expanded content.
/// TS parity: `getPromptForCommand()` in loadSkillsDir.ts:270-401.
pub fn expand_skill_for_command(entry: &SkillEntry, args: &str) -> String {
    // Try to read the skill file body
    let body = if entry.path.to_string_lossy().starts_with("(bundled:") {
        // Bundled skills: use the body map from skills crate
        ::skills::bundled::bundled_body(&entry.name)
            .unwrap_or_else(|| format!("# {}\n\n{}", entry.name, entry.description))
            .to_string()
    } else if entry.path.to_string_lossy().starts_with("(mcp:") {
        // MCP skills: use the in-memory body from mcp_builder
        ::skills::mcp_builder::mcp_skill_body(&entry.name)
            .unwrap_or_else(|| format!("# {}\n\n{}", entry.name, entry.description))
    } else {
        // Disk skills: read from filesystem
        std::fs::read_to_string(&entry.path)
            .unwrap_or_else(|_| format!("# {}\n\n{}", entry.name, entry.description))
    };

    // Expand variables: {args}, $ARGUMENTS, etc.
    let expanded = base::frozen::expand_skill_vars(&body, args);

    // Wrap with invocation header (TS parity: command-message/command-name XML tags)
    format!(
        "\n<command-message>{name} is running...</command-message>\n\
         <command-name>{name}{args_suffix}</command-name>\n\
         {expanded}",
        name = entry.name,
        args_suffix = if args.is_empty() {
            String::new()
        } else {
            format!(" {}", args)
        },
    )
}

/// Handle a prompt command: expand skill body and return the replacement content.
pub fn handle_prompt_command(entry: &SkillEntry, cmd: &SlashCommand) -> String {
    expand_skill_for_command(entry, &cmd.args)
}

/// Handle a local command: execute the handler and return the result.
pub fn handle_local_command(cmd: &Command, sc: &SlashCommand) -> CommandResult {
    match cmd {
        Command::Local { handler, .. } => handler(sc),
        _ => CommandResult {
            text: "Internal error: expected local command".into(),
            should_query: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_command() {
        let result = parse_slash_command("/simplify").unwrap();
        assert_eq!(result.name, "simplify");
        assert_eq!(result.args, "");
    }

    #[test]
    fn parse_command_with_args() {
        let result = parse_slash_command("/simplify src/main.rs").unwrap();
        assert_eq!(result.name, "simplify");
        assert_eq!(result.args, "src/main.rs");
    }

    #[test]
    fn parse_command_with_multi_word_args() {
        let result = parse_slash_command("/debug the auth null pointer").unwrap();
        assert_eq!(result.name, "debug");
        assert_eq!(result.args, "the auth null pointer");
    }

    #[test]
    fn parse_rejects_non_slash() {
        assert!(parse_slash_command("hello").is_none());
        assert!(parse_slash_command("").is_none());
    }

    #[test]
    fn parse_rejects_bare_slash() {
        assert!(parse_slash_command("/").is_none());
    }

    #[test]
    fn registry_default_has_builtins() {
        let registry = CommandRegistry::default();
        assert!(registry.resolve("help").is_some());
        assert!(registry.resolve("skills").is_some());
        assert!(registry.resolve("clear").is_some());
        assert!(registry.resolve("compact").is_some());
        assert!(registry.resolve("cost").is_some());
    }

    #[test]
    fn resolve_unknown_returns_none() {
        let registry = CommandRegistry::default();
        assert!(registry.resolve("nonexistent").is_none());
    }

    #[test]
    fn prompt_command_expands_bundled_skill() {
        let entry = SkillEntry {
            name: "test-skill".into(),
            description: "A test skill".into(),
            source: base::frozen::SkillSource::User,
            path: std::path::PathBuf::from("(bundled:test-skill)"),
            ..Default::default()
        };
        let expanded = expand_skill_for_command(&entry, "hello");
        assert!(expanded.contains("test-skill"));
        assert!(!expanded.is_empty());
    }
}
