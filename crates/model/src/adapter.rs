//! AnthropicModel — adapts `AnthropicClient` to implement `crate::Model`.

use crate::client::AnthropicClient;
use crate::types::{
    BuiltinTool, CacheControl, MessageParam, MessagesRequest, SystemBlock, ThinkingConfig,
};
use async_trait::async_trait;
use base::interface::model::{
    MessageRole, Model, ModelContentBlock, ModelError, ModelEvent, ModelMessage, ModelStream,
    StreamParams, ToolDef, Usage,
};
use base::interface::prompt::{CacheStrategy, PromptBlock};
use base::interface::settings::ThinkingMode;
use base::message::{CacheEdit, ContentBlock, Role};
use base::provider::ApiType;
use futures::StreamExt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct AnthropicModel {
    inner: Arc<dyn AnthropicClient>,
}

impl AnthropicModel {
    pub fn new(client: Arc<dyn AnthropicClient>) -> Self {
        Self { inner: client }
    }
}

#[async_trait]
impl Model for AnthropicModel {
    fn api_type(&self) -> ApiType {
        ApiType::Anthropic
    }

    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        params: StreamParams,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let system: Vec<SystemBlock> = prompt_blocks
            .into_iter()
            .map(|pb| match pb.cache_strategy {
                Some(CacheStrategy::Ephemeral) => {
                    SystemBlock::text_cached(pb.content, CacheControl::ephemeral_1h())
                }
                Some(CacheStrategy::Global) => {
                    SystemBlock::text_cached(pb.content, CacheControl::ephemeral_1h_global())
                }
                None => SystemBlock::text(pb.content),
            })
            .collect();

        let mut message_params: Vec<MessageParam> = messages
            .into_iter()
            .map(|m| MessageParam {
                role: match m.role {
                    MessageRole::System | MessageRole::User => Role::User,
                    MessageRole::Assistant => Role::Assistant,
                },
                content: m
                    .content
                    .into_iter()
                    .map(|b| match b {
                        ModelContentBlock::Text { text } => ContentBlock::Text {
                            text,
                            cache_control: None,
                        },
                        ModelContentBlock::ToolUse { id, name, input } => {
                            ContentBlock::ToolUse { id, name, input }
                        }
                        ModelContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => ContentBlock::ToolResult {
                            tool_use_id,
                            content: base::message::ToolResultContent::Text(content),
                            is_error: is_error.unwrap_or(false),
                        },
                    })
                    .collect(),
            })
            .collect();

        // P1-2: Wire cache_edits into the API request.
        // When the compaction system has cleared old tool results, we send their
        // tool_use_ids as `cache_edits` to the Anthropic API so the server can
        // delete those results from its cached prefix without invalidating the
        // global cache. Requires the `context-management-2025-06-27` beta header.
        // TS parity: `addCacheBreakpoints()` in claude.ts.
        let has_cache_edits = !params.cache_edits.is_empty();
        if has_cache_edits {
            if let Some(last_user) = message_params
                .iter_mut()
                .rev()
                .find(|m| m.role == Role::User)
            {
                last_user.content.push(ContentBlock::CacheEdits {
                    cache_edits: params
                        .cache_edits
                        .iter()
                        .map(|id| CacheEdit::DeleteToolResult {
                            tool_use_id: id.clone(),
                        })
                        .collect(),
                });
            }
        }

        // DIRECT mode: WebSearch → built-in web_search_20250305
        let has_websearch = tools.iter().any(|t| t.name == "WebSearch");
        let anthropic_builtins: Vec<BuiltinTool> = if has_websearch {
            vec![BuiltinTool::WebSearch {
                name: "web_search".into(),
                allowed_domains: None,
                blocked_domains: None,
                max_uses: None,
            }]
        } else {
            vec![]
        };
        let mut betas: Vec<String> = if has_websearch {
            vec!["web-search-20250305-2025-03-05".into()]
        } else {
            vec![]
        };
        if has_cache_edits {
            betas.push("context-management-2025-06-27".into());
        }
        let anthropic_tools: Vec<crate::types::ToolDef> = tools
            .into_iter()
            .filter(|t| t.name != "WebSearch")
            .map(|t| crate::types::ToolDef {
                name: t.name,
                description: t.description,
                input_schema: t.input_schema,
                cache_control: None,
                defer_loading: None,
                strict: None,
            })
            .collect();

