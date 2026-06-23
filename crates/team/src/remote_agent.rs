//! Remote sub-agent transport trait and implementations.
//!
//! Provides:
//! - [`RemoteAgentTransport`] trait — the stable API for spawning remote sub-agents.
//! - [`NoopRemoteTransport`] — default placeholder that always returns
//!   `NotConfigured`.
//! - [`HttpRemoteTransport`] — HTTP/JSON-RPC + SSE backend for communicating with
//!   a remote-agent server (e.g. a ClawPod Bridge or standalone attacode receiver).
//! - [`MockHttpRemoteTransport`] — in-memory mock for testing higher-level code.
//!
//! ## Architecture
//!
//! `spawn()` sends a JSON-RPC request to `{endpoint}/rpc` (method `spawn_agent`)
//! and receives an `agent_id` back.  It then opens an SSE stream to
//! `{endpoint}/events?agent_id=...` and returns the event stream to the caller.
//! The caller drains the stream until a `Final` or `Error` event arrives.
//!
//! Auxiliary methods `cancel()`, `status()`, and `health_check()` are ad-hoc
//! calls on `HttpRemoteTransport` — they are not part of the trait.
//!
//! ## Interface stability
//!
//! [`RemoteAgentTransport`], [`RemoteAgentRequest`], [`RemoteAgentEvent`],
//! [`RemoteAgentError`] are **public stable API**.  [`RemoteSpawnRequest`],
//! [`RemoteSpawnResponse`], [`RemoteAgentStatus`], [`HttpRemoteTransport`],
//! [`MockHttpRemoteTransport`] are **public but evolving**.

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ── Public types (stable API) ───────────────────────────────────────────

/// Request to spawn a remote sub-agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAgentRequest {
    /// The system + user prompt to send to the sub-agent.
    pub prompt: String,
    /// Tools the sub-agent is allowed to invoke (empty = none).
    pub allowed_tools: Vec<String>,
    /// Optional worktree slug for the remote side to create (phase-1 style).
    pub worktree_slug: Option<String>,
}

/// Event emitted by a remote sub-agent during its turn.
///
/// Serialized as a tagged JSON object with `"type"` and `"data"` fields for
/// wire transport over SSE.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data")]
pub enum RemoteAgentEvent {
    /// Streamed assistant text delta.
    #[serde(rename = "text_delta")]
    TextDelta(String),
    /// The sub-agent invoked a tool.
    #[serde(rename = "tool_use")]
    ToolUse {
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Result of a tool invocation.
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        text: String,
        is_error: bool,
    },
    /// Turn completed — carries the final aggregated output.
    #[serde(rename = "final")]
    Final {
        stop_reason: String,
        output_text: String,
    },
    /// Business error from the remote engine (not a transport error).
    #[serde(rename = "error")]
    Error(String),
}

/// Pinned, boxed, `Send` stream of remote agent events.
pub type RemoteAgentStream =
    Pin<Box<dyn Stream<Item = Result<RemoteAgentEvent, RemoteAgentError>> + Send>>;

/// Errors originating from the remote-agent layer.
///
/// Each transport-level variant carries the endpoint URL so callers can
/// identify which server produced the error without extra logging.
#[derive(thiserror::Error, Debug, Clone, PartialEq)]
pub enum RemoteAgentError {
    /// No real transport injected (default `NoopRemoteTransport`).
    #[error("remote agent transport not configured (default Noop refuses; inject one to enable)")]
    NotConfigured,

    /// TCP connection refused — server is down or unreachable.
    #[error("connection refused at {endpoint}: {detail}")]
    ConnectionRefused {
        /// The remote endpoint URL.
        endpoint: String,
        /// Underlying error detail.
        detail: String,
    },

    /// Request timed out.
    #[error("request timed out at {endpoint}: {detail}")]
    Timeout {
        /// The remote endpoint URL.
        endpoint: String,
        /// Underlying error detail.
        detail: String,
    },

    /// Other transport-layer failure: connection drop, invalid response, etc.
    #[error("transport at {endpoint}: {detail}")]
    Transport {
        /// The remote endpoint URL.
        endpoint: String,
        /// Error detail.
        detail: String,
    },

    /// Remote side rejected the request (auth, quota, refused prompt).
    #[error("remote rejected at {endpoint}: {detail}")]
    Rejected {
        /// The remote endpoint URL.
        endpoint: String,
        /// Error detail.
        detail: String,
    },

    /// Schema / version incompatibility with the remote side.
    #[error("incompatible remote: {detail}")]
    Incompatible {
        /// Error detail.
        detail: String,
    },
}

// ── Error construction helpers ──────────────────────────────────────────

impl RemoteAgentError {
    /// Build a `ConnectionRefused` error.
    pub fn connection_refused(endpoint: &str, detail: impl std::fmt::Display) -> Self {
        Self::ConnectionRefused {
            endpoint: endpoint.to_owned(),
            detail: detail.to_string(),
        }
    }

    /// Build a `Timeout` error.
    pub fn timeout(endpoint: &str, detail: impl std::fmt::Display) -> Self {
        Self::Timeout {
            endpoint: endpoint.to_owned(),
            detail: detail.to_string(),
        }
    }

    /// Build a generic `Transport` error.
    pub fn transport(endpoint: &str, detail: impl std::fmt::Display) -> Self {
        Self::Transport {
            endpoint: endpoint.to_owned(),
            detail: detail.to_string(),
        }
    }

    /// Build a `Rejected` error.
    pub fn rejected(endpoint: &str, detail: impl std::fmt::Display) -> Self {
        Self::Rejected {
            endpoint: endpoint.to_owned(),
            detail: detail.to_string(),
        }
    }

    /// Build an `Incompatible` error.
    pub fn incompatible(detail: impl std::fmt::Display) -> Self {
        Self::Incompatible {
            detail: detail.to_string(),
        }
    }
}

// ── Trait (stable API) ──────────────────────────────────────────────────

/// Transport abstraction for spawning a remote sub-agent.
///
/// The sole method `spawn()` returns an event stream that the caller must
/// drain until a `Final` or `Error` event.  Dropping the stream signals
/// cancellation to the remote side.
#[async_trait]
pub trait RemoteAgentTransport: Send + Sync {
    /// Spawn a remote sub-agent and return its event stream.
    async fn spawn(&self, req: RemoteAgentRequest) -> Result<RemoteAgentStream, RemoteAgentError>;
}

// ── Noop placeholder ────────────────────────────────────────────────────

/// Placeholder implementation that always returns `NotConfigured`.
///
/// The default injected by attacode-cli when no real transport is configured.
pub struct NoopRemoteTransport;

#[async_trait]
impl RemoteAgentTransport for NoopRemoteTransport {
    async fn spawn(&self, _: RemoteAgentRequest) -> Result<RemoteAgentStream, RemoteAgentError> {
        Err(RemoteAgentError::NotConfigured)
    }
}

// ── JSON-RPC types (private) ────────────────────────────────────────────

/// A JSON-RPC 2.0 request body.
#[derive(Debug, Clone, Serialize)]
struct JsonRpcRequest<T: Serialize> {
    jsonrpc: &'static str,
    method: String,
    params: T,
    id: u64,
}

