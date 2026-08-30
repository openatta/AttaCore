//! Transcript projection over append-only jsonl history.
//!
//! The raw log keeps every turn and compaction marker. The API view is
//! projected from that log on demand so resume / compact / export can all
//! consume the same replayable representation.

use crate::entry::{EnvelopedEntry, LogEntry};
use base::id::Id;
use base::message::{ContentBlock, Message, StopReason, ToolResultContent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeProjectionReport {
    pub entry_count: usize,
    pub projected_message_count: usize,
    pub compact_boundary_count: usize,
    pub replacement_compact_count: usize,
    pub summary_fallback_compact_count: usize,
    pub sidechain_entry_count: usize,
    pub metadata_entry_count: usize,
    pub warning: Option<ResumeProjectionWarning>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeProjectionWarning {
    EmptyHistory,
    EmptyProjection,
    EmptyCompactBoundary,
}

/// Project raw history entries into the message view the model should see.
///
/// Rules:
/// - `Meta`, `System`, `UsageSnapshot` are metadata-only and do not enter API
/// - `User`, `Assistant`, `ToolResult` materialize as messages
/// - `Compact` acts as a replay boundary: the view is reset to the compact
///   summary payload, then subsequent turns are appended on top
pub fn project_messages(entries: &[EnvelopedEntry]) -> Vec<Message> {
    let mut out = Vec::new();
    for env in entries {
        if env.is_sidechain {
            continue;
        }
        apply_entry_to_messages(&env.entry, &mut out);
    }
    out
}

/// Running projected-message count after each successive prefix of `entries`.
///
/// `out[i]` is `project_messages(&entries[..=i]).len()` — computed in one
/// pass instead of N re-projections. Exists for callers that need to map a
/// *message* position (what a client sees, e.g. `session.history`'s indices)
/// back onto an *entry* position (what the on-disk log is made of), which is
/// what branching a transcript at a chosen point requires: the daemon's
/// `session.fork` picks the shortest entry prefix whose projection already
/// contains the requested number of messages.
///
/// The compaction boundary is why this can't be derived by counting entry
/// kinds: a single `Compact` entry *replaces* the whole message view rather
/// than appending to it, so the running count can go **down**. Callers
/// looking for "first prefix with at least N messages" must therefore scan
/// this vector rather than assume it's monotonic.
pub fn projected_lengths(entries: &[EnvelopedEntry]) -> Vec<usize> {
    let mut out = Vec::with_capacity(entries.len());
    let mut messages = Vec::new();
    for env in entries {
        if !env.is_sidechain {
            apply_entry_to_messages(&env.entry, &mut messages);
        }
        out.push(messages.len());
    }
    out
}

pub fn resume_projection_report(entries: &[EnvelopedEntry]) -> ResumeProjectionReport {
    let mut report = ResumeProjectionReport {
        entry_count: entries.len(),
        projected_message_count: 0,
        compact_boundary_count: 0,
        replacement_compact_count: 0,
        summary_fallback_compact_count: 0,
        sidechain_entry_count: 0,
        metadata_entry_count: 0,
        warning: None,
    };

    let mut messages = Vec::new();
    let mut saw_empty_compact = false;
    for env in entries {
        if env.is_sidechain {
            report.sidechain_entry_count += 1;
            continue;
        }
        match &env.entry {
            LogEntry::Meta { .. }
            | LogEntry::System { .. }
            | LogEntry::UsageSnapshot { .. }
            // Counted as metadata so the report still explains itself: a
            // session with many entries and few messages should say where the
            // difference went, and an extension's entries are part of it.
            | LogEntry::Extension { .. } => {
                report.metadata_entry_count += 1;
            }
            LogEntry::Compact {
                replacement_history,
                summary,
                ..
            } => {
                report.compact_boundary_count += 1;
                if replacement_history.is_some() {
                    report.replacement_compact_count += 1;
                } else if summary.as_ref().is_some_and(|summary| !summary.is_empty()) {
                    report.summary_fallback_compact_count += 1;
                } else {
                    saw_empty_compact = true;
                }
                apply_entry_to_messages(&env.entry, &mut messages);
            }
            _ => apply_entry_to_messages(&env.entry, &mut messages),
        }
    }
    report.projected_message_count = messages.len();
    report.warning = if entries.is_empty() {
        Some(ResumeProjectionWarning::EmptyHistory)
    } else if messages.is_empty() {
        Some(ResumeProjectionWarning::EmptyProjection)
    } else if saw_empty_compact {
        Some(ResumeProjectionWarning::EmptyCompactBoundary)
    } else {
        None
    };
    report
}

/// Extract a compact summary payload from a projected message list.
///
/// Current compactor emits a single user message containing the summary marker
/// and markdown body. We persist that payload verbatim in the transcript log
/// so later replays can reconstruct the same API view.
pub fn compact_summary_from_messages(messages: &[Message]) -> Option<Vec<ContentBlock>> {
    let Message::User { content } = messages.first()? else {
        return None;
    };
    if content.is_empty() {
        return None;
    }
    Some(content.clone())
}

/// Recover the latest assistant stop reason from projected messages.
pub fn latest_stop_reason(messages: &[Message]) -> Option<StopReason> {
    messages.iter().rev().find_map(|m| match m {
        Message::Assistant { stop_reason, .. } => *stop_reason,
        _ => None,
    })
}

/// Count projected API messages through the entry with `target_id`.
///
/// This lets session-memory compaction use a stable transcript entry id while
/// still feeding the existing message-slice compactor a projected message
/// boundary. Returns None if the entry id is not present.
pub fn projected_message_count_through_entry_id(
    entries: &[EnvelopedEntry],
    target_id: Id,
) -> Option<usize> {
    let mut messages: Vec<Message> = Vec::new();
    for env in entries {
        if !env.is_sidechain {
            apply_entry_to_messages(&env.entry, &mut messages);
        }
        if env.id == target_id {
            return Some(messages.len());
        }
    }
    None
}

pub fn latest_projected_message_entry_id(entries: &[EnvelopedEntry]) -> Option<Id> {
    entries.iter().rev().find_map(|env| match env.entry {
        _ if env.is_sidechain => None,
        LogEntry::User { .. } | LogEntry::Assistant { .. } | LogEntry::ToolResult { .. } => {
            Some(env.id)
        }
        LogEntry::Meta { .. }
        | LogEntry::System { .. }
        | LogEntry::Compact { .. }
        | LogEntry::UsageSnapshot { .. }
        | LogEntry::PasteRef { .. }
        | LogEntry::Extension { .. }
        | LogEntry::SessionEnd { .. } => None,
    })
}

fn apply_entry_to_messages(entry: &LogEntry, messages: &mut Vec<Message>) {
    match entry {
        LogEntry::Meta { .. }
        | LogEntry::System { .. }
        | LogEntry::UsageSnapshot { .. }
        | LogEntry::PasteRef { .. }
        | LogEntry::SessionEnd { .. } => {} // PasteRef hydrated by store::load()
        // An extension's state is not model-visible. Whether it could be —
        // and what would have to be true for it to be reconstructible from
        // the log if it were — is the projection question, not this one.
        // Skipping here is what makes an entry from an uninstalled plugin
        // inert rather than fatal.
        LogEntry::Extension { .. } => {}
        LogEntry::User { content } => messages.push(Message::User {
            content: content.clone(),
        }),
        LogEntry::Assistant {
            content,
            stop_reason,
            model,
            ..
        } => messages.push(Message::Assistant {
            content: content.clone(),
            stop_reason: *stop_reason,
            model: model.clone(),
        }),
        LogEntry::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let block = ContentBlock::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            };
            match messages.last_mut() {
                Some(Message::User { content }) => content.push(block),
                _ => messages.push(Message::User {
                    content: vec![block],
                }),
            }
        }
        LogEntry::Compact {
            replacement_history,
            summary,
            snip_removed_uuids,
            ..
        } => {
            if let Some(replacement_history) = replacement_history {
                *messages = replacement_history.clone();
                // Note: stale usage zeroing is handled at the session level via
                // SessionManager, not at the message projection level.
            } else if let Some(summary) = summary {
                messages.clear();
                if !summary.is_empty() {
                    messages.push(Message::User {
                        content: summary.clone(),
                    });
                }
            }
            // Snip removal metadata stored for future use. Our current Message
            // model doesn't carry per-message UUIDs, so UUID-based filtering is
            // deferred. The snip_removed_uuids field is kept for future
            // per-message UUID tracking.
            let _ = snip_removed_uuids;
        }
    }
}

