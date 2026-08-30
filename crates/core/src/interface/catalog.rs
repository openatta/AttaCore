//! The extension points, as data.
//!
//! They accumulated one at a time, each documented where it lives, which is
//! the right place for the detail and the wrong place for the overview. There
//! was no answer to "what can I hook, when does it fire, how often, and am I
//! allowed to" that did not involve reading nine modules — and no way to check
//! that the answer written in a document still matched the code.
//!
//! So the overview is a value. [`all`] is the list; the reference table in the
//! extension-point index is [rendered from it](render_markdown) rather than
//! typed out, which is what keeps the two from drifting.
//!
//! # Frequency is a design constraint, not a footnote
//!
//! A point that fires once per session can afford a subprocess. One that fires
//! per streamed chunk cannot afford anything — at ten thousand calls a turn,
//! even a cheap in-process call is the difference between streaming and
//! stuttering. That is why [`Frequency::PerStreamChunk`] points are closed to
//! scripts by default: not because a script would do something dangerous
//! there, but because the cost is not one an author can see when they write
//! it.

/// What kind of seam this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Replace a whole subsystem by implementing a trait.
    Contract,
    /// Contribute something to a collection the engine assembles.
    Registration,
    /// Sit in the path of something the engine is doing, with the option to
    /// change it.
    Interception,
}

/// How often it fires, to an order of magnitude.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frequency {
    /// Once per process.
    PerProcess,
    /// Once per session.
    PerSession,
    /// Once or twice a turn.
    PerTurn,
    /// Once per tool call — tens per turn.
    PerToolCall,
    /// Once per model request — a few per turn.
    PerModelCall,
    /// Per streamed chunk — thousands per turn.
    PerStreamChunk,
}

impl Frequency {
    pub fn label(self) -> &'static str {
        match self {
            Self::PerProcess => "per process",
            Self::PerSession => "per session",
            Self::PerTurn => "per turn",
            Self::PerToolCall => "per tool call",
            Self::PerModelCall => "per model call",
            Self::PerStreamChunk => "per streamed chunk",
        }
    }

    /// Roughly how many times a busy turn hits it.
    pub fn magnitude(self) -> &'static str {
        match self {
            Self::PerProcess | Self::PerSession => "10⁰",
            Self::PerTurn | Self::PerModelCall => "10⁰–10¹",
            Self::PerToolCall => "10¹",
            Self::PerStreamChunk => "10³–10⁴",
        }
    }
}

/// What one kind of author may do at a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Everything the point offers.
    Full,
    /// May contribute, may not change or remove what is already there.
    AddOnly,
    /// The full point, but only with a capability declared at install and
    /// shown to whoever installed it.
    Declared,
    /// Not available.
    Denied,
}

impl Access {
    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::AddOnly => "add only",
            Self::Declared => "declared capability",
            Self::Denied => "closed",
        }
    }
}

/// Who may do what, by where the extension came from.
///
/// The axis is *provenance*, not privilege level: the question a host is
/// really answering is "did the operator write this, or download it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trust {
    /// The engine's own registrations.
    pub kernel: Access,
    /// Written in settings by whoever runs this deployment.
    pub config: Access,
    /// A script in the operator's own project.
    pub script: Access,
    /// An installed plugin.
    pub plugin: Access,
}

impl Trust {
    /// Everything the operator wrote is unrestricted; a plugin may add.
    pub const fn operator_full_plugin_adds() -> Self {
        Self {
            kernel: Access::Full,
            config: Access::Full,
            script: Access::Full,
            plugin: Access::AddOnly,
        }
    }

    /// Everything the operator wrote is unrestricted; a plugin needs to have
    /// declared the capability.
    pub const fn operator_full_plugin_declares() -> Self {
        Self {
            kernel: Access::Full,
            config: Access::Full,
            script: Access::Full,
            plugin: Access::Declared,
        }
    }

    /// A seam only the embedding program can reach — it is a Rust trait
    /// wired at build time, so there is nothing for a script or a plugin to
    /// register through.
    pub const fn host_only() -> Self {
        Self {
            kernel: Access::Full,
            config: Access::Denied,
            script: Access::Denied,
            plugin: Access::Denied,
        }
    }
}

/// One place something can be plugged in.
#[derive(Debug, Clone, Copy)]
pub struct ExtensionPoint {
    /// Stable id. What documentation, configuration and error messages call
    /// this point.
    pub id: &'static str,
    pub kind: Kind,
    /// One line: what it is for.
    pub summary: &'static str,
    /// When in a turn it happens.
    pub timing: &'static str,
    /// What an implementation may change. "nothing" for a pure observer.
    pub rewrites: &'static str,
    pub frequency: Frequency,
    pub trust: Trust,
    /// Where the contract is defined, as a Rust path.
    pub defined_in: &'static str,
}

