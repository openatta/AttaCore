//! Rich AgentContext — bundles the agent Engine for turn execution.
//! Shared by CLI and TUI runners.

use std::sync::Arc;

use crate::agent::{Agent, EventReceiver};
use base::tool::InMemoryToolRegistry;
use base::context::EngineConfig;

/// Bundles the agent Engine and shared config for turn execution.
pub struct AgentContext {
    pub engine: Arc<tokio::sync::Mutex<Agent>>,
    pub event_rx: Option<tokio::sync::Mutex<EventReceiver>>,
    pub config: Arc<EngineConfig>,
    pub tools: Arc<InMemoryToolRegistry>,
}
