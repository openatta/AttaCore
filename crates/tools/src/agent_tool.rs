//! AgentTool — spawn sub-agents for complex, multi-step tasks.
//!
//! Matches Claude Code TS's AgentTool:
//! - Foreground execution: calls `AgentSpawner::spawn_agent` and waits for completion.
//! - Background execution: spawns via `tokio::spawn`, returns immediately with a
//!   `task_id`.
//! - Worktree isolation: supports `isolation: "worktree"` for git worktree isolation.
//! - Depth tracking: checks `ToolContext.agent_depth` against a max depth to prevent
//!   infinite recursion (max 4 by default, or from `ctx.config.max_agent_depth`).
//! - Built-in agent type definitions (general-purpose, Explore, Plan, claude-code-guide).
//! - Structured output: `schema` field for requesting structured output from sub-agents.

use async_trait::async_trait;
use base::error::ToolError;
use base::interface::agent_spawner::AgentSpawner;
use base::tool::{
    ProgressSender, Tool, ToolContext, ToolResult, ToolResultContent, ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tracing;

// ── Constants ──

// ── Model alias resolution ──

/// Resolve a short model alias to a full model identifier.
/// TS parity: `resolveModelAlias()` in AgentTool.
pub fn resolve_model_alias(name: &str) -> String {
    match name {
        "sonnet" => "claude-sonnet-4-20250514",
        "opus" => "claude-opus-4-20250514",
        "haiku" => "claude-haiku-3-5-20250101",
        "fable" => "claude-sonnet-4-20250514",
        _ => name,
    }
    .to_string()
}

// ── Input schema ──

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentInput {
    /// Short description (3-5 words) of what the sub-agent should do.
    pub description: String,

    /// The full task prompt for the sub-agent.
    pub prompt: String,

    /// Type of agent to spawn. Defaults to "general-purpose".
    /// Available: general-purpose, Explore, Plan, claude-code-guide.
    #[serde(default = "default_subagent_type")]
    pub subagent_type: Option<String>,

    /// Model override. Short aliases: "sonnet", "opus", "haiku", "fable".
    /// Also accepts full model names (e.g. "claude-sonnet-4-20250514").
    #[serde(default)]
    pub model: Option<String>,

    /// Run the agent in the background (async). Returns a task_id immediately.
    /// You receive a notification when it completes.
    #[serde(default, alias = "run_in_background")]
    pub run_in_background: Option<bool>,

    /// Isolation mode for the sub-agent. "worktree" creates a git worktree
    /// as the working directory. None = same cwd as parent.
    #[serde(default)]
    pub isolation: Option<String>,

    /// JSON schema for structured output. When set, the sub-agent is instructed
    /// to return output matching this schema.
    #[serde(default)]
    pub schema: Option<Value>,
}

fn default_subagent_type() -> Option<String> {
    Some("general-purpose".into())
}

// ── Agent type definitions ──

/// A named agent type definition with associated allowed tool set and system prompt.
#[derive(Debug, Clone)]
pub struct AgentTypeDefinition {
    /// Unique name (e.g. "Explore", "Plan").
    pub name: String,
    /// Short description of the agent type's purpose.
    pub description: String,
    /// Tool names the agent type is allowed to use (empty = all tools).
    pub allowed_tools: Vec<String>,
    /// System prompt injected into the sub-agent's context.
    pub system_prompt: String,
}

/// Return the four built-in agent types shipped with AttaCore, matching TS AgentTool.
///
/// TS parity: `builtinAgentTypes()` in AgentTool.
pub fn builtin_agent_types() -> Vec<AgentTypeDefinition> {
    vec![
        AgentTypeDefinition {
            name: "general-purpose".into(),
            description: "Catch-all for any task that doesn't fit a more specific \
                          agent. FleetView's default when no agent name is typed."
                .into(),
            allowed_tools: vec![], // empty = all tools
            system_prompt: GENERAL_PURPOSE_PROMPT.into(),
        },
        AgentTypeDefinition {
            name: "Explore".into(),
            description: "Read-only search agent for broad fan-out searches — reads \
                          excerpts rather than whole files."
                .into(),
            allowed_tools: vec![
                "Read".into(),
                "Grep".into(),
                "Glob".into(),
                "WebSearch".into(),
                "WebFetch".into(),
                "LSP".into(),
            ],
            system_prompt: EXPLORE_PROMPT.into(),
        },
        AgentTypeDefinition {
            name: "Plan".into(),
            description: "Software architect agent for designing implementation \
                          plans — crate划分、trait边界、数据流、状态管理、技术决策."
                .into(),
            allowed_tools: vec![
                "Read".into(),
                "Grep".into(),
                "Glob".into(),
                "WebSearch".into(),
                "WebFetch".into(),
                "Write".into(),
            ],
            system_prompt: PLAN_PROMPT.into(),
        },
        AgentTypeDefinition {
            name: "claude-code-guide".into(),
            description: "For questions about Claude Code CLI features, hooks, \
                          slash commands, MCP servers, settings, IDE integrations, \
                          keyboard shortcuts; or Claude Agent SDK / Claude API."
                .into(),
            allowed_tools: vec![
                "Read".into(),
                "Bash".into(),
                "WebSearch".into(),
                "WebFetch".into(),
            ],
            system_prompt: CLAUDE_CODE_GUIDE_PROMPT.into(),
        },
    ]
}

// ── Type-specific system prompts ──

const GENERAL_PURPOSE_PROMPT: &str = "\
You are a general-purpose AI coding agent. Execute the user's request thoroughly.
Use tools to search, read, write, and edit code. Report findings clearly.
Focus on correctness and completeness.";

const EXPLORE_PROMPT: &str = "\
You are a read-only exploration specialist. Your job is to search, read, and
investigate — do NOT edit, write, or delete any files. Use Read/Grep/Glob/
WebFetch/WebSearch/LSP tools to gather information. Return a concise structured
summary with file paths and line references.";

const PLAN_PROMPT: &str = "\
You are a software architect and planning specialist. Your job is to design
implementation plans — do NOT write or edit any code. Use Read/Grep/Glob/
WebFetch/WebSearch tools to explore the codebase. Produce a concrete,
step-by-step plan with specific file paths, crate names, and implementation
approach.";

const CLAUDE_CODE_GUIDE_PROMPT: &str = "\
You are a Claude Code reference specialist. Answer questions about:
- Claude Code CLI features, hooks, slash commands, MCP servers, settings
- IDE integrations and keyboard shortcuts
- Claude Agent SDK for building custom agents
- Claude API usage, model IDs, pricing, params, streaming, tool use, caching

Use WebSearch/WebFetch for up-to-date information. Use Read/Bash to check the
user's local configuration. Provide concise, accurate answers with sources.";

// ════════════════════════════════════════════════════════════════
// Helpers
// ════════════════════════════════════════════════════════════════

/// Resolve allowed tools for a given subagent type.
///
/// Returns `None` = all tools available (empty list sentinel matches AgentSpawner
/// convention). Returns `Some(vec)` for restricted tool sets.
fn resolve_allowed_tools_for(subagent_type: Option<&str>) -> Option<Vec<String>> {
    match subagent_type {
        Some("Explore") => Some(vec![
            "Read".into(),
            "Grep".into(),
            "Glob".into(),
            "WebSearch".into(),
            "WebFetch".into(),
            "LSP".into(),
        ]),
        Some("Plan") => Some(vec![
            "Read".into(),
            "Grep".into(),
            "Glob".into(),
            "WebSearch".into(),
            "WebFetch".into(),
            "Write".into(),
        ]),
        Some("claude-code-guide") => Some(vec![
            "Read".into(),
            "Bash".into(),
            "WebSearch".into(),
            "WebFetch".into(),
        ]),
        // "general-purpose" and unknown types get full tool access
        _ => None,
    }
}

/// Build the final prompt for the sub-agent, prepending any type-specific
/// system prompt and structured output instructions, then appending the task.
fn build_agent_prompt(
    subagent_type: Option<&str>,
    user_prompt: &str,
    model_override: Option<&str>,
    schema: Option<&Value>,
) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Type-specific system prompt
    let sys_prompt = match subagent_type {
        Some("general-purpose") => Some(GENERAL_PURPOSE_PROMPT),
        Some("Explore") => Some(EXPLORE_PROMPT),
        Some("Plan") => Some(PLAN_PROMPT),
        Some("claude-code-guide") => Some(CLAUDE_CODE_GUIDE_PROMPT),
        _ => None,
    };
    if let Some(sp) = sys_prompt {
        parts.push(sp.to_string());
    }

    // Model override hint
    if let Some(model) = model_override {
        let resolved = resolve_model_alias(model);
        parts.push(format!(
            "(Model override: when possible, use the model `{resolved}`.)"
        ));
    }

    // Structured output schema
    if let Some(schema_value) = schema {
        if !schema_value.is_null() {
            let schema_str = serde_json::to_string_pretty(schema_value).unwrap_or_default();
            parts.push(format!(
                "IMPORTANT: You MUST return your output as structured JSON data \
                 matching this schema:\n```json\n{schema_str}\n```"
            ));
        }
    }

    // The actual task
    parts.push(format!("Task: {}", user_prompt));

    parts.join("\n\n")
}