/// A JSON-RPC error object.
#[derive(Debug, Deserialize)]
struct JsonRpcError {
    #[allow(dead_code)]
    code: i64,
    message: String,
}

/// A JSON-RPC 2.0 response body.
#[derive(Debug, Deserialize)]
struct JsonRpcResponse<T> {
    result: Option<T>,
    error: Option<JsonRpcError>,
}

// ── RPC payload types (public, evolving) ────────────────────────────────

/// Parameters sent in the `spawn_agent` JSON-RPC call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSpawnRequest {
    pub prompt: String,
    pub allowed_tools: Vec<String>,
    pub worktree_slug: Option<String>,
}

/// Response from a successful `spawn_agent` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSpawnResponse {
    pub agent_id: String,
}

/// Status snapshot of a previously spawned remote agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteAgentStatus {
    pub agent_id: String,
    /// One of `"running"`, `"completed"`, `"failed"`, `"cancelled"`.
    pub state: String,
    /// Session identifier if the agent is still running.
    pub session_id: Option<String>,
    /// Final output text (present when `state` is `"completed"`).
    pub output_text: Option<String>,
    /// Error message (present when `state` is `"failed"`).
    pub error: Option<String>,
}

// ── HTTP transport ──────────────────────────────────────────────────────

/// HTTP/JSON-RPC + SSE transport for remote agents.
///
/// Communicates with a remote agent server using:
/// - **RPC** — `POST {endpoint}/rpc` with a JSON-RPC 2.0 body.
/// - **Events** — `GET {endpoint}/events?agent_id=<id>` returning an SSE stream.
///
/// ## Retry
///
/// All RPC calls (spawn, cancel, status) retry up to 3 times with
/// exponential backoff (1s → 2s → 4s).  SSE connections are not retried;
/// a failed connection is surfaced as an error in the stream.
///
/// ## Health check
///
/// Call [`health_check`](Self::health_check) before `spawn()` to fail fast
/// when the server is unreachable.
///
/// ## Constructor
///
/// ```ignore
/// let transport = HttpRemoteTransport::new(
///     "https://remote-agent.example.com".into(),
///     "ltt_abc123...".into(),
/// );
/// ```
pub struct HttpRemoteTransport {
    endpoint: String,
    auth_token: String,
    client: reqwest::Client,
    /// Backoff delays between retry attempts.
    retry_delays: Vec<Duration>,
}

impl HttpRemoteTransport {
    /// Create a new transport pointing at a remote-agent server.
    ///
    /// `endpoint` is the base URL (e.g. `https://remote-agent.example.com`).
    /// `auth_token` is the bearer token used in the `Authorization` header.
    ///
    /// Retries use the default backoff: 1s, 2s, 4s (3 retries, 4 total
    /// attempts).  Use [`with_retry_delays`](Self::with_retry_delays) to
    /// customize.
    pub fn new(endpoint: String, auth_token: String) -> Self {
        Self::with_retry_delays(
            endpoint,
            auth_token,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
            ],
        )
    }

    /// Create a transport with custom retry delay durations.
    ///
    /// Each element in `delays` is the sleep time inserted _before_ the
    /// corresponding retry attempt.  The first attempt has no delay.
    /// An empty vector means no retries.
    pub fn with_retry_delays(
        endpoint: String,
        auth_token: String,
        retry_delays: Vec<Duration>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest Client::builder() with default options should never fail");
        Self {
            endpoint,
            auth_token,
            client,
            retry_delays,
        }
    }

    /// Cancel a running remote agent by its agent ID.
    ///
    /// Returns `Ok(())` if the remote acknowledges the cancellation.
    pub async fn cancel(&self, agent_id: &str) -> Result<(), RemoteAgentError> {
        let _: serde_json::Value = self
            .rpc("cancel_agent", serde_json::json!({ "agent_id": agent_id }))
            .await?;
        Ok(())
    }

    /// Query the status of a remote agent by its agent ID.
    pub async fn status(&self, agent_id: &str) -> Result<RemoteAgentStatus, RemoteAgentError> {
        self.rpc("agent_status", serde_json::json!({ "agent_id": agent_id }))
            .await
    }

    /// Check whether the remote endpoint is reachable and healthy.
    ///
    /// Sends a GET `{endpoint}/health`. Returns `Ok(true)` on a 2xx status,
    /// `Ok(false)` on any other response (the server may be reachable but
    /// lack a `/health` handler), and `Err` on transport failures (connection
    /// refused, timeout, etc.).
    ///
    /// Call this before `spawn()` to fail fast with a clear diagnostic when
    /// the server is down.
    pub async fn health_check(&self) -> Result<bool, RemoteAgentError> {
        let url = format!("{}/health", self.endpoint);
        match self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.auth_token))
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) => Ok(resp.status().is_success()),
            Err(e) => {
                if e.is_connect() {
                    Err(RemoteAgentError::connection_refused(
                        &self.endpoint,
                        format_args!("health check: {e}"),
                    ))
                } else if e.is_timeout() {
                    Err(RemoteAgentError::timeout(
                        &self.endpoint,
                        format_args!("health check: {e}"),
                    ))
                } else {
                    Err(RemoteAgentError::transport(
                        &self.endpoint,
                        format_args!("health check failed: {e}"),
                    ))
                }
            }
        }
    }

    /// Open an SSE event stream for an already-running agent.
    ///
    /// The returned stream yields [`RemoteAgentEvent`] items until the
    /// agent completes or the connection drops.
    pub async fn events(&self, agent_id: &str) -> Result<RemoteAgentStream, RemoteAgentError> {
        let url = format!("{}/events", self.endpoint);
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.auth_token))
            .query(&[("agent_id", agent_id)])
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    RemoteAgentError::connection_refused(
                        &self.endpoint,
                        format_args!("SSE connect: {e}"),
                    )
                } else if e.is_timeout() {
                    RemoteAgentError::timeout(
                        &self.endpoint,
                        format_args!("SSE connect: {e}"),
                    )
                } else {
                    RemoteAgentError::transport(
                        &self.endpoint,
                        format_args!("SSE connect: {e}"),
                    )
                }
            })?;

        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(RemoteAgentError::rejected(
                &self.endpoint,
                format_args!(
                    "SSE authentication failed (HTTP {status}). Check your auth token.",
                ),
            ));
        }
        if !status.is_success() {
            return Err(RemoteAgentError::rejected(
                &self.endpoint,
                format_args!("SSE HTTP {status} from remote agent"),
            ));
        }

        let byte_stream = response.bytes_stream();
        let endpoint = self.endpoint.clone();
        let event_stream =
            eventsource_stream::EventStream::new(byte_stream).map(move |event| match event {
                Ok(sse) => {
                    serde_json::from_str::<RemoteAgentEvent>(&sse.data)
                        .map(Ok)
                        .unwrap_or_else(|e| {
                            let preview: String = sse.data.chars().take(200).collect();
                            Err(RemoteAgentError::transport(
                                &endpoint,
                                format_args!("SSE parse error: {e} (raw preview: {preview})"),
                            ))
                        })
                }
                Err(e) => Err(RemoteAgentError::transport(
                    &endpoint,
                    format_args!("SSE stream error: {e}"),
                )),
            });

        Ok(Box::pin(event_stream))
    }

    // ── Private helpers ─────────────────────────────────────────────

    /// Issue a JSON-RPC request with exponential backoff retry.
    ///
    /// Retries up to `self.retry_delays.len()` times using the configured
    /// delays between attempts. On transport-level failures (timeout,
    /// connection refused, HTTP 5xx) all attempts are exhausted before
    /// returning the last error.
    async fn rpc<P, R>(&self, method: &str, params: P) -> Result<R, RemoteAgentError>
    where
        P: Serialize,
        R: serde::de::DeserializeOwned,
    {
        let url = format!("{}/rpc", self.endpoint);
        let body = JsonRpcRequest {
            jsonrpc: "2.0",
            method: method.to_string(),
            params,
            id: 1,
        };

        let mut last_err = None;

        for attempt in 0..=self.retry_delays.len() {
            if attempt > 0 {
                tokio::time::sleep(self.retry_delays[attempt - 1]).await;
            }

            match self.send_rpc(&url, &body).await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    tracing::warn!(
                        method,
                        attempt,
                        endpoint = %self.endpoint,
                        error = %e,
                        "remote-agent RPC attempt failed, will retry",
                    );
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or(RemoteAgentError::transport(
            &self.endpoint,
            "all RPC retries exhausted",
        )))
    }

    /// Single-shot JSON-RPC call (no retry).
    async fn send_rpc<P, R>(
        &self,
        url: &str,
        body: &JsonRpcRequest<P>,
    ) -> Result<R, RemoteAgentError>
    where
        P: Serialize,
        R: serde::de::DeserializeOwned,
    {
        let response = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.auth_token))
            .json(body)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    RemoteAgentError::connection_refused(
                        &self.endpoint,
                        format_args!("{e}"),
                    )
                } else if e.is_timeout() {
                    RemoteAgentError::timeout(
                        &self.endpoint,
                        format_args!("{e}"),
                    )
                } else {
                    RemoteAgentError::transport(
                        &self.endpoint,
                        format_args!("HTTP request: {e}"),
                    )
                }
            })?;

        // Distinguish auth failures (401/403) from other non-success codes.
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(RemoteAgentError::rejected(
                &self.endpoint,
                format_args!(
                    "authentication failed (HTTP {status}). Check your auth token.",
                ),
            ));
        }
        if status.is_server_error() {
            return Err(RemoteAgentError::transport(
                &self.endpoint,
                format_args!("server error HTTP {status}"),
            ));
        }
        if !status.is_success() {
            return Err(RemoteAgentError::rejected(
                &self.endpoint,
                format_args!("unexpected HTTP {status}"),
            ));
        }

        let rpc: JsonRpcResponse<R> = response
            .json()
            .await
            .map_err(|e| {
                RemoteAgentError::transport(
                    &self.endpoint,
                    format_args!("parse RPC response: {e}"),
                )
            })?;

        if let Some(err) = rpc.error {
            return Err(RemoteAgentError::rejected(
                &self.endpoint,
                format_args!("rpc error ({}): {}", err.code, err.message),
            ));
        }

        rpc.result.ok_or_else(|| {
            RemoteAgentError::transport(
                &self.endpoint,
                "RPC response missing both result and error",
            )
        })
    }
}