/// Render a compact, human-readable text view of messages for history search
/// and session previews. This deliberately omits thinking contents.
pub fn render_search_text(messages: &[Message]) -> String {
    let mut out = String::new();
    for message in messages {
        match message {
            Message::User { content } => {
                out.push_str("user: ");
                render_content_blocks(content, &mut out);
            }
            Message::Assistant { content, .. } => {
                out.push_str("assistant: ");
                render_content_blocks(content, &mut out);
            }
            Message::System { content, .. } => {
                out.push_str("system: ");
                out.push_str(content);
                out.push('\n');
            }
        }
    }
    out
}

/// Return a single-line preview from the most recent text-bearing messages.
pub fn preview_messages(messages: &[Message], max_chars: usize) -> String {
    let mut parts = Vec::new();
    for message in messages.iter().rev() {
        let Some(text) = first_text(message) else {
            continue;
        };
        let normalized = normalize_ws(text);
        if !normalized.is_empty() {
            parts.push(normalized);
        }
        if parts.len() >= 2 {
            break;
        }
    }
    parts.reverse();
    truncate_chars(&parts.join(" | "), max_chars)
}

pub fn messages_match_query(messages: &[Message], query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    render_search_text(messages).to_lowercase().contains(&q)
}

fn render_content_blocks(content: &[ContentBlock], out: &mut String) {
    for block in content {
        match block {
            ContentBlock::Text { text, .. } => {
                out.push_str(text);
                out.push('\n');
            }
            ContentBlock::Image { .. } => out.push_str("[image]\n"),
            ContentBlock::ToolUse { name, input, .. } => {
                out.push_str("[tool ");
                out.push_str(name);
                out.push_str("] ");
                out.push_str(&serde_json::to_string(input).unwrap_or_default());
                out.push('\n');
            }
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                if *is_error {
                    out.push_str("[tool result error] ");
                } else {
                    out.push_str("[tool result] ");
                }
                render_tool_result_content(content, out);
                out.push('\n');
            }
            ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => {}
            ContentBlock::CacheEdits { .. } => {} // not rendered in transcript
        }
    }
}

