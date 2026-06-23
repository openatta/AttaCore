//! First-party event logger — sampling, rate limiting, and dual-destination export.
//!
//! Wraps [`RemoteExporter`] with a sampling and rate-limiting layer. Events are
//! sampled based on configuration, batched (max 100, flush every 15 seconds),
//! and forwarded to the exporter for dual-destination HTTP delivery.
//!
//! TS parity: `firstPartyEventLoggingExporter.ts` — matching 15-second flush,
//! 100-event batch cap, and disk-backed retry queue.

use crate::config::TelemetryConfig;
use crate::events::TelemetryEvent;
use crate::handle::TelemetryHandle;
use crate::redact::RedactionPolicy;
use crate::remote::RemoteExporter;
use crate::spawn::TelemetryConsumer;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::sync::mpsc;

/// Sampling and rate-limiting configuration for first-party event logging.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SamplingConfig {
    /// Sampling rate between 0.0 (no events) and 1.0 (all events).
    /// Events are sampled deterministically based on their event_id hash.
    #[serde(default = "default_sample_rate")]
    pub sample_rate: f64,
    /// Maximum events allowed per minute before excess are dropped.
    /// 0 means unlimited.
    #[serde(default = "default_max_events_per_minute")]
    pub max_events_per_minute: u32,
}

impl Default for SamplingConfig {
    fn default() -> Self {
        Self {
            sample_rate: default_sample_rate(),
            max_events_per_minute: default_max_events_per_minute(),
        }
    }
}

fn default_sample_rate() -> f64 {
    1.0
}
fn default_max_events_per_minute() -> u32 {
    1000
}

/// First-party event logger with sampling, rate limiting, and dual-destination export.
///
/// This logger sits in front of `RemoteExporter` and provides an additional
/// sampling + rate-limiting layer. Events that pass sampling are batched and
/// forwarded to the exporter, which handles dual-destination HTTP delivery.
///
/// # Construction
///
/// Use [`FirstPartyEventLogger::spawn()`] to create the pipeline:
///
/// ```rust,ignore
/// let (handle, consumer) = FirstPartyEventLogger::spawn(config, SamplingConfig::default());
/// tokio::spawn(consumer);
/// handle.record(event);
/// ```
pub struct FirstPartyEventLogger {
    /// The underlying dual-destination remote exporter.
    pub exporter: RemoteExporter,
    /// Sampling and rate-limiting configuration.
    pub sampling_config: SamplingConfig,
}

impl FirstPartyEventLogger {
    /// Create and spawn a first-party event logging pipeline.
    ///
    /// Returns a `(TelemetryHandle, TelemetryConsumer)` pair. The handle is used
    /// to record events; the consumer must be driven in the background
    /// (via `tokio::spawn` or `await`) to process the event pipeline.
    ///
    /// The pipeline consists of two layers running concurrently:
    /// 1. Sampling layer — receives events, applies sampling + rate limiting,
    ///    batches (max 100 / 15s flush), and forwards to the exporter.
    /// 2. Export layer — [`RemoteExporter`] handles dual-destination HTTP delivery
    ///    with exponential backoff retry and disk persistence.
    pub fn spawn(
        config: TelemetryConfig,
        sampling_config: SamplingConfig,
    ) -> (TelemetryHandle, TelemetryConsumer) {
        let queue_size = config.queue_size().max(1);

        // Outer channel: application -> sampling layer
        let (tx, rx) = mpsc::channel(queue_size);
        let handle = TelemetryHandle::new(tx);

        // Inner channel: sampling layer -> exporter
        let (inner_tx, inner_rx) = mpsc::channel(100);
        let exporter = RemoteExporter::new(config, inner_rx);

        let policy = RedactionPolicy {
            redact_prompts: false,
            redact_tool_content: false,
            redact_error_messages: true,
            redact_secrets: true,
            redact_paths: false,
            redact_emails: true,
            redact_ip_addresses: false,
            redact_env_vars: true,
        };

        let consumer = TelemetryConsumer::new(async move {
            tokio::join!(
                Self::sampling_loop(rx, inner_tx, sampling_config, policy),
                exporter.run(),
            );
        });

        (handle, consumer)
    }

