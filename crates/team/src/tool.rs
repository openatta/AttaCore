//! `TeamCreate` tool — spawn multiple sub-agents with staged parallel execution.
//! Uses agent's own Coordinator + AgentTool.

use crate::coordinator::{Coordinator, DefaultCoordinator, OrchestrateRequest};
use async_trait::async_trait;
use base::context::EngineConfig;
use base::error::ToolError;
use base::interface::model::Model;
use base::tool::InMemoryToolRegistry;
use base::tool::PromptContext;
use base::tool::ToolContext;
use base::tool::{PermissionDecision, ValidationResult};
use base::tool::{ProgressSender, ToolResult};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TeamCreateInput {
    pub name: String,
    #[serde(default)]
    pub stages: Option<Vec<TeamStage>>,
    #[serde(default)]
    pub agents: Vec<TeamAgentSpec>,
    #[serde(default)]
    pub scratchpad: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AggregateMode {
    Concat,
    Best,
    Aggregate,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TeamStage {
    pub name: String,
    pub agents: Vec<TeamAgentSpec>,
    #[serde(default)]
    pub aggregate: Option<AggregateMode>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TeamAgentSpec {
    pub label: String,
    pub prompt: String,
    #[serde(default)]
    pub agent_type: Option<String>,
}

pub struct TeamCreateTool {
    model: Arc<dyn Model>,
    config: Arc<EngineConfig>,
    parent_tools: Arc<InMemoryToolRegistry>,
    sub_tools: Arc<InMemoryToolRegistry>,
    coordinator: Box<dyn Coordinator>,
}

impl TeamCreateTool {
    pub fn new(
        model: Arc<dyn Model>,
        config: Arc<EngineConfig>,
        parent_tools: Arc<InMemoryToolRegistry>,
        sub_tools: Arc<InMemoryToolRegistry>,
    ) -> Self {
        Self {
            model,
            config,
            parent_tools,
            sub_tools,
            coordinator: Box::new(DefaultCoordinator::new()),
        }
    }

    pub fn with_spawner(
        model: Arc<dyn Model>,
        config: Arc<EngineConfig>,
        parent_tools: Arc<InMemoryToolRegistry>,
        sub_tools: Arc<InMemoryToolRegistry>,
        spawner: Arc<dyn base::interface::agent_spawner::AgentSpawner>,
    ) -> Self {
        Self {
            model,
            config,
            parent_tools,
            sub_tools,
            coordinator: Box::new(DefaultCoordinator::with_agent_spawner(spawner)),
        }
    }
}

#[async_trait]
impl base::tool::Tool for TeamCreateTool {
    fn name(&self) -> &str {
        "TeamCreate"
    }
    fn description(&self) -> &str {
        "Create a team of sub-agents for multi-stage parallel task execution"
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(TeamCreateInput)).unwrap_or(Value::Null)
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("tool.prompt.md").to_string()
    }
    async fn validate_input(&self, _: &Value, _: &ToolContext) -> ValidationResult {
        ValidationResult::Ok
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let inp: TeamCreateInput =
            serde_json::from_value(input).map_err(|e| ToolError::Validation(format!("{e}")))?;
        let stages = inp.stages.unwrap_or_else(|| {
            vec![TeamStage {
                name: "main".into(),
                agents: inp.agents,
                aggregate: None,
            }]
        });
        self.coordinator
            .orchestrate(OrchestrateRequest {
                model: self.model.clone(),
                config: self.config.clone(),
                parent_tools: self.parent_tools.clone(),
                sub_tools: self.sub_tools.clone(),
                stages,
                name: inp.name,
                scratchpad: inp.scratchpad,
                ctx,
            })
            .await
    }
}

// ── TeamDelete ──

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TeamDeleteInput {
    /// Name of the team to delete.
    pub name: String,
}

pub struct TeamDeleteTool {
    coordinator: Box<dyn Coordinator>,
}

impl TeamDeleteTool {
    pub fn new(coordinator: Box<dyn Coordinator>) -> Self {
        Self { coordinator }
    }
}

#[async_trait]
impl base::tool::Tool for TeamDeleteTool {
    fn name(&self) -> &str {
        "TeamDelete"
    }
    fn description(&self) -> &str {
        "Delete a previously created team and clean up its resources"
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(TeamDeleteInput)).unwrap_or(Value::Null)
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        "Delete a team by name. This cancels any running sub-agents and \
         cleans up team-scoped resources (task lists, scratchpads). \
         Use when the team's work is complete or the user asks to stop."
            .into()
    }
    async fn validate_input(&self, _: &Value, _: &ToolContext) -> ValidationResult {
        ValidationResult::Ok
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn call(
        &self,
        input: Value,
        _ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let inp: TeamDeleteInput =
            serde_json::from_value(input).map_err(|e| ToolError::Validation(format!("{e}")))?;
        if let Err(e) = self.coordinator.cleanup_team(&inp.name).await {
            return Ok(ToolResult::error_text(format!(
                "Failed to delete team: {e}"
            )));
        }
        Ok(ToolResult::text(format!("Team '{}' deleted.", inp.name)))
    }
}
// Old Tool bridge impl removed — unified on base::tool::Tool.
