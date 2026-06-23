//! OpenTelemetry OTLP export — optional feature-gated module.
//!
//! When the `otel` feature is enabled, this module provides an [`OtelExporter`]
//! that initialises an OTLP pipeline (trace + metrics) against the configured
//! endpoint. Protocol auto-detection: port 4317 → gRPC, port 4318 → HTTP/protobuf.
//!
//! # Metrics & spans exported
//!
//! - Turn duration histogram (`atta.turn.duration`)
//! - Tool execution counter (`atta.tool.executions`)
//! - API call latency histogram (`atta.api.latency`)
//! - Token usage histogram (`atta.token.usage`)

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use url::Url;

use crate::config::RemoteConfig;

// ── Protocol detection ──────────────────────────────────────────────────────

/// OTLP transport protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtelProtocol {
    /// gRPC (default port 4317).
    Grpc,
    /// HTTP + Protobuf (default port 4318).
    HttpProtobuf,
}

/// Detect transport protocol from the endpoint URL.
///
/// Rules:
/// - Port 4317 → `Grpc`
/// - Port 4318 → `HttpProtobuf`
/// - Fallback → `HttpProtobuf` (safe default for most collector deployments)
pub fn detect_protocol(endpoint: &str) -> OtelProtocol {
    let parsed = Url::parse(endpoint).ok();
    match parsed.and_then(|u| u.port()) {
        Some(4317) => OtelProtocol::Grpc,
        Some(4318) => OtelProtocol::HttpProtobuf,
        _ => OtelProtocol::HttpProtobuf,
    }
}

// ── Error type ──────────────────────────────────────────────────────────────

/// Errors that can occur during OTLP exporter initialisation.
#[derive(Debug, thiserror::Error)]
pub enum OtelError {
    #[error("invalid endpoint URL: {0}")]
    InvalidEndpoint(String),
    #[error("OTLP exporter initialisation failed: {0}")]
    InitFailed(String),
}

// ── Exporter ────────────────────────────────────────────────────────────────

/// Holds the OTLP meter- and tracer-provider so they are kept alive for the
/// lifetime of the application. Dropping the exporter triggers a graceful
/// shutdown of the pipeline.
#[must_use]
pub struct OtelExporter {
    meter_provider: Option<opentelemetry_sdk::metrics::SdkMeterProvider>,
    tracer_provider: Option<opentelemetry_sdk::trace::TracerProvider>,
}

impl OtelExporter {
    /// Shut down the exporter and flush any remaining data.
    pub fn shutdown(&self) {
        if let Some(ref mp) = self.meter_provider {
            if let Err(e) = mp.shutdown() {
                tracing::warn!(error = %e, "otel meter provider shutdown failed");
            }
        }
        if let Some(ref tp) = self.tracer_provider {
            if let Err(e) = tp.shutdown() {
                tracing::warn!(error = %e, "otel tracer provider shutdown failed");
            }
        }
    }
}

impl Drop for OtelExporter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Initialisation ──────────────────────────────────────────────────────────

