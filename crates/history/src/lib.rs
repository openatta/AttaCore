//! Session persistence: the `HistoryStore` contract, the two implementations
//! that ship with it (JSONL on disk, in memory), path sanitization, transcript
//! projection and the session-query vocabulary.

pub mod entry;
pub mod error;
pub mod migrate;
pub mod path;
pub mod project;
pub mod query;
pub mod store;
pub mod transcript;
