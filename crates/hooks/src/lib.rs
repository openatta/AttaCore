//! Lifecycle hook runner — event callbacks (Command / Prompt / HTTP / Agent).
//!
//! 11 event types × 4 hook types. No hooks registered = zero overhead (noop runner).

pub mod config;
pub mod payload;
pub mod runner;
pub mod watcher;

pub use config::{HookConfig, HookEvent, HooksSettings};
pub use payload::{HookDecision, HookInput, HookResponse};
pub use runner::HookRunner;