/// Generate a background task ID (base36-encoded timestamp).
fn bg_task_id() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let chars: Vec<char> = "0123456789abcdefghijklmnopqrstuvwxyz"
        .chars()
        .collect();
    let mut n = ts;
    let mut s = String::new();
    while n > 0 {
        s.push(chars[(n % 36) as usize]);
        n /= 36;
    }
    if s.is_empty() {
        s.push('0');
    }
    format!("ag-{}", s)
}

/// Build a ToolResult for background task launch.
fn bg_result(task_id: &str, status: &str) -> ToolResult {
    ToolResult {
        content: ToolResultContent::Text(format!(
            "background task spawned (task_id: {task_id}, status: {status})"
        )),
        is_error: false,
        structured_content: None,
        mcp_meta: None,
        new_messages: None,
    }
}

// ════════════════════════════════════════════════════════════════
// AgentTool struct and Tool trait impl
// ════════════════════════════════════════════════════════════════

/// The Agent tool — launches sub-agents for complex, multi-step tasks.
///
/// Delegates sub-agent execution to the injected `AgentSpawner` trait.
/// Supports foreground (sync) and background (async via tokio::spawn) modes,
/// worktree isolation, agent depth tracking, and structured output schemas.
pub struct AgentTool {
    spawner: Arc<dyn AgentSpawner>,
}

