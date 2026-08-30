//! `ToolResultTransformer` — the last hands on a tool's output before the
//! model reads it.
//!
//! Truncation, redaction, moving something large out of the conversation and
//! leaving a reference: these are policies about what a result may look like,
//! not about what a tool does. A tool that enforces its own cap enforces it
//! for everyone, in units it chose; a deployment that must never let a
//! credential reach a provider has nowhere to say so at all.
//!
//! # Last, and why that is the whole point
//!
//! The chain runs after every hook, immediately before the outcome is
//! returned. That ordering is what makes a redacting transformer a guarantee
//! rather than a suggestion: nothing downstream can put back what it took
//! out. Run it earlier and a `PostToolUse` hook rewriting the result would
//! quietly undo it.
//!
//! The cost of that choice is that hook payloads see the untransformed text.
//! That is the right trade here — a hook is a local command the operator
//! configured, at the same trust level as the engine itself, while the model
//! is a remote party.
//!
//! # Errors go through too
//!
//! A failure message is as capable of carrying a path, a token or a hostname
//! as a success is. A transformer that only saw successful results would be a
//! redaction policy with a hole in it.

use crate::interface::tool_middleware::ToolCall;

/// A large piece of binary content a tool returned, on its way to becoming an
/// image block beside the tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultImage {
    pub media_type: String,
    /// Base64, without a data-URI prefix.
    pub data: String,
}

/// A tool's output, mid-flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultDraft {
    /// What the model will read.
    pub text: String,
    /// Images that will ride alongside the result. A transformer may drop or
    /// replace these — that is what "move the large thing out of the
    /// conversation" looks like from here.
    pub images: Vec<ResultImage>,
    /// Whether this is a failure. Transformers see failures too; see the
    /// module docs.
    pub is_error: bool,
}

/// A policy about what a tool result may look like.
pub trait ToolResultTransformer: Send + Sync {
    fn transform(&self, call: &ToolCall, draft: &mut ToolResultDraft);
}

/// Run every transformer, in order.
pub fn apply(
    transformers: &[std::sync::Arc<dyn ToolResultTransformer>],
    call: &ToolCall,
    draft: &mut ToolResultDraft,
) {
    for t in transformers {
        t.transform(call, draft);
    }
}

/// Cap a result's length, saying so where it was cut.
///
/// Not registered by default: the engine imposes no cap of its own here, and
/// silently shortening a result is exactly the kind of thing that should be a
/// deployment's decision rather than a default nobody was told about.
pub struct TruncateText {
    pub max_bytes: usize,
}

impl ToolResultTransformer for TruncateText {
    fn transform(&self, _call: &ToolCall, draft: &mut ToolResultDraft) {
        if draft.text.len() <= self.max_bytes {
            return;
        }
        let kept = crate::text::truncate_at_char_boundary(&draft.text, self.max_bytes);
        let dropped = draft.text.len() - kept.len();
        // The note says what was lost. A silently shortened result reads to
        // the model as a complete one, and it will reason from the missing
        // half as though it were not there.
        draft.text = format!("{kept}\n… [{dropped} bytes truncated]");
    }
}

/// Replace known secrets wherever they appear.
///
/// Literal matching, not patterns: a deployment knows its own credentials —
/// it just handed them to a
/// [`CredentialSource`](crate::interface::credentials::CredentialSource) —
/// and matching what it knows cannot misfire the way a heuristic pattern can.
/// A regex that redacts something that merely looks like a token turns a
/// correct tool result into a confusing one.
pub struct RedactLiterals {
    secrets: Vec<String>,
    replacement: String,
}

impl RedactLiterals {
    pub fn new(secrets: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            // Empty strings would match everywhere and replace nothing
            // usefully; dropping them here beats every call site checking.
            secrets: secrets
                .into_iter()
                .map(Into::into)
                .filter(|s: &String| !s.is_empty())
                .collect(),
            replacement: "<redacted>".to_string(),
        }
    }

    pub fn with_replacement(mut self, replacement: impl Into<String>) -> Self {
        self.replacement = replacement.into();
        self
    }
}

impl ToolResultTransformer for RedactLiterals {
    fn transform(&self, _call: &ToolCall, draft: &mut ToolResultDraft) {
        for secret in &self.secrets {
            if draft.text.contains(secret.as_str()) {
                draft.text = draft.text.replace(secret.as_str(), &self.replacement);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn call() -> ToolCall {
        ToolCall {
            name: "Bash".into(),
            input: serde_json::json!({"command": "env"}),
        }
    }

    fn draft(text: &str) -> ToolResultDraft {
        ToolResultDraft {
            text: text.to_string(),
            images: Vec::new(),
            is_error: false,
        }
    }

    #[test]
    fn truncation_says_where_it_cut() {
        let t = TruncateText { max_bytes: 10 };
        let mut d = draft("0123456789abcdef");
        t.transform(&call(), &mut d);
        assert!(d.text.starts_with("0123456789"));
        assert!(d.text.contains("truncated"), "{}", d.text);
    }

    #[test]
    fn truncation_leaves_a_short_result_exactly_as_it_was() {
        let t = TruncateText { max_bytes: 100 };
        let mut d = draft("short");
        t.transform(&call(), &mut d);
        assert_eq!(d.text, "short");
    }

    #[test]
    fn truncation_never_splits_a_character() {
        let t = TruncateText { max_bytes: 4 };
        let mut d = draft("日本語のテキスト");
        t.transform(&call(), &mut d);
        assert!(d.text.starts_with("日"), "{}", d.text);
    }

    #[test]
    fn a_known_secret_does_not_reach_the_model() {
        let t = RedactLiterals::new(["sk-live-abc123"]);
        let mut d = draft("ANTHROPIC_API_KEY=sk-live-abc123\nOTHER=fine");
        t.transform(&call(), &mut d);
        assert!(!d.text.contains("sk-live-abc123"), "{}", d.text);
        assert!(d.text.contains("<redacted>"), "{}", d.text);
        assert!(d.text.contains("OTHER=fine"), "the rest must survive: {}", d.text);
    }

    /// A failure message carries the same things a success does. A policy that
    /// only covered successes would be a policy with a hole in it.
    #[test]
    fn a_failure_is_transformed_too() {
        let t = RedactLiterals::new(["sk-live-abc123"]);
        let mut d = ToolResultDraft {
            text: "command failed: bad token sk-live-abc123".into(),
            images: Vec::new(),
            is_error: true,
        };
        t.transform(&call(), &mut d);
        assert!(!d.text.contains("sk-live-abc123"));
    }

    #[test]
    fn transformers_run_in_order() {
        let chain: Vec<Arc<dyn ToolResultTransformer>> = vec![
            Arc::new(RedactLiterals::new(["secret"])),
            Arc::new(TruncateText { max_bytes: 12 }),
        ];
        let mut d = draft("a secret and a great deal more text");
        apply(&chain, &call(), &mut d);
        assert!(!d.text.contains("secret"));
        assert!(d.text.contains("truncated"));
    }

    #[test]
    fn an_empty_chain_changes_nothing() {
        let mut d = draft("untouched");
        apply(&[], &call(), &mut d);
        assert_eq!(d, draft("untouched"));
    }
}
