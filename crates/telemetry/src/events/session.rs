//! Session lifecycle events.

use serde::Serialize;

/// Session 开始事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct SessionStartPayload {
    pub permission_mode: String,
    pub model: String,
    pub max_tokens: u32,
    pub thinking_mode: bool,
    pub sandbox_enabled: bool,
    pub resume_from: Option<String>,
    pub auth_modes: Vec<String>,
    pub mcp_server_count: usize,
    pub plugin_count: usize,
    pub skill_count: usize,
    pub output_format: String,
    pub started_at_ms: i64,
}

/// Session 结束事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct SessionEndPayload {
    pub duration_ms: i64,
    pub total_turns: u32,
    pub total_api_calls: u32,
    pub total_tool_calls: u32,
    pub total_permission_denials: u32,
    pub total_errors: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_creation: u64,
    pub total_cache_read: u64,
    pub total_cost_usd: f64,
    pub stop_reason: String,
}

/// 配置加载事件的载荷（session 开始后立即触发）。
#[derive(Debug, Clone, Serialize)]
pub struct ConfigLoadedPayload {
    pub config_source: String,
    pub has_project_config: bool,
    pub has_user_config: bool,
    pub model: String,
    pub permission_mode: String,
    pub tool_count: usize,
    pub mcp_server_count: usize,
    pub plugin_count: usize,
    pub hooks_configured: Vec<String>,
    pub custom_commands: Vec<String>,
}

/// 启动耗时分解事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct StartupTimingPayload {
    pub total_startup_ms: u64,
    pub config_load_ms: u64,
    pub tool_registration_ms: u64,
    pub mcp_connect_ms: u64,
    pub plugin_load_ms: u64,
    pub skill_load_ms: u64,
    pub history_resume_ms: u64,
    pub first_api_call_ms: u64,
}

/// 关闭 / 退出原因事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct ShutdownSignalPayload {
    pub reason: String,
    pub duration_ms: i64,
    pub total_turns: u32,
    pub had_errors: bool,
    pub exit_code: i32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::TelemetryEvent;
    use serde_json;

    #[test]
    fn event_has_schema_version_and_timestamps() {
        let event = TelemetryEvent::session_start(
            "sess_01",
            1,
            None,
            SessionStartPayload {
                permission_mode: "default".into(),
                model: "claude".into(),
                max_tokens: 4096,
                thinking_mode: false,
                sandbox_enabled: false,
                resume_from: None,
                auth_modes: vec![],
                mcp_server_count: 0,
                plugin_count: 0,
                skill_count: 0,
                output_format: "text".into(),
                started_at_ms: 1000,
            },
        );
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert!(v.get("event_id").is_some(), "missing event_id");
        assert!(v.get("timestamp_ms").is_some(), "missing timestamp_ms");
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["session_id"], "sess_01");
    }

    #[test]
    fn config_loaded_serializes() {
        let event = TelemetryEvent::config_loaded(
            "sess",
            1,
            None,
            ConfigLoadedPayload {
                config_source: "user".into(),
                has_project_config: true,
                has_user_config: true,
                model: "deepseek-v4".into(),
                permission_mode: "default".into(),
                tool_count: 42,
                mcp_server_count: 3,
                plugin_count: 1,
                hooks_configured: vec!["PreToolUse".into()],
                custom_commands: vec![],
            },
        );
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "config_loaded");
        assert_eq!(v["tool_count"], 42);
        assert_eq!(v["mcp_server_count"], 3);
    }

    #[test]
    fn startup_timing_serializes() {
        let event = TelemetryEvent::startup_timing(
            "sess",
            1,
            None,
            StartupTimingPayload {
                total_startup_ms: 1500,
                config_load_ms: 50,
                tool_registration_ms: 200,
                mcp_connect_ms: 800,
                plugin_load_ms: 100,
                skill_load_ms: 50,
                history_resume_ms: 200,
                first_api_call_ms: 100,
            },
        );
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "startup_timing");
        assert_eq!(v["total_startup_ms"], 1500);
        assert_eq!(v["mcp_connect_ms"], 800);
    }

    #[test]
    fn shutdown_signal_serializes() {
        let event = TelemetryEvent::shutdown_signal(
            "sess",
            10,
            None,
            ShutdownSignalPayload {
                reason: "user_quit".into(),
                duration_ms: 300000,
                total_turns: 10,
                had_errors: false,
                exit_code: 0,
            },
        );
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "shutdown_signal");
        assert_eq!(v["reason"], "user_quit");
        assert_eq!(v["total_turns"], 10);
    }
}
