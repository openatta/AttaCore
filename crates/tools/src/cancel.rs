//! `run_with_cancel` — wrap an async operation with a `CancellationToken`.
//!
//! Every tool's `call()` method currently hand-rolls:
//!
//! ```ignore
//! let result = tokio::select! {
//!     result = perform_work() => result,
//!     _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
//! };
//! ```
//!
//! This module provides a single reusable helper so tools just write:
//!
//! ```ignore
//! run_with_cancel(&ctx.cancel, perform_work()).await?
//! ```

use std::future::Future;

use tokio_util::sync::CancellationToken;

use base::error::ToolError;

/// Race `f` against `cancel` being triggered.
///
/// If the cancellation token fires before `f` completes, the future is
/// dropped and `ToolError::Cancelled` is returned.  If `f` completes first
/// its value (which may itself be an `Err`) is returned as-is.
pub async fn run_with_cancel<F: Future>(
    cancel: &CancellationToken,
    f: F,
) -> Result<F::Output, ToolError> {
    tokio::select! {
        biased; // check cancellation first for promptness
        _ = cancel.cancelled() => Err(ToolError::Cancelled),
        result = f        => Ok(result),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{sleep, timeout, Duration};

    #[tokio::test]
    async fn completes_normally_when_not_cancelled() {
        let cancel = CancellationToken::new();
        let out = run_with_cancel(&cancel, async { 42 }).await;
        assert_eq!(out.unwrap(), 42);
    }

    #[tokio::test]
    async fn returns_cancelled_when_token_is_fired() {
        let cancel = CancellationToken::new();
        cancel.cancel();

        // The future never gets polled because `biased;` checks cancellation first.
        let out: Result<(), ToolError> =
            run_with_cancel(&cancel, async { panic!("should not be polled") }).await;
        assert!(matches!(out, Err(ToolError::Cancelled)));
    }

    #[tokio::test]
    async fn cancels_in_flight_operation() {
        let cancel = CancellationToken::new();

        // Spawn a task that fires the token after 50 ms.
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(50)).await;
            cancel_clone.cancel();
        });

        // The future would sleep for 10 s; cancellation should preempt it.
        let start = tokio::time::Instant::now();
        let out = run_with_cancel(&cancel, async {
            sleep(Duration::from_secs(10)).await;
            Ok::<&str, String>("too late")
        })
        .await;

        assert!(matches!(out, Err(ToolError::Cancelled)));
        assert!(start.elapsed() < Duration::from_secs(5)); // far less than 10 s
    }

    #[tokio::test]
    async fn propagates_errors_from_inner_future() {
        let cancel = CancellationToken::new();
        let out: Result<Result<i32, &str>, ToolError> =
            run_with_cancel(&cancel, async { Err("boom") }).await;
        // The outer `Result` is `Ok` because the future completed; the inner error is
        // the tool's own error variant.
        assert_eq!(out.unwrap(), Err("boom"));
    }

    #[tokio::test]
    async fn respects_timeout_around_run_with_cancel() {
        let cancel = CancellationToken::new();

        // Wrap `run_with_cancel` in an outer timeout — this simulates a
        // tool that wants *both* timeout and cancellation.
        let result = timeout(
            Duration::from_millis(50),
            run_with_cancel(&cancel, async {
                sleep(Duration::from_secs(10)).await;
                99
            }),
        )
        .await;

        assert!(result.is_err()); // timeout error
    }
}
