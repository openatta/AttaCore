//! LLM backend adapter — Anthropic Messages API client.
//!
//! Adapts Anthropic API into the [`base::interface::model::Model`] trait.

pub mod client;
pub mod error;
pub mod adapter;
pub mod parser;
pub mod stream;
pub mod tokens;
pub mod types;
pub mod registry;
pub mod mock;
