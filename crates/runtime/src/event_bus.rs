//! Fan-out from the engine's one emission point to any number of sinks.
//!
//! # The isolation rule
//!
//! A sink must never be able to slow a turn down. That is not politeness; it
//! is what makes the seam safe to open at all. If a host could stall the loop
//! by registering a sink that writes to a slow disk, then "you may observe the
//! engine" would silently mean "you may control its timing".
//!
//! So each sink gets its own bounded queue and its own task. `send` does a
//! `try_send` per sink and returns immediately — no `await`, no lock held
//! across one, no waiting on any consumer. A sink that cannot keep up fills
//! its own queue and then loses events; it does not borrow time from the turn
//! or from the other sinks.
//!
//! # Dropping, not blocking
//!
//! Overflow drops the *newest* event and counts it. Dropping the oldest would
//! be kinder to a sink building a running total and worse for every other
//! use, since the events a stalled consumer has not seen yet are the ones it
//! is furthest behind on. Either way the count is the honest part: a sink that
//! wants to know it missed something can ask.
//!
//! The primary channel — the `event_rx` handed back by `Builder::build()` — is
//! deliberately *not* one of these. It is unbounded and delivered inline, so
//! the existing contract with embedders is unchanged: they still receive every
//! event, in order, with no drops.

use base::event::AgentEvent;
use base::interface::event_sink::EventSink;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// How many events a lagging sink may fall behind before it starts losing
/// them. Large enough that an ordinary hiccup costs nothing, small enough that
/// a permanently stuck sink cannot grow without bound.
const SINK_QUEUE: usize = 1024;

/// One registered sink, plus the queue that keeps it from stalling the turn.
struct Lane {
    tx: tokio::sync::mpsc::Sender<AgentEvent>,
    dropped: Arc<AtomicU64>,
}

/// The engine's single emission point.
#[derive(Clone)]
pub struct EventBus {
    /// The channel `Builder::build()` returns to the caller. Unbounded and
    /// inline: the original contract, unchanged.
    primary: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
    lanes: Arc<Vec<Lane>>,
}

impl EventBus {
    /// A bus with only the primary channel — what every existing caller gets.
    pub fn new(primary: tokio::sync::mpsc::UnboundedSender<AgentEvent>) -> Self {
        Self {
            primary,
            lanes: Arc::new(Vec::new()),
        }
    }

