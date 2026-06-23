//! AnthropicClient trait + HttpAnthropicClient（reqwest 实现）。
//!
//! 重试 / beta header / 多 provider 都在 HttpAnthropicClient 内部；
//! Engine 只看 trait。详见 docs/RUST_ARCHITECTURE.md §6。

use crate::error::AnthropicError;
use crate::parser::parse_sse;
use crate::stream::StreamEvent;
use crate::types::MessagesRequest;
use futures::stream::{Stream, StreamExt, TryStreamExt};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

/// Anthropic API base URL.
pub const ANTHROPIC_API_BASE_URL: &str = "https://api.anthropic.com/";

/// 默认重试退避序列（毫秒）：6 步指数退避，覆盖 ~1 分钟。
const DEFAULT_BACKOFF_MS: &[u64] = &[1_000, 2_000, 4_000, 8_000, 16_000, 32_000];
/// jitter 比例：±25%
const JITTER_RATIO: f32 = 0.25;

/// 把 reqwest::Response 按状态码分流为 Ok(continue) / Err(分类后的 AnthropicError)。
/// 抽到独立 fn：try_stream! 宏内的 ? 不能让借用检查器看出"err 分支总返回"。
async fn classify_response(resp: reqwest::Response) -> Result<reqwest::Response, AnthropicError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let code = status.as_u16();
    // 提前抓 retry-after / anthropic-ratelimit-* 头（resp.text() 后 headers 不可用了）
    let retry_after = parse_retry_after(resp.headers());
    let mut anthropic_headers = HashMap::new();
    for (name, value) in resp.headers() {
        let n = name.as_str().to_lowercase();
        if n.starts_with("anthropic-ratelimit-") || n == "retry-after" {
            if let Ok(v) = value.to_str() {
                anthropic_headers.insert(n, v.to_string());
            }
        }
    }
    // 截掉超长 body 防止日志爆炸
    let body = resp.text().await.unwrap_or_default();
    let body = if body.len() > 4096 {
        body[..4096].to_string()
    } else {
        body
    };
    Err(match code {
        401 | 403 => AnthropicError::Auth(body),
        429 => AnthropicError::RateLimited {
            retry_after,
            headers: anthropic_headers,
        },
        503 | 529 => AnthropicError::Overloaded { status: code },
        _ => AnthropicError::Server { status: code, body },
    })
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let v = headers.get("retry-after")?.to_str().ok()?;
    // RFC 7231 retry-after：要么数字秒数，要么 HTTP 时间。我们只支持数字秒数。
    v.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// 一次 HTTP send + classify。失败时按 AnthropicError 分类返回。
async fn send_one(
    inner: &HttpInner,
    req: &MessagesRequest,
) -> Result<reqwest::Response, AnthropicError> {
    let url = inner
        .base
        .join("v1/messages")
        .map_err(|e| AnthropicError::Transport(anyhow::Error::new(e)))?;

    let body = serde_json::to_vec(req)?;

    let mut builder = inner
        .http
        .post(url)
        .header("anthropic-version", inner.anthropic_version)
        .header("content-type", "application/json");

    // **Q4-followup **: collect beta tags from BOTH the request (per-call
    // explicit) AND the auth mode (e.g. OAuth flows need
    // `anthropic-beta: oauth-2025-04-20` set unconditionally). De-dup before
    // joining.
    let mut betas: Vec<String> = req.betas.clone();
    match &inner.auth {
        AuthMode::ApiKey(k) => builder = builder.header("x-api-key", k),
        AuthMode::OauthToken(t) => {
            builder = builder.bearer_auth(t);
            push_unique(&mut betas, "oauth-2025-04-20");
        }
        AuthMode::OauthRefreshing(provider) => {
            let token = provider.current_bearer_token().await?;
            builder = builder.bearer_auth(&token);
            push_unique(&mut betas, "oauth-2025-04-20");
        }
    }

    if !betas.is_empty() {
        builder = builder.header("anthropic-beta", betas.join(","));
    }

    let resp = builder.body(body).send().await?;
    classify_response(resp).await
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    if !v.iter().any(|x| x == s) {
        v.push(s.to_string());
    }
}