        let thinking = match params.thinking_mode {
            ThinkingMode::Auto => None,
            ThinkingMode::Off => Some(ThinkingConfig::Disabled),
            ThinkingMode::On => Some(ThinkingConfig::Enabled {
                budget_tokens: 4096,
            }),
            ThinkingMode::OnBudget(n) => Some(ThinkingConfig::Enabled { budget_tokens: n }),
        };

        let req = MessagesRequest {
            model: params.model,
            max_tokens: params.max_tokens,
            system,
            messages: message_params,
            tools: anthropic_tools,
            anthropic_tools: anthropic_builtins,
            tool_choice: None,
            stream: true,
            thinking,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: vec![],
            metadata: None,
            betas,
            speed: None,
        };

        let stream = self.inner.stream_messages(req);
        let mapped = stream.map(|result| match result {
            Ok(event) => map_stream_event(event),
            Err(e) => Err(map_error(e)),
        });
        Ok(Box::new(mapped))
    }
}

fn map_stream_event(event: crate::stream::StreamEvent) -> Result<ModelEvent, ModelError> {
    use crate::stream::{BlockDelta, ContentBlockStart, StreamEvent};
    match event {
        StreamEvent::ContentBlockDelta { delta, .. } => match delta {
            BlockDelta::TextDelta { text } => Ok(ModelEvent::TextDelta { text }),
            BlockDelta::InputJsonDelta { partial_json } => {
                Ok(ModelEvent::TextDelta { text: partial_json })
            }
            _ => Ok(ModelEvent::TextDelta {
                text: String::new(),
            }),
        },
        StreamEvent::ContentBlockStart { content_block, .. } => match content_block {
            ContentBlockStart::Text { text } => Ok(ModelEvent::ContentBlockStart {
                index: 0,
                block: ModelContentBlock::Text { text },
            }),
            ContentBlockStart::ToolUse { id, name, input }
            | ContentBlockStart::ServerToolUse { id, name, input } => {
                Ok(ModelEvent::ToolUse { id, name, input })
            }
            _ => Ok(ModelEvent::TextDelta {
                text: String::new(),
            }),
        },
        StreamEvent::MessageDelta { delta, usage } => {
            let stop_reason = delta
                .stop_reason
                .map(|sr| match sr {
                    base::message::StopReason::EndTurn => "end_turn",
                    base::message::StopReason::MaxTokens => "max_tokens",
                    base::message::StopReason::ToolUse => "tool_use",
                    base::message::StopReason::StopSequence => "stop_sequence",
                    base::message::StopReason::PauseTurn => "pause_turn",
                    _ => "unknown",
                })
                .unwrap_or("unknown")
                .to_string();
            Ok(ModelEvent::EndTurn {
                stop_reason,
                usage: usage.map_or(Usage::default(), |u| Usage {
                    input_tokens: u.input_tokens as u32,
                    output_tokens: u.output_tokens as u32,
                }),
            })
        }
        StreamEvent::Error { error } => Err(ModelError::Api {
            status: 500,
            message: error.message,
        }),
        _ => Ok(ModelEvent::TextDelta {
            text: String::new(),
        }),
    }
}

fn map_error(e: crate::error::AnthropicError) -> ModelError {
    use crate::error::AnthropicError;
    match e {
        AnthropicError::Auth(msg) => ModelError::Auth(msg),
        AnthropicError::RateLimited { .. } => ModelError::RateLimited,
        AnthropicError::Overloaded { .. } => ModelError::Overloaded,
        AnthropicError::Transport(e) => ModelError::Network(e.to_string()),
        AnthropicError::Server { status, body } => ModelError::Api {
            status,
            message: body,
        },
        AnthropicError::Cancelled => ModelError::Cancelled,
        other => ModelError::Internal(other.to_string()),
    }
}
