//! Anthropic API 错误分类。

use std::time::Duration;

#[derive(thiserror::Error, Debug)]
pub enum AnthropicError {
    /// 网络层失败 / 连接重置 / DNS / TLS 等
    #[error("transport: {0}")]
    Transport(#[source] anyhow::Error),

    /// HTTP 429 / 服务端 retry-after
    #[error("rate limited; retry-after = {retry_after:?}")]
    RateLimited {
        retry_after: Option<Duration>,
        /// `anthropic-ratelimit-*` headers，原样保留供上层决策
        headers: std::collections::HashMap<String, String>,
    },

    /// HTTP 503 / 529 —— 后端过载，可重试
    #[error("overloaded (HTTP {status})")]
    Overloaded { status: u16 },

    /// 其他 4xx / 5xx —— 不重试，向上抛
    #[error("server error {status}: {body}")]
    Server { status: u16, body: String },

    /// 认证失败（401 / 403）
    #[error("auth: {0}")]
    Auth(String),

    /// SSE 流中途断连
    #[error("stream interrupted")]
    StreamInterrupted,

    /// SSE 解析错（行格式 / data 非 JSON / unknown event）
    #[error("sse: {0}")]
    Sse(String),

    /// JSON schema 错（请求 / 响应反序列化）
    #[error("schema: {0}")]
    Schema(#[from] serde_json::Error),

    /// 模型主动拒绝
    #[error("refused by model")]
    Refused,

    /// 取消（CancellationToken 触发）
    #[error("cancelled")]
    Cancelled,

    /// 解析错误（意外类型 / 格式）
    #[error("parse error: {0}")]
    ParseError(String),
}

impl AnthropicError {
    /// 是否值得在**首字节前**重试。已开始的流不重试（避免重复计费）。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Transport(_) | Self::RateLimited { .. } | Self::Overloaded { .. }
        )
    }

    /// 是否像“上下文过长 / prompt too long / 413”这类可通过压缩缓解的错误。
    pub fn is_context_limit_error(&self) -> bool {
        match self {
            Self::Server { status, body } => {
                if matches!(*status, 400 | 413 | 422) {
                    let body = body.to_ascii_lowercase();
                    body.contains("context")
                        || body.contains("token")
                        || body.contains("prompt")
                        || body.contains("too long")
                        || body.contains("exceed")
                        || body.contains("maximum")
                } else {
                    false
                }
            }
            Self::StreamInterrupted => true,
            _ => false,
        }
    }
}

impl From<reqwest::Error> for AnthropicError {
    fn from(e: reqwest::Error) -> Self {
        Self::Transport(anyhow::Error::new(e))
    }
}
