//! Multi-agent team coordination.
//!
//! - [`coordinator::Coordinator`] — orchestrates sub-agent workflows
//! - [`tool::TeamTool`] — exposes team orchestration as a tool
//! - [`mailbox`] — inter-agent message passing
//! - [`remote_agent::RemoteAgent`] — handle for remote agent communication
//! - [`prompt`] — coordinator system prompt (TS parity)

pub mod coordinator;
pub mod tool;
pub mod mailbox;
pub mod remote_agent;
pub mod prompt;
