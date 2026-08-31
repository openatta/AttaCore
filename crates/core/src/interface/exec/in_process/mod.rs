//! The second provider: everything in this process, nothing on the machine.
//!
//! The acceptance for the execution layer is that two providers exist and
//! switching between them is configuration. This is the second one, and it is
//! not a stub — it is the shape this repository already uses for a second
//! implementation (`InMemoryBlobStore`, `InMemoryLayers`, `FixedEnvironment`),
//! and it has the use those have: running the tool layer without touching
//! anything.
//!
//! Paired with a [`FixedEnvironment`], a whole session runs and replays with
//! no disk, no subprocess and no network in it. Tool tests today create
//! temporary directories and fork real processes, which is how a suite ends up
//! measuring how fast a machine forks rather than what the code decides.
//!
//! # What it deliberately does not do
//!
//! It does not validate the contracts the way a remote provider would. It
//! never has to chunk a transfer, resolve a symlink graph that is not this
//! one, or answer what partial enforcement means across a machine boundary.
//! Those stayed as design constraints in `docs/EXECUTION_LAYER_DESIGN.md`
//! §3.1 precisely because this implementation cannot enforce them.
//!
//! [`FixedEnvironment`]: crate::interface::environment::FixedEnvironment

mod filesystem;
mod network;
mod process;
mod sandbox;

pub use filesystem::InMemoryFileSystem;
pub use network::OfflineNetwork;
pub use process::{ScriptedProcess, ScriptedRun};
pub use sandbox::NoSandbox;
