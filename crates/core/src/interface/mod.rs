//! Injectable contracts — the traits a host implements to replace part of the
//! engine.
//!
//! The entry requirement is narrow on purpose: **every module here defines at
//! least one trait the engine holds as a `dyn` object.** It used to say
//! "traits and types", which is how modules defining no contract at all ended
//! up here — a directory named for extension that a reader could not use to
//! find the extension points. Data types, configuration and concrete stores
//! live at the crate root instead (`base::settings`, `base::event`,
//! `base::prompt`, `base::memory`, `base::rules`); the old
//! `base::interface::*` paths still resolve via the re-exports below.
//!
//! # What is reachable from here, and what is not
//!
//! This crate has **no internal dependencies** — that is what keeps it at the
//! bottom of the graph. So this directory can only host contracts defined in
//! `base` itself:
//!
//! | Contract | Replaces |
//! |---|---|
//! | [`model::Model`] | the LLM backend / wire protocol |
//! | [`model_factory::ModelFactory`] | how a protocol is built from config |
//! | [`credentials::CredentialSource`] | where a provider's API key comes from |
//! | [`tool::Tool`] / [`tool::ToolRegistry`] | a tool, or the whole tool set |
//! | [`tool_middleware::ToolMiddleware`] | a ring around every tool call |
//! | [`tool_result::ToolResultTransformer`] | what a result may look like |
//! | [`scene::AgentScene`] | system prompt, tool surface, budgets |
//! | [`permission::Permission`] | tool authorization |
//! | [`agent_spawner::AgentSpawner`] | how sub-agents are launched |
//! | [`import_callback::ImportCallback`] | the host's answer to an import prompt |
//! | [`log::Logger`] | log routing |
//! | [`event_sink::EventSink`] | where the engine's events go |
//! | [`elicitation::Elicitation`] | how the engine asks a person something |
//! | [`tool::SecondaryLlm`] | the small model tools may call |
//! | [`token_counter::TokenCounter`] | how big the context is judged to be |
//! | [`prompt_registry::PromptRegistry`] | what goes into the system prompt |
//! | [`prompt_assembly::AssemblyHook`] | a last pass over the assembled prompt |
//! | [`script::ScriptEngine`] | running the operator's own code at a hook point |
//! | [`memory_contracts::MemoryStorage`] | where durable memories are kept |
//! | [`memory_contracts::MemoryRetriever`] | which of them a turn sees |
//!
//! [`catalog`] is the whole picture, including the contracts below that this
//! directory cannot re-export: every extension point as data, with when it
//! fires, what it may change, how often, and who is allowed to use it.
//!
//! The engine's other replaceable contracts live in the crate that owns them
//! and **cannot be re-exported here** without pointing a dependency edge
//! downward: `Compactor` (`compaction`), `HistoryStore` (`history`),
//! `PluginHost` (`runtime`), `TelemetryRecorder` (`telemetry`),
//! `PluginResolver` (`plugin`), `AutoClassifier` (`permissions`),
//! `McpClient` (`mcp`), `AnthropicClient` (`model`), `SearchProvider`
//! (`tools`). Anyone looking for the full map wants the extension-point
//! index, not this directory.

pub mod agent_spawner;
pub mod capabilities;
pub mod catalog;
pub mod credentials;
pub mod elicitation;
pub mod event_sink;
#[deprecated(note = "moved to `base::event` — event payloads, not an injectable contract")]
pub use crate::event;
pub mod import_callback;
pub mod log;
#[deprecated(note = "moved to `base::memory` — a concrete store, not an injectable contract")]
pub use crate::memory;
pub mod memory_contracts;
pub mod model;
pub mod model_factory;
pub mod permission;
pub mod prompt_assembly;
pub mod prompt_registry;
#[deprecated(note = "moved to `base::prompt` — prompt data types and the assembler function")]
pub use crate::prompt;
#[deprecated(note = "moved to `base::rules` — this module defines no extension contract")]
pub use crate::rules;
pub mod scene;
pub mod script;
pub mod token_counter;
pub mod tool;
pub mod tool_middleware;
pub mod tool_result;
#[deprecated(note = "moved to `base::settings` — declarative configuration types")]
pub use crate::settings;

/// The re-exports above are the compatibility half of moving these modules;
/// nothing in this repo imports through them any more, so only a check like
/// this notices if one is dropped. `use` resolves at compile time, which is
/// the whole assertion — an external embedder still on the old path keeps
/// building.
#[cfg(test)]
#[allow(deprecated, unused_imports)]
mod old_paths_still_resolve {
    use crate::interface::event::AgentEvent;
    use crate::interface::memory::MemoryStore;
    use crate::interface::prompt::PromptBlock;
    use crate::interface::rules::RuleEntry;
    use crate::interface::settings::Settings;
    // `tool` and `log` moved *into* this directory; their pre-move paths were
    // at the crate root and are re-exported from `lib.rs`.
    use crate::log::Logger;
    use crate::tool::{Tool, ToolRegistry};
}