    /// Attach sinks, spawning one drain task each.
    ///
    /// Called once at build time. Sinks cannot be added to a running bus:
    /// a sink that joins mid-turn would see a truncated event stream and have
    /// no way to know it, which is worse than not being able to join.
    pub fn with_sinks(
        primary: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
        sinks: Vec<Arc<dyn EventSink>>,
    ) -> Self {
        let mut lanes = Vec::with_capacity(sinks.len());
        for sink in sinks {
            let (tx, mut rx) = tokio::sync::mpsc::channel(SINK_QUEUE);
            let dropped = Arc::new(AtomicU64::new(0));
            let counter = dropped.clone();
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    // The sink is synchronous and may be slow; it is slow on
                    // this task, which belongs to it alone.
                    sink.emit(&ev);
                }
                let lost = counter.load(Ordering::Relaxed);
                if lost > 0 {
                    tracing::warn!(dropped = lost, "an event sink fell behind and lost events");
                }
            });
            lanes.push(Lane { tx, dropped });
        }
        Self {
            primary,
            lanes: Arc::new(lanes),
        }
    }

    /// Deliver one event. Never blocks, never awaits.
    ///
    /// Signature matches `mpsc::UnboundedSender::send` so the engine's emission
    /// sites did not have to change shape when this replaced the raw channel.
    /// That parity is why the fat `SendError` stays: shrinking it would be a
    /// gratuitous divergence from the type every caller here already knows,
    /// and the error is constructed only when the host is already gone.
    #[allow(clippy::result_large_err)]
    pub fn send(
        &self,
        event: AgentEvent,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<AgentEvent>> {
        for lane in self.lanes.iter() {
            if lane.tx.try_send(event.clone()).is_err() {
                lane.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.primary.send(event)
    }

    /// A raw channel that feeds this bus, for the crates below `runtime` that
    /// emit onto the session's stream but cannot name `EventBus` without
    /// pointing a dependency edge downward (`team`'s coordinator is the only
    /// one today).
    ///
    /// Events take one extra hop, so one sent through here may land after a
    /// later one sent directly. That was already true of everything this is
    /// for: it runs on its own task, so its ordering against the turn loop
    /// was never fixed to begin with.
    ///
    /// The forwarding task ends when the last sender is dropped.
    pub fn unbounded_bridge(&self) -> tokio::sync::mpsc::UnboundedSender<AgentEvent> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bus = self.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                let _ = bus.send(ev);
            }
        });
        tx
    }

    /// Total events dropped across all sinks, for tests and diagnostics.
    pub fn dropped(&self) -> u64 {
        self.lanes
            .iter()
            .map(|l| l.dropped.load(Ordering::Relaxed))
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::event_sink::CollectingSink;
    use std::time::{Duration, Instant};

    fn text(id: &str) -> AgentEvent {
        AgentEvent::TextDelta {
            text: id.to_string(),
            turn_id: id.to_string(),
        }
    }

    /// A sink that spends real wall-clock time inside `emit`, the way one
    /// writing to a slow disk would.
    struct Molasses {
        per_event: Duration,
        seen: Arc<AtomicU64>,
    }

    impl EventSink for Molasses {
        fn emit(&self, _event: &AgentEvent) {
            std::thread::sleep(self.per_event);
            self.seen.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_slow_sink_costs_the_sender_nothing() {
        let (primary, _rx) = tokio::sync::mpsc::unbounded_channel();
        let seen = Arc::new(AtomicU64::new(0));
        let bus = EventBus::with_sinks(
            primary,
            vec![Arc::new(Molasses {
                per_event: Duration::from_millis(500),
                seen: seen.clone(),
            })],
        );

        let started = Instant::now();
        for i in 0..5 {
            bus.send(text(&i.to_string())).unwrap();
        }
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(100),
            "sending past a sink that takes 500ms per event must not wait for it, took {elapsed:?}"
        );
        assert!(
            seen.load(Ordering::Relaxed) < 5,
            "the sink should still be working through its queue — if it had \
             finished, the sender was waiting for it"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_primary_channel_keeps_every_event_even_when_a_sink_is_stuck() {
        let (primary, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bus = EventBus::with_sinks(
            primary,
            vec![Arc::new(Molasses {
                per_event: Duration::from_millis(50),
                seen: Arc::new(AtomicU64::new(0)),
            })],
        );

        let total = SINK_QUEUE + 500;
        for i in 0..total {
            bus.send(text(&i.to_string())).unwrap();
        }

        assert!(
            bus.dropped() > 0,
            "a sink this far behind must lose events rather than grow without bound"
        );
        for i in 0..total {
            match rx.try_recv() {
                Ok(AgentEvent::TextDelta { turn_id, .. }) => assert_eq!(turn_id, i.to_string()),
                other => panic!("the primary channel must not drop or reorder: {other:?} at {i}"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_sink_sees_every_event() {
        let (primary, _rx) = tokio::sync::mpsc::unbounded_channel();
        let a = CollectingSink::new();
        let b = CollectingSink::new();
        let bus = EventBus::with_sinks(primary, vec![a.clone(), b.clone()]);

        for i in 0..3 {
            bus.send(text(&i.to_string())).unwrap();
        }
        drop(bus);

        for sink in [&a, &b] {
            let deadline = Instant::now() + Duration::from_secs(5);
            while sink.len() < 3 && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(sink.len(), 3, "each sink gets the whole stream, not a share of it");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_bridge_lands_on_the_primary_channel_and_the_sinks() {
        let (primary, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = CollectingSink::new();
        let bus = EventBus::with_sinks(primary, vec![sink.clone()]);

        bus.unbounded_bridge().send(text("from-below")).unwrap();

        let received = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the bridge must forward, not swallow")
            .expect("channel open");
        assert!(matches!(received, AgentEvent::TextDelta { turn_id, .. } if turn_id == "from-below"));

        let deadline = Instant::now() + Duration::from_secs(5);
        while sink.is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            sink.len(),
            1,
            "an event emitted from below `runtime` belongs on the same stream as the rest"
        );
    }
}
