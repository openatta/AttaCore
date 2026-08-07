//! `AgentTool` — spawn sub-agents using the agent's own `Agent` engine.
//!
//! Implements `base::tool::Tool` for the `Agent` invocable by the model.
//! Also provides `resume_agent()` to continue a previous session's transcript.
//!
//! Uses `Builder::build()` + `Agent::run_turn()` instead of the legacy
//! `Engine::new()` path. The sub-agent inherits the parent's authenticated
//! Anthropic client but gets a restricted tool set.
//!
//! # Agent type registry
//!
//! The module defines built-in agent types (`builtin_agent_types()`) and can
//! load user-defined types from a directory (e.g. `~/.atta/<scope>/agents/*.md`
//! or `<project>/.atta/agents/*.md`) via `load_agent_types_from_dir()`. Each
//! type specifies a system prompt and an allowed tool set, which
//! `resolve_tools()` applies when spawning sub-agents.

use crate::agent::{Builder, InputMessage};
use anyhow::anyhow;
use async_trait::async_trait;
use base::context::EngineConfig;
use base::interface::event::AgentEvent;
use base::interface::model::{MessageRole, Model, ModelContentBlock, ModelMessage};
use base::interface::permission::Permission;
use base::interface::scene::AgentScene;
use base::interface::settings::{
    ExecutionSettings, ModelSettings, PathSettings, PermissionMode, SandboxConfig, Settings,
    ThinkingMode,
};
use base::tool::InMemoryToolRegistry;
use base::tool::ProgressSender;
use base::tool::ToolContext;
use base::tool::ToolResultContent;
use futures::StreamExt;
use history::store::HistoryStore;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use team::remote_agent::{
    NoopRemoteTransport, RemoteAgentEvent, RemoteAgentRequest, RemoteAgentTransport,
};
use tools::worktree::create_worktree;

// ═══════════════════════════════════════════════════════════
// Agent type registry
// ═══════════════════════════════════════════════════════════

/// A named agent type definition with associated system prompt and tool set.
#[derive(Debug, Clone, Default)]
pub struct AgentTypeDefinition {
    /// Unique name (e.g. "explore", "plan", "code-reviewer").
    pub name: String,
    /// Short description of the agent type's purpose.
    pub description: String,
    /// Tool names the agent type is allowed to use (empty = all tools).
    pub allowed_tools: Vec<String>,
    /// Tool names removed from the pool, applied before `allowed_tools`
    /// (matches Claude Code's documented order: deny first, then allow).
    pub disallowed_tools: Vec<String>,
    /// Optional model override (e.g. "claude-sonnet-4-20250514").
    pub model: Option<String>,
    /// Optional permission-mode override for spawned subagents of this type,
    /// independent of the parent session's mode.
    pub permission_mode: Option<base::interface::settings::PermissionMode>,
    /// Optional effort/thinking-mode override. Maps onto
    /// `base::interface::settings::ThinkingMode` — not a new concept,
    /// just this field's (Claude-Code-aligned) name.
    pub effort: Option<String>,
    /// Maximum API calls a spawned subagent of this type may make, applied
    /// as this subagent's own `execution.max_api_calls_per_turn` instead of
    /// inheriting the parent's cap wholesale. `None` = inherit parent's cap.
    pub max_turns: Option<u32>,
    /// Skill names to preload in full at subagent startup (not just their
    /// descriptions) — see `AgentTool`'s spawn path. A name that doesn't
    /// resolve, or resolves to a `disable_model_invocation: true` skill, is
    /// skipped with a `tracing::warn!` rather than failing the spawn.
    pub skills: Vec<String>,
    /// MCP server names to grant this subagent, referencing servers already
    /// connected in the parent session (`settings.mcp_servers` keys) — the
    /// parent's live connections are reused, not reconnected. A subagent
    /// gets **zero** MCP tools by default (matches Claude Code: MCP access
    /// is opt-in per subagent, not inherited); this is the opt-in list.
    /// Unlike Claude Code, inline server config (as opposed to referencing
    /// an already-configured name) isn't supported — only name references.
    pub mcp_servers: Vec<String>,
    /// System prompt injected into the sub-agent's context.
    pub system_prompt: String,
}

/// Return the five built-in agent types shipped with AttaCore.
///
/// Each type specifies its allowed tool set and system prompt. Custom types
/// can be loaded from disk via [`load_agent_types_from_dir`].
pub fn builtin_agent_types() -> Vec<AgentTypeDefinition> {
    vec![
        AgentTypeDefinition {
            name: "explore".into(),
            description: "Read-only file search and exploration specialist".into(),
            allowed_tools: vec![
                "Read".into(),
                "Grep".into(),
                "Glob".into(),
                "WebSearch".into(),
                "WebFetch".into(),
                "LSP".into(),
            ],
            model: None,
            system_prompt: EXPLORE_PROMPT.into(),
            ..Default::default()
        },
        AgentTypeDefinition {
            name: "plan".into(),
            description: "Software architect and planning specialist".into(),
            allowed_tools: vec![
                "Read".into(),
                "Grep".into(),
                "Glob".into(),
                "WebSearch".into(),
                "WebFetch".into(),
                "Write".into(),
            ],
            model: None,
            system_prompt: PLAN_PROMPT.into(),
            ..Default::default()
        },
        AgentTypeDefinition {
            name: "general-purpose".into(),
            description: "General-purpose AI coding agent with full tool access".into(),
            allowed_tools: vec![], // empty = all tools
            model: None,
            system_prompt: GENERAL_PURPOSE_PROMPT.into(),
            ..Default::default()
        },
        AgentTypeDefinition {
            name: "claude".into(),
            description: "Claude AI assistant with full tool access".into(),
            allowed_tools: vec![], // empty = all tools
            model: None,
            system_prompt: CLAUDE_PROMPT.into(),
            ..Default::default()
        },
        AgentTypeDefinition {
            name: "code-reviewer".into(),
            description: "Code review specialist using Read/Grep/Glob/LSP/Bash".into(),
            allowed_tools: vec![
                "Read".into(),
                "Grep".into(),
                "Glob".into(),
                "LSP".into(),
                "Bash".into(),
            ],
            model: None,
            system_prompt: CODE_REVIEWER_PROMPT.into(),
            ..Default::default()
        },
        AgentTypeDefinition {
            name: "worker".into(),
            description: "Worker agent executing a precisely-scoped task within a team".into(),
            allowed_tools: vec![], // empty = all tools, matches pre-refactor behavior
            model: None,
            system_prompt: WORKER_PROMPT.into(),
            ..Default::default()
        },
    ]
}

/// Load agent type definitions from a directory of `*.md` files with YAML
/// frontmatter. The expected file format is:
///
/// ```markdown
/// ---
/// name: my-custom-agent
/// description: Specialized agent for custom task
/// allowed_tools: [Read, Grep, Glob, Write]
/// model: claude-sonnet-4-20250514
/// ---
/// System prompt body...
/// ```
///
/// * `name` — required (defaults to filename stem if omitted)
/// * `description` — required
/// * `allowed_tools` — optional comma/array list; empty = all tools
/// * `model` — optional model override
///
/// Returns all successfully parsed definitions. Malformed files are silently
/// skipped with a `tracing::warn!` message.
///
/// Sync (`std::fs`), not async — matches `skills::manager::SkillManager::load_dir`,
/// which is called from the same sync `Builder::build()` context this
/// function is meant to be called from (see `merge_agent_types`).
pub fn load_agent_types_from_dir(dir: &Path) -> Vec<AgentTypeDefinition> {
    let mut types = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return types, // directory doesn't exist yet
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "agent type: failed to read file");
                continue;
            }
        };
        match parse_agent_type_file(&content, &path) {
            Some(def) => types.push(def),
            None => {
                tracing::warn!(path = %path.display(), "agent type: failed to parse frontmatter");
            }
        }
    }
    types
}

/// Merge the built-in agent types with disk-loaded ones from up to three
/// tiers (pass fewer to skip a tier — e.g. tests often only want one dir).
///
/// Precedence low → high: built-in defaults < `plugin_types` (plugin-declared
/// `[[agents]]`) < `dirs[0]` (global) < `dirs[1]` (scene) < `dirs[2]`
/// (project) — same override order as skills (`crates/core/src/frozen/skill.rs`).
/// A definition with the same `name` as a lower-tier one replaces it
/// entirely (not field-merged — a type definition is one atomic unit,
/// same convention as `task_models` overrides in the multi-provider LLM
/// config). This means `.atta/agents/*.md` customization always wins over a
/// plugin's default, but a plugin's agent type still overrides the bare
/// built-in of the same name.
pub fn merge_agent_types(
    dirs: &[&Path],
    plugin_types: &[AgentTypeDefinition],
) -> std::collections::HashMap<String, AgentTypeDefinition> {
    let mut merged: std::collections::HashMap<String, AgentTypeDefinition> = builtin_agent_types()
        .into_iter()
        .map(|d| (d.name.clone(), d))
        .collect();
    for def in plugin_types {
        merged.insert(def.name.clone(), def.clone());
    }
    for dir in dirs {
        for def in load_agent_types_from_dir(dir) {
            merged.insert(def.name.clone(), def);
        }
    }
    merged
}

/// Convert a plugin-declared [`plugin::manifest::AgentDef`] into a runtime
/// [`AgentTypeDefinition`], reading its system prompt file relative to the
/// plugin's root.
///
/// Returns `None` (with a `tracing::warn!`) if the prompt file can't be
/// read — e.g. a built-in plugin's synthetic `(builtin:...)` root, or a disk
/// plugin whose declared path doesn't exist — so one bad plugin definition
/// doesn't break agent type resolution for everyone else.
pub fn agent_def_to_type(
    def: &plugin::manifest::AgentDef,
    plugin_root: &Path,
) -> Option<AgentTypeDefinition> {
    let prompt_path = plugin_root.join(&def.system_prompt_path);
    match std::fs::read_to_string(&prompt_path) {
        Ok(system_prompt) => Some(AgentTypeDefinition {
            name: def.name.clone(),
            description: def.description.clone(),
            allowed_tools: def.allowed_tools.clone(),
            model: def.model.clone(),
            system_prompt,
            ..Default::default()
        }),
        Err(e) => {
            tracing::warn!(
                agent = %def.name,
                path = %prompt_path.display(),
                error = %e,
                "plugin agent type: failed to read system prompt, skipping"
            );
            None
        }
    }
}

/// Render the one-line-per-type catalog injected into `AgentTool::description()`
/// so the model can see what `subagent_type` values are actually available
/// (built-in + any custom types loaded from `.atta/agents/`) without a
/// separate discovery call. Sorted by name for stable output.
fn describe_agent_types(
    agent_types: &std::collections::HashMap<String, AgentTypeDefinition>,
) -> String {
    let mut names: Vec<&String> = agent_types.keys().collect();
    names.sort();
    let mut lines = vec![
        "Launch a sub-agent to handle complex, multi-step tasks independently.".to_string(),
        String::new(),
        "Available subagent_type values:".to_string(),
    ];
    for name in names {
        let d = &agent_types[name];
        lines.push(format!("- {name}: {}", d.description));
    }
    lines.join("\n")
}

