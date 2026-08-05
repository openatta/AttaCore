//! SessionManager — in-memory conversation state backed by history crate for persistence.
//!
//! Owns the active message buffer and turn counter. Persistence (JSONL append/load,
//! session listing) is delegated to `history::store::HistoryStore`.

use base::interface::model::ModelMessage;
use base::session::SessionId;
use crate::session_memory::SessionMemory;
use history::store::HistoryStore;
pub use history::store::SessionSummary;
use std::sync::Arc;
use std::time::Instant;

/// Session manager. Owns the conversation state; delegates persistence to `HistoryStore`.
pub struct SessionManager {
    pub messages: Vec<ModelMessage>,
    /// Per-message wall-clock timestamps (parallel to `messages`).
    /// Used by time-based micro-compaction to determine message age.
    /// Not serialized — ephemeral for the current session only.
    pub message_timestamps: Vec<Instant>,
    pub turn_count: u32,
    pub session_id: String,
    /// Backing store for JSONL persistence (canonical: `history::JsonlHistoryStore`).
    /// When `None`, persist/resume/list are no-ops.
    history_store: Option<Arc<dyn HistoryStore>>,
    /// Auto-maintenance handle for the `session_memory.md` sidecar file.
    /// When `Some`, the runtime may check staleness and prompt the model to
    /// update its cross-session notes.
    pub session_memory: Option<SessionMemory>,
    /// The session ID that spawned this session (parent-child relationship).
    /// Set when a sub-agent or forked conversation is created from an existing session.
    parent_session_id: Option<String>,
    /// Index into `messages` up to which entries have already been appended
    /// to `history_store` — `persist()` only writes `messages[persisted_up_to..]`
    /// each time it's called, so repeated calls (once per turn) don't
    /// re-append the same messages.
    persisted_up_to: usize,
}

impl SessionManager {
    /// Create a new session manager.
    /// `history_store`: backing store for persistence. None = no persistence.
    /// `session_id`: optional pre-set ID; if None, a new UUID is generated.
    /// `parent_session_id`: optional ID of the session that spawned this one.
    pub fn new(
        history_store: Option<Arc<dyn HistoryStore>>,
        session_id: Option<String>,
        parent_session_id: Option<String>,
    ) -> Self {
        let session_id = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        Self {
            messages: Vec::new(),
            message_timestamps: Vec::new(),
            turn_count: 0,
            session_id,
            history_store,
            session_memory: None,
            parent_session_id,
            persisted_up_to: 0,
        }
    }

    /// Convenience: create a no-persistence session manager.
    pub fn in_memory(session_id: Option<String>) -> Self {
        Self::new(None, session_id, None)
    }

    /// Attach a SessionMemory handle. The underlying file is not created until
    /// [`SessionMemory::init_session_memory`] is called explicitly.
    pub fn with_session_memory(mut self, sm: SessionMemory) -> Self {
        self.session_memory = Some(sm);
        self
    }

    // ── In-memory state (Engine uses these directly) ──

    pub fn push_message(&mut self, msg: ModelMessage) {
        self.message_timestamps.push(Instant::now());
        self.messages.push(msg);
    }

    pub fn messages(&self) -> &[ModelMessage] {
        &self.messages
    }

