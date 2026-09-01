//! `tool.result` — what one tool result looks like before the model sees it.

use crate::interface::script::ScriptCarrier;
use crate::interface::tool_middleware::ToolCall;
use crate::interface::tool_result::{ToolResultDraft, ToolResultTransformer};

/// A script bound to the tool-result point.
///
/// Sees the call and the draft result, returns the text it wants. Anything
/// else it returns — a number, an object, nothing — leaves the draft alone,
/// because a transformer that turned a malformed answer into an empty result
/// would be handing the model a tool that silently produced nothing.
pub struct ToolResultScript {
    carrier: std::sync::Arc<ScriptCarrier>,
    entry: String,
}

impl ToolResultScript {
    pub fn new(carrier: std::sync::Arc<ScriptCarrier>, entry: impl Into<String>) -> Self {
        Self {
            carrier,
            entry: entry.into(),
        }
    }
}

impl ToolResultTransformer for ToolResultScript {
    fn transform(&self, call: &ToolCall, draft: &mut ToolResultDraft) {
        let input = serde_json::json!({
            "tool": call.name,
            "input": call.input,
            "text": draft.text,
            "isError": draft.is_error,
        });

        let returned = match self.carrier.call_blocking(&self.entry, input) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    script = %self.carrier.script().id,
                    tool = %call.name,
                    error = %e,
                    "tool-result script did not run; the result is unchanged"
                );
                return;
            }
        };

        // A string is the whole vocabulary. A script that wants to say "leave
        // it alone" returns anything else, which is also what a script with a
        // bug does — and those two should have the same, harmless outcome.
        if let Some(text) = returned.as_str() {
            draft.text = text.to_string();
        }
    }
}