impl AgentTool {
    /// Create a new AgentTool with the given spawner.
    ///
    /// The spawner is responsible for actually running sub-agents (creating
    /// an Agent engine, sending the prompt, collecting text output).
    pub fn new(spawner: Arc<dyn AgentSpawner>) -> Self {
        Self { spawner }
    }

    /// Expose the spawner for sub-agent spawning (used by team orchestration).
    pub fn spawner(&self) -> &Arc<dyn AgentSpawner> {
        &self.spawner
    }

    /// Check agent depth against the maximum allowed.
    ///
    /// Returns an error ToolResult if the depth exceeds the max.
    fn check_depth(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        let max_depth = ctx.config.max_agent_depth;
        if ctx.agent_depth >= max_depth {
            return Err(ToolError::exec(format!(
                "agent depth {} exceeds max agent depth {}. \
                 Cannot spawn further sub-agents — the nesting limit has been reached. \
                 Consider restructuring the task or reducing agent nesting.",
                ctx.agent_depth, max_depth
            )));
        }
        Ok(())
    }

    /// Create a worktree for isolation if requested.
    ///
    /// Returns the working directory path and an optional WorktreeHandle for cleanup.
    /// If isolation is not "worktree", returns (ctx.cwd.clone(), None).
    async fn prepare_cwd(
        &self,
        isolation: Option<&str>,
        ctx: &ToolContext,
    ) -> Result<(PathBuf, Option<crate::worktree::WorktreeHandle>), ToolError> {
        match isolation {
            Some("worktree") => {
                let slug = format!("agent-{}", bg_task_id());
                match crate::worktree::create_worktree(&ctx.session.cwd, &slug).await {
                    Ok(handle) => {
                        let path = handle.path().to_path_buf();
                        tracing::debug!(
                            worktree = %path.display(),
                            slug = %slug,
                            "created worktree for agent isolation"
                        );
                        Ok((path, Some(handle)))
                    }
                    Err(e) => Err(ToolError::exec(format!("worktree creation failed: {e}"))),
                }
            }
            Some(other) => Err(ToolError::exec(format!(
                "unsupported isolation mode: {other}. Supported: \"worktree\"."
            ))),
            None => Ok((ctx.session.cwd.clone(), None)),
        }
    }