#[async_trait]
impl RemoteAgentTransport for HttpRemoteTransport {
    /// Spawn a remote agent via JSON-RPC, then return its SSE event stream.
    async fn spawn(&self, req: RemoteAgentRequest) -> Result<RemoteAgentStream, RemoteAgentError> {
        let spawn_params = RemoteSpawnRequest {
            prompt: req.prompt,
            allowed_tools: req.allowed_tools,
            worktree_slug: req.worktree_slug,
        };

        let response: RemoteSpawnResponse = self.rpc("spawn_agent", spawn_params).await?;
        let agent_id = response.agent_id;

        tracing::debug!(agent_id, endpoint = %self.endpoint, "remote agent spawned, connecting SSE stream");

        self.events(&agent_id).await
    }
}

// ── Mock transport ──────────────────────────────────────────────────────

/// A mock [`RemoteAgentTransport`] for testing.
///
/// Returns pre-configured canned responses without making any real HTTP
/// calls.  Implements [`RemoteAgentTransport`] so it can be injected into
/// higher-level code (Coordinator, AgentTool) during tests.
///
/// # Builder API
///
/// ```ignore
/// let transport = MockHttpRemoteTransport::new()
///     .with_spawn_ok("agent-001")
///     .with_events(vec![
///         Ok(RemoteAgentEvent::TextDelta("hello ".into())),
///         Ok(RemoteAgentEvent::Final { stop_reason: "done".into(), output_text: "hello world".into() }),
///     ]);
/// ```
#[derive(Debug)]
pub struct MockHttpRemoteTransport {
    spawn_result: Result<RemoteSpawnResponse, RemoteAgentError>,
    cancel_result: Result<(), RemoteAgentError>,
    status_result: Result<RemoteAgentStatus, RemoteAgentError>,
    events: Arc<Vec<Result<RemoteAgentEvent, RemoteAgentError>>>,
    spawn_call_count: Arc<AtomicUsize>,
    cancel_call_count: Arc<AtomicUsize>,
    status_call_count: Arc<AtomicUsize>,
}