fn render_tool_result_content(content: &ToolResultContent, out: &mut String) {
    match content {
        ToolResultContent::Text(text) => out.push_str(text),
        ToolResultContent::Blocks(blocks) => render_content_blocks(blocks, out),
    }
}

fn first_text(message: &Message) -> Option<&str> {
    let content = match message {
        Message::User { content } | Message::Assistant { content, .. } => content,
        Message::System { content, .. } => return Some(content.as_str()),
    };
    content.iter().find_map(|block| match block {
        ContentBlock::Text { text, .. } => Some(text.as_str()),
        ContentBlock::ToolUse { name, .. } => Some(name.as_str()),
        ContentBlock::ToolResult { content, .. } => match content {
            ToolResultContent::Text(text) => Some(text.as_str()),
            ToolResultContent::Blocks(blocks) => blocks.iter().find_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            }),
        },
        ContentBlock::Image { .. }
        | ContentBlock::Thinking { .. }
        | ContentBlock::RedactedThinking { .. }
        | ContentBlock::CacheEdits { .. } => None,
    })
}

fn normalize_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EnvelopedEntry;
    use base::id::Id;

    #[test]
    fn compact_resets_view_and_keeps_later_turns() {
        let session = base::session::SessionId::new();
        let entries = vec![
            EnvelopedEntry::new(
                session,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "before".into(),
                        cache_control: None,
                    }],
                },
            ),
            EnvelopedEntry::new(
                session,
                LogEntry::Compact {
                    before_tokens: 100,
                    after_tokens: 40,
                    summary_block_id: Some(Id::new()),
                    replacement_history: Some(vec![Message::User {
                        content: vec![ContentBlock::Text {
                            text: "summary".into(),
                            cache_control: None,
                        }],
                    }]),
                    summary: Some(vec![ContentBlock::Text {
                        text: "summary".into(),
                        cache_control: None,
                    }]),
                    snip_removed_uuids: None,
                },
            ),
            EnvelopedEntry::new(
                session,
                LogEntry::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "after".into(),
                        cache_control: None,
                    }],
                    stop_reason: Some(StopReason::EndTurn),
                    usage: None,
                    model: None,
                },
            ),
        ];
        let projected = project_messages(&entries);
        assert_eq!(projected.len(), 2);
        match &projected[0] {
            Message::User { content } => {
                assert!(
                    matches!(content[0], ContentBlock::Text { ref text, .. } if text == "summary")
                );
            }
            _ => panic!(),
        }
        assert!(matches!(projected[1], Message::Assistant { .. }));
    }

    #[test]
    fn projected_lengths_tracks_the_running_count_including_compact_resets() {
        let session = base::session::SessionId::new();
        let text = |t: &str| ContentBlock::Text {
            text: t.into(),
            cache_control: None,
        };
        let entries = vec![
            // Metadata-only: contributes no message.
            EnvelopedEntry::new(
                session,
                LogEntry::System {
                    subkind: crate::entry::SystemSubkind::Notice,
                    text: "hi".into(),
                },
            ),
            EnvelopedEntry::new(
                session,
                LogEntry::User {
                    content: vec![text("one")],
                },
            ),
            EnvelopedEntry::new(
                session,
                LogEntry::Assistant {
                    content: vec![text("two")],
                    stop_reason: Some(StopReason::EndTurn),
                    usage: None,
                    model: None,
                },
            ),
            // Compaction replaces the whole view with a single message — the
            // running count goes *down* here, which is exactly why callers
            // can't assume monotonicity.
            EnvelopedEntry::new(
                session,
                LogEntry::Compact {
                    before_tokens: 100,
                    after_tokens: 10,
                    summary_block_id: None,
                    replacement_history: Some(vec![Message::User {
                        content: vec![text("summary")],
                    }]),
                    summary: None,
                    snip_removed_uuids: None,
                },
            ),
            EnvelopedEntry::new(
                session,
                LogEntry::User {
                    content: vec![text("three")],
                },
            ),
        ];

        let lengths = projected_lengths(&entries);
        assert_eq!(lengths, vec![0, 1, 2, 1, 2]);
        assert_eq!(
            *lengths.last().unwrap(),
            project_messages(&entries).len(),
            "final running count must agree with a full projection"
        );
    }

    #[test]
    fn projected_lengths_ignores_sidechain_entries() {
        let session = base::session::SessionId::new();
        let text = |t: &str| ContentBlock::Text {
            text: t.into(),
            cache_control: None,
        };
        let entries = vec![
            EnvelopedEntry::new(
                session,
                LogEntry::User {
                    content: vec![text("main")],
                },
            ),
            EnvelopedEntry::new(
                session,
                LogEntry::User {
                    content: vec![text("subagent")],
                },
            )
            .as_sidechain(),
        ];
        assert_eq!(projected_lengths(&entries), vec![1, 1]);
    }

    #[test]
    fn preview_uses_recent_text_and_truncates_safely() {
        let messages = vec![
            Message::User {
                content: vec![ContentBlock::Text {
                    text: "older".into(),
                    cache_control: None,
                }],
            },
            Message::Assistant {
                content: vec![ContentBlock::Text {
                    text: "recent answer with\nspacing".into(),
                    cache_control: None,
                }],
                stop_reason: None,
                model: None,
            },
        ];
        assert_eq!(preview_messages(&messages, 18), "older | recent an…");
        assert!(messages_match_query(&messages, "spacing"));
    }

    #[test]
    fn projected_message_count_can_resume_from_entry_id() {
        let session = base::session::SessionId::new();
        let first = EnvelopedEntry::new(
            session,
            LogEntry::User {
                content: vec![ContentBlock::Text {
                    text: "one".into(),
                    cache_control: None,
                }],
            },
        );
        let first_id = first.id;
        let second = EnvelopedEntry::new(
            session,
            LogEntry::Assistant {
                content: vec![ContentBlock::Text {
                    text: "two".into(),
                    cache_control: None,
                }],
                stop_reason: None,
                usage: None,
                model: None,
            },
        );
        let entries = vec![first, second];

        assert_eq!(
            projected_message_count_through_entry_id(&entries, first_id),
            Some(1)
        );
        assert_eq!(
            latest_projected_message_entry_id(&entries),
            Some(entries[1].id)
        );
    }

    #[test]
    fn sidechain_entries_do_not_pollute_main_projection() {
        let session = base::session::SessionId::new();
        let main = EnvelopedEntry::new(
            session,
            LogEntry::User {
                content: vec![ContentBlock::Text {
                    text: "main".into(),
                    cache_control: None,
                }],
            },
        );
        let side = EnvelopedEntry::new(
            session,
            LogEntry::Assistant {
                content: vec![ContentBlock::Text {
                    text: "side".into(),
                    cache_control: None,
                }],
                stop_reason: Some(StopReason::EndTurn),
                usage: None,
                model: None,
            },
        )
        .as_sidechain();
        let latest_main_id = main.id;
        let entries = vec![main, side];

        let projected = project_messages(&entries);
        assert_eq!(projected.len(), 1);
        assert_eq!(
            latest_projected_message_entry_id(&entries),
            Some(latest_main_id)
        );
        assert_eq!(
            projected_message_count_through_entry_id(&entries, entries[1].id),
            Some(1)
        );
        assert!(!render_search_text(&projected).contains("side"));
    }

    #[test]
    fn compact_summary_fallback_is_replay_boundary() {
        let session = base::session::SessionId::new();
        let entries = vec![
            EnvelopedEntry::new(
                session,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "before".into(),
                        cache_control: None,
                    }],
                },
            ),
            EnvelopedEntry::new(
                session,
                LogEntry::Compact {
                    before_tokens: 100,
                    after_tokens: 20,
                    summary_block_id: None,
                    replacement_history: None,
                    summary: Some(vec![ContentBlock::Text {
                        text: "summary only".into(),
                        cache_control: None,
                    }]),
                    snip_removed_uuids: None,
                },
            ),
            EnvelopedEntry::new(
                session,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "after".into(),
                        cache_control: None,
                    }],
                },
            ),
        ];

        let projected = project_messages(&entries);
        assert_eq!(projected.len(), 2);
        let rendered = render_search_text(&projected);
        assert!(!rendered.contains("before"));
        assert!(rendered.contains("summary only"));
        assert!(rendered.contains("after"));
    }

    #[test]
    fn resume_projection_report_counts_resume_risks() {
        let session = base::session::SessionId::new();
        let entries = vec![
            EnvelopedEntry::new(
                session,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "main".into(),
                        cache_control: None,
                    }],
                },
            ),
            EnvelopedEntry::new(
                session,
                LogEntry::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "side".into(),
                        cache_control: None,
                    }],
                    stop_reason: None,
                    usage: None,
                    model: None,
                },
            )
            .as_sidechain(),
            EnvelopedEntry::new(
                session,
                LogEntry::Compact {
                    before_tokens: 100,
                    after_tokens: 10,
                    summary_block_id: None,
                    replacement_history: None,
                    summary: Some(vec![ContentBlock::Text {
                        text: "summary".into(),
                        cache_control: None,
                    }]),
                    snip_removed_uuids: None,
                },
            ),
        ];

        let report = resume_projection_report(&entries);
        assert_eq!(report.entry_count, 3);
        assert_eq!(report.projected_message_count, 1);
        assert_eq!(report.compact_boundary_count, 1);
        assert_eq!(report.summary_fallback_compact_count, 1);
        assert_eq!(report.sidechain_entry_count, 1);
        assert_eq!(report.warning, None);
    }

    #[test]
    fn resume_projection_report_warns_on_empty_compact_boundary() {
        let session = base::session::SessionId::new();
        let entries = vec![
            EnvelopedEntry::new(
                session,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "before".into(),
                        cache_control: None,
                    }],
                },
            ),
            EnvelopedEntry::new(
                session,
                LogEntry::Compact {
                    before_tokens: 100,
                    after_tokens: 0,
                    summary_block_id: None,
                    replacement_history: None,
                    summary: None,
                    snip_removed_uuids: None,
                },
            ),
        ];

        let report = resume_projection_report(&entries);
        assert_eq!(
            report.warning,
            Some(ResumeProjectionWarning::EmptyCompactBoundary)
        );
        assert_eq!(report.compact_boundary_count, 1);
    }
}

