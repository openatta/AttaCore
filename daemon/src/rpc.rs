//! JSON-RPC 2.0 wire types + error codes.
//!
//! One JSON object per line (newline-delimited) — trivially readable
//! with `tail -f` for debugging.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct RpcRequest {
    /// JSON-RPC version (must be "2.0")
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,
    /// Method name (e.g. "session.create")
    pub method: String,
    /// Method-specific params (object or null)
    #[serde(default)]
    pub params: Value,
    /// Request id; we echo back. Number or string. None = notification.
    pub id: Option<Value>,
}

fn default_jsonrpc() -> String {
    "2.0".to_string()
}

#[derive(Debug, Clone, Serialize)]
pub struct RpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    /// Like [`err`](Self::err), but attaches a structured `data` payload —
    /// for error codes where `docs/daemon_rpc_protocol.md` §9.1 documents the shape
    /// as part of the contract (clients are meant to read fields off it, not
    /// just display `message`).
    pub fn err_with_data(id: Value, code: i32, message: impl Into<String>, data: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: Some(data),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Standard JSON-RPC error codes + daemon extensions.
pub mod codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
    pub const SESSION_NOT_FOUND: i32 = -32000;
    pub const SESSION_CAP_REACHED: i32 = -32001;
    pub const ENGINE_ERROR: i32 = -32002;
    /// TCP handshake (`daemon.auth`) missing, malformed, or wrong token.
    pub const UNAUTHORIZED: i32 = -32003;
    /// `session.resume`/`session.fork`: the session's recorded `Meta.scene`
    /// does not match the scene being resumed into. See
    /// `docs/daemon_rpc_protocol.md` §9.1 for the `data` shape this carries.
    pub const SCENE_MISMATCH: i32 = -32006;
    /// `session.create`: the given `project_root` doesn't exist or isn't a
    /// readable directory. See `docs/daemon_rpc_protocol.md` §9.1 for the `data`
    /// shape this carries.
    pub const PROJECT_NOT_FOUND: i32 = -32010;
    /// `session.create`/`scene.activate`: the given scene isn't one this
    /// daemon binary registers at all.
    pub const SCENE_NOT_FOUND: i32 = -32004;
    /// `scene.deactivate`: refused because sessions are still recorded
    /// under this scene. See `docs/daemon_rpc_protocol.md` §9.1 for the `data` shape
    /// this carries.
    pub const SCENE_HAS_ACTIVE_SESSIONS: i32 = -32009;
    /// `session.resume`: the target is a sidechain that already ran its
    /// one-shot task to conclusion — no continuation semantics, re-derive a
    /// new sub-agent instead. See `docs/daemon_rpc_protocol.md` §9.1 for the `data`
    /// shape this carries.
    pub const SIDECHAIN_TERMINAL: i32 = -32014;
}

// ── Session options (extensible, passed via session.run_turn params) ──

/// Optional per-session configuration passed via `session.run_turn` params.
/// Applied when the session is first created; ignored for existing sessions.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct SessionOptions {
    pub vcr: Option<VcrOptions>,
    pub telemetry: Option<TelemetryOptions>,
    /// Opt-in real permission checking for this session — `None` (the
    /// default) keeps today's behavior (`AllowAllPermission`, every tool
    /// call proceeds, no `session.event{kind:"prompt"}` ever fires). Only a
    /// session that explicitly sets this switches to a real
    /// `RuleSetPermission`. This is deliberate: existing daemon-based
    /// applications that don't know about `session.respondToPrompt` must
    /// see zero behavior change.
    pub permission_mode: Option<base::interface::settings::PermissionMode>,
    /// Allow/deny/ask rules for the `RuleSetPermission` this session's
    /// `permission_mode` (if set) constructs. Ignored when `permission_mode`
    /// is `None`.
    #[serde(default)]
    pub permission_rules: Vec<base::permission::PermissionRule>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct VcrOptions {
    pub mode: String, // "record" | "replay"
    pub scenario: String,
    pub dir: String, // absolute path to VCR fixture directory
    /// Disable the network fallback on a replay miss — fail immediately
    /// instead of silently making a real (billed) API call. Was previously
    /// hardcoded `fallback_on_miss: true` server-side with no way for a
    /// caller to opt out (`api_runner.rs`'s equivalent direct-API path has
    /// supported this via `ATTA_VCR_STRICT` since earlier this session; the
    /// daemon RPC path never got the same knob). Defaults to `false` (the
    /// prior, only-available behavior) so existing callers are unaffected.
    #[serde(default)]
    pub strict: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct TelemetryOptions {
    pub output: String, // absolute path to telemetry output file
}

/// Streaming frame sent during long-running operations (e.g. session.run_turn).
/// Matches stream-json format.
#[derive(Debug, Clone, Serialize)]
pub struct StreamFrame {
    pub jsonrpc: &'static str,
    pub method: &'static str,
    pub params: Value,
}

impl StreamFrame {
    pub fn event(session_id: &str, turn_id: &str, event: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            method: "session.event",
            params: serde_json::json!({"session_id": session_id, "turn_id": turn_id, "event": event}),
        }
    }

    /// A daemon-level async notification (MCP connect outcome, import
    /// auto-detection, ...) pushed to any connection subscribed via
    /// `daemon.subscribeEvents` — not tied to any particular session/turn.
    pub fn daemon_event(event: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            method: "daemon.event",
            params: event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_minimal() {
        let s = r#"{"jsonrpc":"2.0","method":"daemon.status","id":1}"#;
        let r: RpcRequest = serde_json::from_str(s).unwrap();
        assert_eq!(r.method, "daemon.status");
        assert_eq!(r.id, Some(serde_json::json!(1)));
    }

    #[test]
    fn parse_request_no_id_is_notification() {
        let s = r#"{"jsonrpc":"2.0","method":"daemon.ping"}"#;
        let r: RpcRequest = serde_json::from_str(s).unwrap();
        assert!(r.id.is_none());
    }

    #[test]
    fn ok_response_round_trip() {
        let r = RpcResponse::ok(serde_json::json!(42), serde_json::json!({"ok":true}));
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""result":{"ok":true}"#));
        assert!(!s.contains("error"));
    }

    #[test]
    fn err_response_skips_result() {
        let r = RpcResponse::err(serde_json::json!(1), codes::METHOD_NOT_FOUND, "no");
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""error""#));
        assert!(!s.contains(r#""result""#));
    }

    #[test]
    fn stream_frame_format() {
        let f = StreamFrame::event(
            "sess-1",
            "turn-1",
            serde_json::json!({"kind": "text", "data": "hi"}),
        );
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains(r#""method":"session.event""#));
        assert!(s.contains(r#""session_id":"sess-1""#));
        assert!(s.contains(r#""turn_id":"turn-1""#));
    }

    #[test]
    fn rpc_error_code_constants_match_jsonrpc_spec() {
        assert_eq!(codes::PARSE_ERROR, -32700);
        assert_eq!(codes::INVALID_REQUEST, -32600);
        assert_eq!(codes::METHOD_NOT_FOUND, -32601);
        assert_eq!(codes::INVALID_PARAMS, -32602);
        assert_eq!(codes::INTERNAL_ERROR, -32603);
    }
}
