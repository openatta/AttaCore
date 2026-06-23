//! `Model` trait — protocol-agnostic LLM backend interface.

use crate::interface::prompt::PromptBlock;
use crate::interface::settings::ThinkingMode;
use crate::provider::ApiType;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Protocol-agnostic LLM client trait.
///
/// Implementations translate `PromptBlock` and `ToolDef` into
/// the API-specific wire format (Anthropic Messages, OpenAI Chat Completions).
#[async_trait]
pub trait Model: Send + Sync {
    /// Which API protocol this model client uses.
    fn api_type(&self) -> ApiType;

    /// Stream a response from the LLM.
    ///
    /// Returns a stream of `ModelEvent`s (text deltas, tool use blocks).
    /// The cancel token allows the caller to abort mid-stream.
    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        params: StreamParams,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError>;
}

/// Parameters for a streaming LLM call.
#[derive(Debug, Clone)]
pub struct StreamParams {
    pub model: String,
    pub max_tokens: u32,
    pub thinking_mode: ThinkingMode,
    /// Fallback model to switch to on persistent Overloaded/529 errors (e.g. Opus → Sonnet).
    pub fallback_model: Option<String>,
    /// Cache edits: tool_use_ids whose tool result content should be deleted from the
    /// server-side cached prefix. Wired to the Anthropic `cache_edits` content block.
    /// TS parity: `pendingCacheEdits` / `pinnedCacheEdits` in microCompact.ts.
    pub cache_edits: Vec<String>,
}

/// Tool definition sent to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// A message in the model's conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: MessageRole,
    pub content: Vec<ModelContentBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: Option<bool>,
    },
}

/// Stream of model events.
pub type ModelStream =
    Box<dyn futures::Stream<Item = Result<ModelEvent, ModelError>> + Send + Unpin>;

/// Events emitted by the model during streaming.
#[derive(Debug, Clone)]
pub enum ModelEvent {
    TextDelta {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ContentBlockStart {
        index: usize,
        block: ModelContentBlock,
    },
    ContentBlockStop {
        index: usize,
    },
    EndTurn {
        stop_reason: String,
        usage: Usage,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("API error {status}: {message}")]
    Api { status: u16, message: String },
    #[error("auth error: {0}")]
    Auth(String),
    #[error("rate limited")]
    RateLimited,
    #[error("overloaded")]
    Overloaded,
    #[error("network error: {0}")]
    Network(String),
    #[error("cancelled")]
    Cancelled,
    #[error("internal: {0}")]
    Internal(String),
}
