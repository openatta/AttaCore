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
use tokio_util::sync::CancellationToken;

/// A minimal interface for spawning sub-agents and collecting their text output.
/// The implementation in `runtime` wraps `AgentTool::run_sub` logic.
#[async_trait]
pub trait AgentSpawner: Send + Sync {
    /// Spawn a sub-agent with the given prompt and allowed tools, returning its
    /// text output (or an error).
    async fn spawn_agent(
        &self,
        prompt: String,
        allowed_tools: Vec<String>,
        cwd: PathBuf,
        cancel: CancellationToken,
    ) -> Result<String, Box<dyn std::error::Error + Send>>;
}
