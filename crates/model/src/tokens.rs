//! Token 估算 —— 用 tiktoken `cl100k_base` 做近似（Anthropic 不公开精确 tokenizer，
//! cl100k 实测高估约 5-15%，足以用作 autoCompact 阈值判断）。
//!
//! 真实 token 数应当走 Anthropic 的 `/v1/messages/count_tokens` 端点，但那需要
//! 网络调用 + API 配额；本地估算是它的廉价代理。

use crate::types::{MessagesRequest, SystemBlock};
use base::message::{ContentBlock, ToolResultContent};
use std::sync::OnceLock;
use tiktoken_rs::CoreBPE;

/// 进程级 BPE 单例。`cl100k_base` BPE 表会从 tiktoken-rs 内嵌资源里加载（数 MB）；
/// 第一次访问后缓存。
fn bpe() -> &'static CoreBPE {
    static B: OnceLock<CoreBPE> = OnceLock::new();
    B.get_or_init(|| tiktoken_rs::cl100k_base().expect("cl100k_base BPE table bundled and valid"))
}

/// 估算一段 `MessagesRequest` 的输入 token 数（不含模型输出）。
///
/// 覆盖：
/// - system blocks 文本
/// - tools[] 的 description + input_schema（schema 序列化成 JSON 字符串）
/// - messages[] 的所有 ContentBlock
///
/// 不覆盖（轻微低估）：
/// - role / 字段名等 API 框架开销（实际占 ~5-15 token）
/// - JSON 结构本身（`{` `}` `,` 等）
pub fn estimate_input_tokens(req: &MessagesRequest) -> usize {
    let bpe = bpe();
    let mut total = 0usize;

    // system
    for block in &req.system {
        let SystemBlock::Text { text, .. } = block;
        total += bpe.encode_with_special_tokens(text).len();
    }

    // tools
    for tool in &req.tools {
        total += bpe.encode_with_special_tokens(&tool.description).len();
        let schema_str = tool.input_schema.to_string();
        total += bpe.encode_with_special_tokens(&schema_str).len();
        total += bpe.encode_with_special_tokens(&tool.name).len();
    }

    // messages
    for msg in &req.messages {
        for block in &msg.content {
            total += estimate_block_tokens(block, bpe);
        }
    }

    total
}

fn estimate_block_tokens(block: &ContentBlock, bpe: &CoreBPE) -> usize {
    match block {
        ContentBlock::Text { text, .. } => bpe.encode_with_special_tokens(text).len(),
        ContentBlock::ToolUse { name, input, id } => {
            bpe.encode_with_special_tokens(name).len()
                + bpe.encode_with_special_tokens(id).len()
                + bpe.encode_with_special_tokens(&input.to_string()).len()
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => {
            let id_tokens = bpe.encode_with_special_tokens(tool_use_id).len();
            let body_tokens = match content {
                ToolResultContent::Text(t) => bpe.encode_with_special_tokens(t).len(),
                ToolResultContent::Blocks(blocks) => {
                    blocks.iter().map(|b| estimate_block_tokens(b, bpe)).sum()
                }
            };
            id_tokens + body_tokens
        }
        ContentBlock::Thinking { thinking, .. } => bpe.encode_with_special_tokens(thinking).len(),
        ContentBlock::RedactedThinking { data } => bpe.encode_with_special_tokens(data).len(),
        // 图像按 1500 token 粗估（Anthropic 文档说 image ≈ 1500 token）
        ContentBlock::Image { .. } => 1500,
        // Cache edits are metadata-only (delete-tool-result operations), negligible token cost
        ContentBlock::CacheEdits { .. } => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MessageParam, ToolDef};
    use base::message::Role;
    use serde_json::json;

    fn empty_request() -> MessagesRequest {
        MessagesRequest::minimal("test", "")
    }

    #[test]
    fn empty_messages_zero_tokens_in_user() {
        let mut req = empty_request();
        req.messages = vec![]; // 完全空
        assert_eq!(estimate_input_tokens(&req), 0);
    }

    #[test]
    fn user_message_text_counted() {
        let req = MessagesRequest::minimal("test", "hello world");
        let n = estimate_input_tokens(&req);
        // "hello world" 是 2 个 token (按 cl100k)
        assert!((2..=5).contains(&n), "expected 2-5 tokens, got {n}");
    }

    #[test]
    fn longer_text_more_tokens() {
        let short = MessagesRequest::minimal("test", "hi");
        let long = MessagesRequest::minimal(
            "test",
            "hello world this is a longer message with many tokens",
        );
        assert!(estimate_input_tokens(&long) > estimate_input_tokens(&short));
    }

    #[test]
    fn system_block_counted() {
        let mut req = empty_request();
        req.system = vec![SystemBlock::text("system instructions go here")];
        let n_with = estimate_input_tokens(&req);
        req.system.clear();
        let n_without = estimate_input_tokens(&req);
        assert!(n_with > n_without, "system block should increase count");
    }

    #[test]
    fn tools_contribute_tokens() {
        let mut req = empty_request();
        req.tools = vec![ToolDef {
            name: "Bash".into(),
            description: "Run a shell command. Use sparingly.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"]
            }),
            cache_control: None,
            defer_loading: None,
            strict: None,
        }];
        let n = estimate_input_tokens(&req);
        // tools 至少应当 > 5 token
        assert!(n > 5, "expected >5 tokens for one ToolDef, got {n}");
    }

    #[test]
    fn tool_use_block_counted() {
        let mut req = empty_request();
        req.messages = vec![MessageParam {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "toolu_01".into(),
                name: "Bash".into(),
                input: json!({"command": "ls -la"}),
            }],
        }];
        let n = estimate_input_tokens(&req);
        assert!(n > 3, "expected >3 tokens for tool_use, got {n}");
    }

    #[test]
    fn tool_result_text_counted() {
        let mut req = empty_request();
        req.messages = vec![MessageParam {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_01".into(),
                content: ToolResultContent::Text("a\nb\nc".into()),
                is_error: false,
            }],
        }];
        let n = estimate_input_tokens(&req);
        assert!(n > 3);
    }

    #[test]
    fn image_block_costs_1500() {
        let mut req = empty_request();
        req.messages = vec![MessageParam {
            role: Role::User,
            content: vec![ContentBlock::Image {
                source: base::message::ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "tiny".into(), // 数据本身不再额外计 —— 我们用固定估算
                },
            }],
        }];
        let n = estimate_input_tokens(&req);
        assert!(n >= 1500, "image should cost ≈1500 tokens; got {n}");
    }
}