/// Parse a single agent type definition from a markdown file with YAML
/// frontmatter. Returns `None` if the file lacks a valid `description`.
fn parse_permission_mode(value: &str) -> Option<base::interface::settings::PermissionMode> {
    use base::interface::settings::PermissionMode;
    match value {
        "default" => Some(PermissionMode::Default),
        "acceptEdits" | "accept_edits" | "accept-edits" => Some(PermissionMode::AcceptEdits),
        "bypassPermissions" | "bypass_permissions" | "bypass-permissions" => {
            Some(PermissionMode::BypassPermissions)
        }
        "plan" => Some(PermissionMode::Plan),
        "dontAsk" | "dont_ask" | "dont-ask" => Some(PermissionMode::DontAsk),
        // "auto" is program-only (requires a transcript classifier) and not
        // settable from frontmatter; anything else is left unrecognized.
        _ => None,
    }
}

fn parse_agent_type_file(content: &str, path: &Path) -> Option<AgentTypeDefinition> {
    use base::frozen::frontmatter::{first_paragraph, parse_yaml_list, split_frontmatter};

    let (front, body) = split_frontmatter(content);
    let body = body.trim();

    let mut name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    let mut description = String::new();
    let mut allowed_tools: Vec<String> = Vec::new();
    let mut disallowed_tools: Vec<String> = Vec::new();
    let mut model: Option<String> = None;
    let mut permission_mode: Option<base::interface::settings::PermissionMode> = None;
    let mut effort: Option<String> = None;
    let mut max_turns: Option<u32> = None;
    let mut skills: Vec<String> = Vec::new();
    let mut mcp_servers: Vec<String> = Vec::new();

    if let Some(yaml) = front {
        for line in yaml.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            let key = k.trim();
            let raw = v.trim();
            if raw.is_empty() {
                continue;
            }
            let value = raw.trim_matches('"').trim_matches('\'');
            if value.is_empty() {
                continue;
            }
            match key {
                "name" => name = value.to_string(),
                "description" => description = value.to_string(),
                "allowed_tools" | "allowedTools" | "allowed-tools" => {
                    allowed_tools = parse_yaml_list(value);
                }
                "disallowed_tools" | "disallowedTools" | "disallowed-tools" => {
                    disallowed_tools = parse_yaml_list(value);
                }
                "model" => model = Some(value.to_string()),
                "permission_mode" | "permissionMode" => {
                    permission_mode = parse_permission_mode(value);
                }
                "effort" => effort = Some(value.to_string()),
                "max_turns" | "maxTurns" => {
                    max_turns = value.parse::<u32>().ok();
                }
                "skills" => {
                    skills = parse_yaml_list(value);
                }
                "mcp_servers" | "mcpServers" => {
                    mcp_servers = parse_yaml_list(value);
                }
                _ => {}
            }
        }
    }

    if description.is_empty() {
        description = first_paragraph(body);
    }

    if description.is_empty() {
        return None;
    }

    Some(AgentTypeDefinition {
        name,
        description,
        allowed_tools,
        disallowed_tools,
        model,
        permission_mode,
        effort,
        max_turns,
        skills,
        mcp_servers,
        system_prompt: body.to_string(),
    })
}

// ═══════════════════════════════════════════════════════
// Input
// ═══════════════════════════════════════════════════════

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentInput {
    pub prompt: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, alias = "subagentType")]
    pub subagent_type: Option<String>,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default)]
    pub remote: bool,
    #[serde(default, alias = "run_in_background", alias = "runInBackground")]
    pub background: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub auto_background_after_secs: Option<u64>,
}

// ═══════════════════════════════════════════════════════
// Type-specific prompts
// ═══════════════════════════════════════════════════════

const EXPLORE_PROMPT: &str = "\
You are a read-only file search specialist. Your job is to explore, find, and \
report — do NOT edit, write, or delete any files. Use Read/Glob/Grep/WebFetch/\
WebSearch/LSP tools to gather information. Return a concise structured summary \
with file paths and line references.";

const PLAN_PROMPT: &str = "\
You are a software architect and planning specialist. Your job is to design \
implementation plans — do NOT write or edit any code. Use FileRead/Glob/Grep \
to explore the codebase. Produce a concrete, step-by-step plan with specific \
file paths, crate names, and implementation approach.";

const GENERAL_PURPOSE_PROMPT: &str = "\nYou are a general-purpose AI coding agent. Execute the user's request thoroughly.\nUse tools to read, edit, and search code. Report findings clearly.\nFocus on correctness and completeness.";

const CLAUDE_PROMPT: &str = "\nYou are Claude, an AI assistant. Execute the user's request thoroughly.\nUse tools as needed. Report findings clearly and concisely.";

const CODE_REVIEWER_PROMPT: &str = "\
You are a code reviewer. Your job is to review code diffs for correctness, \
performance, and style issues. Use Read/Grep/Glob/LSP to examine the codebase \
and Bash for read-only inspection commands (e.g. git diff, cargo check, \
rustfmt --check). Report findings with specific file paths and line references. \
Do NOT make any edits.";

const WORKER_PROMPT: &str = "\nYou are a worker agent in a team. Execute the assigned task precisely.\nReport results concisely. Do not deviate from the assigned scope.";

fn bg_task_id() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let chars: Vec<char> = "0123456789abcdefghijklmnopqrstuvwxyz".chars().collect();
    let mut n = ts;
    let mut s = String::new();
    while n > 0 {
        s.push(chars[(n % 36) as usize]);
        n /= 36;
    }
    if s.is_empty() {
        s.push('0');
    }
    s
}

/// Build a sub-agent's `Settings` by cloning the parent's real settings and
/// overriding only the model-selection fields — the sub-agent inherits the
/// parent's actual scope/paths (skills/hooks/agents/memory all resolve
/// correctly), auth, sandbox policy, etc.
/// Map Claude Code's five-level `effort` (`low`/`medium`/`high`/`xhigh`/`max`)
/// onto AttaCore's coarser `ThinkingMode` (`Off`/`Auto`/`On`/`OnBudget`).
/// Lossy by construction — AttaCore has no separate effort axis, this is
/// the closest existing knob, not a new concept. Unrecognized values are
/// left as `None` (no override) rather than guessing.
fn effort_to_thinking_mode(effort: &str) -> Option<ThinkingMode> {
    match effort {
        "low" => Some(ThinkingMode::Off),
        "medium" => Some(ThinkingMode::Auto),
        "high" | "xhigh" | "max" => Some(ThinkingMode::On),
        _ => None,
    }
}

/// Apply an `AgentTypeDefinition`'s `permission_mode`/`effort`/`max_turns`
/// overrides to an already-built subagent `Settings`, in place. Called after
/// `sub_settings()`/the inline equivalent in `run_sub_inner` — kept as a
/// separate step (not folded into `settings_from_parent`) so it applies
/// identically regardless of which of the two settings-construction paths
/// built the base `Settings`.
fn apply_agent_type_overrides(settings: &mut Settings, def: &AgentTypeDefinition) {
    if let Some(mode) = def.permission_mode {
        settings.permission_mode = mode;
    }
    if let Some(effort) = &def.effort {
        if let Some(mode) = effort_to_thinking_mode(effort) {
            settings.model.thinking_mode = mode;
        } else {
            tracing::warn!(effort = %effort, agent_type = %def.name, "unrecognized effort value, ignoring");
        }
    }
    if let Some(max_turns) = def.max_turns {
        settings.execution.max_api_calls_per_turn = max_turns;
    }
}

fn settings_from_parent(
    parent: &Settings,
    model_name: String,
    max_tokens: u32,
    fallback_model: Option<String>,
) -> Settings {
    let mut settings = parent.clone();
    settings.model.model_name = model_name;
    settings.model.max_tokens = max_tokens;
    settings.model.fallback_model = fallback_model;
    settings
}

