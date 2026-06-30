//! Remote export — HTTP JSON batch push with dual-endpoint failover.
//! Fire-and-forget: failures are silently dropped (telemetry must never block the agent).
//!
//! The primary endpoint is tried first. On failure, the secondary endpoint is tried.
//! If both fail, events are persisted to disk for later retry.
//! Each endpoint tracks its own consecutive-failure count with exponential backoff to
//! avoid hammering a failing endpoint across flush cycles.

use crate::config::{RemoteConfig, TelemetryConfig};
use crate::events::TelemetryEvent;
use crate::redact::RedactionPolicy;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

// ── Per-endpoint backoff state ─────────────────────────────────────────────

/// Tracks the health of a single HTTP endpoint across flush cycles.
///
/// After a failure the endpoint enters a cooldown window that grows
/// exponentially with each consecutive failure. A success resets the
/// counter and clears the cooldown.
struct EndpointState {
    url: String,
    api_key: String,
    consecutive_failures: u32,
    cooldown_until: Option<Instant>,
}

impl EndpointState {
    fn new(url: String, api_key: String) -> Self {
        Self {
            url,
            api_key,
            consecutive_failures: 0,
            cooldown_until: None,
        }
    }

    /// Whether this endpoint is in cooldown and should be skipped for now.
    fn should_skip(&self) -> bool {
        self.cooldown_until
            .map(|until| Instant::now() < until)
            .unwrap_or(false)
    }

