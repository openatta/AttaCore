//! `Network` — outbound requests, and whether they may leave.
//!
//! Every client in the engine builds its requests through this. What a host
//! can *replace* today is narrower: the provider a tool uses arrives through
//! `ToolContext.exec`, while the model, OAuth, telemetry and registry clients
//! each construct their own. Their traffic is `Origin::Operator`, which no
//! policy restricts, so the behavior is right — but auditing or going offline
//! as a whole needs an injection point that does not exist yet.

use std::collections::BTreeMap;

use super::ExecError;

/// Who chose this destination.
///
/// The distinction the engine's network policy has always needed and never
/// had. `allowed_domains` answers "where may the model reach", not "what may
/// this process connect to" — applying it to everything would cut the agent
/// off from its own model endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The model picked the destination: a `WebFetch` url, a `Ping` host,
    /// `curl` inside a Bash command. Subject to `allowed_domains`.
    Agent,
    /// An operator configured the endpoint: the model API, an MCP server,
    /// telemetry, the plugin marketplace, an OAuth provider. Not subject to
    /// `allowed_domains`, but still routed here so a deployment can audit,
    /// rate-limit or go offline as a whole.
    Operator,
}

/// How many redirects a provider follows when the caller does not say.
///
/// Ten, the figure `reqwest` picks by default, so a caller with no opinion on
/// redirects gets the behavior it would have had without this contract.
pub const DEFAULT_MAX_REDIRECTS: u8 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Option<Vec<u8>>,
    /// Largest response body this caller will accept, in bytes. `None` puts
    /// no ceiling on it.
    pub max_bytes: Option<u64>,
    /// How many redirects the provider may follow on this caller's behalf.
    ///
    /// Zero hands the 3xx back instead, which is what a caller wants when the
    /// redirect target is itself a decision — `WebFetch` returns it to the
    /// model rather than chasing it.
    pub max_redirects: u8,
}

impl HttpRequest {
    pub fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            url: url.into(),
            headers: BTreeMap::new(),
            body: None,
            max_bytes: None,
            max_redirects: DEFAULT_MAX_REDIRECTS,
        }
    }

    pub fn get(url: impl Into<String>) -> Self {
        Self::new("GET", url)
    }

    pub fn post(url: impl Into<String>) -> Self {
        Self::new("POST", url)
    }

    pub fn header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.headers.insert(k.into(), v.into());
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn max_bytes(mut self, n: u64) -> Self {
        self.max_bytes = Some(n);
        self
    }

    pub fn max_redirects(mut self, n: u8) -> Self {
        self.max_redirects = n;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A response whose body has not been read yet.
///
/// The model API is a server-sent-event stream that stays open for the length
/// of an answer, so an egress that could only hand back a finished body could
/// not carry the one request the engine makes most. Status and headers arrive
/// first because that is what the retry ladder classifies on, before a single
/// body byte exists.
pub struct HttpStream {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: futures::stream::BoxStream<'static, Result<Vec<u8>, ExecError>>,
}

/// Every outbound request, in one place.
///
/// A provider follows redirects up to [`HttpRequest::max_redirects`] and must
/// re-apply its policy to each hop: a destination the model was allowed to
/// name could otherwise redirect to one it was not, and the allowlist would
/// hold only for the first request of a chain.
#[async_trait::async_trait]
pub trait Network: Send + Sync {
    /// Send, and read the whole body.
    async fn send(&self, req: HttpRequest, origin: Origin) -> Result<HttpResponse, ExecError>;

    /// Send, and hand back the body as it arrives.
    ///
    /// The default reads it all first, which is right for a provider that
    /// answers from a fixture and wrong for one talking to a live model
    /// endpoint.
    async fn open(&self, req: HttpRequest, origin: Origin) -> Result<HttpStream, ExecError> {
        let r = self.send(req, origin).await?;
        Ok(HttpStream {
            status: r.status,
            headers: r.headers,
            body: Box::pin(futures::stream::once(async move { Ok(r.body) })),
        })
    }

    /// Whether a host would be allowed, without sending anything.
    ///
    /// Exists for the callers that have to decide before there is a request:
    /// `Ping`, and building a sandbox profile that names the hosts a
    /// subprocess may reach.
    fn permits(&self, host: &str, origin: Origin) -> bool;
}