/// Fallback used when no parent `Settings` is available (`AgentTool`
/// constructed without `.with_settings(...)` — tests, or the generic
/// `AgentSpawner` bridge which doesn't carry one). Scope is a fixed
/// `"code"` stand-in since there's no real one to thread through.
fn fallback_settings(
    model_name: String,
    max_tokens: u32,
    fallback_model: Option<String>,
    local_data_dir: std::path::PathBuf,
) -> Settings {
    Settings {
        model: ModelSettings {
            api_type: base::provider::ApiType::Anthropic,
            base_url: String::new(),
            auth_token: String::new(),
            model_name,
            max_tokens,
            thinking_mode: ThinkingMode::Auto,
            fallback_model,
        },
        paths: PathSettings {
            user_data_dir: std::env::var("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".atta/scenes/code"))
                .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/atta/scenes/code")),
            global_data_dir: std::env::var("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".atta"))
                .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/atta")),
            local_data_dir,
            scope: "code".to_string(),
        },
        execution: ExecutionSettings::default(),
        compaction: Default::default(),
        sandbox: SandboxConfig::default(),
        instruction_file: None,
        prompt_append: None,
        prompt_override: None,
        vcr: None,
        telemetry_url: None,
        session_dir: None,
        memory_enabled: true,
        disable_skill_shell_execution: false,
        permission_mode: PermissionMode::default(),
        permission_rules: Vec::new(),
        hooks_config: None,
        mcp_servers: Default::default(),
        providers: Default::default(),
        default_provider: None,
        task_models: Default::default(),
        language: None,
        feature_flags: Default::default(),
    }
}

// ═══════════════════════════════════════════════════════
// Inner state
// ═══════════════════════════════════════════════════════

/// Sub-agents are not allowed to spawn further sub-agents — a "full access"
/// tool set (general-purpose/claude/any custom type with an empty
/// `allowed_tools`) still excludes this tool by name. Without this, a
/// sub-agent that inherits the parent's complete tool set (which includes
/// "Agent" itself) could recursively spawn more sub-agents with no depth
/// limit — `EngineConfig.max_agent_depth` exists as a documented intent but
/// isn't threaded through `Builder`/`Agent` today (see
/// `docs/design/2026-08-04-multi-provider-llm-migration.md`-adjacent notes
/// on `EngineConfig` not being wired end-to-end), so this is a cheap,
/// self-contained substitute: cap recursion at depth 1 by construction
/// instead of by counting.
const AGENT_TOOL_NAME: &str = "Agent";

#[derive(Clone)]
struct Inner {
    model: Arc<dyn Model>,
    config: Arc<EngineConfig>,
    fallback_tools: Arc<InMemoryToolRegistry>,
    parent_tools: Arc<InMemoryToolRegistry>,
    mailbox: Option<(std::sync::Arc<team::mailbox::MailboxStore>, String)>,
    /// Built-in types merged with any disk-loaded custom types (see
    /// `merge_agent_types`). Keyed by `subagent_type` name. `RwLock<Arc<..>>`
    /// (not a plain `Arc<..>`) so `agent_type_watcher::AgentTypeWatcher` can
    /// swap the whole map in place when a `.atta/agents/*.md` file changes —
    /// readers clone the inner `Arc` out under a brief read lock (see
    /// `Inner::agent_type_def`) rather than holding the guard, so a lookup
    /// never blocks a concurrent reload or vice versa.
    agent_types: Arc<std::sync::RwLock<Arc<std::collections::HashMap<String, AgentTypeDefinition>>>>,
    /// Kept alive only so the watcher's background thread keeps running —
    /// dropped (and thus stopped) only once every clone of this `Inner` is
    /// gone. `None` when watching wasn't set up (no watched dirs existed, or
    /// `notify` setup failed) — degrades to the pre-watcher behavior of a
    /// build-time-only catalog, same as `SkillManager::enable_watching`'s
    /// failure mode.
    _agent_type_watcher: Option<Arc<crate::agent_type_watcher::AgentTypeWatcher>>,
    /// Precomputed from the catalog at construction time — see
    /// `describe_agent_types`. Deliberately **not** kept live-reloaded like
    /// `agent_types` itself: `Tool::description(&self) -> &str` returns a
    /// borrowed `&str`, which can't be produced from data read out from
    /// behind a lock inside the function body (nothing to borrow it from).
    /// Net effect: a subagent_type added on disk after this `AgentTool` was
    /// built becomes *usable* immediately (via `agent_type_def`), but won't
    /// appear in the model-facing catalog text (`description()`) until the
    /// next full rebuild. Functional reload without cosmetic reload — an
    /// accepted, documented scope boundary, not an oversight.
    description: Arc<str>,
    /// The parent `Agent`'s own settings, when known (set via
    /// `.with_settings(...)`) — `sub_settings()`/`run_sub_inner()` clone this
    /// (overriding only the model fields) instead of a hardcoded
    /// `scope: "code"` stand-in, so a sub-agent's skills/hooks/agents
    /// lookups resolve against the parent's *real* scope, not a guess.
    /// `None` when unset (e.g. in tests, or the generic `AgentSpawner`
    /// bridge) falls back to the previous hardcoded-`"code"` behavior.
    parent_settings: Option<Arc<Settings>>,
    /// Multi-provider per-task-type model routing — see
    /// `Builder::task_router` / `Inner::model_for_subagent`. `None` (the
    /// default) means every sub-agent spawn inherits `model` unchanged,
    /// exactly matching behavior before multi-provider routing existed.
    task_router: Option<Arc<base::provider::TaskRouter>>,
    /// The parent session's `SkillManager`, when known — used to resolve
    /// the `skills:` preload field on `AgentTypeDefinition`. Set via
    /// `AgentTool::set_skill_manager` *after* construction (interior
    /// mutability, `RwLock` not a plain field) because `Builder::build()`
    /// constructs `AgentTool` before it loads skills — see the call site in
    /// `agent.rs` for why reordering wasn't the simpler fix. `None` (tests,
    /// the generic `AgentSpawner` bridge) means `skills:` preload is
    /// silently skipped, matching how `parent_settings`/`task_router`
    /// absence degrades elsewhere in this struct.
    skill_manager: Arc<std::sync::RwLock<Option<Arc<skills::manager::SkillManager>>>>,
    /// A snapshot of the parent session's connected MCP tools, used to
    /// resolve `AgentTypeDefinition.mcp_servers` by reusing already-
    /// connected tool instances (not reconnecting). Same interior-
    /// mutability / set-after-construction pattern as `skill_manager` and
    /// for the same reason (`Builder::build()` connects MCP after
    /// `AgentTool` exists) — but a `Vec` snapshot rather than a live
    /// `Arc<McpManager>` reference: `McpManager` isn't `Clone` (it holds a
    /// `Vec<Box<dyn McpNotificationHandler>>`) and `refresh_tools()` needs
    /// `&mut self`, which would conflict with `AgentTool` also holding a
    /// shared reference to the same live instance. A subagent therefore
    /// sees the MCP tools connected *as of when this was set* — a later
    /// `config.reload` reconnect in the parent session doesn't retroactively
    /// update it, an accepted scope boundary (matches `description`'s own
    /// "not kept live" doc comment on this same struct).
    mcp_tool_adapters: Arc<std::sync::RwLock<Vec<Arc<dyn base::tool::Tool>>>>,
}

impl Inner {
    /// The model instance a sub-agent spawn should use. All three spawn
    /// points (`run_sub`, `run_sub_inner`, resume) are the same task type —
    /// `task_models` in settings.json is keyed by a small fixed taxonomy
    /// (`main`/`subagent`/`team`/`classifier`/`compact`/`web_fetch`, see
    /// `docs/design/2026-08-04-multi-provider-llm-migration.md` §3.2), not
    /// by `subagent_type` (which agent *kind* — "general-purpose",
    /// "code-reviewer", etc. — a different axis entirely), so the lookup
    /// key here is always the literal `"subagent"` task type regardless of
    /// which `subagent_type` was requested.
    fn model_for_subagent(&self) -> Arc<dyn Model> {
        match &self.task_router {
            Some(router) => router.model_for("subagent"),
            None => self.model.clone(),
        }
    }

    /// Look up one agent type by name, cloning it out from behind the
    /// `agent_types` lock. Always returns an owned value (never a borrow
    /// tied to the read guard) — required now that `agent_types` can be
    /// swapped in place by the watcher; a borrow into the old map would
    /// dangle the instant a reload swaps it out. The clone itself is cheap:
    /// a handful of strings and a short tool-name list, not a hot-path
    /// concern at sub-agent-spawn frequency.
    fn agent_type_def(&self, name: &str) -> Option<AgentTypeDefinition> {
        self.agent_types.read().unwrap().get(name).cloned()
    }

    fn skill_manager(&self) -> Option<Arc<skills::manager::SkillManager>> {
        self.skill_manager.read().unwrap().clone()
    }

    fn mcp_tool_adapters(&self) -> Vec<Arc<dyn base::tool::Tool>> {
        self.mcp_tool_adapters.read().unwrap().clone()
    }
}

/// Render the full content of every named skill for preloading into a
/// spawned subagent's initial context (`AgentTypeDefinition.skills`).
///
/// A name that doesn't resolve, or resolves to a `disable_model_invocation:
/// true` skill (preloading draws from the same invocable set the Skill tool
/// itself can call — matches Claude Code's documented restriction), is
/// skipped with a `tracing::warn!` rather than failing the whole spawn.
/// Returns an empty string when nothing resolved, so callers can
/// unconditionally prepend the result without an extra branch.
fn preload_skills_text(mgr: &skills::manager::SkillManager, skill_names: &[String]) -> String {
    let mut sections = Vec::new();
    for name in skill_names {
        let Some(info) = mgr.get(name) else {
            tracing::warn!(skill = %name, "agent skills: preload — skill not found, skipping");
            continue;
        };
        if info.disable_model_invocation {
            tracing::warn!(
                skill = %name,
                "agent skills: preload — skill has disable_model_invocation set, cannot be preloaded, skipping"
            );
            continue;
        }
        let Some(content) = mgr.get_skill_content(name) else {
            tracing::warn!(skill = %name, "agent skills: preload — skill has no readable content, skipping");
            continue;
        };
        sections.push(format!("## Preloaded skill: {name}\n\n{content}"));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!(
            "<system-reminder>\nThe following skills were preloaded for this task — apply them without needing to call the Skill tool.\n\n{}\n</system-reminder>\n\n",
            sections.join("\n\n---\n\n")
        )
    }
}

pub struct AgentTool {
    inner: Arc<Inner>,
    remote: Arc<dyn RemoteAgentTransport>,
}

impl AgentTool {
    /// Construct with only the built-in agent types (no `.atta/agents/`
    /// scan) — convenience for callers that don't need custom types (e.g.
    /// tests, or embedders that manage their own catalog).
    pub fn new(
        model: Arc<dyn Model>,
        config: Arc<EngineConfig>,
        fallback_tools: Arc<InMemoryToolRegistry>,
    ) -> Self {
        Self::with_parent_tools(
            model,
            config,
            fallback_tools.clone(),
            fallback_tools,
            &[],
            &[],
        )
    }

    /// `agent_dirs` are scanned low → high priority (see `merge_agent_types`)
    /// for `.atta/agents/*.md` custom type definitions, on top of the 6
    /// built-in types and any `plugin_agent_types`. Pass `&[]` for either to
    /// skip that tier.
    pub fn with_parent_tools(
        model: Arc<dyn Model>,
        config: Arc<EngineConfig>,
        parent_tools: Arc<InMemoryToolRegistry>,
        fallback_tools: Arc<InMemoryToolRegistry>,
        agent_dirs: &[&Path],
        plugin_agent_types: &[AgentTypeDefinition],
    ) -> Self {
        let agent_types = merge_agent_types(agent_dirs, plugin_agent_types);
        let description: Arc<str> = describe_agent_types(&agent_types).into();
        let agent_types = Arc::new(std::sync::RwLock::new(Arc::new(agent_types)));

        // Live reload: watch the same directories for `.md` changes and keep
        // `agent_types` current without a session rebuild — see
        // `agent_type_watcher`'s module doc for why a full re-merge (not a
        // per-file patch) is the correct reload strategy here. `notify`
        // setup failing (permissions, inotify/kqueue limits) degrades to the
        // pre-watcher, build-time-only catalog rather than failing
        // construction — same non-fatal handling as
        // `SkillManager::enable_watching`.
        let owned_dirs: Vec<std::path::PathBuf> =
            agent_dirs.iter().map(|p| p.to_path_buf()).collect();
        let _agent_type_watcher = crate::agent_type_watcher::AgentTypeWatcher::watch(
            owned_dirs,
            plugin_agent_types.to_vec(),
            agent_types.clone(),
        )
        .map(Arc::new)
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to enable agent-type file watching; live reload disabled for this session");
        })
        .ok();

        Self {
            inner: Arc::new(Inner {
                model,
                config,
                fallback_tools,
                parent_tools,
                mailbox: None,
                agent_types,
                _agent_type_watcher,
                description,
                parent_settings: None,
                task_router: None,
                skill_manager: Arc::new(std::sync::RwLock::new(None)),
                mcp_tool_adapters: Arc::new(std::sync::RwLock::new(Vec::new())),
            }),
            remote: Arc::new(NoopRemoteTransport),
        }
    }

    /// Attach the parent `Agent`'s own settings so sub-agents inherit its
    /// real scope/paths instead of a hardcoded stand-in. See the
    /// `Inner::parent_settings` doc comment.
    pub fn with_settings(mut self, settings: Arc<Settings>) -> Self {
        let mut inner = (*self.inner).clone();
        inner.parent_settings = Some(settings);
        self.inner = Arc::new(inner);
        self
    }

    /// See `Inner::task_router` / `Inner::model_for_subagent`.
    pub fn with_task_router(mut self, router: Arc<base::provider::TaskRouter>) -> Self {
        let mut inner = (*self.inner).clone();
        inner.task_router = Some(router);
        self.inner = Arc::new(inner);
        self
    }

    /// Attach the session's `SkillManager` so `AgentTypeDefinition.skills`
    /// (preload) can resolve. Callable through `&self` (interior mutability
    /// via `Inner::skill_manager`'s `RwLock`) rather than the `.with_X(self)
    /// -> Self` builder pattern the other two use, because `Builder::build()`
    /// only has a `SkillManager` to hand *after* it already needs the
    /// finished `Arc<AgentTool>` for hook construction — see the call site
    /// in `agent.rs`.
    pub fn set_skill_manager(&self, mgr: Arc<skills::manager::SkillManager>) {
        *self.inner.skill_manager.write().unwrap() = Some(mgr);
    }

    /// Attach a snapshot of the parent session's connected MCP tools — same
    /// interior-mutability, set-after-construction pattern as
    /// `set_skill_manager`. Resolves `AgentTypeDefinition.mcp_servers` when
    /// a subagent is spawned. Pass `mcp_manager.tool_adapters().to_vec()`;
    /// see `Inner::mcp_tool_adapters`'s doc comment for why this takes a
    /// snapshot `Vec` rather than the live `McpManager`.
    pub fn set_mcp_tool_adapters(&self, adapters: Vec<Arc<dyn base::tool::Tool>>) {
        *self.inner.mcp_tool_adapters.write().unwrap() = adapters;
    }

    pub fn with_mailbox(
        mut self,
        store: std::sync::Arc<team::mailbox::MailboxStore>,
        label: impl Into<String>,
    ) -> Self {
        let mut inner = (*self.inner).clone();
        inner.mailbox = Some((store, label.into()));
        self.inner = Arc::new(inner);
        self
    }

    /// Returns the fallback tool registry for sub-agent creation.
    pub(crate) fn sub_tools(&self) -> Arc<InMemoryToolRegistry> {
        self.inner.fallback_tools.clone()
    }

    /// Returns a permission handler for sub-agent creation.
    pub(crate) fn sub_permission(&self) -> Arc<dyn Permission> {
        self.permission_handler()
    }

    /// System prompt injected into the sub-agent's context for the given
    /// `subagent_type`, looked up from the merged built-in + disk catalog.
    fn type_prompt(&self, t: Option<&str>) -> Option<String> {
        t.and_then(|name| self.inner.agent_type_def(name))
            .map(|d| d.system_prompt)
    }

    fn build_prompt(&self, input: &AgentInput) -> String {
        if let Some(p) = self.type_prompt(input.subagent_type.as_deref()) {
            format!("{p}\n\nTask: {}", input.prompt)
        } else {
            input.prompt.clone()
        }
    }

    /// Does `tool_name` match an `allowed_tools`/`disallowed_tools` entry?
    /// Exact match, or an MCP server-level pattern: `mcp__<server>` or
    /// `mcp__<server>__*` matches every tool from that server (AttaCore's
    /// MCP tools are already named `mcp__<server>__<tool>`, so this is a
    /// plain prefix check, not a general glob).
    fn tool_name_matches(tool_name: &str, pattern: &str) -> bool {
        if tool_name == pattern {
            return true;
        }
        if let Some(server) = pattern.strip_prefix("mcp__").map(|s| s.trim_end_matches("__*")) {
            return tool_name
                .strip_prefix("mcp__")
                .and_then(|rest| rest.split("__").next())
                == Some(server);
        }
        false
    }

    /// Resolve the tool set for a given subagent type.
    ///
    /// Returns a filtered [`InMemoryToolRegistry`] containing only the
    /// built-in tools the named agent type is allowed to use, plus (if
    /// `AgentTypeDefinition.mcp_servers` names any) the matching MCP tools
    /// from the parent's already-connected `McpManager`. Unknown types, and
    /// types whose `allowed_tools` is empty (the "full access" convention —
    /// see `general-purpose`/`claude`), fall back to the full built-in tool
    /// set **minus "Agent" itself** — see `AGENT_TOOL_NAME` doc comment for
    /// why. MCP is never part of that "full access" default: a subagent
    /// gets zero MCP tools unless `mcp_servers` explicitly grants them,
    /// regardless of how the built-in `allowed_tools` resolves — matches
    /// Claude Code (MCP access is opt-in per subagent, not inherited).
    fn resolve_tools(&self, subagent_type: Option<&str>) -> Arc<InMemoryToolRegistry> {
        let def = subagent_type.and_then(|name| self.inner.agent_type_def(name));
        let disallowed: Vec<String> = def
            .as_ref()
            .map(|d| d.disallowed_tools.clone())
            .unwrap_or_default();
        let allowed_names: Option<Vec<String>> = def
            .as_ref()
            .map(|d| d.allowed_tools.clone())
            .filter(|tools| !tools.is_empty());

        let registry = InMemoryToolRegistry::new();
        match &allowed_names {
            None => {
                // Full access minus `disallowed_tools` — every tool except
                // "Agent" (no recursive spawning) and anything explicitly denied.
                for tool in self
                    .inner
                    .parent_tools
                    .all()
                    .iter()
                    .chain(self.inner.fallback_tools.all().iter())
                {
                    if tool.name() != AGENT_TOOL_NAME
                        && !disallowed.iter().any(|n| Self::tool_name_matches(tool.name(), n))
                    {
                        registry.register(tool.clone());
                    }
                }
            }
            Some(allowed) => {
                // Collect from both parent and fallback to cover all available tools.
                // `disallowed_tools` is applied first, then `allowed_tools` is
                // resolved against what's left — matches Claude Code's documented
                // order; a tool listed in both ends up removed either way.
                for tool in self
                    .inner
                    .parent_tools
                    .all()
                    .iter()
                    .chain(self.inner.fallback_tools.all().iter())
                {
                    if tool.name() != AGENT_TOOL_NAME
                        && !disallowed.iter().any(|n| Self::tool_name_matches(tool.name(), n))
                        && allowed.iter().any(|n| Self::tool_name_matches(tool.name(), n))
                    {
                        registry.register(tool.clone());
                    }
                }
            }
        }

        // MCP: additive, opt-in-only grant from `mcp_servers`, reusing the
        // parent's live connections rather than reconnecting. Applies on
        // top of either branch above — a restricted `allowed_tools` list
        // that doesn't mention any `mcp__...` name still lets a granted MCP
        // server's tools through, since `mcp_servers` is a separate axis
        // from the built-in tool allow/deny lists.
        if let Some(d) = &def {
            if !d.mcp_servers.is_empty() {
                let adapters = self.inner.mcp_tool_adapters();
                if adapters.is_empty() {
                    tracing::warn!(
                        agent_type = %d.name,
                        "agent mcp_servers: set but no MCP tools are attached to this AgentTool — no MCP tools granted"
                    );
                }
                for tool in &adapters {
                    if d.mcp_servers
                        .iter()
                        .any(|server| Self::tool_name_matches(tool.name(), &format!("mcp__{server}")))
                    {
                        registry.register(tool.clone());
                    }
                }
            }
        }

        Arc::new(registry)
    }

    fn sub_settings(&self, model_name: Option<&str>, cwd: std::path::PathBuf) -> Settings {
        let c = &self.inner.config;
        let model_name = model_name.unwrap_or(&c.model).to_string();
        match &self.inner.parent_settings {
            Some(parent) => {
                settings_from_parent(parent, model_name, c.max_tokens, c.fallback_model.clone())
            }
            None => fallback_settings(model_name, c.max_tokens, c.fallback_model.clone(), cwd),
        }
    }

    /// Create a permission handler appropriate for this sub-agent's context.
    ///
    /// When the `AgentTool` has a mailbox configured (i.e. it is running as a
    /// team member), a [`PermissionBridge`] is created that forwards permission
    /// decisions to the parent agent. Otherwise, [`AlwaysPermit`] is used.
    pub(crate) fn permission_handler(&self) -> Arc<dyn Permission> {
        if let Some((ref mailbox, ref label)) = self.inner.mailbox {
            let bridge = team::coordinator::PermissionBridge::new(
                mailbox.clone(),
                label.clone(),
                "coordinator",
            );
            Arc::new(bridge)
        } else {
            Arc::new(AlwaysPermit)
        }
    }

    /// Core: run sub-agent and collect text output.
    ///
    /// `subagent_type` — when it names a type in the merged catalog (built-in
    /// or `.atta/agents/*.md`) that declares a `model` override, that model
    /// is used instead of the parent's. `None` (or an unknown/type-less
    /// call, e.g. from the generic `AgentSpawner` bridge used by team/skill-fork)
    /// keeps the parent's model, matching prior behavior.
    pub(crate) async fn run_sub(
        &self,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        perm: Arc<dyn Permission>,
        subagent_type: Option<&str>,
    ) -> Result<String, base::error::ToolError> {
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);
        let def = subagent_type.and_then(|t| self.inner.agent_type_def(t));
        let model_override = def.as_ref().and_then(|d| d.model.clone());
        let mut sub_settings = self.sub_settings(model_override.as_deref(), cwd);
        if let Some(d) = &def {
            apply_agent_type_overrides(&mut sub_settings, d);
        }
        let settings = Arc::new(sub_settings);
        let _ = &perm; // used below in Builder

        let prompt = match (&def, self.inner.skill_manager()) {
            (Some(d), Some(mgr)) if !d.skills.is_empty() => {
                format!("{}{prompt}", preload_skills_text(&mgr, &d.skills))
            }
            _ => prompt,
        };

        let sid = uuid::Uuid::new_v4().to_string();
        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .session_id(sid)
            .scene(scene)
            .model(self.inner.model_for_subagent())
            .tools(tools)
            .settings(settings)
            .permission(perm)
            .build()
            .map_err(|e| base::error::ToolError::Execution(anyhow!("build: {e}")))?;

        let turn_id = uuid::Uuid::new_v4().to_string();

        let t_handle = tokio::spawn(async move {
            let mut t = String::new();
            while let Some(ev) = event_rx.recv().await {
                match &ev {
                    AgentEvent::TextDelta { text, .. } => {
                        t.push_str(text);
                    }
                    AgentEvent::TurnComplete { .. } => break,
                    _ => {}
                }
            }
            t
        });

        let _ = input_tx.send(InputMessage::User {
            content: prompt.clone(),
            attachments: vec![],
            turn_id: turn_id.clone(),
        });
        let outcome = agent.run_turn(prompt, turn_id, cancel).await;
        drop(input_tx);
        let text = t_handle.await.unwrap_or_default();

        match outcome {
            Ok(_) | Err(crate::turn::TurnError::Shutdown) => Ok(text),
            Err(e) => Err(base::error::ToolError::Execution(anyhow!("sub: {e}"))),
        }
    }

    /// Background counterpart to `run_sub`, for `AgentSpawner::spawn_agent_background`
    /// (the `context: fork` + `background: true` skill path) — same shape as
    /// `launch_bg` (task registration + `tokio::spawn(run_sub_inner(..))` +
    /// status/output bookkeeping on completion), minus the `worktree`
    /// handling `launch_bg` does from `AgentInput.worktree`: skill fork
    /// doesn't have an equivalent frontmatter field, so there's nothing to
    /// branch on here. Returns the task id immediately; the caller (the
    /// `Skill` tool) hands that id back to the model to poll via
    /// `TaskOutput`/`TaskStop` — the same mechanism `Agent`'s own
    /// `background: true` argument already uses.
    pub(crate) async fn spawn_background(
        &self,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        subagent_type: Option<&str>,
        session: Arc<base::context::SessionState>,
    ) -> String {
        let tid = bg_task_id();
        let task = session.register_running_task(tid.clone());
        let subagent_type = subagent_type.map(str::to_string);
        let inner = self.inner.clone();
        let tc = task.clone();
        let tid_c = tid.clone();
        let session_c = session.clone();

        tokio::spawn(async move {
            let r = Self::run_sub_inner(
                &inner,
                prompt,
                tools,
                cwd,
                cancel,
                subagent_type.as_deref(),
            )
            .await;
            let mut s = tc.status.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(*s, base::context::RunningStatus::Running) {
                *s = match &r {
                    Ok(_) => base::context::RunningStatus::Completed,
                    Err(e) => base::context::RunningStatus::Failed(e.to_string()),
                };
            }
            drop(s);
            if let Ok(ref text) = r {
                tc.output
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_str(text);
            }
            session_c.persist_running_task(&tc);
            session_c.remove_running_task_persistence(&tid_c);
        });
        tid
    }

    async fn launch_bg(
        &self,
        input: &AgentInput,
        ctx: &ToolContext,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        let tid = bg_task_id();
        let task = ctx.session.register_running_task(tid.clone());
        let cwd = match &input.worktree {
            Some(s) => match create_worktree(&ctx.session.cwd, s).await {
                Ok(h) => h.path().to_path_buf(),
                Err(e) => {
                    *task.status.lock().unwrap_or_else(|e| e.into_inner()) =
                        base::context::RunningStatus::Failed(format!("worktree: {e}"));
                    return Ok(bg_result(&tid, "worktree failed"));
                }
            },
            None => ctx.session.cwd.clone(),
        };
        let tools = self.resolve_tools(input.subagent_type.as_deref());
        let prompt = self.build_prompt(input);
        let subagent_type = input.subagent_type.clone();
        let inner = self.inner.clone();
        let tc = task.clone();
        let tid_c = tid.clone();
        let outer_cancel = ctx.cancel.child_token();
        let session = ctx.session.clone();
        let _events_tx = ctx.events_tx.clone();

        tokio::spawn(async move {
            let r = Self::run_sub_inner(
                &inner,
                prompt,
                tools,
                cwd,
                outer_cancel,
                subagent_type.as_deref(),
            )
            .await;
            let mut s = tc.status.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(*s, base::context::RunningStatus::Running) {
                *s = match &r {
                    Ok(_) => base::context::RunningStatus::Completed,
                    Err(e) => base::context::RunningStatus::Failed(e.to_string()),
                };
            }
            if let Ok(ref text) = r {
                tc.output
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_str(text);
            }
            session.persist_running_task(&tc);
            session.remove_running_task_persistence(&tid_c);
        });
        Ok(bg_result(&tid, "spawned"))
    }

    /// Static helper for background execution. `subagent_type` — see
    /// `run_sub`'s doc comment; resolved the same way against `inner.agent_types`.
    async fn run_sub_inner(
        inner: &Inner,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        subagent_type: Option<&str>,
    ) -> Result<String, base::error::ToolError> {
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);
        let perm: Arc<dyn Permission> = Arc::new(AlwaysPermit);
        let def = subagent_type.and_then(|t| inner.agent_type_def(t));
        let model_name = def
            .as_ref()
            .and_then(|d| d.model.clone())
            .unwrap_or_else(|| inner.config.model.clone());
        let mut sub_settings = match &inner.parent_settings {
            Some(parent) => settings_from_parent(
                parent,
                model_name,
                inner.config.max_tokens,
                inner.config.fallback_model.clone(),
            ),
            None => fallback_settings(
                model_name,
                inner.config.max_tokens,
                inner.config.fallback_model.clone(),
                cwd,
            ),
        };
        if let Some(d) = &def {
            apply_agent_type_overrides(&mut sub_settings, d);
        }
        let settings = Arc::new(sub_settings);

        let prompt = match (&def, inner.skill_manager()) {
            (Some(d), Some(mgr)) if !d.skills.is_empty() => {
                format!("{}{prompt}", preload_skills_text(&mgr, &d.skills))
            }
            _ => prompt,
        };

        let sid = uuid::Uuid::new_v4().to_string();
        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .session_id(sid)
            .scene(scene)
            .model(inner.model_for_subagent())
            .tools(tools)
            .settings(settings)
            .permission(perm)
            .build()
            .map_err(|e| base::error::ToolError::Execution(anyhow!("build: {e}")))?;

        let turn_id = uuid::Uuid::new_v4().to_string();
        let t_handle = tokio::spawn(async move {
            let mut t = String::new();
            while let Some(ev) = event_rx.recv().await {
                match &ev {
                    AgentEvent::TextDelta { text, .. } => {
                        t.push_str(text);
                    }
                    AgentEvent::TurnComplete { .. } => break,
                    _ => {}
                }
            }
            t
        });

        let _ = input_tx.send(InputMessage::User {
            content: prompt.clone(),
            attachments: vec![],
            turn_id: turn_id.clone(),
        });
        let outcome = agent.run_turn(prompt, turn_id, cancel).await;
        drop(input_tx);
        let text = t_handle.await.unwrap_or_default();

        match outcome {
            Ok(_) | Err(crate::turn::TurnError::Shutdown) => Ok(text),
            Err(e) => Err(base::error::ToolError::Execution(anyhow!("sub: {e}"))),
        }
    }

    // ── Feature #27: Resume agent — continue a previous session ──

    /// Resume a sub-agent from a previous session's transcript.
    ///
    /// Loads the transcript entries from `history_store`, projects them into
    /// model messages, creates a new agent pre-populated with those messages,
    /// and runs the given `prompt` as the resumed task.
    ///
    /// Emits a `resume_action` telemetry event via structured tracing.
    pub async fn resume_agent(
        &self,
        session_id: &str,
        history_store: Arc<dyn HistoryStore>,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<String, base::error::ToolError> {
        let start = std::time::Instant::now();

        // 1. Load transcript from history store
        let sid = base::session::SessionId::parse(session_id)
            .map_err(|e| base::error::ToolError::Execution(anyhow!("invalid session id: {e}")))?;
        let entries = history_store
            .load(sid)
            .await
            .map_err(|e| base::error::ToolError::Execution(anyhow!("load transcript: {e}")))?;
        if entries.is_empty() {
            return Err(base::error::ToolError::Execution(anyhow!(
                "no entries found for session {session_id}"
            )));
        }
        let projected = history::transcript::project_messages(&entries);
        let report = history::transcript::resume_projection_report(&entries);

        // 2. Convert projected history messages to ModelMessages
        let mut model_messages: Vec<ModelMessage> = Vec::with_capacity(projected.len());
        for msg in &projected {
            match msg {
                base::message::Message::User { content } => {
                    model_messages.push(ModelMessage {
                        role: MessageRole::User,
                        content: convert_content_blocks(content),
                    });
                }
                base::message::Message::Assistant { content, .. } => {
                    model_messages.push(ModelMessage {
                        role: MessageRole::Assistant,
                        content: convert_content_blocks(content),
                    });
                }
                base::message::Message::System { .. } => {
                    // UI-only notifications, skip for API
                }
            }
        }

        // 3. Inject resume context as a system-reminder user message
        let resume_context = format!(
            "<system-reminder>\n\
             This is a **resumed** session (previous session: `{session_id}`).\n\
             The following transcript has been loaded into context. \
             Continue from where you left off.\n\n\
             New task: {prompt}\n\
             </system-reminder>"
        );
        model_messages.push(ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: resume_context,
            }],
        });

        // 4. Build and run sub-agent with pre-loaded messages
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);
        let settings = Arc::new(self.sub_settings(None, cwd));
        let perm: Arc<dyn Permission> = Arc::new(AlwaysPermit);

        let new_sid = uuid::Uuid::new_v4().to_string();
        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .session_id(new_sid.clone())
            .scene(scene)
            .model(self.inner.model_for_subagent())
            .tools(tools)
            .settings(settings)
            .permission(perm)
            .build()
            .map_err(|e| base::error::ToolError::Execution(anyhow!("build: {e}")))?;

        // Pre-load historical messages into the new agent's session
        agent.session.messages = model_messages;
        agent.session.turn_count = projected.len() as u32;

        // 5. Run the agent
        let turn_id = uuid::Uuid::new_v4().to_string();
        let t_handle = tokio::spawn(async move {
            let mut t = String::new();
            while let Some(ev) = event_rx.recv().await {
                match &ev {
                    AgentEvent::TextDelta { text, .. } => t.push_str(text),
                    AgentEvent::TurnComplete { .. } => break,
                    _ => {}
                }
            }
            t
        });

        let _ = input_tx.send(InputMessage::User {
            content: prompt.clone(),
            attachments: vec![],
            turn_id: turn_id.clone(),
        });
        let outcome = agent.run_turn(prompt, turn_id, cancel).await;
        drop(input_tx);
        let text = t_handle.await.unwrap_or_default();

        // 6. Emit resume telemetry via structured tracing
        let latency_ms = start.elapsed().as_millis() as u64;
        let warning_str: Option<String> = report.warning.map(|w| format!("{:?}", w));
        tracing::info!(
            target: "telemetry",
            event_type = "resume_action",
            session_id = %session_id,
            new_session_id = %new_sid,
            source = "jsonl",
            entry_count = report.entry_count,
            projected_message_count = report.projected_message_count,
            compact_boundary_count = report.compact_boundary_count,
            sidechain_entry_count = report.sidechain_entry_count,
            warning = %warning_str.unwrap_or_default(),
            latency_ms,
            "resume agent completed"
        );

        match outcome {
            Ok(_) | Err(crate::turn::TurnError::Shutdown) => Ok(text),
            Err(e) => Err(base::error::ToolError::Execution(anyhow!("resume: {e}"))),
        }
    }
}

