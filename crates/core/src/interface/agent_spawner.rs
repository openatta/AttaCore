//! Agent spawner trait — breaks the circular dependency between team and runtime crates.
//!
//! The `team` crate needs to spawn sub-agents during orchestration but `runtime` (which
//! contains the Agent/AgentTool logic) already depends on `team`. This trait lives in `base`
//! (below both) so `runtime` can implement it and `team` can consume it without creating
//! a cycle.
//!
//! TS parity: Claude Code's `runAgent.ts` receives `availableTools` as a parameter —
//! the tool pool is assembled by the caller and passed in, avoiding circular imports.

use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A minimal interface for spawning sub-agents and collecting their text output.
/// The implementation in `runtime` wraps `AgentTool::run_sub` logic.
#[async_trait]
pub trait AgentSpawner: Send + Sync {
    /// Spawn a sub-agent with the given prompt and allowed tools, returning its
    /// text output (or an error).
    ///
    /// `agent_type`, when `Some` and it names a type in the runtime's agent
    /// catalog, selects which agent type to spawn as (matching Claude
    /// Code's skill `agent:` frontmatter field for `context: fork`) — the
    /// same catalog `AgentTool::run_sub`'s own `subagent_type` resolves
    /// against. `None` keeps the prior behavior (parent's model, no
    /// per-type overrides).
    async fn spawn_agent(
        &self,
        prompt: String,
        allowed_tools: Vec<String>,
        cwd: PathBuf,
        cancel: CancellationToken,
        agent_type: Option<String>,
    ) -> Result<String, Box<dyn std::error::Error + Send>>;

    /// Background variant of `spawn_agent` — used by a skill's `context:
    /// fork` + `background: true` frontmatter. Returns a task id
    /// immediately instead of awaiting the sub-agent's final text; the
    /// caller polls it via `TaskOutput`/`TaskStop`, the same mechanism the
    /// `Agent` tool's own `background: true` call argument already uses.
    /// `session` is the *parent* session the task gets registered on (and
    /// thus where `TaskOutput`/`TaskStop` will later look it up) — not the
    /// sub-agent's own, which doesn't exist yet when this is called.
    async fn spawn_agent_background(
        &self,
        prompt: String,
        allowed_tools: Vec<String>,
        cwd: PathBuf,
        cancel: CancellationToken,
        agent_type: Option<String>,
        session: Arc<crate::context::SessionState>,
    ) -> Result<String, Box<dyn std::error::Error + Send>>;
}