#[cfg(test)]
mod extension_entry_tests {
    use super::*;
    use crate::entry::LogEntry;
    use base::message::ContentBlock;
    use base::session::SessionId;

    fn text(t: &str) -> LogEntry {
        LogEntry::User {
            content: vec![ContentBlock::Text {
                text: t.into(),
                cache_control: None,
            }],
        }
    }

    fn ext(ns: &str) -> LogEntry {
        LogEntry::Extension {
            ns: ns.into(),
            event: "checkpoint".into(),
            payload: serde_json::json!({"step": 3}),
        }
    }

    /// The invariant that makes uninstalling a plugin safe: its entries are
    /// still in the log, and the conversation reads exactly as it did.
    #[test]
    fn entries_from_an_unknown_namespace_change_nothing_the_model_sees() {
        let s = SessionId::new();
        let without: Vec<_> = [text("one"), text("two")]
            .into_iter()
            .map(|e| EnvelopedEntry::new(s, e))
            .collect();
        let with: Vec<_> = [
            ext("com.example.gone"),
            text("one"),
            ext("com.example.gone"),
            text("two"),
            ext("com.example.gone"),
        ]
        .into_iter()
        .map(|e| EnvelopedEntry::new(s, e))
        .collect();

        assert_eq!(
            project_messages(&with).len(),
            project_messages(&without).len(),
            "an extension's state is not a message"
        );
        assert_eq!(
            latest_projected_message_entry_id(&with),
            with.iter()
                .rev()
                .find(|e| matches!(e.entry, LogEntry::User { .. }))
                .map(|e| e.id),
            "the last model-visible entry must not become an extension's"
        );
    }