/// 在首字节前重试：429 / 503 / 529 / 网络错按指数退避（含 jitter）；
/// 429 优先尊重 retry-after 头。最多 6 次（覆盖 ~1 分钟）。
async fn send_with_retry(
    inner: &HttpInner,
    req: &MessagesRequest,
) -> Result<reqwest::Response, AnthropicError> {
    let backoff = inner.backoff_ms.as_slice();
    let mut attempt = 0usize;
    loop {
        match send_one(inner, req).await {
            Ok(resp) => return Ok(resp),
            Err(e) if e.is_retryable() && attempt < backoff.len() => {
                let base = match &e {
                    AnthropicError::RateLimited {
                        retry_after: Some(d),
                        ..
                    } => d.as_millis() as u64,
                    _ => backoff[attempt],
                };
                let delay = jittered_delay(base);
                tracing::warn!(
                    attempt = attempt + 1,
                    delay_ms = delay.as_millis() as u64,
                    error = %e,
                    "anthropic request retryable; backing off"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// 给定的 base 退避基础上加 ±25% jitter。
fn jittered_delay(base_ms: u64) -> Duration {
    use std::time::SystemTime;
    // 用 nanos 当随机源，足够散；不上 rand 依赖
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let unit = (nanos % 1_000_000) as f32 / 1_000_000.0; // [0, 1)
    let signed = (unit - 0.5) * 2.0; // [-1, 1)
    let delta_ms = (base_ms as f32 * JITTER_RATIO * signed) as i64;
    let final_ms = (base_ms as i64 + delta_ms).max(0) as u64;
    Duration::from_millis(final_ms)
}

pub type EventStream =
    Pin<Box<dyn Stream<Item = Result<StreamEvent, AnthropicError>> + Send + 'static>>;

pub type CountFuture<'a> = Pin<Box<dyn Future<Output = Result<usize, AnthropicError>> + Send + 'a>>;

/// 不上 `#[async_trait]` —— 流式返回 `Box<dyn Stream>` 比 async_trait 改写
/// 出的 future-of-stream 更直观，也省一层 box。
pub trait AnthropicClient: Send + Sync {
    fn stream_messages(&self, req: MessagesRequest) -> EventStream;

    fn count_tokens<'a>(&'a self, req: &'a MessagesRequest) -> CountFuture<'a>;
}

#[derive(Clone)]
pub enum AuthMode {
    /// `x-api-key: <token>`（标准）
    ApiKey(String),
    /// `authorization: Bearer <token>`（OAuth / Anthropic session token，静态）
    OauthToken(String),
    /// **P3 **: dynamic OAuth token. Each request consults the provider,
    /// which may pre-emptively refresh if the token is about to expire and
    /// write the new token back to its underlying store.
    OauthRefreshing(Arc<dyn BearerTokenProvider>),
}

impl std::fmt::Debug for AuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey(_) => f.write_str("AuthMode::ApiKey(<redacted>)"),
            Self::OauthToken(_) => f.write_str("AuthMode::OauthToken(<redacted>)"),
            Self::OauthRefreshing(_) => f.write_str("AuthMode::OauthRefreshing(<provider>)"),
        }
    }
}

/// **P3 **: token-source abstraction. Implementors handle refresh +
/// store mutation behind the scenes; the client only ever asks for "the
/// current valid bearer token".
#[async_trait::async_trait]
pub trait BearerTokenProvider: Send + Sync {
    async fn current_bearer_token(&self) -> Result<String, AnthropicError>;
}

#[derive(Clone)]
pub struct HttpAnthropicClient {
    inner: Arc<HttpInner>,
}

struct HttpInner {
    http: reqwest::Client,
    base: Url,
    auth: AuthMode,
    anthropic_version: &'static str,
    /// 退避序列（毫秒）；测试用 with_backoff 覆盖以缩短测试时间
    backoff_ms: Vec<u64>,
}

impl HttpAnthropicClient {
    /// 创建一个默认指向 `https://api.anthropic.com` 的 client。
    pub fn new(auth: AuthMode) -> Result<Self, AnthropicError> {
        Self::with_base(auth, Url::parse(ANTHROPIC_API_BASE_URL).unwrap())
    }

    /// 自定义 base URL（用于本地 mock server / Bedrock relay 等）。
    pub fn with_base(auth: AuthMode, base: Url) -> Result<Self, AnthropicError> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("attacode/", env!("CARGO_PKG_VERSION")))
            // 连接超时：30s 内连不上就放弃
            .connect_timeout(Duration::from_secs(30))
            // **重要**：不使用 ClientBuilder::timeout()，因为它会包装整个响应体流，
            // 对于 SSE 流式场景会导致连接在 300s 后断开。流式超时由引擎层 CancellationToken 负责。
            .build()
            .map_err(|e| AnthropicError::Transport(anyhow::Error::new(e)))?;
        Ok(Self {
            inner: Arc::new(HttpInner {
                http,
                base,
                auth,
                anthropic_version: "2023-06-01",
                backoff_ms: DEFAULT_BACKOFF_MS.to_vec(),
            }),
        })
    }

    /// 自定义退避序列（毫秒）。测试用——缩短重试间隔避免拖长 CI。
    pub fn with_backoff(mut self, backoff_ms: Vec<u64>) -> Self {
        // 不能 mutate Arc<HttpInner>；重建一遍
        let inner = (*self.inner).clone_with_backoff(backoff_ms);
        self.inner = Arc::new(inner);
        self
    }
}

