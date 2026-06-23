//! Post-compact cleanup — re-inject critical context lost during compaction.
//! TS parity: `postCompactCleanup.ts` in Claude Code's compact service.
//!
//! After compaction, certain context must be re-surfaced to the model:
//! - Skills that were loaded (they were in the old system prompt)
//! - Session memory (re-extract from MemoryStore)
//! - Active plan (if one exists)
//! - Deferred tool listings (if ToolSearch was used)
//!
//! v2: Post-compact cache clearing callback mechanism.
//! Consumers (e.g. runtime) can register cleanup callbacks that fire after
//! each successful compaction. TS parity: postCompactCleanup clears
//! systemPromptSections, classifierApprovals, speculativeChecks, etc.

use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage};
use std::sync::Mutex;

/// Result of post-compact cleanup: messages to inject into the session.
#[derive(Debug, Clone, Default)]
pub struct PostCompactContext {
    /// Messages to append after the compact boundary.
    pub inject_messages: Vec<ModelMessage>,
    /// Skills text to re-inject (will be added to the next prompt assembly).
    pub skills_text: Option<String>,
    /// Whether skills were re-injected.
    pub skills_reinjected: bool,
}

/// Global registry of post-compact cleanup callbacks.
/// Each callback is called after a successful compaction to clear caches.
#[allow(clippy::type_complexity)]
static CLEANUP_CALLBACKS: std::sync::LazyLock<Mutex<Vec<Box<dyn Fn() + Send + Sync>>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

/// Register a cleanup callback that will fire after each successful compaction.
/// TS parity: postCompactCleanup cache clearing hooks.
pub fn register_cleanup_callback(cb: Box<dyn Fn() + Send + Sync>) {
    if let Ok(mut guard) = CLEANUP_CALLBACKS.lock() {
        guard.push(cb);
    }
}

/// Run all registered post-compact cleanup callbacks.
/// Called after a successful compaction.
pub fn run_cleanup_callbacks() {
    if let Ok(guard) = CLEANUP_CALLBACKS.lock() {
        for cb in guard.iter() {
            cb();
        }
    }
}

/// Build post-compact injection messages.
///
/// Called after compaction succeeds. The caller provides the current state
/// and gets back messages to inject into the session.
pub fn build_post_compact_injections(
    _skills_text: Option<&str>,
    memory_summary: Option<&str>,
) -> PostCompactContext {
    let mut ctx = PostCompactContext::default();

    // Re-inject memory context if available
    if let Some(summary) = memory_summary {
        if !summary.is_empty() {
            ctx.inject_messages.push(ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text {
                    text: format!(
                        "<system-reminder>\nSession memory (re-injected after compaction):\n{summary}\n</system-reminder>"
                    ),
                }],
            });
            ctx.skills_reinjected = true;
        }
    }

    ctx
}

/// Mark that skills should be re-injected on the next turn.
/// This doesn't add messages but sets a flag that the next prompt assembly
/// should load skills again from the SkillManager.
pub fn skills_need_reinjection(skills_text: Option<String>) -> PostCompactContext {
    PostCompactContext {
        inject_messages: vec![],
        skills_text,
        skills_reinjected: true,
    }
}
