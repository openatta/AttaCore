//! Session persistence: the `HistoryStore` contract, the two implementations
//! that ship with it (JSONL on disk, in memory), path sanitization and
//! transcript projection.

pub mod entry;
pub mod error;
pub mod migrate;
pub mod path;
pub mod project;
pub mod store;
pub mod transcript;