impl HttpInner {
    fn clone_with_backoff(&self, backoff_ms: Vec<u64>) -> Self {
        Self {
            http: self.http.clone(),
            base: self.base.clone(),
            auth: self.auth.clone(),
            anthropic_version: self.anthropic_version,
            backoff_ms,
        }
    }
}

impl AnthropicClient for HttpAnthropicClient {
    fn stream_messages(&self, req: MessagesRequest) -> EventStream {
        let inner = self.inner.clone();

        Box::pin(async_stream::try_stream! {
            // 重试只在**首字节前**做：classify_response 拿到 Ok 之后开始流式，
            // 流中错（或服务端 error 事件）不重试 —— 避免重复计 token。
            let resp = send_with_retry(&inner, &req).await?;

            // bytes_stream 的 Err 是 reqwest::Error，转成 AnthropicError 才能喂 parse_sse
            let byte_stream = resp
                .bytes_stream()
                .map_err(|e| AnthropicError::Transport(anyhow::Error::new(e)));

            let mut events = parse_sse(byte_stream);
            while let Some(ev) = events.next().await {
                yield ev?;
            }
        })
    }

    fn count_tokens<'a>(&'a self, req: &'a MessagesRequest) -> CountFuture<'a> {
        // 本地 tiktoken 估算（cl100k_base，Anthropic 不公开精确 tokenizer）。
        // 用于 autoCompact 的阈值检查，足够。
        // 实际精确计 token 走 Anthropic /v1/messages/count_tokens 端点 —— 推迟到 。
        Box::pin(async move { Ok(crate::tokens::estimate_input_tokens(req)) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// trait object 可以装 —— 这是给 Engine 用的方式。
    #[test]
    fn boxes_into_trait_object() {
        let c = HttpAnthropicClient::new(AuthMode::ApiKey("dummy".into())).unwrap();
        let _: Arc<dyn AnthropicClient> = Arc::new(c);
    }

    /// 没有 ANTHROPIC_API_KEY / 没有网络也不能 panic。仅检查 URL 拼写 +
    /// header 装载逻辑能编过。实际 HTTP 调用在 integration test 里走 mock server。
    #[test]
    fn build_http_client_does_not_panic() {
        let c = HttpAnthropicClient::new(AuthMode::ApiKey("x".into()));
        assert!(c.is_ok());
        let c2 = HttpAnthropicClient::with_base(
            AuthMode::OauthToken("y".into()),
            Url::parse("http://127.0.0.1:1/").unwrap(),
        );
        assert!(c2.is_ok());
    }
}
