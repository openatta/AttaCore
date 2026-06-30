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
//! load user-defined types from `~/.atta/code/agents/*.md` via
//! `load_agent_types_from_dir()`. Each type specifies a system prompt and an
//! allowed tool set, which `resolve_tools()` applies when spawning sub-agents.

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
#[derive(Debug, Clone)]
pub struct AgentTypeDefinition {
    /// Unique name (e.g. "explore", "plan", "code-reviewer").
    pub name: String,
    /// Short description of the agent type's purpose.
    pub description: String,
    /// Tool names the agent type is allowed to use (empty = all tools).
    pub allowed_tools: Vec<String>,
    /// Optional model override (e.g. "claude-sonnet-4-20250514").
    pub model: Option<String>,
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
        },
        AgentTypeDefinition {
            name: "general-purpose".into(),
            description: "General-purpose AI coding agent with full tool access".into(),
            allowed_tools: vec![], // empty = all tools
            model: None,
            system_prompt: GENERAL_PURPOSE_PROMPT.into(),
        },
        AgentTypeDefinition {
            name: "claude".into(),
            description: "Claude AI assistant with full tool access".into(),
            allowed_tools: vec![], // empty = all tools
            model: None,
            system_prompt: CLAUDE_PROMPT.into(),
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
pub async fn load_agent_types_from_dir(dir: &Path) -> Vec<AgentTypeDefinition> {
    let mut types = Vec::new();
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(_) => return types, // directory doesn't exist yet
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let content = match tokio::fs::read_to_string(&path).await {
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

/// Parse a single agent type definition from a markdown file with YAML
/// frontmatter. Returns `None` if the file lacks a valid `description`.
fn parse_agent_type_file(content: &str, path: &Path) -> Option<AgentTypeDefinition> {
    use base::frozen::frontmatter::split_frontmatter;

    let (front, body) = split_frontmatter(content);
    let body = body.trim();

    let mut name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    let mut description = String::new();
    let mut allowed_tools: Vec<String> = Vec::new();
    let mut model: Option<String> = None;

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
                "model" => model = Some(value.to_string()),
                _ => {}
            }
        }
    }

    // Fallback: use first body line as description
    if description.is_empty() {
        for line in body.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                let stripped = trimmed.trim_start_matches('#').trim();
                if !stripped.is_empty() {
                    description = stripped.to_string();
                }
                break;
            }
        }
    }

    if description.is_empty() {
        return None;
    }

    Some(AgentTypeDefinition {
        name,
        description,
        allowed_tools,
        model,
        system_prompt: body.to_string(),
    })
}

/// Parse a YAML inline list `[a, b, c]` or comma-separated bare list.
fn parse_yaml_list(raw: &str) -> Vec<String> {
    let s = raw.trim();
    let inner = if s.starts_with('[') && s.ends_with(']') {
        &s[1..s.len() - 1]
    } else {
        s
    };
    inner
        .split(',')
        .map(|item| item.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|item| !item.is_empty())
        .collect()
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

fn type_prompt(t: Option<&str>) -> Option<&'static str> {
    match t {
        Some("explore") => Some(EXPLORE_PROMPT),
        Some("plan") => Some(PLAN_PROMPT),
        Some("general-purpose") => Some(GENERAL_PURPOSE_PROMPT),
        Some("claude") => Some(CLAUDE_PROMPT),
        Some("code-reviewer") => Some(CODE_REVIEWER_PROMPT),
        Some("worker") => Some(WORKER_PROMPT),
        _ => None,
    }
}

fn build_prompt(input: &AgentInput) -> String {
    if let Some(p) = type_prompt(input.subagent_type.as_deref()) {
        format!("{p}\n\nTask: {}", input.prompt)
    } else {
        input.prompt.clone()
    }
}

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

// ═══════════════════════════════════════════════════════
// Inner state
// ═══════════════════════════════════════════════════════

#[derive(Clone)]
struct Inner {
    model: Arc<dyn Model>,
    config: Arc<EngineConfig>,
    fallback_tools: Arc<InMemoryToolRegistry>,
    parent_tools: Arc<InMemoryToolRegistry>,
    mailbox: Option<(std::sync::Arc<team::mailbox::MailboxStore>, String)>,
}

pub struct AgentTool {
    inner: Arc<Inner>,
    remote: Arc<dyn RemoteAgentTransport>,
}

impl AgentTool {
    pub fn new(
        model: Arc<dyn Model>,
        config: Arc<EngineConfig>,
        fallback_tools: Arc<InMemoryToolRegistry>,
    ) -> Self {
        Self::with_parent_tools(model, config, fallback_tools.clone(), fallback_tools)
    }

