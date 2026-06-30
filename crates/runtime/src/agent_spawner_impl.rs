//! Runtime implementation of the AgentSpawner trait.
//!
//! Wraps AgentTool::run_sub from this crate to provide sub-agent spawning
//! to consumers (e.g., the team coordinator) without creating a circular dependency.

use async_trait::async_trait;
use base::interface::agent_spawner::AgentSpawner;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::agent_tool::AgentTool;

/// Spawns sub-agents by delegating to AgentTool::run_sub.
pub struct RuntimeAgentSpawner {
    agent_tool: Arc<AgentTool>,
}

impl RuntimeAgentSpawner {
    pub fn new(agent_tool: Arc<AgentTool>) -> Self {
        Self { agent_tool }
    }
}

#[async_trait]
impl AgentSpawner for RuntimeAgentSpawner {
    async fn spawn_agent(
        &self,
        prompt: String,
        _allowed_tools: Vec<String>,
        cwd: PathBuf,
        cancel: CancellationToken,
    ) -> Result<String, Box<dyn std::error::Error + Send>> {
        // Delegate to AgentTool::run_sub with all tools available.
        // allowed_tools filtering is not yet implemented — the sub-agent gets
        // the same tool pool as the parent (matching the TS reference where
        // workers run with the full tool set unless explicitly restricted).
        let tools = self.agent_tool.sub_tools();
        let perm = self.agent_tool.sub_permission();

        self.agent_tool
            .run_sub(prompt, tools, cwd, cancel, perm)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)
    }
}
