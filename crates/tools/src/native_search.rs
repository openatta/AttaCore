//! NativeSearchProvider —— 用 provider 自带的服务端搜索取代 DuckDuckGo 抓取。
//!
//! 调用 Anthropic Messages API，传入 `web_search_20250305` built-in tool，
//! 让 API 服务端执行搜索后返回结构化的 `web_search_tool_result` 块。
//! DeepSeek 兼容模式下同样走 Server-Side Search（差异仅在于 beta header 与
//! model name）。
//!
//! 参见 docs/COMPARISON_EXPERIMENT.md。

use crate::web_search::{
    SearchError, SearchOutput, SearchOutputItem, SearchProvider, SearchResult,
};
use async_trait::async_trait;
use base::message::{ContentBlock, Role};
use futures::stream::StreamExt;
use model::client::AnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, StreamEvent};
use model::types::{
    BuiltinTool, MessageParam, MessagesRequest, SystemBlock, ThinkingConfig, ToolChoice,
};
use std::sync::Arc;

/// Calls the provider API with a built-in web search tool (`web_search_20250305`)
/// and extracts structured search results from `web_search_tool_result` blocks.
pub struct NativeSearchProvider {
    client: Arc<dyn AnthropicClient>,
    model: String,
    /// Thinking config for the sub-call, matching the main loop's configuration.
    thinking: Option<ThinkingConfig>,
}

impl std::fmt::Debug for NativeSearchProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSearchProvider")
            .field("model", &self.model)
            .field("thinking", &self.thinking)
            .finish_non_exhaustive()
    }
}

impl NativeSearchProvider {
    pub fn new(
        client: Arc<dyn AnthropicClient>,
        model: impl Into<String>,
        thinking: Option<ThinkingConfig>,
    ) -> Self {
        Self {
            client,
            model: model.into(),
            thinking,
        }
    }
}

