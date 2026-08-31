//! `OpenAICompatibleModel` — a second protocol implementation of
//! [`base::interface::model::Model`], speaking OpenAI **Chat Completions**
//! (`POST <base>/v1/chat/completions`) instead of the Anthropic Messages API.
//!
//! Why this exists: `base::provider::ApiType::OpenAICompatible` has been a
//! valid `settings.json` value since multi-provider config landed, but the
//! only `impl Model` in the workspace spoke Anthropic, so
//! `daemon::model_router` had to hard-error on it at startup. That meant the
//! product could only talk to Anthropic or to Anthropic-protocol relays — not
//! OpenAI, vLLM, Ollama, or any of the many gateways that only expose the
//! OpenAI shape. (Audit finding N-16.)
//!
//! Layout mirrors the Anthropic side so the two are comparable file-for-file:
//!
//! | Anthropic            | here                  |
//! |----------------------|-----------------------|
//! | `crate::types`       | [`request`]           |
//! | `crate::stream` + the mapping half of `crate::adapter` | [`stream`] |
//! | `crate::parser`      | [`stream::events_from_bytes`] |
//! | `crate::client`      | this module           |
//! | `crate::adapter`     | this module           |
//!
//! The HTTP layer is deliberately not abstracted behind a trait the way
//! `crate::client::AnthropicClient` is: that trait exists so the mock
//! recorder can substitute a fake, and nothing records OpenAI traffic yet.
//! When it does, the split point is [`OpenAICompatibleModel::stream`].

pub mod request;
pub mod stream;

use async_trait::async_trait;
use base::interface::backoff::{
    Backoff, BackoffPolicy, FailedAttempt, LadderBackoff, RequestFailure,
};
use base::interface::model::{Model, ModelError, ModelMessage, ModelStream, StreamParams, ToolDef};
use base::interface::prompt::PromptBlock;
use base::provider::ApiType;
use futures::stream::TryStreamExt;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

use self::request::ChatCompletionsRequest;

/// Maximum silence between two SSE events before the stream is declared dead.
/// See [`stream::events_from_bytes`]; same value and same reasoning as
/// `crate::client::STREAM_IDLE_TIMEOUT`.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// An OpenAI-Chat-Completions-speaking [`Model`].
#[derive(Clone)]
pub struct OpenAICompatibleModel {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    /// Fully resolved `.../chat/completions` endpoint (see
    /// [`chat_completions_url`]).
    endpoint: Url,
    api_key: String,
    /// Provider-specific headers (`OpenAI-Organization`, an Azure
    /// `api-version`, a gateway's routing header, …). Applied verbatim.
    extra_headers: Vec<(String, String)>,
    /// Used when [`StreamParams::model`] is empty — the `default_model` from
    /// the provider's config.
    default_model: String,
    /// Shared with `crate::client` — see
    /// [`BackoffPolicy`](base::interface::backoff::BackoffPolicy). Retrying
    /// only before the first byte is deliberate and stays here: once tokens
    /// have been billed, a retry double-charges them, and that is a fact about
    /// this protocol rather than a thing a policy gets to decide.
    backoff: Arc<dyn BackoffPolicy>,
}

impl OpenAICompatibleModel {
    /// `base_url` may be given with or without the `/v1` suffix, and with or
    /// without a trailing slash — all four spellings appear in the wild, and
    /// getting it wrong yields a 404 at first use rather than at startup.
    ///
    /// `model_default` is the provider's configured `default_model`; a
    /// per-request `StreamParams::model` overrides it.
    pub fn new(
        base_url: &str,
        api_key: impl Into<String>,
        model_default: impl Into<String>,
    ) -> Result<Self, ModelError> {
        let endpoint = chat_completions_url(base_url)?;
        // No `ClientBuilder::timeout()`: it bounds the whole response body,
        // which for SSE means the connection is torn down mid-stream after the
        // timeout. Streaming lifetime is the engine's `CancellationToken` to
        // manage. (Same trap as `crate::client`.)
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("attacode/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30));
        if crate::client::is_loopback(&endpoint) {
            builder = builder.no_proxy();
        }
        let http = builder
            .build()
            .map_err(|e| ModelError::Network(e.to_string()))?;

        Ok(Self {
            inner: Arc::new(Inner {
                http,
                endpoint,
                api_key: api_key.into(),
                extra_headers: Vec::new(),
                default_model: model_default.into(),
                backoff: Arc::new(LadderBackoff::new()),
            }),
        })
    }

    /// Attach provider-specific request headers.
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        let inner = Arc::make_mut(&mut self.inner);
        inner.extra_headers = headers;
        self
    }

    /// Shorten the retry ladder. Tests only — the default covers ~1 minute.
    pub fn with_backoff(self, backoff_ms: Vec<u64>) -> Self {
        self.with_backoff_policy(Arc::new(LadderBackoff::from_millis(backoff_ms)))
    }

    /// Replace the backoff policy outright. Where a host writing its own
    /// `ModelFactory` plugs in.
    pub fn with_backoff_policy(mut self, backoff: Arc<dyn BackoffPolicy>) -> Self {
        let inner = Arc::make_mut(&mut self.inner);
        inner.backoff = backoff;
        self
    }

