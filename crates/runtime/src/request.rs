//! API request building — assemble prompt blocks, tools, and messages into a
//! structured request context that Model implementations consume.
//!
//! TS parity: cache breakpoint computation matching Claude Code's
//! `splitSysPromptPrefix` / `buildSystemPromptBlocks` logic.

use base::interface::model::{ModelMessage, ToolDef};
use base::interface::prompt::{CacheStrategy, PromptBlock};
use base::interface::settings::ThinkingMode;

/// Assembled request context — everything a Model implementation needs to
/// construct a provider-specific API request.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// System prompt blocks (may carry cache_control annotations).
    pub system_blocks: Vec<PromptBlock>,
    /// Tool definitions for this request.
    pub tools: Vec<ToolDef>,
    /// Conversation messages (user + assistant + tool results).
    pub messages: Vec<ModelMessage>,
    /// Streaming parameters (model id, max tokens, thinking mode).
    pub params: RequestParams,
}

#[derive(Debug, Clone)]
pub struct RequestParams {
    pub model: String,
    pub max_tokens: u32,
    pub thinking_mode: ThinkingMode,
    pub fallback_model: Option<String>,
}

/// Cache breakpoint descriptor — marks where the prompt prefix cache boundary
/// should be placed for Anthropic-compatible APIs.
///
/// In Claude Code TS, `splitSysPromptPrefix` places `cache_control: {type: "ephemeral"}`
/// on the LAST static system block (everything before `SYSTEM_PROMPT_DYNAMIC_BOUNDARY`).
/// The Model implementation uses this to annotate the actual API request.
#[derive(Debug, Clone)]
pub struct CacheBreakpoint {
    /// Index within `system_blocks` where ephemeral cache_control should be placed.
    pub cached_prefix_end: usize,
    /// Total number of system blocks.
    pub total_blocks: usize,
}

impl RequestContext {
    /// Build the request context from its parts.
    pub fn new(
        system_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        params: RequestParams,
    ) -> Self {
        Self {
            system_blocks,
            tools,
            messages,
            params,
        }
    }

    /// Compute the cache breakpoint for this request.
    ///
    /// Strategy (TS parity):
    /// - Global-cache blocks form the cacheable prefix.
    /// - The LAST global-cache block gets the ephemeral cache marker.
    /// - Ephemeral blocks and blocks with no strategy are NOT cached.
    pub fn cache_breakpoint(&self) -> Option<CacheBreakpoint> {
        let last_global = self
            .system_blocks
            .iter()
            .enumerate()
            .rev()
            .find(|(_, b)| b.cache_strategy == Some(CacheStrategy::Global))
            .map(|(i, _)| i);

        last_global.map(|i| CacheBreakpoint {
            cached_prefix_end: i,
            total_blocks: self.system_blocks.len(),
        })
    }

    /// Approximate token count for the system prompt prefix (cacheable portion).
    pub fn cached_prefix_tokens(&self) -> usize {
        self.system_blocks
            .iter()
            .take_while(|b| b.cache_strategy == Some(CacheStrategy::Global))
            .map(|b| b.content.len() / 4)
            .sum()
    }

    /// Approximate total input token count for the request.
    pub fn estimated_input_tokens(&self) -> usize {
        let system_tokens: usize = self.system_blocks.iter().map(|b| b.content.len() / 4).sum();
        let message_tokens: usize = self
            .messages
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .map(|b| match b {
                        base::interface::model::ModelContentBlock::Text { text } => text.len() / 4,
                        _ => 50, // rough estimate for tool blocks
                    })
                    .sum::<usize>()
            })
            .sum();
        let tool_tokens: usize = self
            .tools
            .iter()
            .map(|t| {
                t.description.len() / 4
                    + serde_json::to_string(&t.input_schema)
                        .map(|s| s.len() / 4)
                        .unwrap_or(100)
            })
            .sum();
        system_tokens + message_tokens + tool_tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::prompt::BlockRole;

    fn make_block(content: &str, strategy: Option<CacheStrategy>) -> PromptBlock {
        PromptBlock {
            role: BlockRole::System,
            content: content.to_string(),
            cache_strategy: strategy,
        }
    }

    #[test]
    fn cache_breakpoint_finds_last_global() {
        let blocks = vec![
            make_block("static-A", Some(CacheStrategy::Global)),
            make_block("static-B", Some(CacheStrategy::Global)),
            make_block("dynamic-C", Some(CacheStrategy::Ephemeral)),
            make_block("dynamic-D", None),
        ];
        let ctx = RequestContext::new(
            blocks,
            vec![],
            vec![],
            RequestParams {
                model: "test".into(),
                max_tokens: 4096,
                thinking_mode: ThinkingMode::Auto,
                fallback_model: None,
            },
        );
        let bp = ctx.cache_breakpoint().unwrap();
        assert_eq!(bp.cached_prefix_end, 1); // index 1 = "static-B"
        assert_eq!(bp.total_blocks, 4);
    }

    #[test]
    fn no_cache_breakpoint_when_no_global() {
        let blocks = vec![
            make_block("A", None),
            make_block("B", Some(CacheStrategy::Ephemeral)),
        ];
        let ctx = RequestContext::new(
            blocks,
            vec![],
            vec![],
            RequestParams {
                model: "test".into(),
                max_tokens: 4096,
                thinking_mode: ThinkingMode::Auto,
                fallback_model: None,
            },
        );
        assert!(ctx.cache_breakpoint().is_none());
    }

    #[test]
    fn estimated_tokens_is_positive() {
        let blocks = vec![make_block("Hello, world!", Some(CacheStrategy::Global))];
        let ctx = RequestContext::new(
            blocks,
            vec![],
            vec![],
            RequestParams {
                model: "test".into(),
                max_tokens: 4096,
                thinking_mode: ThinkingMode::Auto,
                fallback_model: None,
            },
        );
        assert!(ctx.estimated_input_tokens() > 0);
    }
}