impl MockHttpRemoteTransport {
    /// Create a new mock with default successful responses.
    ///
    /// The default `spawn` succeeds with agent ID `"mock-agent-001"` and
    /// returns an empty event stream.  `cancel` succeeds.  `status` returns
    /// a `completed` state.
    pub fn new() -> Self {
        Self {
            spawn_result: Ok(RemoteSpawnResponse {
                agent_id: "mock-agent-001".into(),
            }),
            cancel_result: Ok(()),
            status_result: Ok(RemoteAgentStatus {
                agent_id: "mock-agent-001".into(),
                state: "completed".into(),
                session_id: None,
                output_text: Some("done".into()),
                error: None,
            }),
            events: Arc::new(Vec::new()),
            spawn_call_count: Arc::new(AtomicUsize::new(0)),
            cancel_call_count: Arc::new(AtomicUsize::new(0)),
            status_call_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    // ── Spawn configuration ─────────────────────────────────────

    /// Set the spawn response to the given result.
    pub fn with_spawn_result(mut self, result: Result<RemoteSpawnResponse, RemoteAgentError>) -> Self {
        self.spawn_result = result;
        self
    }

    /// Set spawn to succeed with the given agent ID.
    pub fn with_spawn_ok(self, agent_id: &str) -> Self {
        self.with_spawn_result(Ok(RemoteSpawnResponse {
            agent_id: agent_id.into(),
        }))
    }

    /// Set spawn to fail with the given error.
    pub fn with_spawn_err(self, error: RemoteAgentError) -> Self {
        self.with_spawn_result(Err(error))
    }

    // ── Cancel configuration ────────────────────────────────────

    /// Set the cancel response to the given result.
    pub fn with_cancel_result(mut self, result: Result<(), RemoteAgentError>) -> Self {
        self.cancel_result = result;
        self
    }

    /// Set cancel to succeed.
    pub fn with_cancel_ok(self) -> Self {
        self.with_cancel_result(Ok(()))
    }

    /// Set cancel to fail with the given error.
    pub fn with_cancel_err(self, error: RemoteAgentError) -> Self {
        self.with_cancel_result(Err(error))
    }

    // ── Status configuration ─────────────────────────────────────

    /// Set the status response to the given result.
    pub fn with_status_result(
        mut self,
        result: Result<RemoteAgentStatus, RemoteAgentError>,
    ) -> Self {
        self.status_result = result;
        self
    }

    /// Set status to return a running state.
    pub fn with_status_running(self, agent_id: &str) -> Self {
        self.with_status_result(Ok(RemoteAgentStatus {
            agent_id: agent_id.into(),
            state: "running".into(),
            session_id: Some("session-001".into()),
            output_text: None,
            error: None,
        }))
    }

    /// Set status to return a completed state.
    pub fn with_status_completed(self, agent_id: &str, output: &str) -> Self {
        self.with_status_result(Ok(RemoteAgentStatus {
            agent_id: agent_id.into(),
            state: "completed".into(),
            session_id: None,
            output_text: Some(output.into()),
            error: None,
        }))
    }

    /// Set status to fail with the given error.
    pub fn with_status_err(self, error: RemoteAgentError) -> Self {
        self.with_status_result(Err(error))
    }

    // ── Events configuration ─────────────────────────────────────

    /// Set the events returned by `spawn`'s event stream.
    pub fn with_events(
        mut self,
        events: Vec<Result<RemoteAgentEvent, RemoteAgentError>>,
    ) -> Self {
        self.events = Arc::new(events);
        self
    }

    // ── Inspection helpers ───────────────────────────────────────

    /// Number of times `spawn()` was called.
    pub fn spawn_count(&self) -> usize {
        self.spawn_call_count.load(Ordering::Relaxed)
    }

    /// Number of times `cancel()` was called.
    pub fn cancel_count(&self) -> usize {
        self.cancel_call_count.load(Ordering::Relaxed)
    }

    /// Number of times `status()` was called.
    pub fn status_count(&self) -> usize {
        self.status_call_count.load(Ordering::Relaxed)
    }
}

impl Default for MockHttpRemoteTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RemoteAgentTransport for MockHttpRemoteTransport {
    async fn spawn(&self, _req: RemoteAgentRequest) -> Result<RemoteAgentStream, RemoteAgentError> {
        self.spawn_call_count.fetch_add(1, Ordering::Relaxed);

        match &self.spawn_result {
            Ok(_response) => {
                let events: Vec<_> = self.events.iter().cloned().collect();
                let stream = futures::stream::iter(events);
                Ok(Box::pin(stream))
            }
            Err(e) => Err(e.clone()),
        }
    }
}

impl MockHttpRemoteTransport {
    /// Mock cancel — does not perform any HTTP call.
    pub async fn cancel_agent(&self, _agent_id: &str) -> Result<(), RemoteAgentError> {
        self.cancel_call_count.fetch_add(1, Ordering::Relaxed);
        self.cancel_result.clone()
    }

    /// Mock status — does not perform any HTTP call.
    pub async fn status_agent(
        &self,
        _agent_id: &str,
    ) -> Result<RemoteAgentStatus, RemoteAgentError> {
        self.status_call_count.fetch_add(1, Ordering::Relaxed);
        self.status_result.clone()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::future::Future;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::watch;

    // ── Noop transport tests ──────────────────────────────────────────

    #[tokio::test]
    async fn noop_transport_refuses_spawn() {
        let t = NoopRemoteTransport;
        let result = t
            .spawn(RemoteAgentRequest {
                prompt: "anything".into(),
                allowed_tools: Vec::new(),
                worktree_slug: None,
            })
            .await;
        assert!(matches!(result, Err(RemoteAgentError::NotConfigured)));
    }

    #[tokio::test]
    async fn noop_error_message_is_actionable() {
        let t = NoopRemoteTransport;
        let result = t
            .spawn(RemoteAgentRequest {
                prompt: "x".into(),
                allowed_tools: vec!["FileRead".into()],
                worktree_slug: Some("probe".into()),
            })
            .await;

        match result {
            Err(e) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("not configured"),
                    "error should mention 'not configured', got: {msg}"
                );
            }
            Ok(_) => panic!("expected Err"),
        }
    }

    // ── RemoteAgentEvent serde round-trips ────────────────────────────

    #[tokio::test]
    async fn remote_agent_event_round_trips_serde() {
        let cases: Vec<RemoteAgentEvent> = vec![
            RemoteAgentEvent::TextDelta("hello world".into()),
            RemoteAgentEvent::ToolUse {
                tool_use_id: "call_1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "foo.txt"}),
            },
            RemoteAgentEvent::ToolResult {
                tool_use_id: "call_1".into(),
                text: "file contents".into(),
                is_error: false,
            },
            RemoteAgentEvent::Final {
                stop_reason: "end_turn".into(),
                output_text: "Done!".into(),
            },
            RemoteAgentEvent::Error("something broke".into()),
        ];

        for evt in cases {
            let json = serde_json::to_value(&evt).expect("serialize");
            let back: RemoteAgentEvent = serde_json::from_value(json).expect("deserialize");
            assert_eq!(evt, back);
        }
    }

    // ── JSON-RPC request serialization ────────────────────────────────

    #[tokio::test]
    async fn json_rpc_request_serializes_correctly() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            method: "spawn_agent".to_string(),
            params: serde_json::json!({"prompt": "hello"}),
            id: 1,
        };
        let json = serde_json::to_value(&req).expect("serialize");
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["method"], "spawn_agent");
        assert_eq!(json["id"], 1);
        assert_eq!(json["params"]["prompt"], "hello");
    }

    #[tokio::test]
    async fn json_rpc_request_allows_custom_params() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            method: "cancel_agent".to_string(),
            params: serde_json::json!({"agent_id": "abc-123"}),
            id: 42,
        };
        let json = serde_json::to_value(&req).expect("serialize");
        assert_eq!(json["method"], "cancel_agent");
        assert_eq!(json["params"]["agent_id"], "abc-123");
        assert_eq!(json["id"], 42);
    }

    // ── RemoteAgentError display / construction ──────────────────────

    #[tokio::test]
    async fn remote_agent_error_display_contains_endpoint() {
        let err = RemoteAgentError::transport("http://example.com", "something broke");
        let msg = format!("{err}");
        assert!(msg.contains("example.com"), "error should contain endpoint, got: {msg}");
        assert!(msg.contains("something broke"), "error should contain detail, got: {msg}");
    }

    #[tokio::test]
    async fn remote_agent_error_connection_refused_message() {
        let err = RemoteAgentError::connection_refused("http://down-server:8080", "connection reset");
        let msg = format!("{err}");
        assert!(msg.contains("connection refused"));
        assert!(msg.contains("down-server:8080"));
        assert!(msg.contains("connection reset"));
    }

    #[tokio::test]
    async fn remote_agent_error_timeout_message() {
        let err = RemoteAgentError::timeout("http://slow:8080", "timed out after 5s");
        let msg = format!("{err}");
        assert!(msg.contains("timed out"));
        assert!(msg.contains("slow:8080"));
    }

    #[tokio::test]
    async fn remote_agent_error_rejected_message() {
        let err = RemoteAgentError::rejected("http://secure:8080", "authentication failed");
        let msg = format!("{err}");
        assert!(msg.contains("rejected"));
        assert!(msg.contains("secure:8080"));
        assert!(msg.contains("authentication failed"));
    }

    #[tokio::test]
    async fn remote_agent_error_incompatible_message() {
        let err = RemoteAgentError::incompatible("version 2.0 required, got 1.0");
        let msg = format!("{err}");
        assert!(msg.contains("incompatible"));
        assert!(msg.contains("version 2.0"));
    }

    #[tokio::test]
    async fn remote_agent_error_partial_eq() {
        let a = RemoteAgentError::connection_refused("x", "down");
        let b = RemoteAgentError::connection_refused("x", "down");
        let c = RemoteAgentError::connection_refused("y", "down");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, RemoteAgentError::NotConfigured);
        assert_eq!(RemoteAgentError::NotConfigured, RemoteAgentError::NotConfigured);
    }

    // ── RemoteAgentEvent SSE event parsing ────────────────────────────

    #[tokio::test]
    async fn sse_parses_text_delta() {
        let raw = r#"{"type":"text_delta","data":"Hello!"}"#;
        let event: RemoteAgentEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(event, RemoteAgentEvent::TextDelta("Hello!".into()));
    }

    #[tokio::test]
    async fn sse_parses_tool_use() {
        let raw = r#"{"type":"tool_use","data":{"tool_use_id":"call_1","name":"read","input":{"path":"x.txt"}}}"#;
        let event: RemoteAgentEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(
            event,
            RemoteAgentEvent::ToolUse {
                tool_use_id: "call_1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "x.txt"}),
            }
        );
    }

    #[tokio::test]
    async fn sse_parses_tool_result() {
        let raw = r#"{"type":"tool_result","data":{"tool_use_id":"call_1","text":"ok","is_error":false}}"#;
        let event: RemoteAgentEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(
            event,
            RemoteAgentEvent::ToolResult {
                tool_use_id: "call_1".into(),
                text: "ok".into(),
                is_error: false,
            }
        );
    }

    #[tokio::test]
    async fn sse_parses_final_event() {
        let raw =
            r#"{"type":"final","data":{"stop_reason":"end_turn","output_text":"Completed!"}}"#;
        let event: RemoteAgentEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(
            event,
            RemoteAgentEvent::Final {
                stop_reason: "end_turn".into(),
                output_text: "Completed!".into(),
            }
        );
    }

    #[tokio::test]
    async fn sse_parses_error_event() {
        let raw = r#"{"type":"error","data":"something went wrong"}"#;
        let event: RemoteAgentEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(event, RemoteAgentEvent::Error("something went wrong".into()));
    }

    #[tokio::test]
    async fn sse_rejects_invalid_json() {
        let raw = r#"not-json-at-all"#;
        let result: Result<RemoteAgentEvent, _> = serde_json::from_str(raw);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn sse_rejects_unknown_event_type() {
        let raw = r#"{"type":"unknown_event","data":"garbage"}"#;
        let result: Result<RemoteAgentEvent, _> = serde_json::from_str(raw);
        assert!(result.is_err());
    }

    // ── MockHttpRemoteTransport tests ─────────────────────────────────

    #[tokio::test]
    async fn mock_spawn_returns_configured_agent_id() {
        let transport = MockHttpRemoteTransport::new().with_spawn_ok("agent-42");
        let stream = transport
            .spawn(RemoteAgentRequest {
                prompt: "test".into(),
                allowed_tools: vec![],
                worktree_slug: None,
            })
            .await
            .expect("spawn should succeed");
        // Default events are empty; the stream ends immediately.
        let results: Vec<_> = stream.collect().await;
        assert!(results.is_empty());
        assert_eq!(transport.spawn_count(), 1);
    }

    #[tokio::test]
    async fn mock_spawn_returns_configured_error() {
        let err = RemoteAgentError::connection_refused("http://mock", "server down for test");
        let transport = MockHttpRemoteTransport::new().with_spawn_err(err.clone());
        let result = transport
            .spawn(RemoteAgentRequest {
                prompt: "test".into(),
                allowed_tools: vec![],
                worktree_slug: None,
            })
            .await;
        match result {
            Err(e) => {
                assert_eq!(e, err);
            }
            Ok(_) => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn mock_spawn_returns_event_stream() {
        let transport = MockHttpRemoteTransport::new()
            .with_spawn_ok("agent-events")
            .with_events(vec![
                Ok(RemoteAgentEvent::TextDelta("Hello ".into())),
                Ok(RemoteAgentEvent::TextDelta("World".into())),
                Ok(RemoteAgentEvent::Final {
                    stop_reason: "end_turn".into(),
                    output_text: "Hello World".into(),
                }),
            ]);

        let stream = transport
            .spawn(RemoteAgentRequest {
                prompt: "write hello".into(),
                allowed_tools: vec![],
                worktree_slug: None,
            })
            .await
            .expect("spawn should succeed");

        let events: Vec<_> = stream.collect().await;
        assert_eq!(events.len(), 3);
        assert_eq!(
            events[0].as_ref().unwrap(),
            &RemoteAgentEvent::TextDelta("Hello ".into())
        );
        assert_eq!(
            events[2].as_ref().unwrap(),
            &RemoteAgentEvent::Final {
                stop_reason: "end_turn".into(),
                output_text: "Hello World".into(),
            }
        );
    }

    #[tokio::test]
    async fn mock_spawn_counts_calls() {
        let transport = MockHttpRemoteTransport::new().with_spawn_ok("counted");
        assert_eq!(transport.spawn_count(), 0);

        let _ = transport
            .spawn(RemoteAgentRequest {
                prompt: "a".into(),
                allowed_tools: vec![],
                worktree_slug: None,
            })
            .await;
        assert_eq!(transport.spawn_count(), 1);

        let _ = transport
            .spawn(RemoteAgentRequest {
                prompt: "b".into(),
                allowed_tools: vec![],
                worktree_slug: None,
            })
            .await;
        assert_eq!(transport.spawn_count(), 2);
    }

    #[tokio::test]
    async fn mock_cancel_counts_calls() {
        let transport = MockHttpRemoteTransport::new().with_cancel_ok();
        assert_eq!(transport.cancel_count(), 0);

        let _ = transport.cancel_agent("agent-1").await;
        assert_eq!(transport.cancel_count(), 1);

        let _ = transport.cancel_agent("agent-2").await;
        assert_eq!(transport.cancel_count(), 2);
    }

    #[tokio::test]
    async fn mock_cancel_returns_configured_error() {
        let err = RemoteAgentError::rejected("http://mock", "cancel not allowed");
        let transport = MockHttpRemoteTransport::new().with_cancel_err(err.clone());
        let result = transport.cancel_agent("agent-1").await;
        assert_eq!(result.unwrap_err(), err);
    }

    #[tokio::test]
    async fn mock_status_returns_configured_state() {
        let transport = MockHttpRemoteTransport::new()
            .with_status_completed("agent-42", "all done");
        let status = transport.status_agent("agent-42").await.expect("status should succeed");
        assert_eq!(status.state, "completed");
        assert_eq!(status.output_text.as_deref(), Some("all done"));
    }

    #[tokio::test]
    async fn mock_status_returns_running_state() {
        let transport = MockHttpRemoteTransport::new()
            .with_status_running("agent-99");
        let status = transport.status_agent("agent-99").await.expect("status should succeed");
        assert_eq!(status.state, "running");
        assert!(status.session_id.is_some());
    }

    #[tokio::test]
    async fn mock_status_counts_calls() {
        let transport = MockHttpRemoteTransport::new();
        assert_eq!(transport.status_count(), 0);

        let _ = transport.status_agent("a").await;
        assert_eq!(transport.status_count(), 1);

        let _ = transport.status_agent("b").await;
        assert_eq!(transport.status_count(), 2);
    }

    #[tokio::test]
    async fn mock_default_constructor_works() {
        let transport = MockHttpRemoteTransport::default();
        let stream = transport
            .spawn(RemoteAgentRequest {
                prompt: "x".into(),
                allowed_tools: vec![],
                worktree_slug: None,
            })
            .await;
        assert!(stream.is_ok());
    }

    #[tokio::test]
    async fn mock_multiple_spawns_independent() {
        let transport = MockHttpRemoteTransport::new()
            .with_spawn_ok("multi-agent");

        // First spawn
        let s1 = transport
            .spawn(RemoteAgentRequest {
                prompt: "first".into(),
                allowed_tools: vec![],
                worktree_slug: None,
            })
            .await;
        assert!(s1.is_ok());

        // Second spawn
        let s2 = transport
            .spawn(RemoteAgentRequest {
                prompt: "second".into(),
                allowed_tools: vec![],
                worktree_slug: None,
            })
            .await;
        assert!(s2.is_ok());

        assert_eq!(transport.spawn_count(), 2);
    }

    #[tokio::test]
    async fn mock_events_can_include_errors() {
        let err = RemoteAgentError::transport("mock", "simulated stream error");
        let transport = MockHttpRemoteTransport::new()
            .with_spawn_ok("err-agent")
            .with_events(vec![
                Ok(RemoteAgentEvent::TextDelta("partial".into())),
                Err(err.clone()),
            ]);

        let stream = transport
            .spawn(RemoteAgentRequest {
                prompt: "test".into(),
                allowed_tools: vec![],
                worktree_slug: None,
            })
            .await
            .expect("spawn should succeed");

        let events: Vec<_> = stream.collect().await;
        assert_eq!(events.len(), 2);
        assert!(events[0].is_ok());
        assert_eq!(events[1].as_ref().unwrap_err(), &err);
    }

    // ── HTTP transport tests (local test server) ─────────────────────

    /// Minimal test HTTP server on a random local port.
    struct TestServer {
        addr: SocketAddr,
        _stop_tx: watch::Sender<bool>,
        _handle: tokio::task::JoinHandle<()>,
    }

    impl TestServer {
        /// Create a server that responds to every request with a fixed
        /// HTTP status, content-type, and body.
        async fn fixed(status: StatusCode, content_type: &str, body: &str) -> Self {
            let body = body.to_owned();
            let ct = content_type.to_owned();
            Self::new(move |_req| {
                let body = body.clone();
                let ct = ct.clone();
                async move {
                    let status_text = status
                        .canonical_reason()
                        .unwrap_or("Unknown");
                    format!(
                        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n{}",
                        status.as_u16(),
                        status_text,
                        body.len(),
                        ct,
                        body,
                    )
                }
            })
            .await
        }

        /// Create a server with a custom handler.
        /// The handler receives the raw HTTP request string and must return
        /// the complete HTTP response.
        async fn new<F, Fut>(handler: F) -> Self
        where
            F: Fn(String) -> Fut + Send + 'static,
            Fut: Future<Output = String> + Send,
        {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            let (stop_tx, mut stop_rx) = watch::channel(false);

            let handle = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = stop_rx.changed() => break,
                        result = listener.accept() => {
                            if let Ok((mut stream, _)) = result {
                                let mut buf = vec![0u8; 8192];
                                let n = stream.read(&mut buf).await.unwrap_or(0);
                                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                                let response = handler(request).await;
                                let _ = stream.write_all(response.as_bytes()).await;
                            }
                        }
                    }
                }
            });

            Self {
                addr,
                _stop_tx: stop_tx,
                _handle: handle,
            }
        }

        /// Base URL of this server (e.g. `http://127.0.0.1:12345`).
        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    fn test_request(prompt: &str) -> RemoteAgentRequest {
        RemoteAgentRequest {
            prompt: prompt.into(),
            allowed_tools: vec![],
            worktree_slug: None,
        }
    }

    #[tokio::test]
    async fn http_health_check_returns_true_on_200() {
        let server = TestServer::fixed(StatusCode::OK, "text/plain", "healthy").await;
        let transport = HttpRemoteTransport::with_retry_delays(
            server.url(),
            "test-token".into(),
            vec![], // no retries for faster tests
        );
        let healthy = transport.health_check().await.expect("health check should succeed");
        assert!(healthy);
    }

    #[tokio::test]
    async fn http_health_check_returns_false_on_non_200() {
        let server = TestServer::fixed(StatusCode::NOT_FOUND, "text/plain", "not found").await;
        let transport = HttpRemoteTransport::with_retry_delays(
            server.url(),
            "test-token".into(),
            vec![],
        );
        let healthy = transport.health_check().await.expect("health check should return Ok(false)");
        assert!(!healthy);
    }

    #[tokio::test]
    async fn http_health_check_fails_on_connection_refused() {
        // Use a port that nothing is listening on.
        let transport = HttpRemoteTransport::with_retry_delays(
            "http://127.0.0.1:1".into(),
            "test-token".into(),
            vec![],
        );
        let result = transport.health_check().await;
        match result {
            Err(RemoteAgentError::ConnectionRefused { endpoint, .. }) => {
                assert!(endpoint.contains("127.0.0.1:1"));
            }
            other => panic!("expected ConnectionRefused, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_rpc_rejects_401_as_auth_error() {
        let server = TestServer::fixed(
            StatusCode::UNAUTHORIZED,
            "application/json",
            r#"{"error":{"code":-32001,"message":"unauthorized"}}"#,
        )
        .await;
        let transport = HttpRemoteTransport::with_retry_delays(
            server.url(),
            "bad-token".into(),
            vec![],
        );
        let result = transport.cancel("agent-1").await;
        match result {
            Err(RemoteAgentError::Rejected { detail, .. }) => {
                assert!(
                    detail.contains("auth") || detail.contains("401"),
                    "expected auth-related message, got: {detail}"
                );
            }
            other => panic!("expected Rejected(401), got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_rpc_rejects_403_as_auth_error() {
        let server = TestServer::fixed(
            StatusCode::FORBIDDEN,
            "application/json",
            r#"{"error":{"code":-32003,"message":"forbidden"}}"#,
        )
        .await;
        let transport = HttpRemoteTransport::with_retry_delays(
            server.url(),
            "bad-token".into(),
            vec![],
        );
        let result = transport.cancel("agent-1").await;
        match result {
            Err(RemoteAgentError::Rejected { detail, .. }) => {
                assert!(
                    detail.contains("auth") || detail.contains("403"),
                    "expected auth-related message, got: {detail}"
                );
            }
            other => panic!("expected Rejected(403), got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_rpc_returns_parse_error_on_invalid_json() {
        let server = TestServer::fixed(
            StatusCode::OK,
            "application/json",
            "this is not json",
        )
        .await;
        let transport = HttpRemoteTransport::with_retry_delays(
            server.url(),
            "test-token".into(),
            vec![],
        );
        let result = transport.cancel("agent-1").await;
        match result {
            Err(RemoteAgentError::Transport { detail, .. }) => {
                assert!(
                    detail.contains("parse") || detail.contains("json"),
                    "expected parse error, got: {detail}"
                );
            }
            other => panic!("expected Transport, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_rpc_handles_json_rpc_error_response() {
        let server = TestServer::fixed(
            StatusCode::OK,
            "application/json",
            r#"{"error":{"code":-32603,"message":"internal engine error"},"id":1}"#,
        )
        .await;
        let transport = HttpRemoteTransport::with_retry_delays(
            server.url(),
            "test-token".into(),
            vec![],
        );
        let result: Result<(), _> = transport.cancel("agent-1").await;
        match result {
            Err(RemoteAgentError::Rejected { detail, .. }) => {
                assert!(
                    detail.contains("internal engine error"),
                    "expected rpc error message, got: {detail}"
                );
            }
            other => panic!("expected Rejected, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_rpc_handles_500_as_transport_error() {
        let server = TestServer::fixed(
            StatusCode::INTERNAL_SERVER_ERROR,
            "text/plain",
            "server error",
        )
        .await;
        let transport = HttpRemoteTransport::with_retry_delays(
            server.url(),
            "test-token".into(),
            vec![], // no retries, fail fast
        );
        let result: Result<(), _> = transport.cancel("agent-1").await;
        match result {
            Err(RemoteAgentError::Transport { detail, .. }) => {
                assert!(
                    detail.contains("500"),
                    "expected 500 detail, got: {detail}"
                );
            }
            other => panic!("expected Transport(500), got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_events_connects_and_parses_sse() {
        // SSE data for two text deltas + final event.
        let sse_body = concat!(
            "data: ",
            r#"{"type":"text_delta","data":"Hello"}"#,
            "\n\n",
            "data: ",
            r#"{"type":"text_delta","data":" World"}"#,
            "\n\n",
            "data: ",
            r#"{"type":"final","data":{"stop_reason":"done","output_text":"Hello World"}}"#,
            "\n\n",
        );
        let server = TestServer::fixed(
            StatusCode::OK,
            "text/event-stream",
            sse_body,
        )
        .await;
        let transport = HttpRemoteTransport::with_retry_delays(
            server.url(),
            "test-token".into(),
            vec![],
        );

        let mut stream = transport
            .events("agent-sse")
            .await
            .expect("SSE connect should succeed");

        let event1 = stream.next().await;
        assert!(event1.is_some());
        assert_eq!(
            event1.unwrap().unwrap(),
            RemoteAgentEvent::TextDelta("Hello".into())
        );

        let event2 = stream.next().await;
        assert!(event2.is_some());
        assert_eq!(
            event2.unwrap().unwrap(),
            RemoteAgentEvent::TextDelta(" World".into())
        );

        let event3 = stream.next().await;
        assert!(event3.is_some());
        assert_eq!(
            event3.unwrap().unwrap(),
            RemoteAgentEvent::Final {
                stop_reason: "done".into(),
                output_text: "Hello World".into(),
            }
        );

        // Stream should end
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn http_spawn_and_events_full_lifecycle() {
        // This test simulates the full spawn → events lifecycle using
        // two separate test server connections.
        //
        // Step 1: POST /rpc with spawn_agent → returns agent_id
        let spawn_response = r#"{"result":{"agent_id":"agent-001"},"id":1}"#;
        let rpc_server = TestServer::fixed(
            StatusCode::OK,
            "application/json",
            spawn_response,
        )
        .await;

        // Build transport pointing at the RPC server.
        // `spawn()` first connects to the RPC server, then to the events
        // endpoint at the same URL.  Since the RPC server already served
        // its response and closed, `events()` opens a second connection
        // which also hits the same handler — but for this test we use
        // separate server instances by stopping the first one.
        //
        // To avoid this problem we use one server for both calls:
        // the handler returns the spawn response for /rpc and the SSE
        // response for /events.
        //
        // For simplicity we test spawn and events separately below.
        drop(rpc_server);

        // Step 2: Use a fresh server that responds to both /rpc and /events.
        let sse_body = concat!(
            "data: ",
            r#"{"type":"text_delta","data":"work"}"#,
            "\n\n",
            "data: ",
            r#"{"type":"final","data":{"stop_reason":"done","output_text":"work done"}}"#,
            "\n\n",
        );

        // We use a handler that reads the request and decides what to respond.
        let server = TestServer::new(move |request| {
            let sse_body = sse_body.to_owned();
            async move {
                if request.contains("/events") {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{}",
                        sse_body.len(),
                        sse_body,
                    )
                } else {
                    // RPC response
                    let body = r#"{"result":{"agent_id":"agent-001"},"id":1}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body,
                    )
                }
            }
        })
        .await;

        let transport = HttpRemoteTransport::with_retry_delays(
            server.url(),
            "test-token".into(),
            vec![],
        );

        let mut stream = transport
            .spawn(test_request("do some work"))
            .await
            .expect("spawn should succeed");

        let event1 = stream.next().await;
        assert!(event1.is_some());
        assert_eq!(
            event1.unwrap().unwrap(),
            RemoteAgentEvent::TextDelta("work".into())
        );

        let event2 = stream.next().await;
        assert!(event2.is_some());
        assert_eq!(
            event2.unwrap().unwrap(),
            RemoteAgentEvent::Final {
                stop_reason: "done".into(),
                output_text: "work done".into(),
            }
        );

        assert!(stream.next().await.is_none(), "stream should end after final event");
    }

    #[tokio::test]
    async fn http_rpc_retry_eventually_succeeds() {
        // A handler that fails the first two times, then succeeds.
        let attempt = Arc::new(AtomicUsize::new(0));
        let attempt_clone = attempt.clone();

        let server = TestServer::new(move |_req| {
            let n = attempt_clone.fetch_add(1, Ordering::Relaxed);
            let body: String;
            let status: u16;
            if n < 2 {
                // Return 500 for first two attempts
                status = 500;
                body = "internal error".to_owned();
            } else {
                status = 200;
                body = r#"{"result":{"agent_id":"retry-agent"},"id":1}"#.to_owned();
            }
            let status_text = if status == 200 { "OK" } else { "Internal Server Error" };
            async move {
                format!(
                    "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
                    status,
                    status_text,
                    body.len(),
                    body,
                )
            }
        })
        .await;

        // Use tiny retry delays so the test runs quickly.
        let transport = HttpRemoteTransport::with_retry_delays(
            server.url(),
            "test-token".into(),
            vec![Duration::from_millis(5), Duration::from_millis(5), Duration::from_millis(5)],
        );

        let response: RemoteSpawnResponse =
            transport.rpc("spawn_agent", RemoteSpawnRequest {
                prompt: "test".into(),
                allowed_tools: vec![],
                worktree_slug: None,
            }).await.expect("rpc should succeed after retries");

        assert_eq!(response.agent_id, "retry-agent");
        // The handler was called 3 times (2 fails + 1 success).
        let calls = attempt.load(Ordering::Relaxed);
        assert!(
            calls >= 3,
            "expected at least 3 handler calls (2 retries + success), got {calls}"
        );
    }

    #[tokio::test]
    async fn http_rpc_retry_exhaustion_returns_last_error() {
        // A handler that always returns 500.
        let attempt = Arc::new(AtomicUsize::new(0));
        let attempt_clone = attempt.clone();

        let server = TestServer::new(move |_req| {
            attempt_clone.fetch_add(1, Ordering::Relaxed);
            async move {
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 15\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\ninternal error".to_string()
            }
        })
        .await;

        let transport = HttpRemoteTransport::with_retry_delays(
            server.url(),
            "test-token".into(),
            vec![Duration::from_millis(5), Duration::from_millis(5)],
        );

        let result: Result<(), _> = transport.cancel("agent-1").await;
        match result {
            Err(RemoteAgentError::Transport { detail, .. }) => {
                assert!(
                    detail.contains("500"),
                    "expected 500 in detail, got: {detail}"
                );
            }
            other => panic!("expected Transport(500), got: {other:?}"),
        }

        // Should have attempted exactly 3 times (0 + 2 retries)
        assert_eq!(attempt.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn http_events_rejects_401() {
        let server = TestServer::fixed(
            StatusCode::UNAUTHORIZED,
            "text/plain",
            "unauthorized",
        )
        .await;
        let transport = HttpRemoteTransport::with_retry_delays(
            server.url(),
            "bad-token".into(),
            vec![],
        );
        let result = transport.events("agent-1").await;
        match result {
            Err(RemoteAgentError::Rejected { detail, .. }) => {
                assert!(
                    detail.contains("auth") || detail.contains("401"),
                    "expected auth error, got: {detail}"
                );
            }
            _ => panic!("expected Rejected(401)"),
        }
    }

    #[tokio::test]
    async fn http_event_stream_rejects_bad_json() {
        // SSE with bad inner JSON
        let sse_body = "data: not-valid-json\n\n";
        let server = TestServer::fixed(
            StatusCode::OK,
            "text/event-stream",
            sse_body,
        )
        .await;
        let transport = HttpRemoteTransport::with_retry_delays(
            server.url(),
            "test-token".into(),
            vec![],
        );
        let mut stream = transport
            .events("agent-1")
            .await
            .expect("SSE connect should succeed");

        let event = stream.next().await;
        assert!(event.is_some());
        match event.unwrap() {
            Err(RemoteAgentError::Transport { detail, .. }) => {
                assert!(
                    detail.contains("parse"),
                    "expected parse error, got: {detail}"
                );
            }
            other => panic!("expected Transport(parse), got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_events_connection_refused() {
        let transport = HttpRemoteTransport::with_retry_delays(
            "http://127.0.0.1:1".into(),
            "test-token".into(),
            vec![],
        );
        let result = transport.events("agent-1").await;
        match result {
            Err(RemoteAgentError::ConnectionRefused { endpoint, .. }) => {
                assert!(endpoint.contains("127.0.0.1:1"));
            }
            _ => panic!("expected ConnectionRefused"),
        }
    }

    #[tokio::test]
    async fn http_status_returns_agent_state() {
        let body = r#"{"result":{"agent_id":"agent-1","state":"running","session_id":"sess-1","output_text":null,"error":null},"id":1}"#;
        let server = TestServer::fixed(
            StatusCode::OK,
            "application/json",
            body,
        )
        .await;
        let transport = HttpRemoteTransport::with_retry_delays(
            server.url(),
            "test-token".into(),
            vec![],
        );
        let status = transport.status("agent-1").await.expect("status should succeed");
        assert_eq!(status.agent_id, "agent-1");
        assert_eq!(status.state, "running");
        assert_eq!(status.session_id.as_deref(), Some("sess-1"));
    }

    #[tokio::test]
    async fn http_rpc_endpoint_appears_in_error_messages() {
        let server = TestServer::fixed(
            StatusCode::OK,
            "application/json",
            "bad json",
        )
        .await;
        let url = server.url();
        let transport = HttpRemoteTransport::with_retry_delays(
            url.clone(),
            "test-token".into(),
            vec![],
        );
        let result: Result<(), _> = transport.cancel("agent-1").await;
        match result {
            Err(e) => {
                let msg = format!("{e}");
                // The endpoint URL should appear in the error display
                assert!(
                    msg.contains(url.trim_start_matches("http://")),
                    "error should contain endpoint URL, got: {msg}"
                );
            }
            other => panic!("expected error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_rpc_handles_304_redirect_status() {
        let server = TestServer::fixed(
            StatusCode::NOT_MODIFIED,
            "text/plain",
            "not modified",
        )
        .await;
        let transport = HttpRemoteTransport::with_retry_delays(
            server.url(),
            "test-token".into(),
            vec![],
        );
        let result: Result<(), _> = transport.cancel("agent-1").await;
        match result {
            Err(RemoteAgentError::Rejected { detail, .. }) => {
                assert!(
                    detail.contains("304"),
                    "expected 304 in detail, got: {detail}"
                );
            }
            other => panic!("expected Rejected, got: {other:?}"),
        }
    }

    /// Verify that error messages from the HTTP transport include the
    /// endpoint URL rather than a cryptic generic message.
    #[tokio::test]
    async fn http_error_messages_are_diagnostic() {
        let server = TestServer::fixed(
            StatusCode::INTERNAL_SERVER_ERROR,
            "text/plain",
            "crash",
        )
        .await;
        let url = server.url();
        let transport = HttpRemoteTransport::with_retry_delays(
            url.clone(),
            "test-token".into(),
            vec![],
        );
        let result = transport.cancel("agent-x").await;
        let msg = format!("{}", result.unwrap_err());
        // The message should contain the endpoint URL (strip protocol for matching)
        let host_part = url.trim_start_matches("http://");
        assert!(
            msg.contains(host_part),
            "error message should contain host part '{host_part}', got: {msg}"
        );
        assert!(
            msg.contains("500"),
            "error message should contain status 500, got: {msg}"
        );
    }
}
