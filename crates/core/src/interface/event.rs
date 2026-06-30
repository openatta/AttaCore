//! `AgentEvent` — streaming events emitted by the Engine.

use crate::interface::model::Usage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// Events emitted by the Engine during a turn.
///
/// The upper layer (CLI/TUI/daemon/Jiandu desktop) consumes these
/// via `EventReceiver` and renders/dispatches as appropriate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    // ── Streaming ──
    /// Model text delta (high frequency).
    TextDelta { text: String, turn_id: String },

    /// Model requested a tool call.
    ToolUse {
        id: String,
        name: String,
        input: Value,
        turn_id: String,
    },

    /// Tool execution completed.
    ToolResult {
        id: String,
        name: String,
        content: String,
        is_error: Option<bool>,
        turn_id: String,
    },

    // ── Permission ──
    /// Permission check requires upper-layer decision.
    PermissionPrompt {
        prompt_id: String,
        tool_name: String,
        message: String,
        paths: Vec<PathBuf>,
        turn_id: String,
    },

    // ── Turn lifecycle ──
    /// A turn has completed.
    TurnComplete {
        stop_reason: String,
        api_calls: u32,
        tool_calls: u32,
        usage: Usage,
        turn_id: String,
    },

    // ── System ──
    /// System initialization completed.
    SystemInit {
        scene: String,
        tools: Vec<ToolInfo>,
        mcp_servers: Vec<String>,
    },

    /// System notification.
    System { message: String },

    /// Context compaction occurred.
    CompactAction {
        strategy: String,
        messages_before: usize,
        messages_after: usize,
        turn_id: String,
        /// Number of rounds dropped by the Snip strategy (if applicable).
        dropped_rounds: Option<usize>,
        /// Number of messages in dropped rounds (if applicable).
        dropped_messages: Option<usize>,
        /// Estimated tokens saved by the compaction (if applicable).
        estimated_tokens_saved: Option<usize>,
    },

    /// Session was changed (via set_session_id).
    SessionChanged { session_id: String },

    /// Session was persisted to disk.
    SessionPersisted { session_id: String },

    // ── Sub-agent ──
    /// A sub-agent was spawned.
    AgentSpawned {
        agent_id: String,
        parent_turn: u32,
        turn_id: String,
    },

    /// A sub-agent completed.
    AgentCompleted {
        agent_id: String,
        outcome: String,
        turn_id: String,
    },

    // ── Error ──
    /// An error occurred.
    Error {
        code: String,
        message: String,
        turn_id: String,
    },
}

/// Summary info for a registered tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
}