    pub fn token_count(&self) -> usize {
        self.messages
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .map(|b| match b {
                        base::interface::model::ModelContentBlock::Text { text } => {
                            text.len() / 4
                        }
                        _ => 50,
                    })
                    .sum::<usize>()
            })
            .sum()
    }

    pub fn increment_turn(&mut self) {
        self.turn_count += 1;
    }

    /// Return a vector of (message_index, created_at) pairs for time-based
    /// micro-compaction. Messages without a tracked timestamp get `Instant::now()`
    /// as fallback (effectively treating them as fresh).
    pub fn message_ages(&self) -> Vec<(usize, Instant)> {
        self.messages
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let ts = self
                    .message_timestamps
                    .get(i)
                    .copied()
                    .unwrap_or(Instant::now());
                (i, ts)
            })
            .collect()
    }

    // ── Persistence (delegated to history store) ──

    /// Persist messages pushed since the last `persist()` call via the
    /// history store — incremental append, not a full snapshot. Called once
    /// per turn (see `runtime::turn::run_user_turn`). No-op when no store is
    /// configured, or when nothing new has been pushed since the last call.
    ///
    /// `MessageRole::System` entries are skipped (`ModelMessage` rarely if
    /// ever carries this role — the system prompt is assembled separately
    /// per-turn, not stored as a session message — but this keeps `persist()`
    /// total rather than panicking if one ever does appear).
    pub async fn persist(&mut self) -> Result<(), SessionError> {
        let Some(store) = self.history_store.clone() else {
            return Ok(());
        };
        let sid = SessionId::parse(&self.session_id).map_err(|e| SessionError::Id(e.to_string()))?;
        while self.persisted_up_to < self.messages.len() {
            let msg = &self.messages[self.persisted_up_to];
            if let Some(entry) = model_message_to_log_entry(msg) {
                store
                    .append(sid, entry)
                    .await
                    .map_err(|e| SessionError::Store(e.to_string()))?;
            }
            self.persisted_up_to += 1;
        }
        Ok(())
    }

    /// Resume a session by loading messages from the history store and
    /// reconstructing in-memory `ModelMessage`s from the projected transcript
    /// (via `history::transcript::project_messages`, the same projection
    /// `HistoryStore::load_messages` uses). On success, extracts the
    /// `parent_session_id` from the session's Meta entry and marks all
    /// reconstructed messages as already-persisted (so a subsequent
    /// `persist()` call doesn't re-append them).
    pub async fn resume(&mut self, id: &str) -> Result<(), SessionError> {
        let store = match &self.history_store {
            Some(s) => s.clone(),
            None => return Err(SessionError::NotFound(id.to_string())),
        };
        let sid = SessionId::parse(id).map_err(|e| SessionError::Id(e.to_string()))?;
        let entries = store.load(sid).await.map_err(|e| match e {
            history::error::HistoryError::SessionNotFound(id) => SessionError::NotFound(id),
            other => SessionError::Store(other.to_string()),
        })?;
        if entries.is_empty() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        self.session_id = id.to_string();
        self.turn_count = entries.len() as u32;

        // Extract parent_session_id from the Meta entry if present.
        for entry in &entries {
            if let history::entry::LogEntry::Meta { parent_session_id, .. } = &entry.entry {
                self.parent_session_id = parent_session_id.clone();
                break;
            }
        }

        let projected = history::transcript::project_messages(&entries);
        self.messages = projected.iter().filter_map(message_to_model_message).collect();
        self.message_timestamps = vec![Instant::now(); self.messages.len()];
        self.persisted_up_to = self.messages.len();

        Ok(())
    }

    /// Access the session ID (read-only).
    pub fn session_id_str(&self) -> &str {
        &self.session_id
    }

    /// Set/switch the current session ID.
    pub fn set_session_id(&mut self, id: String) {
        self.session_id = id;
    }

    /// Clear all messages and reset turn counter (for `/clear` command).
    pub fn clear(&mut self) {
        self.messages.clear();
        self.message_timestamps.clear();
        self.turn_count = 0;
    }

    // ── Parent session tracking ──

    /// Set the session ID of the parent session that spawned this one.
    pub fn set_parent_session(&mut self, parent_id: String) {
        self.parent_session_id = Some(parent_id);
    }

    /// Return the parent session ID, if set.
    pub fn parent_session_id(&self) -> Option<&str> {
        self.parent_session_id.as_deref()
    }

    /// List all child sessions (sessions whose Meta entry has `parent_session_id`
    /// pointing to this session). Delegates to the HistoryStore.
    pub async fn child_sessions(&self) -> Result<Vec<String>, SessionError> {
        let store = match &self.history_store {
            Some(s) => s,
            None => return Ok(vec![]),
        };
        let children = store
            .child_sessions(&self.session_id)
            .await
            .map_err(|e| SessionError::Store(e.to_string()))?;
        Ok(children.into_iter().map(|s| s.to_string()).collect())
    }

    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            session_id: SessionId::parse(&self.session_id).unwrap_or_default(),
            last_modified: String::new(),
            entry_count: self.messages.len(),
            message_count: self.messages.len(),
            preview: String::new(),
            canonical_cwd: None,
            title: None,
            total_input_tokens: None,
            total_output_tokens: None,
            compact_count: 0,
        }
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, SessionError> {
        let store = match &self.history_store {
            Some(s) => s,
            None => return Ok(vec![]),
        };
        let sids = store.list_sessions().await.map_err(|e| SessionError::Store(e.to_string()))?;
        // For each session, load a summary — simplified for now
        let mut out = Vec::new();
        for sid in sids {
            out.push(SessionSummary {
                session_id: sid,
                last_modified: String::new(),
                entry_count: 0,
                message_count: 0,
                preview: String::new(),
                canonical_cwd: None,
                title: None,
                total_input_tokens: None,
                total_output_tokens: None,
                compact_count: 0,
            });
        }
        Ok(out)
    }

    /// 从 HistoryStore 中删除指定 session 的全部持久化数据。
    /// 库模式下由用户自行管理 Agent 实例生命周期；此方法仅操作磁盘。
    pub async fn delete_session(&self, id: &str) -> Result<(), SessionError> {
        let store = match &self.history_store {
            Some(s) => s,
            None => return Err(SessionError::NotFound(id.to_string())),
        };
        let sid = SessionId::parse(id).map_err(|e| SessionError::Id(e.to_string()))?;
        store.delete(sid).await.map_err(|e| SessionError::Store(e.to_string()))
    }
}

