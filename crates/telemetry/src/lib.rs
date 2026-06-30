//! Telemetry — structured event recording with optional HTTP export.
//!
//! Default: noop (zero overhead). When `telemetry_url` is configured, events are
//! batched and POSTed as plain JSON to the configured endpoint.
//!
//! Events are dropped silently if the channel is full — telemetry never blocks
//! the agent.

pub mod config;
pub mod cost;
pub mod events;
pub mod file_recorder;
pub mod handle;
#[cfg(feature = "otel")]
pub mod otel;
pub mod perf;
pub mod redact;
pub mod remote;
pub mod spawn;
pub mod stats;
pub mod vcr;

pub use config::{RemoteConfig, TelemetryConfig, TelemetryMode};
pub use events::{
    AgentCompletedPayload, AgentMessagePayload, AgentSpawnedPayload, ApiErrorPayload,
    ApiRequestPayload, CompactActionPayload, CompactOutcome, ConfigLoadedPayload,
    ContextWindowReportPayload, ErrorRecordPayload, EventPayload, FileOperationPayload,
    HookExecutionPayload, IntentClassifiedPayload, InterruptSignalPayload,
    McpConnectionErrorPayload, McpServerConnectedPayload, McpServerDisconnectedPayload,
    McpToolCallPayload, MemorySnapshotPayload, ModelRoutePayload, OutcomeRecordPayload,
    PermissionDecisionOutcome, PermissionDecisionPayload, ResumeActionPayload, ResumeOutcome,
    SessionEndPayload, SessionStartPayload, ShutdownSignalPayload, SlashCommandUsedPayload,
    StartupTimingPayload, TeamStageCompletePayload, TelemetryEvent, ToolCancelledPayload,
    ToolDecisionPayload, ToolExecutionPayload, ToolOutcome, ToolStartPayload, TuiActionPayload,
    TurnCompletePayload, TurnStartPayload, UserPromptSubmitPayload,
};
pub use file_recorder::FileRecorder;
pub use handle::{NoopHandle, TelemetryHandle, TelemetryHandleError, TelemetryRecorder};
pub use redact::RedactionPolicy;
pub use spawn::{spawn, SpawnError, TelemetryConsumer};
pub use vcr::VcrModel;