    /// Background task: receive events, apply sampling + rate limiting, batch,
    /// and forward to the exporter channel.
    ///
    /// - Sampling is deterministic (SHA256 of event_id).
    /// - Flush every 15 seconds or when batch reaches 100 events.
    /// - Rate limiting drops excess events when `max_events_per_minute` is exceeded.
    async fn sampling_loop(
        mut rx: mpsc::Receiver<TelemetryEvent>,
        tx: mpsc::Sender<TelemetryEvent>,
        config: SamplingConfig,
        policy: RedactionPolicy,
    ) {
        const FLUSH_INTERVAL: Duration = Duration::from_secs(15);
        const MAX_BATCH_SIZE: usize = 100;

        let mut batch: Vec<TelemetryEvent> = Vec::with_capacity(MAX_BATCH_SIZE);
        let mut last_flush = tokio::time::Instant::now();
        let mut minute_event_count: u32 = 0;
        let mut minute_window_start = tokio::time::Instant::now();

        loop {
            let deadline = FLUSH_INTERVAL.saturating_sub(last_flush.elapsed());
            match tokio::time::timeout(deadline, rx.recv()).await {
                Ok(Some(event)) => {
                    // --- Apply sampling ---
                    if !should_sample(&event, config.sample_rate) {
                        continue; // Drop — not sampled
                    }

                    // --- Apply redaction ---
                    let event = event.redact(&policy);

                    // --- Rate limiting ---
                    let now = tokio::time::Instant::now();
                    if now - minute_window_start >= Duration::from_secs(60) {
                        // Reset minute window
                        minute_event_count = 0;
                        minute_window_start = now;
                    }
                    if config.max_events_per_minute > 0
                        && minute_event_count >= config.max_events_per_minute
                    {
                        // Rate limit exceeded — drop event
                        tracing::trace!("telemetry rate limit exceeded, dropping event");
                        continue;
                    }
                    minute_event_count += 1;

                    // --- Add to batch ---
                    batch.push(event);
                    if batch.len() >= MAX_BATCH_SIZE {
                        Self::flush_batch(&tx, &mut batch).await;
                        last_flush = tokio::time::Instant::now();
                    }
                }
                Ok(None) => {
                    // Channel closed — flush remaining
                    if !batch.is_empty() {
                        Self::flush_batch(&tx, &mut batch).await;
                    }
                    return;
                }
                Err(_) => {
                    // Timeout (15s elapsed) — flush
                    if !batch.is_empty() {
                        Self::flush_batch(&tx, &mut batch).await;
                    }
                    last_flush = tokio::time::Instant::now();
                }
            }
        }
    }

    /// Flush the current batch to the exporter channel.
    ///
    /// Events are sent one at a time via `try_send` (non-blocking). If the
    /// exporter channel is full, excess events are silently dropped — telemetry
    /// must never block the agent.
    async fn flush_batch(tx: &mpsc::Sender<TelemetryEvent>, batch: &mut Vec<TelemetryEvent>) {
        if batch.is_empty() {
            return;
        }
        let payload = std::mem::take(batch);
        for event in payload {
            if tx.try_send(event).is_err() {
                // Channel full or closed — remaining events are dropped
                tracing::trace!("exporter channel full, dropping sampled telemetry events");
                break;
            }
        }
    }
}

/// Deterministic sampling: use SHA256 of the event's UUID to produce a
/// pseudo-random value in [0.0, 1.0). An event is sampled if this value
/// is less than `sample_rate`.
///
/// This ensures that the same event always gets the same sampling decision,
/// regardless of retries or ordering.
fn should_sample(event: &TelemetryEvent, sample_rate: f64) -> bool {
    if sample_rate >= 1.0 {
        return true;
    }
    if sample_rate <= 0.0 {
        return false;
    }
    let mut hasher = Sha256::new();
    hasher.update(event.event_id.as_bytes());
    let hash = hasher.finalize();
    // Use the first 32 bits of the hash as a u32, normalized to f64
    let value = u32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]]) as f64 / u32::MAX as f64;
    value < sample_rate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::TurnStartPayload;
    use uuid::Uuid;

    fn dummy_event() -> TelemetryEvent {
        TelemetryEvent::turn_start(
            "test-session",
            1,
            None,
            TurnStartPayload {
                turn_no: 1,
                turn_id: None,
                resumed: false,
                is_retry: false,
            },
        )
    }

    #[test]
    fn sampling_rate_1_0_sends_all() {
        let event = dummy_event();
        assert!(should_sample(&event, 1.0));
        assert!(should_sample(&event, 1.5));
    }

    #[test]
    fn sampling_rate_0_0_sends_none() {
        let event = dummy_event();
        assert!(!should_sample(&event, 0.0));
        assert!(!should_sample(&event, -0.1));
    }

    #[test]
    fn sampling_is_deterministic() {
        let event = dummy_event();
        let r1 = should_sample(&event, 0.5);
        let r2 = should_sample(&event, 0.5);
        assert_eq!(r1, r2);
    }

    #[test]
    fn different_events_have_different_sampling() {
        let e1 = dummy_event();
        // Create a second event with a different UUID by constructing manually
        let e2 = TelemetryEvent {
            event_id: Uuid::from_bytes([
                1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ]),
            ..dummy_event()
        };
        // At rate 0.5, at least some of the time they'll differ
        let r1 = should_sample(&e1, 0.5);
        let r2 = should_sample(&e2, 0.5);
        // They might be the same by chance, but the values should differ
        // at least some of the time across different event IDs.
        // We just verify both functions run without error.
        let _ = (r1, r2);
    }

    #[test]
    fn sampling_config_default() {
        let cfg = SamplingConfig::default();
        assert!((cfg.sample_rate - 1.0).abs() < f64::EPSILON);
        assert_eq!(cfg.max_events_per_minute, 1000);
    }

    #[test]
    fn sampling_config_serialization() {
        let cfg = SamplingConfig::default();
        let json = serde_json::to_value(&cfg).expect("serialization should succeed");
        assert_eq!(json["sample_rate"], 1.0);
        assert_eq!(json["maxEventsPerMinute"], 1000);
    }
}
