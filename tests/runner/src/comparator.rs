//! OutputComparator — 用 LLM 比对实际输出与预期描述。

use crate::api_runner::TurnOutput;
use crate::script::Turn;
use base::interface::model::{
    MessageRole, Model, ModelContentBlock, ModelEvent, ModelMessage, StreamParams,
};
use base::interface::settings::ThinkingMode;
use futures::StreamExt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ComparisonResult {
    pub turn_index: usize,
    pub verdict: Verdict,
    pub reasoning: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
    Partial,
    Skipped,
}

/// Compare actual output against expected description using a comparison LLM.
pub async fn compare_output(
    model: &dyn Model,
    turn: &Turn,
    actual: &TurnOutput,
) -> anyhow::Result<ComparisonResult> {
    if turn.expected.is_empty() {
        return Ok(ComparisonResult {
            turn_index: turn.index,
            verdict: Verdict::Skipped,
            reasoning: "无预期输出描述，跳过比对。".into(),
        });
    }

    let prompt = format!(
        "\
你是一个测试比对助手。请判断 AI Agent 的实际输出是否符合预期。

## 用户输入
{user_input}

## 预期行为描述
{expected}

## Agent 实际输出
### 文本回复
{text}

### 调用的工具
{tools}

## 判定规则
- 如果实际行为与预期描述一致，判定为 pass
- 如果部分一致但有偏差，判定为 partial
- 如果明显不符合预期，判定为 fail
- 请用中文回复，先给出判定（pass/partial/fail），再简述理由。

判定:",
        user_input = turn.input,
        expected = turn.expected,
        text = if actual.text.is_empty() {
            "(无文本输出)"
        } else {
            &actual.text
        },
        tools = if actual.tool_uses.is_empty() {
            "(未调用工具)".to_string()
        } else {
            actual
                .tool_uses
                .iter()
                .map(|(name, input)| format!("- {name}: {input}"))
                .collect::<Vec<_>>()
                .join("\n")
        },
    );

    let messages = vec![ModelMessage {
        role: MessageRole::User,
        content: vec![ModelContentBlock::Text { text: prompt }],
    }];

    let params = StreamParams {
        model: std::env::var("ANTHROPIC_SMALL_FAST_MODEL")
            .unwrap_or_else(|_| "claude-haiku-4-5".into()),
        max_tokens: 256,
        thinking_mode: ThinkingMode::Off,
        fallback_model: None,
        cache_edits: vec![],
    };

    let cancel = CancellationToken::new();
    let mut stream = model
        .stream(vec![], vec![], messages, params, cancel)
        .await?;
    let mut text = String::new();
    while let Some(e) = stream.next().await {
        if let Ok(ModelEvent::TextDelta { text: t }) = e {
            text.push_str(&t);
        }
    }

    let text_lower = text.to_lowercase();
    let verdict = if text_lower.starts_with("pass") {
        Verdict::Pass
    } else if text_lower.starts_with("partial") {
        Verdict::Partial
    } else if text_lower.starts_with("fail") {
        Verdict::Fail
    } else {
        // Fuzzy match
        if text_lower.contains("pass") && !text_lower.contains("partial") {
            Verdict::Pass
        } else if text_lower.contains("partial") {
            Verdict::Partial
        } else {
            Verdict::Fail
        }
    };

    Ok(ComparisonResult {
        turn_index: turn.index,
        verdict,
        reasoning: text.trim().to_string(),
    })
}
