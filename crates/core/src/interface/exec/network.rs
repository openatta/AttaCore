//! `Network` — every request that leaves the process.

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Option<Vec<u8>>,
}

impl HttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: "GET".into(),
            url: url.into(),
            headers: BTreeMap::new(),
            body: None,
        }
    }

    pub fn header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.headers.insert(k.into(), v.into());
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

/// Every outbound request, in one place.
#[async_trait::async_trait]
pub trait Network: Send + Sync {
    async fn send(&self, req: HttpRequest, origin: Origin) -> Result<HttpResponse, ExecError>;

    /// Whether a host would be allowed, without sending anything.
    ///
    /// Exists for the callers that have to decide before there is a request:
    /// `Ping`, and building a sandbox profile that names the hosts a
    /// subprocess may reach.
    fn permits(&self, host: &str, origin: Origin) -> bool;
}