    /// Clean up a worktree handle if one was created.
    async fn cleanup_worktree(handle: &mut Option<crate::worktree::WorktreeHandle>) {
        if let Some(h) = handle.as_mut() {
            h.cleanup().await;
        }
    }

    /// Run a sub-agent in the foreground, collecting text output.
    async fn run_foreground(
        &self,
        prompt: String,
        allowed_tools: Vec<String>,
        cwd: PathBuf,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<String, ToolError> {
        self.spawner
            .spawn_agent(prompt, allowed_tools, cwd, cancel)
            .await
            .map_err(|e| ToolError::Execution(anyhow::anyhow!("agent spawn failed: {e}")))
    }

    /// Launch a sub-agent in the background, returning immediately with a task_id.
    ///
    /// The background task is registered in the session's running tasks map and
    /// its status is updated on completion/cancellation.
    async fn run_background(
        &self,
        prompt: String,
        allowed_tools: Vec<String>,
        cwd: PathBuf,
        ctx: &ToolContext,
        worktree_handle: Option<crate::worktree::WorktreeHandle>,
    ) -> Result<ToolResult, ToolError> {
        let tid = bg_task_id();
        let task = ctx.session.register_running_task(tid.clone());

        let spawner = self.spawner.clone();
        let child_cancel = task.cancel.clone();
        let task_clone = task.clone();
        let tid_clone = tid.clone();

        tokio::spawn(async move {
            tracing::debug!(task_id = %tid_clone, "background agent started");

            let mut wt = worktree_handle;

            let result = spawner
                .spawn_agent(prompt, allowed_tools, cwd, child_cancel)
                .await;

            // Update task status — MUST drop MutexGuard before any .await
            // to keep the future Send (MutexGuard is !Send).
            let new_status = match &result {
                Ok(_) => base::context::task::RunningStatus::Completed,
                Err(e) => base::context::task::RunningStatus::Failed(e.to_string()),
            };
            {
                let mut status = task_clone.status.lock().unwrap_or_else(|e| e.into_inner());
                if matches!(*status, base::context::task::RunningStatus::Running) {
                    *status = new_status;
                }
            } // MutexGuard dropped here

            // Store output
            if let Ok(ref text) = result {
                let mut out = task_clone.output.lock().unwrap_or_else(|e| e.into_inner());
                out.push_str(text);
            } // MutexGuard dropped here

            // Cleanup worktree (no locks held)
            if let Some(ref mut h) = wt {
                h.cleanup().await;
            }

            tracing::debug!(task_id = %tid_clone, completed = %result.is_ok(), "background agent finished");
        });

        Ok(bg_result(&tid, "spawned"))
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        "Agent"
    }

    fn description(&self) -> &str {
        "Launch a new agent to handle complex, multi-step tasks. \
         Each agent type has specific capabilities and tools available to it. \
         For a single-fact lookup use the relevant tool directly instead."
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn is_deferred(&self) -> bool {
        false
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(AgentInput))
            .expect("schemars output is valid JSON")
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    async fn prompt(&self, _: &base::tool::PromptContext) -> String {
        include_str!("agent_tool.prompt.md").to_string()
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<AgentInput>(input.clone()) {
            Ok(inp) => {
                if inp.description.trim().is_empty() {
                    return ValidationResult::err("description must not be empty", 1);
                }
                if inp.prompt.trim().is_empty() {
                    return ValidationResult::err("prompt must not be empty", 2);
                }
                if let Some(ref isolation) = inp.isolation {
                    if isolation != "worktree" {
                        return ValidationResult::err(
                            format!("unsupported isolation mode: {isolation}. Supported: \"worktree\"."),
                            3,
                        );
                    }
                }
                ValidationResult::Ok
            }
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 4),
        }
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let inp: AgentInput = serde_json::from_value(input)
            .map_err(|e| ToolError::Validation(format!("invalid AgentTool input: {e}")))?;

        // 1. Depth check — prevent infinite recursion
        self.check_depth(&ctx)?;

        // 2. Resolve allowed tools for the requested agent type
        let agent_type = inp.subagent_type.as_deref();
        let allowed_tools = resolve_allowed_tools_for(agent_type).unwrap_or_default();

        // 3. Build the prompt (with type-specific system prompt + schema instructions)
        let prompt = build_agent_prompt(
            agent_type,
            &inp.prompt,
            inp.model.as_deref(),
            inp.schema.as_ref(),
        );

        // 4. Prepare working directory (worktree isolation)
        let (cwd, worktree_handle) = self.prepare_cwd(inp.isolation.as_deref(), &ctx).await?;

        // 5. Execution mode: foreground or background
        let bg = inp.run_in_background.unwrap_or(false);
        if bg {
            self.run_background(
                prompt,
                allowed_tools,
                cwd,
                &ctx,
                worktree_handle,
            )
            .await
        } else {
            // Foreground: run and collect output
            let result = self
                .run_foreground(
                    prompt,
                    allowed_tools,
                    cwd,
                    ctx.cancel.child_token(),
                )
                .await;

            // Cleanup worktree after foreground execution
            let mut wt = worktree_handle;
            Self::cleanup_worktree(&mut wt).await;

            match result {
                Ok(text) => Ok(ToolResult {
                    content: ToolResultContent::Text(text),
                    is_error: false,
                    structured_content: None,
                    mcp_meta: None,
                    new_messages: None,
                }),
                Err(e) => Ok(ToolResult {
                    content: ToolResultContent::Text(format!("sub-agent error: {e}")),
                    is_error: true,
                    structured_content: None,
                    mcp_meta: None,
                    new_messages: None,
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::context::EngineConfig;
    use base::context::SessionState;
    use std::path::PathBuf;
    use tokio_util::sync::CancellationToken;

    /// A simple test spawner that just echoes the prompt back.
    struct EchoSpawner;
    #[async_trait]
    impl AgentSpawner for EchoSpawner {
        async fn spawn_agent(
            &self,
            prompt: String,
            _allowed_tools: Vec<String>,
            _cwd: PathBuf,
            _cancel: CancellationToken,
        ) -> Result<String, Box<dyn std::error::Error + Send>> {
            Ok(format!("echo: {prompt}"))
        }
    }

    struct FailSpawner;
    #[async_trait]
    impl AgentSpawner for FailSpawner {
        async fn spawn_agent(
            &self,
            _prompt: String,
            _allowed_tools: Vec<String>,
            _cwd: PathBuf,
            _cancel: CancellationToken,
        ) -> Result<String, Box<dyn std::error::Error + Send>> {
            Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                "simulated failure",
            )))
        }
    }

    fn test_tool(spawner: Arc<dyn AgentSpawner>) -> AgentTool {
        AgentTool::new(spawner)
    }

    fn test_ctx(depth: u32) -> ToolContext {
        let mut config = EngineConfig::defaults_for("test");
        config.max_agent_depth = 4;
        let mut ctx = ToolContext::for_test(PathBuf::from("/tmp"));
        ctx.agent_depth = depth;
        ctx.config = Arc::new(config);
        ctx
    }

    #[tokio::test]
    async fn name_is_agent() {
        let tool = test_tool(Arc::new(EchoSpawner));
        assert_eq!(tool.name(), "Agent");
    }

    #[tokio::test]
    async fn description_not_empty() {
        let tool = test_tool(Arc::new(EchoSpawner));
        assert!(!tool.description().is_empty());
    }

    #[tokio::test]
    async fn input_schema_has_required_fields() {
        let tool = test_tool(Arc::new(EchoSpawner));
        let schema = tool.input_schema();
        // Check that the schema has properties with description and prompt
        let defs = schema
            .as_object()
            .and_then(|m| m.get("properties"))
            .and_then(|p| p.as_object())
            .expect("schema should have properties");
        assert!(defs.contains_key("description"), "description field required");
        assert!(defs.contains_key("prompt"), "prompt field required");
        assert!(defs.contains_key("subagent_type"), "subagent_type field required");
        assert!(defs.contains_key("model"), "model field required");
        assert!(defs.contains_key("run_in_background"), "run_in_background field required");
        assert!(defs.contains_key("isolation"), "isolation field required");
        assert!(defs.contains_key("schema"), "schema field required");
    }

    #[tokio::test]
    async fn concurrency_safe() {
        let tool = test_tool(Arc::new(EchoSpawner));
        assert!(tool.is_concurrency_safe(&Value::Null));
    }

    #[tokio::test]
    async fn not_deferred() {
        let tool = test_tool(Arc::new(EchoSpawner));
        assert!(!tool.is_deferred());
    }

    #[tokio::test]
    async fn foreground_execution_returns_spawner_output() {
        let tool = test_tool(Arc::new(EchoSpawner));
        let result = tool
            .call(
                serde_json::json!({
                    "description": "test task",
                    "prompt": "do something"
                }),
                test_ctx(0),
                ProgressSender::noop("t1"),
            )
            .await
            .expect("call should succeed");
        match result.content {
            ToolResultContent::Text(t) => assert!(t.contains("echo:")),
            _ => panic!("expected text content"),
        }
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn foreground_spawn_error_returns_error_result() {
        let tool = test_tool(Arc::new(FailSpawner));
        let result = tool
            .call(
                serde_json::json!({
                    "description": "fail task",
                    "prompt": "this will fail"
                }),
                test_ctx(0),
                ProgressSender::noop("t2"),
            )
            .await
            .expect("call should return a ToolResult even on error");
        assert!(result.is_error);
        match result.content {
            ToolResultContent::Text(t) => assert!(t.contains("error") || t.contains("fail")),
            _ => panic!("expected text content"),
        }
    }

    #[tokio::test]
    async fn depth_check_rejects_excessive_depth() {
        let tool = test_tool(Arc::new(EchoSpawner));
        let result = tool
            .call(
                serde_json::json!({
                    "description": "deep task",
                    "prompt": "too deep"
                }),
                test_ctx(4), // depth >= max (4)
                ProgressSender::noop("t3"),
            )
            .await;
        assert!(result.is_err(), "should error on excessive depth");
    }

    #[tokio::test]
    async fn validate_empty_description_rejected() {
        let tool = test_tool(Arc::new(EchoSpawner));
        let r = tool
            .validate_input(
                &serde_json::json!({"description": "", "prompt": "do something"}),
                &test_ctx(0),
            )
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn validate_empty_prompt_rejected() {
        let tool = test_tool(Arc::new(EchoSpawner));
        let r = tool
            .validate_input(
                &serde_json::json!({"description": "test", "prompt": ""}),
                &test_ctx(0),
            )
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn model_alias_resolution() {
        assert_eq!(
            resolve_model_alias("sonnet"),
            "claude-sonnet-4-20250514"
        );
        assert_eq!(resolve_model_alias("opus"), "claude-opus-4-20250514");
        assert_eq!(resolve_model_alias("haiku"), "claude-haiku-3-5-20250101");
        assert_eq!(resolve_model_alias("fable"), "claude-sonnet-4-20250514");
        assert_eq!(
            resolve_model_alias("claude-sonnet-4-20250514"),
            "claude-sonnet-4-20250514"
        );
    }

    #[test]
    fn builtin_agent_types_contains_expected() {
        let types = builtin_agent_types();
        let names: Vec<&str> = types.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"general-purpose"));
        assert!(names.contains(&"Explore"));
        assert!(names.contains(&"Plan"));
        assert!(names.contains(&"claude-code-guide"));
    }

    #[test]
    fn general_purpose_gets_all_tools() {
        let allowed = resolve_allowed_tools_for(Some("general-purpose"));
        assert!(allowed.is_none(), "general-purpose should have no restrictions");
    }

    #[test]
    fn explore_gets_restricted_tools() {
        let allowed = resolve_allowed_tools_for(Some("Explore"))
            .expect("Explore should have tool restrictions");
        assert!(allowed.contains(&"Read".to_string()));
        assert!(allowed.contains(&"Grep".to_string()));
        assert!(allowed.contains(&"Glob".to_string()));
        assert!(!allowed.contains(&"Write".to_string()));
        assert!(!allowed.contains(&"Edit".to_string()));
    }

    #[test]
    fn plan_gets_read_write_tools() {
        let allowed =
            resolve_allowed_tools_for(Some("Plan")).expect("Plan should have tool restrictions");
        assert!(allowed.contains(&"Read".to_string()));
        assert!(allowed.contains(&"Write".to_string()));
        assert!(!allowed.contains(&"Edit".to_string()));
    }

    #[test]
    fn claude_code_guide_gets_webbash_tools() {
        let allowed = resolve_allowed_tools_for(Some("claude-code-guide"))
            .expect("claude-code-guide should have tool restrictions");
        assert!(allowed.contains(&"WebSearch".to_string()));
        assert!(allowed.contains(&"Bash".to_string()));
        assert!(!allowed.contains(&"Write".to_string()));
    }

    #[tokio::test]
    async fn prompt_includes_agent_types() {
        let tool = test_tool(Arc::new(EchoSpawner));
        let prompt = tool
            .prompt(&base::tool::PromptContext::default())
            .await;
        assert!(prompt.contains("general-purpose"));
        assert!(prompt.contains("Explore"));
        assert!(prompt.contains("Plan"));
        assert!(prompt.contains("claude-code-guide"));
    }

    #[tokio::test]
    async fn background_execution_returns_task_id() {
        let tool = test_tool(Arc::new(EchoSpawner));
        let ctx = test_ctx(0);
        let result = tool
            .call(
                serde_json::json!({
                    "description": "bg test",
                    "prompt": "run in background",
                    "run_in_background": true
                }),
                ctx,
                ProgressSender::noop("t4"),
            )
            .await
            .expect("call should succeed");
        match result.content {
            ToolResultContent::Text(t) => {
                assert!(t.contains("background task spawned"), "got: {t}");
                assert!(t.contains("task_id:"), "got: {t}");
            }
            _ => panic!("expected text content"),
        }
        // Give the background task time to complete
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn build_agent_prompt_includes_structured_output() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"}
            }
        });
        let prompt = build_agent_prompt(Some("general-purpose"), "do something", None, Some(&schema));
        assert!(prompt.contains("structured JSON"));
        assert!(prompt.contains("\"name\""));
        assert!(prompt.contains("do something"));
    }

    #[tokio::test]
    async fn build_agent_prompt_includes_model_hint() {
        let prompt = build_agent_prompt(Some("Explore"), "find the code", Some("sonnet"), None);
        assert!(prompt.contains("claude-sonnet-4-20250514"));
        assert!(prompt.contains("find the code"));
    }

    #[tokio::test]
    async fn validate_unknown_isolation_rejected() {
        let tool = test_tool(Arc::new(EchoSpawner));
        let r = tool
            .validate_input(
                &serde_json::json!({
                    "description": "test",
                    "prompt": "test",
                    "isolation": "docker"
                }),
                &test_ctx(0),
            )
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn validate_worktree_isolation_accepted() {
        let tool = test_tool(Arc::new(EchoSpawner));
        let r = tool
            .validate_input(
                &serde_json::json!({
                    "description": "test",
                    "prompt": "test",
                    "isolation": "worktree"
                }),
                &test_ctx(0),
            )
            .await;
        assert!(r.is_ok());
    }
}
