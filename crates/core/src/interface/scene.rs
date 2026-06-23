//! `AgentScene` trait — defines agent behavior for a domain.
//!
//! Scenes are code-level (compile-time), bound at Engine creation, immutable.

use crate::interface::prompt::PromptBlock;
use std::borrow::Cow;

/// Context passed to `AgentScene::build_system_prompt()`.
#[derive(Debug, Clone)]
pub struct ScenePromptContext<'a> {
    pub cwd: Cow<'a, str>,
    pub os: Cow<'a, str>,
    pub shell: Cow<'a, str>,
    pub home_dir: Cow<'a, str>,
    /// Current date string (e.g. "2026-06-10")
    pub date: Cow<'a, str>,
    /// Resolved model name (e.g. "claude-sonnet-4-6")
    pub model_name: Cow<'a, str>,
    pub skills_text: Option<Cow<'a, str>>,
    pub mcp_instructions: Option<Cow<'a, str>>,
    pub session_memory: Option<Cow<'a, str>>,
    /// Whether the working directory is a git repository.
    pub is_git: bool,
    /// Current git branch name (e.g. "main").
    pub git_branch: Option<Cow<'a, str>>,
    /// Whether the cwd is a git worktree.
    pub is_worktree: bool,
    /// Raw git status output (truncated). TS parity: gitStatus in appendSystemContext.
    pub git_status: Option<Cow<'a, str>>,
    /// User's language preference (e.g. "zh-CN"). None = no preference.
    pub language: Option<Cow<'a, str>>,
    /// Scratchpad directory path for temporary files.
    pub scratchpad_dir: Option<Cow<'a, str>>,
    /// Output style content (loaded from config). None = default style.
    pub output_style_content: Option<Cow<'a, str>>,
    /// Comma-separated list of tool names available in this session.
    /// Used to conditionally include tool-specific guidance.
    pub available_tools: Option<Cow<'a, str>>,
}

/// Context for building the `<system-reminder>` block.
#[derive(Debug, Clone)]
pub struct ReminderContext<'a> {
    pub cwd: Cow<'a, str>,
    pub git_status: Option<Cow<'a, str>>,
    pub memory_summary: Option<Cow<'a, str>>,
}

/// Token budget configuration for a scene.
#[derive(Debug, Clone)]
pub struct TokenBudget {
    /// Trigger auto-compact when input exceeds this threshold (0 = disabled).
    pub compact_threshold: usize,
    /// Number of recent messages to keep during compaction.
    pub compact_keep_recent: usize,
}

/// Execution parameters defined by a scene.
#[derive(Debug, Clone)]
pub struct ExecutionParams {
    pub max_parallelism: usize,
    pub max_api_calls_per_turn: u32,
    pub max_agent_depth: u32,
}

impl Default for ExecutionParams {
    fn default() -> Self {
        Self {
            max_parallelism: 10,
            max_api_calls_per_turn: 200,
            max_agent_depth: 16,
        }
    }
}

/// The central trait that defines a scene (domain) for the AGENT.
///
/// Implementations are code-level (Rust source files), registered at compile time,
/// bound to an Engine instance at creation, and immutable thereafter.
pub trait AgentScene: Send + Sync + 'static {
    /// Unique scene identifier (e.g. "coding", "demo").
    fn id(&self) -> &str;

    /// Human-readable name.
    fn name(&self) -> &str;

    /// Short description.
    fn description(&self) -> &str;

    /// Build the system prompt skeleton as protocol-agnostic blocks.
    fn build_system_prompt(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock>;

    /// Tool whitelist for this scene (empty = all registered tools).
    fn tools(&self) -> Vec<String>;

    /// Token budget configuration.
    fn token_budget(&self) -> TokenBudget;

    // ── AttaCode-specific extensions (default noop) ──

    /// Build the `<system-reminder>` content injected before each turn.
    fn build_system_reminder(&self, _ctx: &ReminderContext) -> String {
        String::new()
    }

    /// Tools explicitly disallowed in this scene.
    fn disallowed_tools(&self) -> Vec<String> {
        vec![]
    }

    /// Skills loaded by default for this scene.
    fn default_skills(&self) -> Vec<String> {
        vec![]
    }

    /// Execution parameters.
    fn execution_params(&self) -> ExecutionParams {
        ExecutionParams::default()
    }

    /// 是否在首轮完成后自动生成 session 名称（通过额外的 LLM 调用）。
    /// 默认 false —— CODING 等场景不需要。
    fn auto_name_session(&self) -> bool {
        false
    }

    /// 生成 session 名称的 prompt（仅当 auto_name_session() = true 时调用）。
    /// 参数 `first_message` 是用户的首条消息内容。
    fn session_name_prompt(&self, _first_message: &str) -> Option<String> {
        None
    }
}