// ── ModelMessage <-> history::LogEntry/Message conversion ──
//
// Two independent, block-level conversions (write direction and read
// direction) rather than a single bidirectional mapping — `ModelContentBlock`
// (runtime wire format) is a strict subset of `base::message::ContentBlock`
// (storage format, which also covers Image/Thinking/RedactedThinking/CacheEdits
// that never appear in a live `ModelMessage`), so the two directions aren't
// symmetric and are clearer written separately.

fn model_content_block_to_content_block(
    b: &base::interface::model::ModelContentBlock,
) -> base::message::ContentBlock {
    use base::interface::model::ModelContentBlock as M;
    use base::message::ContentBlock as C;
    match b {
        M::Text { text } => C::Text {
            text: text.clone(),
            cache_control: None,
        },
        M::ToolUse { id, name, input } => C::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
        M::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => C::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: base::message::ToolResultContent::Text(content.clone()),
            is_error: is_error.unwrap_or(false),
        },
    }
}

/// `None` for message roles/content blocks with no `LogEntry` equivalent —
/// currently only `MessageRole::System` (see `persist()` doc comment).
fn model_message_to_log_entry(msg: &ModelMessage) -> Option<history::entry::LogEntry> {
    use base::interface::model::MessageRole;
    let content: Vec<base::message::ContentBlock> = msg
        .content
        .iter()
        .map(model_content_block_to_content_block)
        .collect();
    match msg.role {
        MessageRole::User => Some(history::entry::LogEntry::User { content }),
        MessageRole::Assistant => Some(history::entry::LogEntry::Assistant {
            content,
            stop_reason: None,
            usage: None,
            model: None,
        }),
        MessageRole::System => None,
    }
}

fn content_block_to_model_content_block(
    b: &base::message::ContentBlock,
) -> Option<base::interface::model::ModelContentBlock> {
    use base::interface::model::ModelContentBlock as M;
    use base::message::ContentBlock as C;
    match b {
        C::Text { text, .. } => Some(M::Text { text: text.clone() }),
        C::ToolUse { id, name, input } => Some(M::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        }),
        C::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let content_str = match content {
                base::message::ToolResultContent::Text(s) => s.clone(),
                base::message::ToolResultContent::Blocks(blocks) => {
                    serde_json::to_string(blocks).unwrap_or_default()
                }
            };
            Some(M::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content_str,
                is_error: Some(*is_error),
            })
        }
        // Image/Thinking/RedactedThinking/CacheEdits have no `ModelContentBlock`
        // equivalent (not part of the live model-facing wire format) — drop.
        _ => None,
    }
}

