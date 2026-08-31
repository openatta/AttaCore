//! The execution layer — how a tool touches the machine.
//!
//! Four contracts, designed as one because they are entangled: a sandbox
//! constrains a process, a process needs files and a network, and the network
//! policy has to reach inside the sandbox. `docs/EXECUTION_LAYER_DESIGN.md` is
//! the blueprint; this module is its vocabulary.
//!
//! # What is deliberately not here
//!
//! An execution layer is the easiest place in an engine for a second kernel to
//! grow, so the boundary is written down rather than assumed. Retries,
//! timeouts, caching and metrics belong to [`tool_middleware`]; whether a
//! provider is healthy belongs to [`health`]; kept time belongs to
//! [`environment`]; path safety is a policy and stays with `permissions`; and
//! the sandbox *policy* — what to constrain and whether to constrain at all —
//! is the kernel's. A provider decides *how* to constrain, never *whether*.
//!
//! [`tool_middleware`]: crate::interface::tool_middleware
//! [`health`]: crate::interface::health
//! [`environment`]: crate::interface::environment

pub mod filesystem;
pub mod network;
pub mod process;
pub mod sandbox;

pub use filesystem::{DirEntry, FileSystem, Metadata};
pub use network::{HttpRequest, HttpResponse, Network, Origin};
pub use process::{ExitStatus, OutputChunk, OutputStream, Process, ProcessHandle, ProcessSpec};
pub use sandbox::{
    default_deny_read, Confined, Enforcement, NetworkMode, Sandbox, SandboxMode, SandboxPolicy,
};

/// Why an execution-layer call did not do what was asked.
///
/// Three variants because a tool has three different things to say about
/// them, and collapsing any two would make one of those sentences
/// unavailable. "The execution environment is unreachable" is the host's
/// problem; "the policy refused" is something the model should work around;
/// "the file does not exist" is the model's to interpret.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ExecError {
    /// The provider itself is not usable — a remote that cannot be reached, a
    /// backend that was never mounted.
    ///
    /// A provider that reports this must never have fallen back to executing
    /// locally instead. A deployment configured to run elsewhere, quietly
    /// running here when the elsewhere is down, has an isolation promise that
    /// fails exactly when it is needed.
    #[error("execution environment unavailable: {0}")]
    Unavailable(String),

    /// A policy refused: the sandbox, the network egress rules, a path
    /// outside what may be written.
    #[error("refused by policy: {0}")]
    Denied(String),

    /// The target's own failure — no such file, a non-zero exit, an HTTP 500.
    #[error("{0}")]
    Failed(String),
}

impl ExecError {
    pub fn unavailable(m: impl std::fmt::Display) -> Self {
        Self::Unavailable(m.to_string())
    }
    pub fn denied(m: impl std::fmt::Display) -> Self {
        Self::Denied(m.to_string())
    }
    pub fn failed(m: impl std::fmt::Display) -> Self {
        Self::Failed(m.to_string())
    }
}
