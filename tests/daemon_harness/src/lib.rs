//! Runs a case against a daemon, over the daemon's own RPC surface.
//!
//! The engine has a behavior net (`tests/runner`) that asks what the turn
//! loop decides. This asks a different question: given a settings file, a
//! project and a client speaking JSON-RPC, does the assembled product do what
//! it says — are the tools registered, the scripts bound, the telemetry
//! written, the permission asked. Those are properties of the composition
//! root, and every one of them has been broken at some point by a change that
//! left the libraries themselves passing.
//!
//! Three pieces, because a case needs to control three things and observe
//! five:
//!
//! - [`world::World`] is where the run happens — settings roots, projects,
//!   the socket. Nothing under it is shared with the machine.
//! - [`provider::ProviderStub`] is the model, as a server. It answers from a
//!   script and keeps what it was sent, which is the only place the effect of
//!   a prompt block or a bound script is visible from outside.
//! - [`handle::Daemon`] is the daemon itself and the connection to it.
//!
//! The five surfaces a case can assert on: the RPC response, the
//! `session.event` frames, what reached the provider stub, what is on disk
//! (the working directory, the transcript, the telemetry file), and the
//! script ledger `session.get` reports. The last two exist because the first
//! three cannot tell a script that ran and changed nothing from one that was
//! never bound.

pub mod handle;
pub mod provider;
pub mod world;

pub use handle::{Daemon, DaemonOptions, Wire};
pub use provider::{Block, ProviderStub, Reply, SeenRequest};
pub use world::World;
