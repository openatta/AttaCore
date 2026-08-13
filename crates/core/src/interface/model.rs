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

/// Token cost to charge a [`ModelContentBlock::Image`] in context-budget
/// estimates.
///
/// Anthropic bills images at roughly `width * height / 750` tokens, capped near
/// 1600 for an image at the ~1.15 MP resize ceiling. We do not have pixel
/// dimensions at this layer, so we charge the ceiling — deliberately
/// pessimistic, because under-counting context is what causes a request to be
/// rejected for length.
///
/// The trap this constant exists to avoid: estimating an image the same way as
/// text (`data.len() / 4`) reads a 1 MB screenshot as ~250_000 tokens, which
/// would trip compaction immediately on the first image the user pastes. The
/// base64 payload length says almost nothing about the token cost.
pub const IMAGE_TOKEN_ESTIMATE: usize = 1600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelContentBlock {
    Text {
        text: String,
    },
    /// A base64-encoded image. Carries the same two fields the Anthropic API's
    /// `source` object needs, kept flat here because every provider that
    /// supports images needs exactly these and nothing more.
    ///
    /// `data` is **not** logged or Debug-printed usefully — it is megabytes of
    /// base64. Anything that renders a message for humans or for a token
    /// estimate must special-case this variant rather than falling through to
    /// a generic text path.
    Image {
        /// e.g. `image/png`, `image/jpeg`.
        media_type: String,
        /// Base64-encoded bytes, no data-URI prefix.
        data: String,
    },
    /// An extended-thinking block with its cryptographic signature.
    ///
    /// The signature is **load-bearing, not decorative**: when thinking is
    /// enabled and the assistant's turn contained tool use, the Anthropic API
    /// requires the previous assistant turn's thinking blocks to be echoed
    /// back *with their signatures* on the following request. Dropping these
    /// blocks from the transcript (which is what this codebase did before this
    /// variant existed) makes `thinking + tool_use` fail or silently degrade.
    Thinking {
        text: String,
        signature: String,
    },
    /// Thinking the API chose to encrypt. Opaque to us; must still be echoed
    /// back verbatim for the same reason as [`ModelContentBlock::Thinking`].
    RedactedThinking {
        data: String,
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
    /// Incremental extended-thinking text. Emitted so hosts can render the
    /// model's reasoning as it streams; the accumulated text plus the
    /// signature from [`ModelEvent::ThinkingSignature`] become a
    /// [`ModelContentBlock::Thinking`] in the transcript.
    ThinkingDelta {
        text: String,
    },
    /// The signature that closes the current thinking block. The API sends it
    /// once, at the end of the block.
    ThinkingSignature {
        signature: String,
    },
    /// A thinking block the API encrypted; opaque, forwarded whole.
    RedactedThinking {
        data: String,
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