    /// Record a successful send — reset failure counter and clear cooldown.
    fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.cooldown_until = None;
    }

    /// Record a failure — increment counter and set cooldown window.
    ///
    /// The cooldown duration is `backoff_base_ms * 2^(consecutive_failures-1)`,
    /// capped at ~17 minutes (10 doublings).
    fn record_failure(&mut self, backoff_base_ms: u64) {
        self.consecutive_failures += 1;
        let shift = (self.consecutive_failures - 1).min(10);
        let wait_ms = backoff_base_ms.saturating_mul(1u64 << shift);
        self.cooldown_until = Some(Instant::now() + Duration::from_millis(wait_ms));
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Resolve the primary endpoint from [`TelemetryConfig`].
///
/// Priority:
/// 1. `config.primary_endpoint` (new top-level field)
/// 2. `config.remote.endpoint` (backward-compatible fallback)
fn resolve_primary(config: &TelemetryConfig) -> Option<(String, String)> {
    if let Some(url) = &config.primary_endpoint {
        Some((
            url.clone(),
            config.primary_api_key.clone().unwrap_or_default(),
        ))
    } else if let Some(ref r) = config.remote {
        if !r.endpoint.is_empty() {
            Some((r.endpoint.clone(), r.telemetry_key.clone()))
        } else {
            None
        }
    } else {
        None
    }
}

/// Resolve the secondary endpoint from [`TelemetryConfig`].
///
/// Only available via the new `secondary_endpoint` top-level field.
/// Returns `None` if not configured (backward-compatible).
fn resolve_secondary(config: &TelemetryConfig) -> Option<(String, String)> {
    config.secondary_endpoint.as_ref().map(|url| {
        (
            url.clone(),
            config.secondary_api_key.clone().unwrap_or_default(),
        )
    })
}

/// Send a JSON payload to a single HTTP endpoint with configurable retries.
///
/// The retry loop uses exponential backoff: `backoff_base_ms * 2^(attempt)`.
/// Returns `Ok(())` on a 2xx response, `Err(())` after all retries are exhausted.
async fn send_to_endpoint(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    json_body: &str,
    retry_max_attempts: u32,
    retry_backoff_base_ms: u64,
) -> Result<(), ()> {
    let mut attempts = 0u32;
    loop {
        let result = client
            .post(url)
            .header("content-type", "application/json")
            .header("x-telemetry-key", api_key)
            .body(json_body.to_owned())
            .send()
            .await;
        match result {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            _ if attempts < retry_max_attempts => {
                attempts += 1;
                let ms = retry_backoff_base_ms * (1u64 << (attempts - 1));
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            _ => return Err(()),
        }
    }
}

// ── RemoteExporter ─────────────────────────────────────────────────────────

/// Spawn a background task that drains the receiver, batches events, and POSTs
/// them to the configured endpoint(s) as plain JSON.
///
/// When two HTTP endpoints are configured the exporter tries the primary first
/// and only falls back to the secondary on failure. Each endpoint tracks its
/// own independent backoff timer.
pub struct RemoteExporter {
    config: TelemetryConfig,
    rx: mpsc::Receiver<TelemetryEvent>,
    policy: RedactionPolicy,
    primary_ep: Option<EndpointState>,
    secondary_ep: Option<EndpointState>,
}

impl RemoteExporter {
    pub fn new(config: TelemetryConfig, rx: mpsc::Receiver<TelemetryEvent>) -> Self {
        let policy = RedactionPolicy {
            redact_prompts: config.redact_prompts,
            redact_tool_content: config.redact_tool_content,
            redact_error_messages: true,
            redact_secrets: true,
            redact_paths: false,
            redact_emails: true,
            redact_ip_addresses: false,
            redact_env_vars: true,
        };
        let primary_ep = resolve_primary(&config).map(|(u, k)| EndpointState::new(u, k));
        let secondary_ep = resolve_secondary(&config).map(|(u, k)| EndpointState::new(u, k));
        Self {
            config,
            rx,
            policy,
            primary_ep,
            secondary_ep,
        }
    }

    /// Drive the export loop. Returns when the channel is closed (handle dropped).
    pub async fn run(mut self) {
        let has_endpoint = self.primary_ep.is_some();
        if !has_endpoint {
            // No endpoint configured — drain and discard
            while self.rx.recv().await.is_some() {}
            return;
        }

        // Clone remote config for retry parameters; use a default
        // when no legacy `remote` block exists.
        let remote = self.config.remote.clone().unwrap_or_default();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();

        let flush_interval = Duration::from_secs(remote.flush_interval_secs);
        let mut batch: Vec<TelemetryEvent> = Vec::with_capacity(remote.max_queue_size);
        let mut last_flush = tokio::time::Instant::now();

        loop {
            let deadline = flush_interval.saturating_sub(last_flush.elapsed());
            match tokio::time::timeout(deadline, self.rx.recv()).await {
                Ok(Some(event)) => {
                    let event = event.redact(&self.policy);
                    batch.push(event);
                    if batch.len() >= remote.max_queue_size {
                        self.flush(&client, &remote, &mut batch).await;
                        last_flush = tokio::time::Instant::now();
                    }
                }
                Ok(None) => {
                    // Channel closed — flush remaining
                    if !batch.is_empty() {
                        self.flush(&client, &remote, &mut batch).await;
                    }
                    return;
                }
                Err(_) => {
                    // Timeout — flush
                    if !batch.is_empty() {
                        self.flush(&client, &remote, &mut batch).await;
                    }
                    last_flush = tokio::time::Instant::now();
                }
            }
        }
    }

    /// Flush the current batch to the configured endpoints with failover.
    ///
    /// 1. Try the primary endpoint (skip if in cooldown).
    /// 2. On failure, try the secondary endpoint (if configured, skip if in cooldown).
    /// 3. On both failure, persist to disk for later retry.
    ///
    /// Each endpoint tracks its own consecutive-failure count for cooldown.
    async fn flush(
        &mut self,
        client: &reqwest::Client,
        remote: &RemoteConfig,
        batch: &mut Vec<TelemetryEvent>,
    ) {
        if batch.is_empty() {
            return;
        }
        let payload = std::mem::take(batch);
        let json = serde_json::to_string(&payload).unwrap_or_default();

        // ── Try primary endpoint ─────────────────────────────────────────
        if let Some(ref mut ep) = self.primary_ep {
            if !ep.should_skip() {
                match send_to_endpoint(
                    client,
                    &ep.url,
                    &ep.api_key,
                    &json,
                    remote.retry_max_attempts,
                    remote.retry_backoff_base_ms,
                )
                .await
                {
                    Ok(()) => {
                        ep.record_success();
                        // Reset secondary too — the pipeline is healthy again
                        if let Some(ref mut sec) = self.secondary_ep {
                            sec.record_success();
                        }
                        return;
                    }
                    Err(()) => {
                        ep.record_failure(remote.retry_backoff_base_ms);
                    }
                }
            }
        }

        // ── Try secondary endpoint (failover) ────────────────────────────
        if let Some(ref mut ep) = self.secondary_ep {
            if !ep.should_skip() {
                match send_to_endpoint(
                    client,
                    &ep.url,
                    &ep.api_key,
                    &json,
                    remote.retry_max_attempts,
                    remote.retry_backoff_base_ms,
                )
                .await
                {
                    Ok(()) => {
                        ep.record_success();
                        return;
                    }
                    Err(()) => {
                        ep.record_failure(remote.retry_backoff_base_ms);
                    }
                }
            }
        }

        // ── Both failed (or only primary failed and no secondary) — persist ─
        if let Some(ref dir) = remote.disk_fallback_dir {
            Self::persist_failed_events(dir, &payload);
        }
    }

    /// Retry previously failed batches from disk on startup.
    /// Tries primary then secondary endpoint with failover.
    /// TS parity: `retryPreviousBatches()` in firstPartyEventLoggingExporter.ts.
    pub async fn retry_previous_batches(client: &reqwest::Client, config: &TelemetryConfig) {
        let Some(remote) = config.remote.as_ref() else {
            return;
        };
        let Some(dir) = remote.disk_fallback_dir.as_ref() else {
            return;
        };

        let primary = resolve_primary(config);
        let secondary = resolve_secondary(config);

        let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Ok(content) = tokio::fs::read_to_string(&path).await else {
                continue;
            };

            // Try primary
            let mut sent = false;
            if let Some((ref url, ref key)) = primary {
                sent = send_to_endpoint(
                    client,
                    url,
                    key,
                    &content,
                    remote.retry_max_attempts,
                    remote.retry_backoff_base_ms,
                )
                .await
                .is_ok();
            }

            // Try secondary if primary failed
            if !sent {
                if let Some((ref url, ref key)) = secondary {
                    sent = send_to_endpoint(
                        client,
                        url,
                        key,
                        &content,
                        remote.retry_max_attempts,
                        remote.retry_backoff_base_ms,
                    )
                    .await
                    .is_ok();
                }
            }

            if sent {
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
    }

    /// Persist failed events to disk for later retry.
    fn persist_failed_events(dir: &std::path::Path, events: &[TelemetryEvent]) {
        let Ok(json) = serde_json::to_string(events) else {
            return;
        };
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = dir.join(format!("failed_events_{timestamp}.json"));
        if let Err(e) = std::fs::write(&path, json) {
            tracing::warn!(path = %path.display(), error = %e, "failed to persist telemetry events to disk");
        }
    }
}