struct AlwaysPermit;
#[async_trait]
impl Permission for AlwaysPermit {
    async fn check(
        &self,
        _tn: &str,
        _i: &Value,
        _c: &std::path::Path,
        _s: &str,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Permit
    }
}

/// Convert from history/content-block format into the model-runtime format.
/// Skips image, thinking, and redacted-thinking blocks (not supported by model API).
fn convert_content_blocks(blocks: &[base::message::ContentBlock]) -> Vec<ModelContentBlock> {
    blocks
        .iter()
        .filter_map(|block| match block {
            base::message::ContentBlock::Text { text, .. } => {
                Some(ModelContentBlock::Text { text: text.clone() })
            }
            base::message::ContentBlock::ToolUse { id, name, input } => {
                Some(ModelContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                })
            }
            base::message::ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let content_str = match content {
                    base::message::ToolResultContent::Text(s) => s.clone(),
                    base::message::ToolResultContent::Blocks(blocks) => {
                        serde_json::to_string(blocks).unwrap_or_default()
                    }
                };
                Some(ModelContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: content_str,
                    is_error: Some(*is_error),
                })
            }
            // Skip Image, Thinking, RedactedThinking — not supported by model API
            _ => None,
        })
        .collect()
}

fn bg_result(task_id: &str, status: &str) -> base::tool::ToolResult {
    base::tool::ToolResult {
        content: ToolResultContent::Text(format!(
            "background task spawned (task_id: {task_id}, status: {status})"
        )),
        is_error: false,
        structured_content: None,
        mcp_meta: None,
        new_messages: None,
    }
}

