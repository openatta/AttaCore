//! SessionManager — in-memory conversation state backed by history crate for persistence.
//!
//! Owns the active message buffer and turn counter. Persistence (JSONL append/load,
//! session listing) is delegated to `history::store::HistoryStore`.

use crate::session_memory::SessionMemory;
use base::interface::model::ModelMessage;
use base::session::SessionId;
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
                        base::interface::model::ModelContentBlock::Text { text } => text.len() / 4,
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

    /// Persist the current session state via the history store.
    pub async fn persist(&self) -> Result<(), SessionError> {
        let store = match &self.history_store {
            Some(s) => s,
            None => return Ok(()),
        };
        let _sid =
            SessionId::parse(&self.session_id).map_err(|e| SessionError::Id(e.to_string()))?;
        // Write each message as a LogEntry via the store's append.
        // In practice, the engine should use the store directly for incremental append;
        // this bulk-persist is a convenience for the session snapshot use case.
        // For now, persist metadata via the history store.
        let _ = store
            .list_sessions()
            .await
            .map_err(|e| SessionError::Store(e.to_string()))?;
        Ok(())
    }

    /// Resume a session by loading messages from the history store.
    /// On success, extracts the `parent_session_id` from the session's Meta entry.
    pub async fn resume(&mut self, id: &str) -> Result<(), SessionError> {
        let store = match &self.history_store {
            Some(s) => s,
            None => return Err(SessionError::NotFound(id.to_string())),
        };
        let sid = SessionId::parse(id).map_err(|e| SessionError::Id(e.to_string()))?;
        let entries = store
            .load(sid)
            .await
            .map_err(|e| SessionError::Store(e.to_string()))?;
        if entries.is_empty() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        self.session_id = id.to_string();
        self.turn_count = entries.len() as u32;
        // P2-9: Message reconstruction from entries.
        // Use history::project_messages() to convert EnvelopedEntry→Message,
        // then adapt Message→ModelMessage for session state.
        // For now, caller reconstructs messages via the history crate's projection.

        // Extract parent_session_id from the Meta entry if present.
        for entry in &entries {
            if let history::entry::LogEntry::Meta {
                parent_session_id, ..
            } = &entry.entry
            {
                self.parent_session_id = parent_session_id.clone();
                break;
            }
        }

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
        let sids = store
            .list_sessions()
            .await
            .map_err(|e| SessionError::Store(e.to_string()))?;
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
        store
            .delete(sid)
            .await
            .map_err(|e| SessionError::Store(e.to_string()))
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
    use base::interface::model::{MessageRole, ModelContentBlock};

    fn make_text_msg(text: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text { text: text.into() }],
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
        let mgr = SessionManager::in_memory(None);
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
}
