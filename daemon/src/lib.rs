//! Daemon library — JSON-RPC 2.0 server over Unix socket / TCP.
//!
//! This is a sample application showing how to build a long-running
//! agent service using the AttaCore crates.

pub mod config;
pub mod discovery;
pub mod rpc;
pub mod server;
pub mod session_manager;
pub mod session_pool;

pub use discovery::write_lock_file;
pub use server::DaemonServer;
pub use session_pool::SessionPool;