/// `None` for `Message::System` — matches `model_message_to_log_entry`'s
/// symmetric skip on the write side.
fn message_to_model_message(msg: &base::message::Message) -> Option<ModelMessage> {
    use base::interface::model::MessageRole;
    match msg {
        base::message::Message::User { content } => Some(ModelMessage {
            role: MessageRole::User,
            content: content
                .iter()
                .filter_map(content_block_to_model_content_block)
                .collect(),
        }),
        base::message::Message::Assistant { content, .. } => Some(ModelMessage {
            role: MessageRole::Assistant,
            content: content
                .iter()
                .filter_map(content_block_to_model_content_block)
                .collect(),
        }),
        base::message::Message::System { .. } => None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("store error: {0}")]
    Store(String),
    #[error("invalid id: {0}")]
    Id(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::{ModelContentBlock, MessageRole};

    fn make_text_msg(text: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: text.into(),
            }],
        }
    }

    #[test]
    fn new_session_starts_empty() {
        let mgr = SessionManager::in_memory(None);
        assert_eq!(mgr.messages.len(), 0);
        assert_eq!(mgr.turn_count, 0);
        assert!(!mgr.session_id.is_empty());
    }

    #[test]
    fn new_with_session_id_preserves_it() {
        let mgr = SessionManager::in_memory(Some("test-session-1".into()));
        assert_eq!(mgr.session_id, "test-session-1");
    }

    #[test]
    fn push_message_appends() {
        let mut mgr = SessionManager::in_memory(None);
        mgr.push_message(make_text_msg("hello"));
        assert_eq!(mgr.messages.len(), 1);
        mgr.push_message(make_text_msg("world"));
        assert_eq!(mgr.messages.len(), 2);
    }

    #[test]
    fn messages_method_returns_all() {
        let mut mgr = SessionManager::in_memory(None);
        mgr.push_message(make_text_msg("a"));
        mgr.push_message(make_text_msg("b"));
        assert_eq!(mgr.messages().len(), 2);
    }

    #[test]
    fn increment_turn_monotonic() {
        let mut mgr = SessionManager::in_memory(None);
        assert_eq!(mgr.turn_count, 0);
        mgr.increment_turn();
        assert_eq!(mgr.turn_count, 1);
        mgr.increment_turn();
        assert_eq!(mgr.turn_count, 2);
    }

    #[test]
    fn token_count_empty_is_zero() {
        let mgr = SessionManager::in_memory(None);
        assert_eq!(mgr.token_count(), 0);
    }

    #[test]
    fn token_count_approximates_text_length() {
        let mut mgr = SessionManager::in_memory(None);
        // 40 chars → 40/4 = 10 tokens
        mgr.push_message(make_text_msg("hello world this is a test message here"));
        assert!(mgr.token_count() > 0);
    }

    #[test]
    fn summary_reflects_state() {
        let mut mgr = SessionManager::in_memory(Some("summary-test".into()));
        mgr.push_message(make_text_msg("hi"));
        let s = mgr.summary();
        assert_eq!(s.entry_count, 1);
        assert_eq!(s.message_count, 1);
    }

    #[test]
    fn set_session_id_updates() {
        let mut mgr = SessionManager::in_memory(None);
        mgr.set_session_id("new-id".into());
        assert_eq!(mgr.session_id_str(), "new-id");
    }

    #[tokio::test]
    async fn persist_without_store_is_noop() {
        let mut mgr = SessionManager::in_memory(None);
        let r = mgr.persist().await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn resume_without_store_returns_not_found() {
        let mut mgr = SessionManager::in_memory(None);
        let r = mgr.resume("nonexistent").await;
        assert!(r.is_err());
        match r {
            Err(SessionError::NotFound(_)) => {}
            _ => panic!("expected NotFound"),
        }
    }

    #[tokio::test]
    async fn list_sessions_without_store_returns_empty() {
        let mgr = SessionManager::in_memory(None);
        let sessions = mgr.list_sessions().await.unwrap();
        assert!(sessions.is_empty());
    }

    async fn real_store(tmp: &std::path::Path) -> Arc<dyn HistoryStore> {
        let cwd = tmp.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let projects_root = tmp.join("projects");
        Arc::new(
            history::store::JsonlHistoryStore::with_root(&cwd, projects_root)
                .await
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn persist_writes_new_messages_and_does_not_reappend_on_second_call() {
        let tmp = tempfile::tempdir().unwrap();
        let store = real_store(tmp.path()).await;
        let sid = SessionId::new();
        let mut mgr = SessionManager::new(Some(store.clone()), Some(sid.to_string()), None);

        mgr.push_message(make_text_msg("hello"));
        mgr.persist().await.unwrap();

        let entries = store.load(sid).await.unwrap();
        assert_eq!(entries.len(), 1, "one message should have been appended");

        // Calling persist() again with no new messages must not duplicate.
        mgr.persist().await.unwrap();
        let entries = store.load(sid).await.unwrap();
        assert_eq!(entries.len(), 1, "persist() must not re-append already-persisted messages");

        // Pushing one more message and persisting appends exactly one more entry.
        mgr.push_message(make_text_msg("second message"));
        mgr.persist().await.unwrap();
        let entries = store.load(sid).await.unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn resume_reconstructs_messages_and_marks_them_already_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let store = real_store(tmp.path()).await;
        let sid = SessionId::new();

        // Write a session with one writer...
        {
            let mut writer = SessionManager::new(Some(store.clone()), Some(sid.to_string()), None);
            writer.push_message(make_text_msg("first"));
            writer.persist().await.unwrap();
        }

        // ...and reconstruct it with a fresh SessionManager via resume().
        let mut reader = SessionManager::new(Some(store.clone()), None, None);
        reader.resume(&sid.to_string()).await.unwrap();
        assert_eq!(reader.messages().len(), 1);
        match &reader.messages()[0].content[0] {
            ModelContentBlock::Text { text } => assert_eq!(text, "first"),
            other => panic!("unexpected content block: {other:?}"),
        }

        // resume() must mark reconstructed messages as already-persisted so a
        // subsequent persist() doesn't re-append them.
        reader.persist().await.unwrap();
        let entries = store.load(sid).await.unwrap();
        assert_eq!(entries.len(), 1, "resume()'d messages must not be re-persisted");
    }

    #[tokio::test]
    async fn resume_missing_session_returns_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let store = real_store(tmp.path()).await;
        let mut mgr = SessionManager::new(Some(store), None, None);
        let err = mgr.resume(&SessionId::new().to_string()).await.unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }
}
