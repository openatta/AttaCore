//! Model / LLM API events.

use serde::Serialize;

use crate::RedactionPolicy;

/// API 请求成功事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct ApiRequestPayload {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub latency_ms: u64,
    pub ttfb_ms: u64,
    pub retry_count: u32,
    pub stop_reason: String,
    pub input_message_count: usize,
    pub tool_count: usize,
    pub default_model: bool,
    pub escalated: bool,
}

/// API 请求失败事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct ApiErrorPayload {
    pub model: String,
    pub error_kind: String,
    pub http_status: u16,
    pub error_message: String,
    pub latency_ms: u64,
    pub retry_count: u32,
}

/// 模型路由决策事件的载荷（含 fallback）。
#[derive(Debug, Clone, Serialize)]
pub struct ModelRoutePayload {
    pub requested_model: String,
    pub resolved_model: String,
    pub reason: String,
    pub is_fallback: bool,
    pub is_escalated: bool,
}

/// 意图分类事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct IntentClassifiedPayload {
    pub heuristic_class: Option<String>,
    pub llm_class: Option<String>,
    pub llm_latency_ms: u64,
    pub cache_hit: bool,
}

/// 上下文窗口状态报告事件的载荷（压缩决策前）。
#[derive(Debug, Clone, Serialize)]
pub struct ContextWindowReportPayload {
    pub total_tokens: u64,
    pub message_count: usize,
    pub tool_result_tokens: u64,
    pub compact_eligible: bool,
    pub exceeded_threshold: bool,
    pub threshold_pct: f64,
}

// ---- redact impls ----

impl ApiErrorPayload {
    pub(crate) fn redact(mut self, _policy: &RedactionPolicy) -> Self {
        self.error_message = crate::events::redact_string(&self.error_message);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::TelemetryEvent;
    use crate::redact::RedactionPolicy;

    #[test]
    fn api_error_redacts_error_message() {
        let policy = RedactionPolicy::all();
        let event = TelemetryEvent::api_error(
            "sess",
            1,
            None,
            ApiErrorPayload {
                model: "claude".into(),
                error_kind: "rate_limited".into(),
                http_status: 429,
                error_message: "You have exceeded your rate limit".into(),
                latency_ms: 500,
                retry_count: 0,
            },
        )
        .redact(&policy);
        let v = serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["error_message"], "[REDACTED]");
        assert_eq!(v["error_kind"], "rate_limited");
        assert_eq!(v["http_status"], 429);
    }

    #[test]
    fn context_window_report_serializes() {
        let event = TelemetryEvent::context_window_report(
            "sess",
            5,
            None,
            ContextWindowReportPayload {
                total_tokens: 45000,
                message_count: 12,
                tool_result_tokens: 20000,
                compact_eligible: true,
                exceeded_threshold: true,
                threshold_pct: 80.0,
            },
        );
        let v = serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "context_window_report");
        assert_eq!(v["total_tokens"], 45000);
        assert_eq!(v["exceeded_threshold"], true);
    }

    #[test]
    fn model_route_serializes() {
        let event = TelemetryEvent::model_route(
            "sess",
            1,
            None,
            ModelRoutePayload {
                requested_model: "claude-sonnet-4-6".into(),
                resolved_model: "claude-opus-4-6".into(),
                reason: "rate_limited".into(),
                is_fallback: true,
                is_escalated: false,
            },
        );
        let v = serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "model_route");
        assert_eq!(v["resolved_model"], "claude-opus-4-6");
        assert_eq!(v["is_fallback"], true);
    }
}