// ═══════════════════════════════════════════════════════
// Core Tool impl
// ═══════════════════════════════════════════════════════

#[async_trait]
impl base::tool::Tool for AgentTool {
    fn name(&self) -> &str {
        AGENT_TOOL_NAME
    }
    fn description(&self) -> &str {
        &self.inner.description
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(AgentInput)).unwrap_or(Value::Null)
    }
    async fn prompt(&self, _: &base::tool::PromptContext) -> String {
        include_str!("agent_tool.prompt.md").to_string()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        let inp: AgentInput = serde_json::from_value(input)
            .map_err(|e| base::error::ToolError::Validation(format!("{e}")))?;

        // Remote
        if inp.remote {
            let req = RemoteAgentRequest {
                prompt: inp.prompt.clone(),
                allowed_tools: vec![],
                worktree_slug: inp.worktree.clone(),
            };
            let stream = self
                .remote
                .spawn(req)
                .await
                .map_err(|e| base::error::ToolError::Execution(anyhow!("remote: {e}")))?;
            tokio::pin!(stream);
            let mut text = String::new();
            while let Some(ev) = stream.next().await {
                match ev {
                    Ok(RemoteAgentEvent::TextDelta(t)) => text.push_str(&t),
                    Ok(RemoteAgentEvent::Final { output_text, .. }) => {
                        text = output_text;
                        break;
                    }
                    Ok(RemoteAgentEvent::Error(m)) => {
                        return Err(base::error::ToolError::Execution(anyhow!("{m}")))
                    }
                    Err(e) => return Err(base::error::ToolError::Execution(anyhow!("{e}"))),
                    _ => {}
                }
            }
            return Ok(base::tool::ToolResult {
                content: ToolResultContent::Text(text),
                is_error: false,
                structured_content: None,
                mcp_meta: None,
                new_messages: None,
            });
        }

        // Background
        if inp.background {
            return self.launch_bg(&inp, &ctx).await;
        }

        // Sync
        let cwd = match &inp.worktree {
            Some(s) => match create_worktree(&ctx.session.cwd, s).await {
                Ok(h) => h.path().to_path_buf(),
                Err(e) => return Err(base::error::ToolError::Execution(anyhow!("worktree: {e}"))),
            },
            None => ctx.session.cwd.clone(),
        };
        let tools = self.resolve_tools(inp.subagent_type.as_deref());
        let prompt = self.build_prompt(&inp);
        let perm = self.permission_handler();

        match self
            .run_sub(
                prompt,
                tools,
                cwd,
                ctx.cancel.child_token(),
                perm,
                inp.subagent_type.as_deref(),
            )
            .await
        {
            Ok(text) => Ok(base::tool::ToolResult {
                content: ToolResultContent::Text(text),
                is_error: false,
                structured_content: None,
                mcp_meta: None,
                new_messages: None,
            }),
            Err(e) => Ok(base::tool::ToolResult {
                content: ToolResultContent::Text(format!("sub-agent error: {e}")),
                is_error: true,
                structured_content: None,
                mcp_meta: None,
                new_messages: None,
            }),
        }
    }
}