    pub fn with_parent_tools(
        model: Arc<dyn Model>,
        config: Arc<EngineConfig>,
        parent_tools: Arc<InMemoryToolRegistry>,
        fallback_tools: Arc<InMemoryToolRegistry>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                model,
                config,
                fallback_tools,
                parent_tools,
                mailbox: None,
            }),
            remote: Arc::new(NoopRemoteTransport),
        }
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

    /// Resolve the tool set for a given subagent type.
    ///
    /// Returns a filtered [`InMemoryToolRegistry`] containing only the tools
    /// that the named agent type is allowed to use. Unknown types fall back
    /// to the full `fallback_tools` set.
    fn resolve_tools(&self, subagent_type: Option<&str>) -> Arc<InMemoryToolRegistry> {
        let allowed_names: Option<Vec<&str>> = match subagent_type {
            Some("explore") => Some(vec!["Read", "Grep", "Glob", "WebSearch", "WebFetch", "LSP"]),
            Some("plan") => Some(vec![
                "Read",
                "Grep",
                "Glob",
                "WebSearch",
                "WebFetch",
                "Write",
            ]),
            Some("general-purpose") | Some("claude") => None,
            Some("code-reviewer") => Some(vec!["Read", "Grep", "Glob", "LSP", "Bash"]),
            _ => None,
        };

        let Some(ref allowed) = allowed_names else {
            // Full access — return the parent's tool set (which includes all tools).
            return self.inner.parent_tools.clone();
        };

        let registry = InMemoryToolRegistry::new();
        // Collect from both parent and fallback to cover all available tools.
        for tool in self
            .inner
            .parent_tools
            .all()
            .iter()
            .chain(self.inner.fallback_tools.all().iter())
        {
            if allowed.iter().any(|n| tool.name() == *n) {
                registry.register(tool.clone());
            }
        }
        Arc::new(registry)
    }

    fn sub_settings(&self, model_name: Option<&str>) -> Settings {
        let c = &self.inner.config;
        Settings {
            model: ModelSettings {
                api_type: base::provider::ApiType::Anthropic,
                base_url: String::new(),
                auth_token: String::new(),
                model_name: model_name.unwrap_or(&c.model).to_string(),
                max_tokens: c.max_tokens,
                thinking_mode: ThinkingMode::Auto,
                fallback_model: c.fallback_model.clone(),
            },
            paths: PathSettings {
                user_data_dir: std::env::var("HOME")
                    .map(|h| std::path::PathBuf::from(h).join(".atta/code"))
                    .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/atta/code")),
                local_data_dir: std::path::PathBuf::from("."),
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
            permission_mode: PermissionMode::default(),
            permission_rules: Vec::new(),
            hooks_config: None,
            mcp_servers: Vec::new(),
            language: None,
            feature_flags: Default::default(),
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
    pub(crate) async fn run_sub(
        &self,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        _cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        perm: Arc<dyn Permission>,
    ) -> Result<String, base::error::ToolError> {
        let scene: Arc<dyn AgentScene> =
            Arc::new(scene::scene::coding::CodingScene::default_scene());
        let settings = Arc::new(self.sub_settings(None));
        let _ = &perm; // used below in Builder

        let sid = uuid::Uuid::new_v4().to_string();
        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .session_id(sid)
            .scene(scene)
            .model(self.inner.model.clone())
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
        let prompt = build_prompt(input);
        let inner = self.inner.clone();
        let tc = task.clone();
        let tid_c = tid.clone();
        let outer_cancel = ctx.cancel.child_token();
        let session = ctx.session.clone();
        let _events_tx = ctx.events_tx.clone();

        tokio::spawn(async move {
            let r = Self::run_sub_inner(&inner, prompt, tools, cwd, outer_cancel).await;
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

    /// Static helper for background execution.
    async fn run_sub_inner(
        inner: &Inner,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<String, base::error::ToolError> {
        let scene: Arc<dyn AgentScene> =
            Arc::new(scene::scene::coding::CodingScene::default_scene());
        let perm: Arc<dyn Permission> = Arc::new(AlwaysPermit);
        let settings = Arc::new(Settings {
            model: ModelSettings {
                api_type: base::provider::ApiType::Anthropic,
                base_url: String::new(),
                auth_token: String::new(),
                model_name: inner.config.model.clone(),
                max_tokens: inner.config.max_tokens,
                thinking_mode: ThinkingMode::Auto,
                fallback_model: inner.config.fallback_model.clone(),
            },
            paths: PathSettings {
                user_data_dir: std::env::var("HOME")
                    .map(|h| std::path::PathBuf::from(h).join(".atta/code"))
                    .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/atta/code")),
                local_data_dir: cwd,
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
            permission_mode: PermissionMode::default(),
            permission_rules: Vec::new(),
            hooks_config: None,
            mcp_servers: Vec::new(),
            language: None,
            feature_flags: Default::default(),
        });

        let sid = uuid::Uuid::new_v4().to_string();
        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .session_id(sid)
            .scene(scene)
            .model(inner.model.clone())
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
        _cwd: std::path::PathBuf,
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
        let scene: Arc<dyn AgentScene> =
            Arc::new(scene::scene::coding::CodingScene::default_scene());
        let settings = Arc::new(self.sub_settings(None));
        let perm: Arc<dyn Permission> = Arc::new(AlwaysPermit);

        let new_sid = uuid::Uuid::new_v4().to_string();
        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .session_id(new_sid.clone())
            .scene(scene)
            .model(self.inner.model.clone())
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
        "Agent"
    }
    fn description(&self) -> &str {
        "Launch a sub-agent to handle complex, multi-step tasks independently"
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
        let prompt = build_prompt(&inp);
        let perm = self.permission_handler();

        match self
            .run_sub(prompt, tools, cwd, ctx.cancel.child_token(), perm)
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
