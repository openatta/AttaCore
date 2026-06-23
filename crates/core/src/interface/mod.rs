//! External interfaces — traits and types shared across crates.
//!
//! These define the AGENT's contract with the outside world.
//! Application layers inject implementations of these traits.

pub mod agent_spawner;
pub mod event;
pub mod memory;
pub mod model;
pub mod permission;
pub mod prompt;
pub mod scene;
pub mod settings;