// Only base::tool::Tool impl — legacy bridge removed.

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use std::io::Write;

    fn write_agent_md(dir: &Path, filename: &str, content: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let mut f = std::fs::File::create(dir.join(filename)).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn merge_agent_types_with_no_dirs_returns_only_builtins() {
        let merged = merge_agent_types(&[], &[]);
        assert_eq!(merged.len(), builtin_agent_types().len());
        assert!(merged.contains_key("explore"));
        assert!(merged.contains_key("code-reviewer"));
        assert!(merged.contains_key("worker"));
    }

    #[test]
    fn load_agent_types_from_dir_parses_frontmatter() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-types-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_agent_md(
            &dir,
            "security-auditor.md",
            "---\nname: security-auditor\ndescription: Finds security issues\nallowed_tools: [Read, Grep, Glob]\nmodel: claude-opus-4-8\n---\nYou are a security auditor.",
        );
        let types = load_agent_types_from_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(types.len(), 1);
        let t = &types[0];
        assert_eq!(t.name, "security-auditor");
        assert_eq!(t.description, "Finds security issues");
        assert_eq!(t.allowed_tools, vec!["Read", "Grep", "Glob"]);
        assert_eq!(t.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(t.system_prompt, "You are a security auditor.");
    }

    #[test]
    fn load_agent_types_from_dir_parses_new_fields() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-types-test-newfields-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_agent_md(
            &dir,
            "db-reader.md",
            "---\nname: db-reader\ndescription: Read-only DB queries\ndisallowed_tools: AskUserQuestion\npermission_mode: plan\neffort: high\nmax_turns: 10\nskills: api-conventions, error-handling\nmcp_servers: github, sentry\n---\nBody.",
        );
        let types = load_agent_types_from_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(types.len(), 1);
        let t = &types[0];
        assert_eq!(t.disallowed_tools, vec!["AskUserQuestion"]);
        assert_eq!(t.permission_mode, Some(base::interface::settings::PermissionMode::Plan));
        assert_eq!(t.effort.as_deref(), Some("high"));
        assert_eq!(t.max_turns, Some(10));
        assert_eq!(t.skills, vec!["api-conventions", "error-handling"]);
        assert_eq!(t.mcp_servers, vec!["github", "sentry"]);
    }

    #[test]
    fn load_agent_types_from_dir_falls_back_to_first_paragraph_description() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-types-test-para-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_agent_md(
            &dir,
            "helper.md",
            "---\nname: helper\n---\nThis agent helps\nwith multi-line stuff.\n\nMore body.",
        );
        let types = load_agent_types_from_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].description, "This agent helps with multi-line stuff.");
    }

    #[test]
    fn preload_skills_text_includes_full_body_and_skips_missing_and_disabled() {
        let dir = std::env::temp_dir().join(format!(
            "atta-skills-preload-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("api-conventions")).unwrap();
        std::fs::write(
            dir.join("api-conventions").join("SKILL.md"),
            "---\ndescription: API conventions\n---\nUse RESTful naming.",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("manual-only")).unwrap();
        std::fs::write(
            dir.join("manual-only").join("SKILL.md"),
            "---\ndescription: Manual only\ndisable_model_invocation: true\n---\nSecret steps.",
        )
        .unwrap();

        let mgr = skills::manager::SkillManager::new();
        mgr.load_dir_subdirs(&dir, skills::manager::SkillSource::Project).unwrap();

        // `get_skill_content` reads a disk skill's body lazily at lookup
        // time (not cached at load time) — the fixture directory must still
        // exist when `preload_skills_text` runs, so cleanup happens after.
        let text = preload_skills_text(
            &mgr,
            &[
                "api-conventions".to_string(),
                "manual-only".to_string(),
                "does-not-exist".to_string(),
            ],
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert!(text.contains("api-conventions"));
        assert!(text.contains("Use RESTful naming."));
        // disable_model_invocation and unresolved names are skipped, not
        // included and not fatal to the rest of the preload.
        assert!(!text.contains("Secret steps."));
    }

    #[test]
    fn preload_skills_text_is_empty_when_nothing_resolves() {
        let mgr = skills::manager::SkillManager::new();
        let text = preload_skills_text(&mgr, &["nope".to_string()]);
        assert_eq!(text, "");
    }

    #[test]
    fn set_skill_manager_makes_it_available_via_inner() {
        let tools = Arc::new(InMemoryToolRegistry::new());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("test-model"));
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[], &[]);
        assert!(agent_tool.inner.skill_manager().is_none());
        agent_tool.set_skill_manager(Arc::new(skills::manager::SkillManager::new()));
        assert!(agent_tool.inner.skill_manager().is_some());
    }

    #[test]
    fn tool_name_matches_exact_and_mcp_server_pattern() {
        assert!(AgentTool::tool_name_matches("Read", "Read"));
        assert!(!AgentTool::tool_name_matches("Read", "Write"));
        assert!(AgentTool::tool_name_matches("mcp__github__list_prs", "mcp__github"));
        assert!(AgentTool::tool_name_matches("mcp__github__list_prs", "mcp__github__*"));
        assert!(!AgentTool::tool_name_matches("mcp__gitlab__list_prs", "mcp__github"));
    }

    #[test]
    fn merge_agent_types_disk_definition_overrides_builtin_by_name() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-types-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_agent_md(
            &dir,
            "explore.md",
            "---\ndescription: Custom explore override\n---\nCustom body.",
        );
        let merged = merge_agent_types(&[&dir], &[]);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(merged["explore"].description, "Custom explore override");
        assert_eq!(merged["explore"].system_prompt, "Custom body.");
        // Untouched built-ins survive.
        assert!(merged.contains_key("code-reviewer"));
    }

    #[test]
    fn merge_agent_types_precedence_is_low_to_high_across_dirs() {
        let base = std::env::temp_dir().join(format!(
            "atta-agent-types-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let global_dir = base.join("global");
        let scene_dir = base.join("scene");
        let project_dir = base.join("project");
        write_agent_md(
            &global_dir,
            "reviewer.md",
            "---\ndescription: from global\n---\nbody",
        );
        write_agent_md(
            &scene_dir,
            "reviewer.md",
            "---\ndescription: from scene\n---\nbody",
        );
        write_agent_md(
            &project_dir,
            "reviewer.md",
            "---\ndescription: from project\n---\nbody",
        );

        let merged = merge_agent_types(&[&global_dir, &scene_dir, &project_dir], &[]);
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(merged["reviewer"].description, "from project");
    }

    #[test]
    fn merge_agent_types_plugin_definition_overrides_builtin_by_name() {
        let plugin_types = vec![AgentTypeDefinition {
            name: "explore".into(),
            description: "Plugin-provided explore override".into(),
            allowed_tools: vec!["Read".into()],
            model: None,
            system_prompt: "Plugin explore body.".into(),
            ..Default::default()
        }];
        let merged = merge_agent_types(&[], &plugin_types);
        assert_eq!(
            merged["explore"].description,
            "Plugin-provided explore override"
        );
        assert_eq!(merged["explore"].system_prompt, "Plugin explore body.");
        // Untouched built-ins survive.
        assert!(merged.contains_key("code-reviewer"));
    }

    #[test]
    fn merge_agent_types_dirs_override_plugin_definition_of_same_name() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-types-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_agent_md(
            &dir,
            "explore.md",
            "---\ndescription: from disk dir\n---\nDisk body.",
        );
        let plugin_types = vec![AgentTypeDefinition {
            name: "explore".into(),
            description: "from plugin".into(),
            allowed_tools: vec![],
            model: None,
            system_prompt: "Plugin body.".into(),
            ..Default::default()
        }];
        // Confirm priority order: built-in < plugin < dirs.
        let merged_plugin_only = merge_agent_types(&[], &plugin_types);
        assert_eq!(merged_plugin_only["explore"].description, "from plugin");

        let merged = merge_agent_types(&[&dir], &plugin_types);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(merged["explore"].description, "from disk dir");
        assert_eq!(merged["explore"].system_prompt, "Disk body.");
    }

    #[test]
    fn agent_def_to_type_returns_none_when_prompt_file_missing() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-def-missing-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let def = plugin::manifest::AgentDef {
            name: "ghost".into(),
            description: "A ghost agent".into(),
            system_prompt_path: std::path::PathBuf::from("does-not-exist.md"),
            allowed_tools: vec![],
            model: None,
        };
        let result = agent_def_to_type(&def, &dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(result.is_none());
    }

    #[test]
    fn agent_def_to_type_reads_system_prompt_from_disk() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-def-ok-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("prompt.md"), "You are a specialized plugin agent.").unwrap();
        let def = plugin::manifest::AgentDef {
            name: "plugin-agent".into(),
            description: "A plugin-declared agent".into(),
            system_prompt_path: std::path::PathBuf::from("prompt.md"),
            allowed_tools: vec!["Read".into()],
            model: Some("claude-opus-4-8".into()),
        };
        let result = agent_def_to_type(&def, &dir);
        let _ = std::fs::remove_dir_all(&dir);
        let def_out = result.expect("prompt file should be read successfully");
        assert_eq!(def_out.name, "plugin-agent");
        assert_eq!(def_out.description, "A plugin-declared agent");
        assert_eq!(def_out.system_prompt, "You are a specialized plugin agent.");
        assert_eq!(def_out.allowed_tools, vec!["Read"]);
        assert_eq!(def_out.model.as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn describe_agent_types_lists_every_type_sorted_by_name() {
        let merged = merge_agent_types(&[], &[]);
        let text = describe_agent_types(&merged);
        assert!(text.contains("- code-reviewer: "));
        assert!(text.contains("- explore: "));
        // Sorted: "claude" sorts before "code-reviewer" before "explore".
        let claude_pos = text.find("- claude:").unwrap();
        let explore_pos = text.find("- explore:").unwrap();
        assert!(claude_pos < explore_pos);
    }

    struct DummyModel;
    #[async_trait]
    impl Model for DummyModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            // Most tests using `DummyModel` only exercise settings/catalog
            // derivation and never actually call `.stream()`. The
            // background-spawn test below is the exception — it needs
            // `run_sub_inner`'s `agent.run_turn(...)` to return promptly
            // (through the real error path, not a hang) so it can observe
            // the task transition to `Failed` without a full mock model.
            Err(base::interface::model::ModelError::Internal(
                "DummyModel does not implement streaming".into(),
            ))
        }
    }

    struct NamedTool(&'static str);
    #[async_trait]
    impl base::tool::Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({})
        }
        fn is_read_only(&self, _: &Value) -> bool {
            true
        }
        fn is_concurrency_safe(&self, _: &Value) -> bool {
            true
        }
        async fn call(
            &self,
            _input: Value,
            _ctx: ToolContext,
            _progress: ProgressSender,
        ) -> Result<base::tool::ToolResult, base::error::ToolError> {
            Ok(base::tool::ToolResult {
                content: ToolResultContent::Text("ok".into()),
                is_error: false,
                structured_content: None,
                mcp_meta: None,
                new_messages: None,
            })
        }
    }

    fn test_agent_tool() -> AgentTool {
        let tools = InMemoryToolRegistry::new();
        tools.register(Arc::new(NamedTool("Read")));
        tools.register(Arc::new(NamedTool("Bash")));
        let tools = Arc::new(tools);
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("test-model"));
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[], &[]);
        // Register self into the same registry it holds, matching how
        // `Builder::build()` wires it — needed so `resolve_tools()`'s
        // recursion guard has something to actually filter out.
        agent_tool
    }

    // ---- model_for_subagent / multi-provider routing ----

    #[test]
    fn model_for_subagent_uses_parent_model_when_no_router_configured() {
        let agent_tool = test_agent_tool();
        let picked = agent_tool.inner.model_for_subagent();
        assert!(Arc::ptr_eq(&picked, &agent_tool.inner.model));
    }

    #[test]
    fn model_for_subagent_routes_through_task_router_when_configured() {
        let routed_model: Arc<dyn Model> = Arc::new(DummyModel);
        let mut providers = std::collections::HashMap::new();
        providers.insert("other".to_string(), routed_model.clone());
        let mut resolved = std::collections::HashMap::new();
        resolved.insert(
            "subagent".to_string(),
            base::provider::ResolvedModel {
                provider_id: "other".into(),
                model: "x".into(),
            },
        );
        let default_model: Arc<dyn Model> = Arc::new(DummyModel);
        let router = Arc::new(base::provider::TaskRouter::new(
            providers,
            resolved,
            default_model,
        ));

        let agent_tool = test_agent_tool().with_task_router(router);
        let picked = agent_tool.inner.model_for_subagent();
        assert!(Arc::ptr_eq(&picked, &routed_model));
        assert!(!Arc::ptr_eq(&picked, &agent_tool.inner.model));
    }

    #[test]
    fn model_for_subagent_falls_back_to_default_when_no_subagent_override() {
        // No "subagent" entry in `resolved` — router's own `default`
        // (which may differ from `inner.model`) should win, proving the
        // router is actually consulted rather than short-circuited.
        let router_default: Arc<dyn Model> = Arc::new(DummyModel);
        let router = Arc::new(base::provider::TaskRouter::new(
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            router_default.clone(),
        ));

        let agent_tool = test_agent_tool().with_task_router(router);
        let picked = agent_tool.inner.model_for_subagent();
        assert!(Arc::ptr_eq(&picked, &router_default));
        assert!(!Arc::ptr_eq(&picked, &agent_tool.inner.model));
    }

    #[test]
    fn resolve_tools_full_access_excludes_agent_itself() {
        let agent_tool = test_agent_tool();
        // Register "Agent" into the same registry the tool was built with,
        // simulating `Builder::build()`'s self-registration.
        let self_arc = Arc::new(AgentTool::with_parent_tools(
            agent_tool.inner.model.clone(),
            agent_tool.inner.config.clone(),
            agent_tool.inner.parent_tools.clone(),
            agent_tool.inner.fallback_tools.clone(),
            &[],
            &[],
        ));
        agent_tool.inner.parent_tools.register(self_arc);

        // "general-purpose" is a full-access type (empty allowed_tools).
        let resolved = agent_tool.resolve_tools(Some("general-purpose"));
        let names: Vec<String> = resolved
            .all()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "Read"));
        assert!(names.iter().any(|n| n == "Bash"));
        assert!(
            !names.iter().any(|n| n == AGENT_TOOL_NAME),
            "sub-agent must not be able to spawn further sub-agents"
        );
    }

    #[test]
    fn resolve_tools_restricted_type_filters_to_allowed_list() {
        let agent_tool = test_agent_tool();
        let resolved = agent_tool.resolve_tools(Some("explore"));
        let names: Vec<String> = resolved
            .all()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "Read")); // explore allows Read
        assert!(!names.iter().any(|n| n == "Bash")); // explore doesn't allow Bash
    }

    #[test]
    fn resolve_tools_mcp_servers_grants_matching_server_tools_only() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-mcp-servers-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_agent_md(
            &dir,
            "github-agent.md",
            "---\ndescription: Uses only the github MCP server\nmcp_servers: github\n---\nbody",
        );
        let tools = Arc::new(InMemoryToolRegistry::new());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[&dir], &[]);
        let _ = std::fs::remove_dir_all(&dir);

        // Two servers' worth of MCP tools attached to the parent — only
        // "github"'s should make it into the resolved registry.
        let adapters: Vec<Arc<dyn base::tool::Tool>> = vec![
            Arc::new(NamedTool("mcp__github__list_prs")),
            Arc::new(NamedTool("mcp__gitlab__list_mrs")),
        ];
        agent_tool.set_mcp_tool_adapters(adapters);

        let resolved = agent_tool.resolve_tools(Some("github-agent"));
        let names: Vec<String> = resolved.all().iter().map(|t| t.name().to_string()).collect();
        assert!(names.iter().any(|n| n == "mcp__github__list_prs"));
        assert!(!names.iter().any(|n| n == "mcp__gitlab__list_mrs"));
    }

    #[test]
    fn resolve_tools_without_mcp_servers_field_grants_no_mcp_tools() {
        // Confirms MCP access is opt-in, not inherited by default — even
        // the "full access" (empty allowed_tools) convention doesn't pull
        // in MCP tools unless mcp_servers explicitly names a server.
        let agent_tool = test_agent_tool();
        agent_tool.set_mcp_tool_adapters(vec![Arc::new(NamedTool("mcp__github__list_prs"))]);

        let resolved = agent_tool.resolve_tools(Some("general-purpose"));
        let names: Vec<String> = resolved.all().iter().map(|t| t.name().to_string()).collect();
        assert!(!names.iter().any(|n| n.starts_with("mcp__")));
    }

    #[test]
    fn resolve_tools_disallowed_tools_removes_from_full_access_pool() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-disallowed-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_agent_md(
            &dir,
            "no-writes.md",
            "---\ndescription: Inherits every tool except file writes\ndisallowed_tools: Write, Edit\n---\nbody",
        );
        let tools = InMemoryToolRegistry::new();
        tools.register(Arc::new(NamedTool("Read")));
        tools.register(Arc::new(NamedTool("Write")));
        tools.register(Arc::new(NamedTool("Edit")));
        let tools = Arc::new(tools);
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[&dir], &[]);
        let _ = std::fs::remove_dir_all(&dir);

        let resolved = agent_tool.resolve_tools(Some("no-writes"));
        let names: Vec<String> = resolved.all().iter().map(|t| t.name().to_string()).collect();
        assert!(!names.iter().any(|n| n == "Write"));
        assert!(!names.iter().any(|n| n == "Edit"));
        // Full-access convention (empty allowed_tools) still applies to
        // everything else — disallowed_tools only removes, doesn't restrict
        // to an allowlist.
        assert!(names.iter().any(|n| n == "Read"));
    }

    #[test]
    fn resolve_tools_disallowed_tools_applied_before_allowed_tools() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-disallowed-allowed-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // A tool listed in both allowed_tools and disallowed_tools must end
        // up removed either way — matches Claude Code's documented order.
        write_agent_md(
            &dir,
            "conflicting.md",
            "---\ndescription: Lists Bash in both allow and deny\nallowed_tools: Read, Bash\ndisallowed_tools: Bash\n---\nbody",
        );
        let tools = InMemoryToolRegistry::new();
        tools.register(Arc::new(NamedTool("Read")));
        tools.register(Arc::new(NamedTool("Bash")));
        let tools = Arc::new(tools);
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[&dir], &[]);
        let _ = std::fs::remove_dir_all(&dir);

        let resolved = agent_tool.resolve_tools(Some("conflicting"));
        let names: Vec<String> = resolved.all().iter().map(|t| t.name().to_string()).collect();
        assert!(names.iter().any(|n| n == "Read"));
        assert!(!names.iter().any(|n| n == "Bash"));
    }

    #[test]
    fn custom_agent_type_model_override_is_applied_to_sub_settings() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-model-override-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_agent_md(
            &dir,
            "opus-reviewer.md",
            "---\ndescription: Reviewer pinned to a specific model\nmodel: claude-opus-4-8\n---\nbody",
        );
        let tools = Arc::new(InMemoryToolRegistry::new());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[&dir], &[]);
        let _ = std::fs::remove_dir_all(&dir);

        // Same lookup `run_sub`/`run_sub_inner` perform: resolve the type's
        // `model` override, then thread it into `sub_settings()`.
        let model_override = agent_tool
            .inner
            .agent_type_def("opus-reviewer")
            .and_then(|d| d.model);
        assert_eq!(model_override.as_deref(), Some("claude-opus-4-8"));

        let settings = agent_tool
            .sub_settings(model_override.as_deref(), std::path::PathBuf::from("/tmp/cwd"));
        assert_eq!(settings.model.model_name, "claude-opus-4-8");

        // A call with no override (or an unknown type) still falls back to
        // the parent's configured model — this must not regress.
        let settings_default = agent_tool.sub_settings(None, std::path::PathBuf::from("/tmp/cwd"));
        assert_eq!(settings_default.model.model_name, "parent-default-model");
    }

    #[test]
    fn effort_to_thinking_mode_maps_five_levels_onto_three() {
        assert_eq!(effort_to_thinking_mode("low"), Some(ThinkingMode::Off));
        assert_eq!(effort_to_thinking_mode("medium"), Some(ThinkingMode::Auto));
        assert_eq!(effort_to_thinking_mode("high"), Some(ThinkingMode::On));
        assert_eq!(effort_to_thinking_mode("xhigh"), Some(ThinkingMode::On));
        assert_eq!(effort_to_thinking_mode("max"), Some(ThinkingMode::On));
        assert_eq!(effort_to_thinking_mode("not-a-level"), None);
    }

    #[test]
    fn apply_agent_type_overrides_sets_permission_mode_effort_and_max_turns() {
        let def = AgentTypeDefinition {
            name: "db-reader".into(),
            description: "d".into(),
            permission_mode: Some(base::interface::settings::PermissionMode::Plan),
            effort: Some("high".into()),
            max_turns: Some(5),
            ..Default::default()
        };
        let mut settings = Settings::defaults_for("test-model");
        let original_max_calls = settings.execution.max_api_calls_per_turn;
        apply_agent_type_overrides(&mut settings, &def);
        assert_eq!(settings.permission_mode, base::interface::settings::PermissionMode::Plan);
        assert_eq!(settings.model.thinking_mode, ThinkingMode::On);
        assert_eq!(settings.execution.max_api_calls_per_turn, 5);
        assert_ne!(settings.execution.max_api_calls_per_turn, original_max_calls);
    }

    #[test]
    fn apply_agent_type_overrides_leaves_settings_untouched_when_fields_absent() {
        let def = AgentTypeDefinition {
            name: "plain".into(),
            description: "d".into(),
            ..Default::default()
        };
        let mut settings = Settings::defaults_for("test-model");
        let before = (
            settings.permission_mode,
            settings.model.thinking_mode.clone(),
            settings.execution.max_api_calls_per_turn,
        );
        apply_agent_type_overrides(&mut settings, &def);
        assert_eq!(
            (
                settings.permission_mode,
                settings.model.thinking_mode,
                settings.execution.max_api_calls_per_turn
            ),
            before
        );
    }

    #[test]
    fn run_sub_applies_permission_mode_and_max_turns_override_end_to_end() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-permmode-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_agent_md(
            &dir,
            "cautious.md",
            "---\ndescription: A cautious read-only researcher\npermission_mode: plan\nmax_turns: 3\n---\nbody",
        );
        let tools = Arc::new(InMemoryToolRegistry::new());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[&dir], &[]);
        let _ = std::fs::remove_dir_all(&dir);

        let def = agent_tool.inner.agent_type_def("cautious").unwrap();
        let mut settings = agent_tool.sub_settings(None, std::path::PathBuf::from("/tmp/cwd"));
        apply_agent_type_overrides(&mut settings, &def);
        assert_eq!(settings.permission_mode, base::interface::settings::PermissionMode::Plan);
        assert_eq!(settings.execution.max_api_calls_per_turn, 3);
    }

    /// `AgentTool::spawn_background` (the `context: fork` + `background:
    /// true` skill path, via `RuntimeAgentSpawner::spawn_agent_background`)
    /// must return a task id immediately — without waiting for the spawned
    /// sub-agent turn to finish — and that id must already be registered on
    /// the *caller's* session (the real `session.register_running_task` /
    /// `find_running_task` machinery `TaskOutput`/`TaskStop` also use), in
    /// `Running` status. Driving a full turn to actual completion needs a
    /// working mock `Model` (`DummyModel` only covers settings-derivation
    /// tests elsewhere in this file) — out of scope here: this test is
    /// about the background-spawn plumbing (does it return without
    /// blocking, does it register under the id it hands back), not turn
    /// execution.
    #[tokio::test]
    async fn spawn_background_returns_immediately_with_a_registered_task() {
        let tools = Arc::new(InMemoryToolRegistry::new());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
        let agent_tool = AgentTool::with_parent_tools(
            model,
            config,
            tools.clone(),
            tools.clone(),
            &[],
            &[],
        );
        use base::interface::agent_spawner::AgentSpawner as _;
        let spawner = crate::agent_spawner_impl::RuntimeAgentSpawner::new(Arc::new(agent_tool));
        let session = Arc::new(base::context::SessionState::new(std::path::PathBuf::from("/tmp")));

        let start = std::time::Instant::now();
        let task_id = spawner
            .spawn_agent_background(
                "do the thing".into(),
                vec![],
                std::path::PathBuf::from("/tmp"),
                tokio_util::sync::CancellationToken::new(),
                None,
                session.clone(),
            )
            .await
            .expect("spawn_agent_background itself should not error — it only registers the task");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "spawn_agent_background must return immediately, not block on the sub-agent turn"
        );

        let task = session
            .find_running_task(&task_id)
            .expect("spawn_background must register the task under the id it returns");
        assert!(matches!(
            *task.status.lock().unwrap_or_else(|e| e.into_inner()),
            base::context::RunningStatus::Running
        ));
    }

    /// Regression: `agent_types` used to be a build-time-only snapshot — a
    /// `.atta/agents/*.md` file edited after `with_parent_tools()` had no
    /// effect on a running `AgentTool` at all, not even a partial one; the
    /// only way to pick it up was rebuilding the whole `Agent`. This
    /// exercises the real `notify` watcher end to end (not just the merge
    /// logic in isolation), the same way
    /// `skills::manager::editing_a_watched_skill_file_is_picked_up_without_a_manual_reload`
    /// does for Skills.
    #[test]
    fn editing_an_agent_type_file_is_picked_up_without_rebuilding_agent_tool() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-watch-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_agent_md(
            &dir,
            "watched-reviewer.md",
            "---\ndescription: original\nmodel: claude-sonnet-4-6\n---\noriginal body",
        );

        let tools = Arc::new(InMemoryToolRegistry::new());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[&dir], &[]);

        assert_eq!(
            agent_tool.inner.agent_type_def("watched-reviewer").unwrap().model.as_deref(),
            Some("claude-sonnet-4-6"),
            "sanity check on the initial build-time load"
        );

        // Edit the file after the `AgentTool` (and its watcher) already exist
        // — this is the part that used to have zero effect.
        write_agent_md(
            &dir,
            "watched-reviewer.md",
            "---\ndescription: updated\nmodel: claude-opus-4-8\n---\nupdated body",
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut picked_up = false;
        while std::time::Instant::now() < deadline {
            if agent_tool
                .inner
                .agent_type_def("watched-reviewer")
                .and_then(|d| d.model)
                .as_deref()
                == Some("claude-opus-4-8")
            {
                picked_up = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let _ = std::fs::remove_dir_all(&dir);
        assert!(picked_up, "watcher never reported the agent-type file change within 5s");
    }

    #[test]
    fn sub_settings_without_parent_settings_uses_the_passed_cwd() {
        // `test_agent_tool()` never calls `.with_settings(...)`, so this
        // exercises the `fallback_settings()` branch — `local_data_dir` must
        // reflect the `cwd` argument, not a hardcoded `"."`.
        let agent_tool = test_agent_tool();
        let cwd = std::path::PathBuf::from("/tmp/some-real-project");
        let settings = agent_tool.sub_settings(None, cwd.clone());
        assert_eq!(settings.paths.local_data_dir, cwd);
    }
}