    /// The resolved endpoint this model will POST to. Exposed for startup
    /// logging and tests.
    pub fn endpoint(&self) -> &Url {
        &self.inner.endpoint
    }
}

// `Arc::make_mut` needs `Clone`; `reqwest::Client` is itself an `Arc` handle,
// so this clones a handful of pointers, not a connection pool.
impl Clone for Inner {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            endpoint: self.endpoint.clone(),
            api_key: self.api_key.clone(),
            extra_headers: self.extra_headers.clone(),
            default_model: self.default_model.clone(),
            backoff: self.backoff.clone(),
        }
    }
}

/// Resolve a configured `base_url` to the chat-completions endpoint.
///
/// `Url::join` alone is not enough here: joining `"v1/chat/completions"` onto
/// `https://host/v1/` produces `https://host/v1/v1/chat/completions`, while
/// joining `"chat/completions"` onto `https://host` drops the path the user
/// configured. Both `https://api.openai.com/v1` and a bare
/// `http://localhost:11434` are things people put in config, so the `/v1`
/// segment is detected rather than assumed either way. A base that already
/// names the full endpoint is passed through untouched.
fn chat_completions_url(base_url: &str) -> Result<Url, ModelError> {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(ModelError::Internal("base_url is empty".into()));
    }
    let full = if trimmed.ends_with("/chat/completions") {
        trimmed.to_string()
    } else if trimmed.ends_with("/v1") {
        format!("{trimmed}/chat/completions")
    } else {
        format!("{trimmed}/v1/chat/completions")
    };
    Url::parse(&full)
        .map_err(|e| ModelError::Internal(format!("invalid base_url '{base_url}': {e}")))
}

/// One HTTP attempt, with the response classified into [`ModelError`].
async fn send_once(
    inner: &Inner,
    req: &ChatCompletionsRequest,
) -> Result<reqwest::Response, ModelError> {
    let mut builder = inner
        .http
        .post(inner.endpoint.clone())
        .header("content-type", "application/json")
        .bearer_auth(&inner.api_key);
    for (k, v) in &inner.extra_headers {
        builder = builder.header(k, v);
    }

    let resp = builder
        .json(req)
        .send()
        .await
        .map_err(|e| ModelError::Network(e.to_string()))?;

    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let code = status.as_u16();
    // Truncate: an error body can be an entire HTML error page.
    let body = resp.text().await.unwrap_or_default();
    let body = if body.len() > 4096 {
        body[..4096].to_string()
    } else {
        body
    };
    Err(match code {
        401 | 403 => ModelError::Auth(body),
        429 => ModelError::RateLimited,
        503 | 529 => ModelError::Overloaded,
        _ => ModelError::Api {
            status: code,
            message: body,
        },
    })
}

/// Translate a [`ModelError`] into the backoff policy's vocabulary.
///
/// Chat Completions carries no `retry-after` the way the Messages API does, so
/// this side never supplies a server hint — the policy that reads one is the
/// same policy either way.
fn failure_kind(e: &ModelError) -> RequestFailure {
    match e {
        ModelError::Network(_) => RequestFailure::Transport,
        ModelError::RateLimited => RequestFailure::RateLimited,
        ModelError::Overloaded => RequestFailure::Overloaded,
        _ => RequestFailure::Other,
    }
}