#[async_trait]
impl SearchProvider for NativeSearchProvider {
    async fn search(&self, query: &str, max_results: usize) -> Result<SearchOutput, SearchError> {
        let req = MessagesRequest {
            model: self.model.clone(),
            max_tokens: 8192,
            system: vec![SystemBlock::text(
                "You are an assistant for performing a web search tool use.",
            )],
            messages: vec![MessageParam {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: format!("Perform a web search for the query: {query}"),
                    cache_control: None,
                }],
            }],
            tools: vec![],
            anthropic_tools: vec![BuiltinTool::WebSearch {
                name: "web_search".into(),
                allowed_domains: None,
                blocked_domains: None,
                max_uses: Some(max_results.min(8) as u32),
            }],
            tool_choice: Some(ToolChoice::Tool {
                name: "web_search".into(),
            }),
            stream: true,
            thinking: self.thinking.clone(),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: vec![],
            metadata: None,
            // `web-search-20250305-2025-03-05` beta is required for the
            // `web_search_20250305` built-in tool on the Anthropic API.
            // DeepSeek-compatible backends may accept or ignore it.
            betas: vec!["web-search-20250305-2025-03-05".to_string()],
            speed: None,
        };

        let mut stream = self.client.stream_messages(req);
        let mut items: Vec<SearchOutputItem> = Vec::new();

        while let Some(ev) = stream.next().await {
            let event = ev.map_err(|e| SearchError::Other(anyhow::Error::new(e)))?;
            match event {
                StreamEvent::ContentBlockStart {
                    content_block:
                        ContentBlockStart::WebSearchToolResult {
                            tool_use_id: _,
                            content,
                        },
                    ..
                } => {
                    // content 可能是 Vec<{title, url}> 或错误对象
                    let mut links = Vec::new();
                    if let Some(arr) = content.as_array() {
                        for item in arr {
                            let title = item
                                .get("title")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let url = item
                                .get("url")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            links.push(SearchResult {
                                title,
                                url,
                                snippet: String::new(),
                            });
                        }
                    }
                    items.push(SearchOutputItem::Links(links));
                }
                // Capture text commentary — merge consecutive text into one item
                // to preserve the natural ordering alongside search result links.
                StreamEvent::ContentBlockStart {
                    content_block: ContentBlockStart::Text { text },
                    ..
                } => {
                    if let Some(SearchOutputItem::Text(last)) = items.last_mut() {
                        last.push_str(&text);
                    } else {
                        items.push(SearchOutputItem::Text(text));
                    }
                }
                StreamEvent::ContentBlockDelta {
                    delta: BlockDelta::TextDelta { text },
                    ..
                } => {
                    if let Some(SearchOutputItem::Text(last)) = items.last_mut() {
                        last.push_str(&text);
                    } else {
                        items.push(SearchOutputItem::Text(text));
                    }
                }
                StreamEvent::MessageStop => break,
                _ => {}
            }
        }

        Ok(SearchOutput { items })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::mock::MockAnthropicClient;
    use model::stream::{ContentBlockStart, MessageDeltaPayload, MessageStartPayload, Usage};

    fn web_search_stream(results: &[(&str, &str)]) -> Vec<StreamEvent> {
        let content: Vec<serde_json::Value> = results
            .iter()
            .map(|(t, u)| serde_json::json!({"title": t, "url": u}))
            .collect();

        vec![
            StreamEvent::MessageStart {
                message: MessageStartPayload {
                    id: "msg_ws".into(),
                    role: "assistant".into(),
                    model: "test".into(),
                    usage: Usage::default(),
                    stop_reason: None,
                },
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::WebSearchToolResult {
                    tool_use_id: "stoolu_01".into(),
                    content: serde_json::Value::Array(content),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaPayload {
                    stop_reason: Some(base::message::StopReason::EndTurn),
                    stop_sequence: None,
                },
                usage: None,
            },
            StreamEvent::MessageStop,
        ]
    }

    fn web_search_stream_with_commentary(
        results: &[(&str, &str)],
        commentary_text: &str,
    ) -> Vec<StreamEvent> {
        let search_content: Vec<serde_json::Value> = results
            .iter()
            .map(|(t, u)| serde_json::json!({"title": t, "url": u}))
            .collect();

        vec![
            StreamEvent::MessageStart {
                message: MessageStartPayload {
                    id: "msg_ws".into(),
                    role: "assistant".into(),
                    model: "test".into(),
                    usage: Usage::default(),
                    stop_reason: None,
                },
            },
            // Search result block
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::WebSearchToolResult {
                    tool_use_id: "stoolu_01".into(),
                    content: serde_json::Value::Array(search_content),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            // Model commentary text block
            StreamEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlockStart::Text {
                    text: commentary_text.into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 1 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaPayload {
                    stop_reason: Some(base::message::StopReason::EndTurn),
                    stop_sequence: None,
                },
                usage: None,
            },
            StreamEvent::MessageStop,
        ]
    }

    #[tokio::test]
    async fn returns_search_results_from_web_search_result_blocks() {
        let mock = Arc::new(MockAnthropicClient::new());
        mock.push_turn(web_search_stream(&[
            ("Rust Lang", "https://rust-lang.org"),
            ("Learn Rust", "https://learn-rust.org"),
        ]));

        let provider = NativeSearchProvider::new(mock, "test-model", None);
        let output = provider.search("rust programming", 10).await.unwrap();

        let (links, texts): (Vec<_>, Vec<_>) = output
            .items
            .iter()
            .partition(|item| matches!(item, SearchOutputItem::Links(_)));
        assert_eq!(links.len(), 1);
        if let SearchOutputItem::Links(results) = &output.items[0] {
            assert_eq!(results.len(), 2);
            assert_eq!(results[0].title, "Rust Lang");
            assert_eq!(results[0].url, "https://rust-lang.org");
            assert_eq!(results[1].title, "Learn Rust");
            assert_eq!(results[1].url, "https://learn-rust.org");
        } else {
            panic!("expected Links item");
        }
        assert!(texts.is_empty());
    }

    #[tokio::test]
    async fn empty_results_when_no_web_search_blocks() {
        let mock = Arc::new(MockAnthropicClient::new());
        mock.push_turn(vec![
            StreamEvent::MessageStart {
                message: MessageStartPayload {
                    id: "msg".into(),
                    role: "assistant".into(),
                    model: "test".into(),
                    usage: Usage::default(),
                    stop_reason: None,
                },
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Text {
                    text: "no search here".into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaPayload {
                    stop_reason: Some(base::message::StopReason::EndTurn),
                    stop_sequence: None,
                },
                usage: None,
            },
            StreamEvent::MessageStop,
        ]);

        let provider = NativeSearchProvider::new(mock, "test-model", None);
        let output = provider.search("anything", 10).await.unwrap();
        // No Links items, only a Text item
        let has_links = output
            .items
            .iter()
            .any(|item| matches!(item, SearchOutputItem::Links(_)));
        assert!(!has_links);
        // The text block "no search here" is captured as ordered text
        let texts: Vec<_> = output
            .items
            .iter()
            .filter_map(|item| {
                if let SearchOutputItem::Text(t) = item {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(texts, vec!["no search here"]);
    }

    #[tokio::test]
    async fn captures_text_blocks_as_commentary() {
        let mock = Arc::new(MockAnthropicClient::new());
        mock.push_turn(web_search_stream_with_commentary(
            &[("Rust Lang", "https://rust-lang.org")],
            "I found a systems programming language called Rust.",
        ));

        let provider = NativeSearchProvider::new(mock, "test-model", None);
        let output = provider.search("rust language", 10).await.unwrap();

        // First item should be the search result
        assert!(matches!(output.items[0], SearchOutputItem::Links(_)));
        // Second item should be the commentary
        assert!(
            matches!(&output.items[1], SearchOutputItem::Text(t) if t.contains("systems programming language"))
        );
    }
}