    /// Fork copies entries forward by cloning and re-appending them; an entry
    /// the kernel cannot read must still come out the far side byte for byte,
    /// or forking a session would quietly strip an extension's state.
    #[test]
    fn an_extension_entry_survives_being_copied_forward() {
        let original = ext("com.example.gone");
        let line = serde_json::to_string(&original).unwrap();
        let parsed: LogEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(serde_json::to_string(&parsed).unwrap(), line);

        let LogEntry::Extension { ns, event, payload } = parsed else {
            panic!("expected an extension entry");
        };
        assert_eq!(ns, "com.example.gone");
        assert_eq!(event, "checkpoint");
        assert_eq!(payload, serde_json::json!({"step": 3}));
    }

    #[test]
    fn resume_counts_an_extension_entry_without_warning_about_it() {
        let s = SessionId::new();
        let entries: Vec<_> = [text("one"), ext("com.example.gone")]
            .into_iter()
            .map(|e| EnvelopedEntry::new(s, e))
            .collect();
        let report = resume_projection_report(&entries);
        assert_eq!(report.entry_count, 2);
        assert_eq!(report.projected_message_count, 1);
        assert_eq!(report.metadata_entry_count, 1);
        assert!(report.warning.is_none(), "{:?}", report.warning);
    }
}