/// Initialise an OTLP exporter from a [`RemoteConfig`].
///
/// Protocol is auto-detected from the endpoint URL (see [`detect_protocol`]).
/// Returns an error if the endpoint is empty.
///
/// # Metrics created
///
/// The global meter provider is configured with instruments for:
///
/// | Instrument        | Name                   | Type        | Unit    |
/// |-------------------|------------------------|-------------|---------|
/// | Turn duration     | `atta.turn.duration`   | Histogram   | ms      |
/// | Tool executions   | `atta.tool.executions` | Counter     | 1       |
/// | API call latency  | `atta.api.latency`     | Histogram   | ms      |
/// | Token usage       | `atta.token.usage`     | Histogram   | 1       |
pub fn start_otel(config: &RemoteConfig) -> Result<OtelExporter, OtelError> {
    if config.endpoint.is_empty() {
        return Err(OtelError::InvalidEndpoint("endpoint is empty".into()));
    }

    let protocol = detect_protocol(&config.endpoint);
    let endpoint = config.endpoint.clone();

    let resource = opentelemetry_sdk::Resource::new(vec![
        KeyValue::new("service.name", "attacore"),
    ]);

    // ── Trace exporter ──────────────────────────────────────────────────
    let trace_exporter = match protocol {
        OtelProtocol::Grpc => {
            opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .with_endpoint(&endpoint)
                .with_timeout(Duration::from_secs(10))
                .build()
                .map_err(|e| OtelError::InitFailed(e.to_string()))?
        }
        OtelProtocol::HttpProtobuf => {
            opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_endpoint(&endpoint)
                .with_timeout(Duration::from_secs(10))
                .build()
                .map_err(|e| OtelError::InitFailed(e.to_string()))?
        }
    };

    let tracer_provider = opentelemetry_sdk::trace::TracerProvider::builder()
        .with_batch_exporter(trace_exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(resource.clone())
        .build();

    opentelemetry::global::set_tracer_provider(tracer_provider.clone());

    // ── Metric exporter ─────────────────────────────────────────────────
    let metric_exporter = match protocol {
        OtelProtocol::Grpc => {
            opentelemetry_otlp::MetricExporter::builder()
                .with_tonic()
                .with_endpoint(&endpoint)
                .with_timeout(Duration::from_secs(10))
                .build()
                .map_err(|e| OtelError::InitFailed(e.to_string()))?
        }
        OtelProtocol::HttpProtobuf => {
            opentelemetry_otlp::MetricExporter::builder()
                .with_http()
                .with_endpoint(&endpoint)
                .with_timeout(Duration::from_secs(10))
                .build()
                .map_err(|e| OtelError::InitFailed(e.to_string()))?
        }
    };

    let meter_provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_reader(opentelemetry_sdk::metrics::PeriodicReader::builder(
            metric_exporter,
            opentelemetry_sdk::runtime::Tokio,
        )
        .with_interval(Duration::from_secs(60))
        .build())
        .with_resource(resource)
        .build();

    opentelemetry::global::set_meter_provider(meter_provider.clone());

    // Pre-register standard instruments via the global meter.
    let meter = opentelemetry::global::meter("attacore");

    let _turn_duration = meter
        .f64_histogram("atta.turn.duration")
        .with_description("Turn execution duration")
        .with_unit("ms")
        .build();

    let _tool_executions = meter
        .u64_counter("atta.tool.executions")
        .with_description("Number of tool executions")
        .with_unit("1")
        .build();

    let _api_latency = meter
        .f64_histogram("atta.api.latency")
        .with_description("API call latency")
        .with_unit("ms")
        .build();

    let _token_usage = meter
        .u64_histogram("atta.token.usage")
        .with_description("Token usage distribution")
        .with_unit("1")
        .build();

    Ok(OtelExporter {
        meter_provider: Some(meter_provider),
        tracer_provider: Some(tracer_provider),
    })
}

// ── Instrument helpers ──────────────────────────────────────────────────────

/// Record a turn duration via the global meter provider.
///
/// This is a no-op if the global meter provider has not been configured
/// (i.e. `start_otel` was not called).
pub fn record_turn_duration(duration_ms: f64) {
    let meter = opentelemetry::global::meter("attacore");
    meter
        .f64_histogram("atta.turn.duration")
        .with_description("Turn execution duration")
        .with_unit("ms")
        .build()
        .record(duration_ms, &[]);
}

/// Record a tool execution via the global meter provider.
///
/// This is a no-op if the global meter provider has not been configured.
pub fn record_tool_execution(tool_name: &str, success: bool) {
    let meter = opentelemetry::global::meter("attacore");
    let outcome = if success { "success" } else { "failure" };
    meter
        .u64_counter("atta.tool.executions")
        .with_description("Number of tool executions")
        .with_unit("1")
        .build()
        .add(
            1,
            &[
                KeyValue::new("tool", tool_name.to_string()),
                KeyValue::new("outcome", outcome),
            ],
        );
}

/// Record API call latency via the global meter provider.
///
/// This is a no-op if the global meter provider has not been configured.
pub fn record_api_latency(latency_ms: f64, model: &str) {
    let meter = opentelemetry::global::meter("attacore");
    meter
        .f64_histogram("atta.api.latency")
        .with_description("API call latency")
        .with_unit("ms")
        .build()
        .record(latency_ms, &[KeyValue::new("model", model.to_string())]);
}

/// Record token usage via the global meter provider.
///
/// This is a no-op if the global meter provider has not been configured.
pub fn record_token_usage(input_tokens: u64, output_tokens: u64, model: &str) {
    let meter = opentelemetry::global::meter("attacore");
    let hist = meter
        .u64_histogram("atta.token.usage")
        .with_description("Token usage distribution")
        .with_unit("1")
        .build();
    hist.record(
        input_tokens,
        &[
            KeyValue::new("direction", "input"),
            KeyValue::new("model", model.to_string()),
        ],
    );
    hist.record(
        output_tokens,
        &[
            KeyValue::new("direction", "output"),
            KeyValue::new("model", model.to_string()),
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_grpc_from_port_4317() {
        assert_eq!(
            detect_protocol("http://localhost:4317"),
            OtelProtocol::Grpc
        );
    }

    #[test]
    fn detect_http_from_port_4318() {
        assert_eq!(
            detect_protocol("http://localhost:4318"),
            OtelProtocol::HttpProtobuf
        );
    }

    #[test]
    fn detect_defaults_to_http() {
        assert_eq!(
            detect_protocol("http://otel-collector.example.com:55681"),
            OtelProtocol::HttpProtobuf
        );
    }

    #[test]
    fn detect_fallback_on_unparseable_url() {
        assert_eq!(
            detect_protocol("not-a-valid-url"),
            OtelProtocol::HttpProtobuf
        );
    }

    #[test]
    fn detect_handles_https_and_paths() {
        assert_eq!(
            detect_protocol("https://collector.example.com/v1/traces"),
            OtelProtocol::HttpProtobuf
        );
    }

    #[test]
    fn start_otel_returns_error_on_empty_endpoint() {
        let config = RemoteConfig {
            endpoint: String::new(),
            ..RemoteConfig::default()
        };
        let result = start_otel(&config);
        assert!(result.is_err());
    }
}
