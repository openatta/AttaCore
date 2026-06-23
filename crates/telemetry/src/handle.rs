//! `TelemetryHandle` — 插桩侧持有的 channel sender。

use crate::events::TelemetryEvent;
use std::sync::Arc;

/// 插桩侧句柄：Clone + Send，可注入到 Engine / Gate / Client 中。
///
/// 内部是 `tokio::sync::mpsc::Sender`，所有 `record` 调用是**非阻塞**的
///（channel 满时直接丢弃——遥测不应阻塞主流程）。
#[derive(Debug, Clone)]
pub struct TelemetryHandle {
    tx: tokio::sync::mpsc::Sender<TelemetryEvent>,
}

/// `telemetry::record()` 可能返回的错误。
#[derive(Debug, Clone, thiserror::Error)]
pub enum TelemetryHandleError {
    /// Channel 满，事件被丢弃。
    #[error("telemetry channel full, event dropped")]
    ChannelFull,
    /// 遥测未启用（handle 是 Noop）。
    #[error("telemetry not enabled")]
    NotEnabled,
}

impl TelemetryHandle {
    /// 创建新 handle。一般情况下通过 `telemetry::spawn` 获取；
    /// 此构造函数暴露以支持集成测试（in-memory pipeline）。
    pub fn new(tx: tokio::sync::mpsc::Sender<TelemetryEvent>) -> Self {
        Self { tx }
    }

    /// Create a noop handle — events are silently dropped.
    pub fn noop() -> Self {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        Self { tx }
    }

    /// 记录一个遥测事件。
    ///
    /// 非阻塞：channel 满时丢弃事件而非等待。channel 关闭（receiver 已 drop，
    /// 如 disabled mode）时静默忽略而非返回错误。
    pub fn record(&self, event: TelemetryEvent) -> Result<(), TelemetryHandleError> {
        match self.tx.try_send(event) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("telemetry channel full, event dropped");
                Err(TelemetryHandleError::ChannelFull)
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                // Receiver dropped (deliberate, e.g. disabled mode) — silently noop
                Ok(())
            }
        }
    }

    /// 优雅关闭：等待 backlog 排出后返回。
    pub async fn shutdown(&self) {
        self.tx.closed().await;
    }
}

/// 什么都不做的 handle —— 遥测关闭时代的替身。
#[derive(Debug, Clone)]
pub struct NoopHandle;

impl NoopHandle {
    /// 创建。
    pub fn new() -> Self {
        Self
    }
    /// record 永远 Ok。
    pub fn record(&self, _event: TelemetryEvent) -> Result<(), TelemetryHandleError> {
        Ok(())
    }
    /// shutdown 是立即完成。
    pub async fn shutdown(&self) {}
}

impl Default for NoopHandle {
    fn default() -> Self {
        Self::new()
    }
}

// 让两个 handle 可互换的 trait —— 插桩点统一接收 `impl TelemetryRecorder`。
//
// 使用方式：
// ```rust
// fn run_turn(&self, telemetry: impl TelemetryRecorder) { ... }
// ```
// 或存储 `Box<dyn TelemetryRecorder + Send + Sync>`。

/// 统一遥测记录接口（方便 inject 时不需要 enum）。
pub trait TelemetryRecorder: Send + Sync + 'static {
    /// 记录一个事件。
    fn record(&self, event: TelemetryEvent) -> Result<(), TelemetryHandleError>;
    /// 等待关闭（Boxed future 以保持 dyn 兼容）。
    fn shutdown(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;
}

impl TelemetryRecorder for TelemetryHandle {
    fn record(&self, event: TelemetryEvent) -> Result<(), TelemetryHandleError> {
        self.record(event)
    }
    fn shutdown(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(self.shutdown())
    }
}

impl TelemetryRecorder for NoopHandle {
    fn record(&self, event: TelemetryEvent) -> Result<(), TelemetryHandleError> {
        self.record(event)
    }
    fn shutdown(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(self.shutdown())
    }
}

// 为 Arc-wrapped handle 实现 TelemetryRecorder（用于多人共享）
impl<T: TelemetryRecorder> TelemetryRecorder for Arc<T> {
    fn record(&self, event: TelemetryEvent) -> Result<(), TelemetryHandleError> {
        (**self).record(event)
    }
    fn shutdown(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        (**self).shutdown()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::TurnStartPayload;

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
    fn handle_record_normal_send_ok() {
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let handle = TelemetryHandle::new(tx);
        // try_send is synchronous
        assert!(handle.record(dummy_event()).is_ok());
    }

    #[test]
    fn handle_record_full_channel_returns_error() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let handle = TelemetryHandle::new(tx);

        // Fill the channel (capacity is 1)
        assert!(handle.record(dummy_event()).is_ok());
        // Second send should fail
        let result = handle.record(dummy_event());
        assert!(
            matches!(result, Err(TelemetryHandleError::ChannelFull)),
            "expected ChannelFull, got {:?}",
            result
        );
    }

    #[test]
    fn handle_record_closed_channel_silently_ok() {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let handle = TelemetryHandle::new(tx);
        drop(rx); // close receiver

        // Should return Ok, not error, when receiver is gone
        assert!(handle.record(dummy_event()).is_ok());
    }

    #[test]
    fn noop_handle_always_ok() {
        let noop = NoopHandle::new();
        assert!(noop.record(dummy_event()).is_ok());
        assert!(noop.record(dummy_event()).is_ok());
    }

    #[test]
    fn noop_handle_default_ok() {
        let noop = NoopHandle;
        assert!(noop.record(dummy_event()).is_ok());
    }

    #[test]
    fn telemetry_recorder_trait_dispatch_handle() {
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let handle = TelemetryHandle::new(tx);
        let recorder: &dyn TelemetryRecorder = &handle;
        assert!(recorder.record(dummy_event()).is_ok());
    }

    #[test]
    fn telemetry_recorder_trait_dispatch_noop() {
        let noop = NoopHandle::new();
        let recorder: &dyn TelemetryRecorder = &noop;
        assert!(recorder.record(dummy_event()).is_ok());
    }

    #[test]
    fn arc_handle_delegates_record() {
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let handle = Arc::new(TelemetryHandle::new(tx));
        let recorder: &dyn TelemetryRecorder = &handle as &dyn TelemetryRecorder;
        assert!(recorder.record(dummy_event()).is_ok());
    }
}