/// Every extension point the engine has.
pub fn all() -> &'static [ExtensionPoint] {
    &POINTS
}

static POINTS: [ExtensionPoint; 25] = [
        ExtensionPoint {
            id: "tool.registry",
            kind: Kind::Contract,
            summary: "the whole tool set, or one tool in it",
            timing: "session build, and any time after",
            rewrites: "which tools exist and what they are",
            frequency: Frequency::PerSession,
            trust: Trust::host_only(),
            defined_in: "base::interface::tool::ToolRegistry",
        },
        ExtensionPoint {
            id: "tool.around",
            kind: Kind::Interception,
            summary: "a ring around every tool call: timeouts, retries, caching, metrics",
            timing: "around dispatch, outside permission and hooks",
            rewrites: "the cancellation signal, the outcome; never the input",
            frequency: Frequency::PerToolCall,
            trust: Trust::operator_full_plugin_declares(),
            defined_in: "base::interface::tool_middleware::ToolMiddleware",
        },
        ExtensionPoint {
            id: "tool.result",
            kind: Kind::Interception,
            summary: "what a tool result may look like: truncation, redaction, large content",
            timing: "after every hook, immediately before the model sees it",
            rewrites: "the result text and its images",
            frequency: Frequency::PerToolCall,
            trust: Trust::operator_full_plugin_declares(),
            defined_in: "base::interface::tool_result::ToolResultTransformer",
        },
        ExtensionPoint {
            id: "prompt.block",
            kind: Kind::Registration,
            summary: "a named, ordered, revocable block of the system prompt",
            timing: "prompt assembly, once per turn",
            rewrites: "adds only",
            frequency: Frequency::PerTurn,
            trust: Trust::operator_full_plugin_adds(),
            defined_in: "base::interface::prompt_registry::PromptRegistry::register_block",
        },
        ExtensionPoint {
            id: "prompt.context",
            kind: Kind::Registration,
            summary: "a prompt block whose text is computed when the prompt is assembled",
            timing: "prompt assembly, once per turn",
            rewrites: "adds only",
            frequency: Frequency::PerTurn,
            trust: Trust::operator_full_plugin_adds(),
            defined_in: "base::interface::prompt_registry::PromptRegistry::register_context",
        },
        ExtensionPoint {
            id: "prompt.variable",
            kind: Kind::Registration,
            summary: "what `{{name}}` expands to, in every block",
            timing: "prompt assembly, after blocks are merged",
            rewrites: "its own placeholder, nothing else",
            frequency: Frequency::PerTurn,
            trust: Trust::operator_full_plugin_adds(),
            defined_in: "base::interface::prompt_registry::PromptRegistry::register_variable",
        },
        ExtensionPoint {
            id: "prompt.assemble",
            kind: Kind::Interception,
            summary: "a pass over the whole assembled prompt",
            timing: "prompt assembly, last",
            rewrites: "block content, order and membership",
            frequency: Frequency::PerTurn,
            trust: Trust::operator_full_plugin_declares(),
            defined_in: "base::interface::prompt_assembly::AssemblyHook",
        },
        ExtensionPoint {
            id: "event.sink",
            kind: Kind::Contract,
            summary: "where the engine's events go, beyond the channel it returns",
            timing: "every emission, on the sink's own task",
            rewrites: "nothing — observation only",
            frequency: Frequency::PerStreamChunk,
            trust: Trust::host_only(),
            defined_in: "base::interface::event_sink::EventSink",
        },
        ExtensionPoint {
            id: "elicitation.ask",
            kind: Kind::Contract,
            summary: "how the engine asks a person: authorization, clarification, import",
            timing: "whenever a decision needs a human",
            rewrites: "the answer",
            frequency: Frequency::PerTurn,
            trust: Trust::host_only(),
            defined_in: "base::interface::elicitation::Elicitation",
        },
        ExtensionPoint {
            id: "permission.check",
            kind: Kind::Contract,
            summary: "whether a tool call is allowed",
            timing: "before every tool call",
            rewrites: "permit, deny, or ask",
            frequency: Frequency::PerToolCall,
            trust: Trust::host_only(),
            defined_in: "base::interface::permission::Permission",
        },
        ExtensionPoint {
            id: "scene",
            kind: Kind::Contract,
            summary: "system prompt skeleton, tool surface and budgets",
            timing: "session build",
            rewrites: "everything about how an agent presents itself",
            frequency: Frequency::PerSession,
            trust: Trust::host_only(),
            defined_in: "base::interface::scene::AgentScene",
        },
        ExtensionPoint {
            id: "model",
            kind: Kind::Contract,
            summary: "the LLM backend and wire protocol",
            timing: "every model request",
            rewrites: "the whole exchange",
            frequency: Frequency::PerModelCall,
            trust: Trust::host_only(),
            defined_in: "base::interface::model::Model",
        },
        ExtensionPoint {
            id: "model.factory",
            kind: Kind::Registration,
            summary: "how an `api_type` in settings becomes a live model",
            timing: "startup, when provider config is read",
            rewrites: "which protocols can be configured",
            frequency: Frequency::PerProcess,
            trust: Trust::host_only(),
            defined_in: "base::interface::model_factory::ModelFactory",
        },
        ExtensionPoint {
            id: "model.request",
            kind: Kind::Interception,
            summary: "the assembled request before it is sent: messages, tools, params",
            timing: "immediately before each model call",
            rewrites: "everything in the request",
            frequency: Frequency::PerModelCall,
            trust: Trust::operator_full_plugin_declares(),
            defined_in: "base::interface::model_interceptor::ModelInterceptor::on_request",
        },
        ExtensionPoint {
            id: "model.message",
            kind: Kind::Interception,
            summary: "a complete message the model produced, before it is recorded",
            timing: "after the stream carrying it finishes",
            rewrites: "the message content",
            frequency: Frequency::PerModelCall,
            trust: Trust::operator_full_plugin_declares(),
            defined_in: "base::interface::model_interceptor::ModelInterceptor::on_message",
        },
        ExtensionPoint {
            id: "credentials",
            kind: Kind::Contract,
            summary: "where a provider's API key comes from",
            timing: "startup, when provider config is read",
            rewrites: "the credential",
            frequency: Frequency::PerProcess,
            trust: Trust::host_only(),
            defined_in: "base::interface::credentials::CredentialSource",
        },
        ExtensionPoint {
            id: "token.count",
            kind: Kind::Contract,
            summary: "how big the context is judged to be",
            timing: "every budget check",
            rewrites: "the number compaction triggers on",
            frequency: Frequency::PerTurn,
            trust: Trust::host_only(),
            defined_in: "base::interface::token_counter::TokenCounter",
        },
        ExtensionPoint {
            id: "history.store",
            kind: Kind::Contract,
            summary: "where a session's log lives between runs",
            timing: "every append, and on resume",
            rewrites: "how and where the log persists",
            frequency: Frequency::PerTurn,
            trust: Trust::host_only(),
            defined_in: "history::store::HistoryStore",
        },
        ExtensionPoint {
            id: "history.extension_entry",
            kind: Kind::Registration,
            summary: "state of your own in the session log, under your namespace",
            timing: "any time; ordered with everything else",
            rewrites: "adds only; the kernel never reads the payload",
            frequency: Frequency::PerTurn,
            trust: Trust::operator_full_plugin_adds(),
            defined_in: "history::entry::LogEntry::Extension",
        },
        ExtensionPoint {
            id: "memory.storage",
            kind: Kind::Contract,
            summary: "where durable memories are kept",
            timing: "recall, and whenever a memory is written",
            rewrites: "how and where memories persist",
            frequency: Frequency::PerTurn,
            trust: Trust::host_only(),
            defined_in: "base::interface::memory_contracts::MemoryStorage",
        },
        ExtensionPoint {
            id: "memory.retriever",
            kind: Kind::Contract,
            summary: "which memories a turn recalls",
            timing: "once per user message, in the background",
            rewrites: "the recalled set",
            frequency: Frequency::PerTurn,
            trust: Trust::host_only(),
            defined_in: "base::interface::memory_contracts::MemoryRetriever",
        },
        ExtensionPoint {
            id: "memory.retrieval_hook",
            kind: Kind::Interception,
            summary: "the recall question before it is asked, the answer before it is used",
            timing: "around retrieval",
            rewrites: "the query and the recalled names",
            frequency: Frequency::PerTurn,
            trust: Trust::operator_full_plugin_declares(),
            defined_in: "base::interface::memory_contracts::RetrievalHook",
        },
        ExtensionPoint {
            id: "compaction",
            kind: Kind::Contract,
            summary: "how a conversation is shortened when it stops fitting",
            timing: "when the budget threshold is crossed",
            rewrites: "the message history",
            frequency: Frequency::PerTurn,
            trust: Trust::host_only(),
            defined_in: "compaction::Compactor",
        },
        ExtensionPoint {
            id: "script.carrier",
            kind: Kind::Contract,
            summary: "run the operator's own code at a hook point, in this process",
            timing: "wherever the carrier is bound; governed by a per-turn quota",
            rewrites: "whatever the bound point allows, under the script's own provenance",
            frequency: Frequency::PerTurn,
            trust: Trust::operator_full_plugin_declares(),
            defined_in: "base::interface::script::ScriptEngine",
        },
        ExtensionPoint {
            id: "hooks",
            kind: Kind::Interception,
            summary: "lifecycle callbacks — command, prompt, HTTP, agent or wasm",
            timing: "thirty named moments; see the hook event list",
            rewrites: "varies by event: block, rewrite input, end the turn",
            frequency: Frequency::PerToolCall,
            trust: Trust {
                kernel: Access::Full,
                config: Access::Full,
                script: Access::Full,
                plugin: Access::Declared,
            },
            defined_in: "hooks::HookRunner",
        },
];

