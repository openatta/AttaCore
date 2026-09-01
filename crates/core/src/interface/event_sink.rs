//! `EventSink` — where the engine's events go.
//!
//! The engine has always emitted into an `mpsc` channel handed back from
//! `Builder::build()`. That works when the embedder's world is also a channel
//! and fails as soon as it is not: a host that wants to fan events into its own
//! bus, write them to a log, or drive a UI has to sit a translating task
//! between the two and keep it alive for the life of the session. This is the
//! seam that lets it plug in directly.
//!
//! # Delivery is the kernel's problem, not the sink's
//!
//! A sink is a plain synchronous callback, deliberately. Implementors do not
//! need to be async and do not need to think about backpressure — the kernel
//! buffers per sink and drops on overflow, so a sink that falls behind loses
//! its own events and nobody else's. That is the whole reason `emit` takes
//! `&self` and returns nothing: there is no answer a sink could give that the
//! turn should wait for.
//!
//! One sink must never be able to stall a turn. Everything else about this
//! contract follows from that.
//!
//! What it does *not* buy: `emit` is called from a task on the host's async
//! runtime, so a sink that blocks for a long time occupies a runtime worker
//! for that long. It costs the turn nothing (the turn only enqueues), but a
//! sink whose work is genuinely slow — a network round-trip, an fsync —
//! should hand that work to a thread of its own rather than do it here.

use crate::event::AgentEvent;
use std::sync::{Arc, Mutex};

/// Somewhere the engine's events go.
///
/// Called from the engine's own task. Implementations must not block for long
/// and must not panic; the kernel isolates sinks from each other but cannot
/// un-panic one.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: &AgentEvent);
}

/// Forwards into an unbounded `tokio` channel.
///
/// For a host that already has a channel to feed — a second consumer beside
/// the `event_rx` `Builder::build()` returns, say, or a re-broadcast into
/// something with a different lifetime than the session.
///
/// The primary `event_rx` is deliberately *not* one of these: it is fed
/// inline so that a failed send still reports back, which the permission
/// path needs in order to fail closed when the host is gone.
pub struct ChannelSink {
    tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
}

impl ChannelSink {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>) -> Self {
        Self { tx }
    }
}

impl EventSink for ChannelSink {
    fn emit(&self, event: &AgentEvent) {
        // A closed receiver is the normal end of a session, not an error.
        let _ = self.tx.send(event.clone());
    }
}

/// Collects events in memory.
///
/// The second implementation the contract needs to be a contract, and the one
/// tests assert against. Kept beside `ChannelSink` rather than in a test module
/// because embedders writing their own sink want something to compare against,
/// and because a trait with one production implementation and no other is
/// exactly the shape this refactor exists to stop shipping.
#[derive(Default)]
pub struct CollectingSink {
    events: Mutex<Vec<AgentEvent>>,
}

impl CollectingSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn events(&self) -> Vec<AgentEvent> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn len(&self) -> usize {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl EventSink for CollectingSink {
    fn emit(&self, event: &AgentEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event.clone());
    }
}
