//! AttaCore foundation — types, traits, and context shared by all crates.

pub mod error;
pub mod id;
pub mod message;
pub mod permission;
pub mod result;
pub mod session;
pub mod tool;
// The durable memory store lives in `interface::memory`. A second, JSON-backed
// `memory::DurableMemoryStore` used to sit here with no production wiring — it
// was deleted rather than kept as a parallel format.
pub mod context;
pub mod features;
pub mod frozen;
pub mod intent;
pub mod interface;
pub mod log;
pub mod path;
pub mod paths;
pub mod process_identity;
pub mod provider;
pub mod rpc;
pub mod summary;
pub mod text;
