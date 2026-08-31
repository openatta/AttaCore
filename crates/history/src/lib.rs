//! Session persistence: the `HistoryStore` contract, the two implementations
//! that ship with it (JSONL on disk, in memory), path sanitization, transcript
//! projection, the session-query vocabulary and the blob store large content
//! is moved into, and the observers that watch appends go past.

pub mod blob;
pub mod entry;
pub mod error;
pub mod migrate;
pub mod observers;
pub mod path;
pub mod project;
pub mod query;
pub mod store;
pub mod transcript;
