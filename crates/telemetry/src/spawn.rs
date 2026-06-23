//! Spawn telemetry pipeline — returns a handle and a background consumer future.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::config::{TelemetryConfig, TelemetryMode};
use crate::handle::TelemetryHandle;
use crate::remote::RemoteExporter;

/// Concrete consumer future (not type-erased, so channel rx lifetime is clear).
pub struct TelemetryConsumer {
    inner: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl TelemetryConsumer {
    pub(crate) fn new(fut: impl Future<Output = ()> + Send + 'static) -> Self {
        Self { inner: Some(Box::pin(fut)) }
    }
    pub(crate) fn disabled() -> Self {
        Self { inner: None }
    }
}

impl Future for TelemetryConsumer {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match &mut self.get_mut().inner {
            Some(fut) => fut.as_mut().poll(cx),
            None => Poll::Ready(()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("invalid telemetry configuration: {0}")]
    InvalidConfig(String),
}

/// Create a telemetry pipeline. Returns `(handle, consumer)`:
/// - `handle` for recording events (cloneable, non-blocking)
/// - `consumer` to drive in background (`tokio::spawn` or `await`)
///
/// If `otel_enabled` is true in the config and the `otel` feature is active,
/// the OTLP exporter is also initialised (warnings are logged on failure but
/// the pipeline itself still starts).
pub fn spawn(
    config: TelemetryConfig,
) -> Result<(TelemetryHandle, TelemetryConsumer), SpawnError> {
    if !config.enabled || matches!(config.mode, TelemetryMode::Disabled) {
        return Ok((TelemetryHandle::noop(), TelemetryConsumer::disabled()));
    }

    let queue_size = config.queue_size();
    if queue_size == 0 {
        return Ok((TelemetryHandle::noop(), TelemetryConsumer::disabled()));
    }

    // Conditionally start the OTLP exporter.
    if config.otel_enabled {
        if config.remote.is_some() {
            #[cfg(feature = "otel")]
            {
                let remote = config.remote.as_ref().expect("just checked is_some");
                match crate::otel::start_otel(remote) {
                    Ok(exporter) => {
                        // Keep the exporter alive for the consumer lifetime.
                        // It will be dropped when this scope exits, which is
                        // fine because the OTLP batch processors run on their
                        // own Tokio tasks.
                        let _exporter = exporter;
                        tracing::info!(
                            endpoint = %remote.endpoint,
                            "OTLP exporter initialised"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            endpoint = %remote.endpoint,
                            "failed to initialise OTLP exporter"
                        );
                    }
                }
            }
            #[cfg(not(feature = "otel"))]
            {
                tracing::warn!("otel_enabled is true but the `otel` feature is not enabled");
            }
        } else {
            tracing::warn!("otel_enabled is true but no remote endpoint is configured");
        }
    }

    match config.mode {
        TelemetryMode::Remote => {
            let (tx, rx) = tokio::sync::mpsc::channel(queue_size);
            let handle = TelemetryHandle::new(tx);
            let exporter = RemoteExporter::new(config, rx);
            Ok((handle, TelemetryConsumer::new(exporter.run())))
        }
        TelemetryMode::Disabled => {
            Ok((TelemetryHandle::noop(), TelemetryConsumer::disabled()))
        }
    }
}
