//! Telemetry configuration — deserialized from settings.json `telemetry` block.

use serde::Deserialize;

/// Telemetry configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TelemetryConfig {
    pub enabled: bool,
    pub mode: TelemetryMode,
    #[serde(default = "default_true")]
    pub redact_prompts: bool,
    #[serde(default = "default_true")]
    pub redact_tool_content: bool,
    pub remote: Option<RemoteConfig>,
    #[serde(default)]
    pub otel_enabled: bool,
    #[serde(default)]
    pub otel_endpoint: Option<String>,
    #[serde(skip)]
    pub queue_size: usize,
    // ── Dual-endpoint fields ──────────────────────────────────────────
    /// Primary HTTP endpoint for telemetry events.
    /// If set, takes precedence over `remote.endpoint`.
    #[serde(default)]
    pub primary_endpoint: Option<String>,
    /// API key for the primary HTTP endpoint.
    #[serde(default)]
    pub primary_api_key: Option<String>,
    /// Secondary HTTP endpoint for failover.
    /// Events are sent here only if the primary endpoint is unreachable.
    #[serde(default)]
    pub secondary_endpoint: Option<String>,
    /// API key for the secondary HTTP endpoint.
    #[serde(default)]
    pub secondary_api_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TelemetryMode {
    #[default]
    Remote,
    Disabled,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoteConfig {
    pub endpoint: String,
    pub telemetry_key: String,
    #[serde(default = "default_flush_interval")]
    pub flush_interval_secs: u64,
    #[serde(default = "default_max_queue_size")]
    pub max_queue_size: usize,
    #[serde(default)]
    pub retry_max_attempts: u32,
    #[serde(default = "default_retry_backoff_ms")]
    pub retry_backoff_base_ms: u64,
    /// Disk fallback directory for failed events (TS parity: `appendEventsToFile`).
    /// When HTTP export exhausts retries, events are persisted here for later retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_fallback_dir: Option<std::path::PathBuf>,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            telemetry_key: String::new(),
            flush_interval_secs: default_flush_interval(),
            max_queue_size: default_max_queue_size(),
            retry_max_attempts: 0,
            retry_backoff_base_ms: default_retry_backoff_ms(),
            disk_fallback_dir: None,
        }
    }
}

fn default_true() -> bool { true }
fn default_flush_interval() -> u64 { 60 }
fn default_max_queue_size() -> usize { 500 }
fn default_retry_backoff_ms() -> u64 { 1000 }

impl TelemetryConfig {
    pub fn disabled() -> Self {
        Self { enabled: false, mode: TelemetryMode::Disabled, ..Default::default() }
    }
    pub fn queue_size(&self) -> usize {
        if self.queue_size > 0 { self.queue_size }
        else { self.remote.as_ref().map(|r| r.max_queue_size).unwrap_or(500) }
    }
}
