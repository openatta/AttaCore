//! LLM backend adapter — Anthropic Messages API client.
//!
//! Adapts Anthropic API into the [`base::interface::model::Model`] trait.

pub mod adapter;
pub mod client;
pub mod error;
pub mod mock;
pub mod parser;
pub mod registry;
pub mod router;
pub mod stream;
pub mod tokens;
pub mod types;