async fn send_with_retry(
    inner: &Inner,
    req: &ChatCompletionsRequest,
) -> Result<reqwest::Response, ModelError> {
    let mut attempt = 0u32;
    loop {
        // A server that accepts the connection and then never responds would
        // otherwise hang here indefinitely — the connect timeout does not
        // cover it. Same bound as the mid-stream idle timeout.
        let result = match tokio::time::timeout(STREAM_IDLE_TIMEOUT, send_once(inner, req)).await {
            Ok(r) => r,
            Err(_elapsed) => Err(ModelError::Network(
                "no response headers within the idle window".into(),
            )),
        };
        let e = match result {
            Ok(resp) => return Ok(resp),
            Err(e) => e,
        };
        let verdict = inner.backoff.after_failure(&FailedAttempt {
            index: attempt,
            failure: failure_kind(&e),
            server_hint: None,
        });
        match verdict {
            Backoff::GiveUp => return Err(e),
            Backoff::WaitThenRetry(delay) => {
                tracing::warn!(
                    attempt = attempt + 1,
                    delay_ms = delay.as_millis() as u64,
                    error = %e,
                    "openai-compatible request retryable; backing off"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

#[async_trait]
impl Model for OpenAICompatibleModel {
    fn api_type(&self) -> ApiType {
        ApiType::OpenAICompatible
    }

    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        params: StreamParams,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        // Ignored, with reasons:
        // - `thinking_mode`: Chat Completions has no thinking configuration.
        //   Providers that reason (o-series, DeepSeek) decide for themselves,
        //   and their output is picked up as `reasoning_content` in `stream`.
        // - `cache_edits`: server-side cache surgery is Anthropic-only; there
        //   is no equivalent, and the compaction that produced the ids has
        //   already removed the content locally.
        // - `fallback_model`: model fallback is the caller's policy, and the
        //   Anthropic adapter does not act on it either.
        // - `_cancel`: dropping the returned stream ends the request, which is
        //   what the engine does on cancel (same as the Anthropic path).
        let model = if params.model.is_empty() {
            self.inner.default_model.clone()
        } else {
            params.model
        };

        let req = request::build_request(
            model,
            prompt_blocks,
            tools,
            messages,
            Some(params.max_tokens),
        );

        let inner = self.inner.clone();
        let s = async_stream::stream! {
            let resp = match send_with_retry(&inner, &req).await {
                Ok(r) => r,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };
            let bytes = resp
                .bytes_stream()
                .map_err(|e| ModelError::Network(e.to_string()));

            let mut events = Box::pin(stream::events_from_bytes(bytes, STREAM_IDLE_TIMEOUT));
            while let Some(event) = futures::StreamExt::next(&mut events).await {
                yield event;
            }
        };

        // `ModelStream` requires `Unpin`; `async_stream`'s generator is not,
        // so it is pinned first and the pin is what gets boxed.
        Ok(Box::new(Box::pin(s)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All four spellings of a base URL that appear in real config must land
    /// on the same endpoint. Getting this wrong is a 404 on first use, long
    /// after startup validation has passed.
    #[test]
    fn base_url_variants_all_resolve_to_chat_completions() {
        for base in [
            "https://api.openai.com",
            "https://api.openai.com/",
            "https://api.openai.com/v1",
            "https://api.openai.com/v1/",
        ] {
            assert_eq!(
                chat_completions_url(base).unwrap().as_str(),
                "https://api.openai.com/v1/chat/completions",
                "base = {base}"
            );
        }
    }

    /// A gateway mounted under a path prefix keeps it — the `/v1` is appended
    /// to the configured path, not to the host.
    #[test]
    fn path_prefixed_gateways_keep_their_prefix() {
        assert_eq!(
            chat_completions_url("https://gw.internal/llm/openai")
                .unwrap()
                .as_str(),
            "https://gw.internal/llm/openai/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("http://127.0.0.1:11434/v1")
                .unwrap()
                .as_str(),
            "http://127.0.0.1:11434/v1/chat/completions"
        );
    }

    /// Someone who configures the full endpoint gets what they asked for
    /// rather than a doubled path.
    #[test]
    fn fully_qualified_endpoint_is_passed_through() {
        assert_eq!(
            chat_completions_url("https://gw.internal/v1/chat/completions")
                .unwrap()
                .as_str(),
            "https://gw.internal/v1/chat/completions"
        );
    }

    #[test]
    fn empty_or_invalid_base_url_is_rejected_at_construction() {
        assert!(chat_completions_url("").is_err());
        assert!(chat_completions_url("   ").is_err());
        assert!(chat_completions_url("not a url").is_err());
        assert!(OpenAICompatibleModel::new("", "k", "m").is_err());
    }

    /// The whole point of the module: `daemon::model_router` can route on
    /// `api_type()` and hand this back as an `Arc<dyn Model>`.
    #[test]
    fn declares_openai_compatible_and_boxes_into_a_trait_object() {
        let m = OpenAICompatibleModel::new("https://api.openai.com/v1", "sk-test", "gpt-4o")
            .expect("valid config");
        assert_eq!(m.api_type(), ApiType::OpenAICompatible);
        assert_eq!(
            m.endpoint().as_str(),
            "https://api.openai.com/v1/chat/completions"
        );
        let _: Arc<dyn Model> = Arc::new(m);
    }

    /// Extra headers survive the builder chain (the `Arc::make_mut` path).
    #[test]
    fn extra_headers_are_retained() {
        let m = OpenAICompatibleModel::new("https://x/v1", "k", "m")
            .unwrap()
            .with_headers(vec![("OpenAI-Organization".into(), "org_1".into())]);
        assert_eq!(
            m.inner.extra_headers,
            vec![("OpenAI-Organization".to_string(), "org_1".to_string())]
        );
    }

    /// Which failures are worth another attempt, in the vocabulary the shared
    /// policy reads. An API-level 400 is the caller's problem and replaying it
    /// just burns time; `Other` is what the default policy refuses outright.
    #[test]
    fn only_pre_stream_failures_classify_as_retryable_kinds() {
        assert_eq!(
            failure_kind(&ModelError::RateLimited),
            RequestFailure::RateLimited
        );
        assert_eq!(
            failure_kind(&ModelError::Overloaded),
            RequestFailure::Overloaded
        );
        assert_eq!(
            failure_kind(&ModelError::Network("reset".into())),
            RequestFailure::Transport
        );
        for fatal in [
            ModelError::Auth("bad key".into()),
            ModelError::Api {
                status: 400,
                message: "bad request".into(),
            },
            ModelError::Internal("boom".into()),
        ] {
            assert_eq!(failure_kind(&fatal), RequestFailure::Other, "{fatal:?}");
        }
    }
}
