//! AttaCore — the local execution layer.
//!
//! Implementations of `base::interface::exec`'s four contracts for the machine
//! this process is running on. They are the default everywhere; a deployment
//! that executes somewhere else replaces them without any tool knowing.

pub mod filesystem;
pub mod network;
pub mod process;
pub mod sandbox;

pub use filesystem::LocalFileSystem;
pub use network::LocalNetwork;
pub use process::LocalProcess;
pub use sandbox::PlatformSandbox;