/// Look one up by id.
pub fn find(id: &str) -> Option<&'static ExtensionPoint> {
    all().iter().find(|p| p.id == id)
}

/// The reference table, generated.
///
/// The extension-point index embeds this rather than restating it, so a point
/// added to [`all`] appears in the documentation without anybody remembering
/// to write it down, and a point whose trust rules change cannot leave a
/// stale row behind.
pub fn render_markdown() -> String {
    let mut out = String::new();
    out.push_str("| Point | Kind | When | May change | Frequency | Config | Script | Plugin |\n");
    out.push_str("|---|---|---|---|---|---|---|---|\n");
    for p in all() {
        let kind = match p.kind {
            Kind::Contract => "contract",
            Kind::Registration => "registration",
            Kind::Interception => "interception",
        };
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} ({}) | {} | {} | {} |\n",
            p.id,
            kind,
            p.timing,
            p.rewrites,
            p.frequency.label(),
            p.frequency.magnitude(),
            p.trust.config.label(),
            p.trust.script.label(),
            p.trust.plugin.label(),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_point_says_all_the_things_a_reader_needs() {
        for p in all() {
            for (field, value) in [
                ("id", p.id),
                ("summary", p.summary),
                ("timing", p.timing),
                ("rewrites", p.rewrites),
                ("defined_in", p.defined_in),
            ] {
                assert!(
                    !value.trim().is_empty(),
                    "extension point '{}' has an empty {field}",
                    p.id
                );
            }
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut seen = HashSet::new();
        for p in all() {
            assert!(seen.insert(p.id), "duplicate extension point id '{}'", p.id);
        }
    }

    /// The rule the frequency column exists to enforce: a point that fires
    /// thousands of times a turn is not open to a script, because the cost is
    /// invisible to whoever writes one.
    #[test]
    fn the_highest_frequency_points_are_closed_to_scripts() {
        for p in all() {
            if p.frequency == Frequency::PerStreamChunk {
                assert_eq!(
                    p.trust.script,
                    Access::Denied,
                    "'{}' fires {} and must not be open to scripts",
                    p.id,
                    p.frequency.magnitude()
                );
                assert_eq!(p.trust.plugin, Access::Denied, "same for '{}'", p.id);
            }
        }
    }

    /// A plugin may only ever *add* without declaring something. Anything a
    /// point lets it rewrite has to be behind a declaration, or the
    /// install-time disclosure is not telling the truth.
    #[test]
    fn a_plugin_never_gets_more_than_add_only_by_default() {
        for p in all() {
            assert!(
                matches!(
                    p.trust.plugin,
                    Access::AddOnly | Access::Declared | Access::Denied
                ),
                "'{}' would give an installed plugin unrestricted access",
                p.id
            );
        }
    }

    #[test]
    fn the_table_has_a_row_for_every_point() {
        let table = render_markdown();
        for p in all() {
            assert!(
                table.contains(&format!("`{}`", p.id)),
                "'{}' is missing from the generated table",
                p.id
            );
        }
        assert_eq!(
            table.lines().count(),
            all().len() + 2,
            "header, separator, and one row each"
        );
    }

    #[test]
    fn find_is_the_id_lookup_it_claims_to_be() {
        assert_eq!(find("tool.around").map(|p| p.kind), Some(Kind::Interception));
        assert!(find("nothing.like.this").is_none());
    }
}
