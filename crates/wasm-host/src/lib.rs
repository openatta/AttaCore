//! wasmtime host for WASM component plugins.
//!
//! Kept as its own crate rather than folded into `plugin`: wasmtime is a
//! heavyweight dependency (minutes of compile time), and `plugin` is a
//! lightweight manifest reader that the daemon, `plugin-host` and the tests
//! all pull in. Paying for wasmtime everywhere something wants to read a
//! `plugin.toml` would be the wrong trade.

pub mod adapter;
pub mod bindings;
pub mod cache;
pub mod capabilities;
pub mod engine;
mod host_impl;
pub mod instance;
pub mod state;

pub use adapter::{qualified_name, WasmToolAdapter};
pub use bindings::{API_VERSION, WIT_PACKAGE};
pub use cache::AotCache;
pub use capabilities::ResolvedCapabilities;
pub use engine::{ComponentHandle, WasmEngine};
pub use instance::{with_deadline, CallFailure, PluginInstance, EPOCH_TICK};
pub use state::{KvNamespace, PluginState, ProgressSink};
