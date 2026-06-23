//! `JsonlHistoryStore` —— 写一个会话目录下的 jsonl 文件。
//!
//! 设计：每个 store 实例绑定**一个 cwd**。append/load/list 都对该 cwd 的项目目录。
//! 多 session 共享一份 store；同 session 串行写（mutex 保护，避免并发 partial line）。
//!
//! 见 docs/DATA_FORMATS.md §A。

use crate::entry::{EnvelopedEntry, LogEntry, PasteStore};
use crate::error::HistoryError;
use crate::path::{
    canonicalize_cwd, project_dir, projects_root, session_file, session_metadata_file,
    sessions_root,
};
use crate::project::SessionMetadata;
use crate::transcript::{messages_match_query, preview_messages, project_messages};
use async_trait::async_trait;
use base::session::SessionId;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub last_modified: String,
    pub entry_count: usize,
    pub message_count: usize,
    pub preview: String,
    pub canonical_cwd: Option<String>,
    pub title: Option<String>,
    pub total_input_tokens: Option<u64>,
    pub total_output_tokens: Option<u64>,
    pub compact_count: u64,
}

/// 持久化抽象。唯一实现是 jsonl；可挂内存版供测试。
#[async_trait]
pub trait HistoryStore: Send + Sync {
    async fn append(&self, session: SessionId, entry: LogEntry) -> Result<(), HistoryError>;
    async fn load(&self, session: SessionId) -> Result<Vec<EnvelopedEntry>, HistoryError>;
    async fn list_sessions(&self) -> Result<Vec<SessionId>, HistoryError>;

    /// 删除指定 session 的全部持久化数据（jsonl + metadata）。
    async fn delete(&self, session: SessionId) -> Result<(), HistoryError>;

    /// 把 jsonl 解析成可直接喂 Engine 的 `Vec<Message>`。默认实现走 `load()`。
    ///
    /// 规则：
    /// - User / Assistant 直接对应；
    /// - 连续多条 ToolResult 合并到单条 Message::User（API 要求 user 消息
    ///   一次带齐所有 tool_result 块）；
    /// - Meta / System / Compact / UsageSnapshot 不进 API。
    async fn load_messages(
        &self,
        session: SessionId,
    ) -> Result<Vec<base::message::Message>, HistoryError> {
        let entries = self.load(session).await?;
        Ok(project_messages(&entries))
    }

    /// List all child sessions whose `LogEntry::Meta` has `parent_session_id` matching
    /// the given value. Scans every session file; O(n) on the number of sessions.
    async fn child_sessions(&self, parent_session_id: &str) -> Result<Vec<SessionId>, HistoryError> {
        let all = self.list_sessions().await?;
        let mut out = Vec::new();
        for sid in all {
            let entries = match self.load(sid).await {
                Ok(e) => e,
                Err(HistoryError::SessionNotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            for entry in &entries {
                if let LogEntry::Meta { parent_session_id: Some(pid), .. } = &entry.entry {
                    if pid == parent_session_id {
                        out.push(entry.session_id);
                        break;
                    }
                }
            }
        }
        Ok(out)
    }
}

/// 把 jsonl 落到 `<projects_root>/<sanitize(cwd)>/<session>.jsonl`。
pub struct JsonlHistoryStore {
    projects_root: PathBuf,
    canonical_cwd: PathBuf,
    /// 序列化 append 调用，避免并发 partial-line 写。
    /// 不是 RwLock —— append 是写操作，读不互让有意义。
    append_lock: Arc<Mutex<()>>,
    /// Optional external content store for deduplicating large content
    /// in the JSONL (see `PasteStore` and `LogEntry::PasteRef`).
    paste_store: Option<PasteStore>,
}

impl JsonlHistoryStore {
    /// 默认指向 `~/.atta/code/projects`。
    pub async fn new(cwd: &Path) -> Result<Self, HistoryError> {
        let root = projects_root()?;
        Self::with_root(cwd, root).await
    }

    /// 自定义 projects_root —— 测试与企业部署用。
    pub async fn with_root(cwd: &Path, projects_root: PathBuf) -> Result<Self, HistoryError> {
        let canonical = canonicalize_cwd(cwd).await?;
        Ok(Self {
            projects_root,
            canonical_cwd: canonical,
            append_lock: Arc::new(Mutex::new(())),
            paste_store: None,
        })
    }

    /// Attach a [`PasteStore`] to this history store. When configured, large
    /// User/Assistant entries (>1024 bytes of serialized content) are stored
    /// externally and replaced with a `PasteRef` entry in the JSONL. On load,
    /// `PasteRef` entries are transparently hydrated back to their original
    /// variant.
    pub fn with_paste_store(mut self, paste_store: PasteStore) -> Self {
        self.paste_store = Some(paste_store);
        self
    }

    pub fn project_dir_path(&self) -> PathBuf {
        project_dir(&self.projects_root, &self.canonical_cwd)
    }

    pub fn session_file_path(&self, session: &SessionId) -> PathBuf {
        session_file(&self.project_dir_path(), session)
    }

    pub fn canonical_cwd(&self) -> &Path {
        &self.canonical_cwd
    }

    /// 列出当前 project 目录下最近 N 个 session 的 (id, last_modified) 元组。
    /// 按修改时间倒序（最新先）。给 `/resume` slash 列最近会话用。
    pub async fn list_recent_sessions(
        &self,
        max: usize,
    ) -> Result<Vec<(SessionId, String)>, HistoryError> {
        Ok(self
            .session_files_by_mtime(max)
            .await?
            .into_iter()
            .map(|(id, _, mtime)| (id, format_mtime(mtime)))
            .collect())
    }

    /// Return the `max` most-recently-modified session summaries for the
    /// current project directory, newest first. Skips sessions that lack
    /// metadata or whose jsonl store is corrupt.
    pub async fn list_recent_session_summaries(
        &self,
        max: usize,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        let files = self.session_files_by_mtime(max).await?;
        let mut out = Vec::new();
        for (session_id, _path, mtime) in files {
            if let Some(summary) = self.session_summary(session_id, mtime).await? {
                out.push(summary);
            }
        }
        Ok(out)
    }

    /// Search session summaries in the current project directory by matching
    /// `query` against session IDs and message content. Falls back to
    /// [`list_recent_session_summaries`] when the query is empty.
    pub async fn search_session_summaries(
        &self,
        query: &str,
        max: usize,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        let query = query.trim();
        if query.is_empty() {
            return self.list_recent_session_summaries(max).await;
        }

        let files = self.session_files_by_mtime(usize::MAX).await?;
        let mut out = Vec::new();
        for (session_id, _path, mtime) in files {
            let entries = match self.load(session_id).await {
                Ok(entries) => entries,
                Err(HistoryError::SessionNotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            let messages = project_messages(&entries);
            if session_id.to_string().contains(query) || messages_match_query(&messages, query) {
                let metadata = load_session_metadata(session_id).await;
                out.push(self.summary_from_parts(
                    session_id,
                    mtime,
                    entries.len(),
                    &messages,
                    metadata,
                ));
            }
            if out.len() >= max {
                break;
            }
        }
        Ok(out)
    }

    /// Search session summaries across **all** project directories under the
    /// history root, filtering by session ID and message content. Used by
    /// `/resume @all <query>`.
    pub async fn search_all_project_session_summaries(
        &self,
        query: &str,
        max: usize,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        self.search_project_dirs(query, max, ProjectDirFilter::All)
            .await
    }

    /// Search session summaries in project directories that share the same
    /// git repository as the current working directory. Useful when a
    /// monorepo has multiple project subdirectories with their own history.
    /// Used by `/resume @repo <query>`.
    pub async fn search_same_repo_session_summaries(
        &self,
        query: &str,
        max: usize,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        let root = repo_root_or_cwd(&self.canonical_cwd).await;
        self.search_project_dirs(query, max, ProjectDirFilter::UnderPath(root))
            .await
    }

    async fn search_project_dirs(
        &self,
        query: &str,
        max: usize,
        filter: ProjectDirFilter,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let query = query.trim().to_lowercase();
        let mut project_dirs = match tokio::fs::read_dir(&self.projects_root).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(HistoryError::Io(e)),
        };
        let mut candidates = Vec::new();
        while let Some(entry) = project_dirs.next_entry().await? {
            let Ok(ft) = entry.file_type().await else {
                continue;
            };
            if !ft.is_dir() {
                continue;
            }
            let mut files = collect_session_files(entry.path()).await?;
            candidates.append(&mut files);
        }
        candidates.sort_by_key(|p| std::cmp::Reverse(p.2));

        let mut out = Vec::new();
        for (session_id, path, mtime) in candidates {
            let entries = load_entries_from_path(&path, session_id).await?;
            let metadata = load_session_metadata(session_id).await;
            if !filter.matches(metadata.as_ref()) {
                continue;
            }
            let messages = project_messages(&entries);
            let haystack = format!(
                "{}\n{}\n{}\n{}",
                session_id,
                metadata
                    .as_ref()
                    .and_then(|m| m.title.as_deref())
                    .unwrap_or_default(),
                metadata
                    .as_ref()
                    .map(|m| m.canonical_cwd.as_str())
                    .unwrap_or_default(),
                crate::transcript::render_search_text(&messages)
            )
            .to_lowercase();
            if query.is_empty() || haystack.contains(&query) {
                out.push(self.summary_from_parts(
                    session_id,
                    mtime,
                    entries.len(),
                    &messages,
                    metadata,
                ));
            }
            if out.len() >= max {
                break;
            }
        }
        Ok(out)
    }

    async fn session_summary(
        &self,
        session_id: SessionId,
        mtime: std::time::SystemTime,
    ) -> Result<Option<SessionSummary>, HistoryError> {
        let entries = match self.load(session_id).await {
            Ok(entries) => entries,
            Err(HistoryError::SessionNotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let messages = project_messages(&entries);
        let metadata = load_session_metadata(session_id).await;
        Ok(Some(self.summary_from_parts(
            session_id,
            mtime,
            entries.len(),
            &messages,
            metadata,
        )))
    }

    fn summary_from_parts(
        &self,
        session_id: SessionId,
        mtime: std::time::SystemTime,
        entry_count: usize,
        messages: &[base::message::Message],
        metadata: Option<SessionMetadata>,
    ) -> SessionSummary {
        SessionSummary {
            session_id,
            last_modified: format_mtime(mtime),
            entry_count,
            message_count: messages.len(),
            preview: metadata
                .as_ref()
                .and_then(|m| m.latest_summary.clone())
                .unwrap_or_else(|| preview_messages(messages, 140)),
            canonical_cwd: metadata.as_ref().map(|m| m.canonical_cwd.clone()),
            title: metadata.as_ref().and_then(|m| m.title.clone()),
            total_input_tokens: metadata.as_ref().and_then(|m| m.total_input_tokens),
            total_output_tokens: metadata.as_ref().and_then(|m| m.total_output_tokens),
            compact_count: metadata.as_ref().map(|m| m.compact_count).unwrap_or(0),
        }
    }

    async fn session_files_by_mtime(
        &self,
        max: usize,
    ) -> Result<Vec<(SessionId, PathBuf, std::time::SystemTime)>, HistoryError> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let dir = self.project_dir_path();
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(_) => return Ok(Vec::new()),
        };
        let mut found: Vec<(SessionId, PathBuf, std::time::SystemTime)> = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            // 文件名 `<id>.jsonl`
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(id) = SessionId::parse(stem) else {
                continue;
            };
            let mtime = match entry.metadata().await {
                Ok(m) => m.modified().unwrap_or(std::time::UNIX_EPOCH),
                Err(_) => std::time::UNIX_EPOCH,
            };
            found.push((id, path, mtime));
        }
        // 倒序按 mtime
        found.sort_by_key(|p| std::cmp::Reverse(p.2));
        found.truncate(max);
        Ok(found)
    }
}

fn format_mtime(t: std::time::SystemTime) -> String {
    let dt = time::OffsetDateTime::from(t);
    dt.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
}

enum ProjectDirFilter {
    All,
    UnderPath(PathBuf),
}

impl ProjectDirFilter {
    fn matches(&self, metadata: Option<&SessionMetadata>) -> bool {
        match self {
            Self::All => true,
            Self::UnderPath(root) => metadata
                .map(|m| Path::new(&m.canonical_cwd).starts_with(root))
                .unwrap_or(false),
        }
    }
}

async fn repo_root_or_cwd(cwd: &Path) -> PathBuf {
    let output = tokio::process::Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .current_dir(cwd)
        .output()
        .await;
    match output {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.is_empty() {
                cwd.to_path_buf()
            } else {
                PathBuf::from(s)
            }
        }
        _ => cwd.to_path_buf(),
    }
}

async fn collect_session_files(
    dir: PathBuf,
) -> Result<Vec<(SessionId, PathBuf, std::time::SystemTime)>, HistoryError> {
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(HistoryError::Io(e)),
    };
    let mut out = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(session_id) = SessionId::parse(stem) else {
            continue;
        };
        let mtime = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::UNIX_EPOCH);
        out.push((session_id, path, mtime));
    }
    Ok(out)
}

async fn load_entries_from_path(
    path: &Path,
    session: SessionId,
) -> Result<Vec<EnvelopedEntry>, HistoryError> {
    let content = match tokio::fs::read_to_string(path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(HistoryError::SessionNotFound(session.to_string()));
        }
        Err(e) => return Err(HistoryError::Io(e)),
    };
    let mut entries = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let env = serde_json::from_str::<EnvelopedEntry>(line)
            .map_err(|error| HistoryError::Parse { line: i + 1, error })?;
        entries.push(env);
    }
    Ok(entries)
}

async fn load_session_metadata(session: SessionId) -> Option<SessionMetadata> {
    let root = sessions_root().ok()?;
    let path = session_metadata_file(&root, &session);
    let content = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str(&content).ok()
}

#[async_trait]
impl HistoryStore for JsonlHistoryStore {
    async fn append(&self, session: SessionId, entry: LogEntry) -> Result<(), HistoryError> {
        let _guard = self.append_lock.lock().await;
        let path = self.session_file_path(&session);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // If a paste store is configured, check if this entry has large
        // content and store it externally as a PasteRef.
        let entry = if let Some(ref store) = self.paste_store {
            maybe_store_entry_content(&entry, store)?
        } else {
            entry
        };

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)?;

        let enveloped = EnvelopedEntry::new(session, entry);
        let line = serde_json::to_string(&enveloped)?;

        // 一次 write_all 把 line + '\n' 一起出，减少 partial line 风险。
        use std::io::Write;
        let mut buf = line.into_bytes();
        buf.push(b'\n');
        f.write_all(&buf)?;
        f.flush()?;
        Ok(())
    }

    async fn load(&self, session: SessionId) -> Result<Vec<EnvelopedEntry>, HistoryError> {
        let path = self.session_file_path(&session);
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(HistoryError::SessionNotFound(session.to_string()));
            }
            Err(e) => return Err(HistoryError::Io(e)),
        };

        let mut entries = Vec::new();
        for (i, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let mut env = serde_json::from_str::<EnvelopedEntry>(line)
                .map_err(|error| HistoryError::Parse { line: i + 1, error })?;

            // Hydrate PasteRef entries if a paste store is configured.
            if let Some(ref store) = self.paste_store {
                hydrate_paste_ref(&mut env, store)?;
            }

            entries.push(env);
        }
        Ok(entries)
    }

    async fn delete(&self, session: SessionId) -> Result<(), HistoryError> {
        let jsonl_path = self.session_file_path(&session);
        // 删除 jsonl 文件
        match tokio::fs::remove_file(&jsonl_path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(HistoryError::Io(e)),
        }
        // 删除 metadata 文件（如果存在）
        if let Ok(root) = sessions_root() {
            let meta_path = session_metadata_file(&root, &session);
            let _ = tokio::fs::remove_file(meta_path).await;
        }
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, HistoryError> {
        let dir = self.project_dir_path();
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(HistoryError::Io(e)),
        };

        let mut out = Vec::new();
        while let Some(ent) = entries.next_entry().await? {
            let name = ent.file_name();
            let Some(s) = name.to_str() else { continue };
            let Some(stem) = s.strip_suffix(".jsonl") else {
                continue;
            };
            if let Ok(id) = SessionId::parse(stem) {
                out.push(id);
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Paste store helpers
// ---------------------------------------------------------------------------

/// If the entry is a `User` or `Assistant` whose serialized content exceeds
/// 1024 bytes, store the full variant JSON in the paste store and return a
/// `PasteRef` replacement. Otherwise return the entry unchanged.
fn maybe_store_entry_content(
    entry: &LogEntry,
    paste_store: &PasteStore,
) -> Result<LogEntry, HistoryError> {
    if !is_content_large(entry) {
        return Ok(entry.clone());
    }

    // Store the full serialized variant (with kind tag, aux fields) so
    // the paste file is self-describing for round-trip hydration.
    let full_variant_json = serde_json::to_string(entry)?;
    let paste_id = paste_store
        .store(&full_variant_json)
        .map_err(HistoryError::Io)?;

    Ok(LogEntry::PasteRef { paste_id })
}

/// If `entry` is a `PasteRef`, load the paste content and replace the entry
/// with the original variant. Otherwise do nothing.
fn hydrate_paste_ref(
    env: &mut EnvelopedEntry,
    paste_store: &PasteStore,
) -> Result<(), HistoryError> {
    let paste_id = match &env.entry {
        LogEntry::PasteRef { paste_id } => paste_id.clone(),
        _ => return Ok(()),
    };

    let paste_json = paste_store
        .load(&paste_id)
        .map_err(HistoryError::Io)?
        .ok_or_else(|| HistoryError::Path(format!("paste file not found: {paste_id}")))?;

    let hydrated: LogEntry = serde_json::from_str(&paste_json)?;
    env.entry = hydrated;
    Ok(())
}

/// Check if entry is a User or Assistant with serialized content > 1024 bytes.
fn is_content_large(entry: &LogEntry) -> bool {
    let content = match entry {
        LogEntry::User { content } => content,
        LogEntry::Assistant { content, .. } => content,
        _ => return false,
    };

    // Measure the JSON size of the content blocks (the dominant term in
    // the serialized line). 1024 bytes is the threshold.
    let Ok(json) = serde_json::to_string(content) else {
        return false;
    };
    json.len() > 1024
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::message::{ContentBlock, StopReason, ToolResultContent};
    use base::permission::PermissionMode;
    use serde_json::json;
    use tempfile::TempDir;
    use time::OffsetDateTime;

    async fn make_store() -> (JsonlHistoryStore, TempDir, TempDir) {
        let cwd_tmp = TempDir::new().unwrap();
        let projects_tmp = TempDir::new().unwrap();
        let store = JsonlHistoryStore::with_root(cwd_tmp.path(), projects_tmp.path().to_path_buf())
            .await
            .unwrap();
        (store, cwd_tmp, projects_tmp)
    }

    #[tokio::test]
    async fn append_creates_file_under_sanitized_dir() {
        let (store, _cwd, _proj) = make_store().await;
        let s = SessionId::new();
        store
            .append(
                s,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "hi".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        let path = store.session_file_path(&s);
        assert!(path.exists());
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.ends_with('\n'));
        assert_eq!(content.lines().count(), 1);
    }

    #[tokio::test]
    async fn append_then_load_roundtrip() {
        let (store, _cwd, _proj) = make_store().await;
        let s = SessionId::new();

        // meta + 3 messages
        store
            .append(
                s,
                LogEntry::Meta {
                    cwd: "/tmp/test".into(),
                    started_at: OffsetDateTime::now_utc(),
                    model: "claude-sonnet-4-6".into(),
                    permission_mode: format!("{:?}", PermissionMode::Default),
                    engine_version: "0.0.1".into(),
                    attacode_version: "0.0.1".into(),
                    parent_session_id: None,
                },
            )
            .await
            .unwrap();

        for txt in &["one", "two", "three"] {
            store
                .append(
                    s,
                    LogEntry::User {
                        content: vec![ContentBlock::Text {
                            text: (*txt).into(),
                            cache_control: None,
                        }],
                    },
                )
                .await
                .unwrap();
        }

        let loaded = store.load(s).await.unwrap();
        assert_eq!(loaded.len(), 4);
        assert!(matches!(loaded[0].entry, LogEntry::Meta { .. }));
        for (env, expected) in loaded.iter().skip(1).zip(["one", "two", "three"].iter()) {
            match &env.entry {
                LogEntry::User { content } => match &content[0] {
                    ContentBlock::Text { text, .. } => assert_eq!(text, expected),
                    _ => panic!(),
                },
                _ => panic!(),
            }
        }
    }

    #[tokio::test]
    async fn load_unknown_session_errors_with_session_not_found() {
        let (store, _cwd, _proj) = make_store().await;
        let s = SessionId::new();
        let err = store.load(s).await.unwrap_err();
        assert!(matches!(err, HistoryError::SessionNotFound(_)));
    }

    #[tokio::test]
    async fn list_sessions_returns_existing_files() {
        let (store, _cwd, _proj) = make_store().await;
        let a = SessionId::new();
        let b = SessionId::new();
        for s in [a, b] {
            store
                .append(
                    s,
                    LogEntry::User {
                        content: vec![ContentBlock::Text {
                            text: "x".into(),
                            cache_control: None,
                        }],
                    },
                )
                .await
                .unwrap();
        }
        let mut listed = store.list_sessions().await.unwrap();
        listed.sort_by_key(|s| s.to_string());
        let mut expected = vec![a, b];
        expected.sort_by_key(|s| s.to_string());
        assert_eq!(listed, expected);
    }

    #[tokio::test]
    async fn recent_session_summaries_include_counts_and_preview() {
        let (store, _cwd, _proj) = make_store().await;
        let s = SessionId::new();
        store
            .append(
                s,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "first request".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();
        store
            .append(
                s,
                LogEntry::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "useful answer".into(),
                        cache_control: None,
                    }],
                    stop_reason: None,
                    usage: None,
                    model: None,
                },
            )
            .await
            .unwrap();

        let summaries = store.list_recent_session_summaries(5).await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].session_id, s);
        assert_eq!(summaries[0].entry_count, 2);
        assert_eq!(summaries[0].message_count, 2);
        assert!(summaries[0].preview.contains("useful answer"));
    }

    #[tokio::test]
    async fn search_session_summaries_matches_transcript_text_and_id() {
        let (store, _cwd, _proj) = make_store().await;
        let matching = SessionId::new();
        let other = SessionId::new();
        store
            .append(
                matching,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "needle topic".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();
        store
            .append(
                other,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "different topic".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        let by_text = store.search_session_summaries("needle", 10).await.unwrap();
        assert_eq!(by_text.len(), 1);
        assert_eq!(by_text[0].session_id, matching);

        let id_prefix = &other.to_string()[..8];
        let by_id = store.search_session_summaries(id_prefix, 10).await.unwrap();
        assert_eq!(by_id.len(), 1);
        assert_eq!(by_id[0].session_id, other);
    }

    #[tokio::test]
    async fn search_all_project_session_summaries_scans_other_project_dirs() {
        let cwd_a = TempDir::new().unwrap();
        let cwd_b = TempDir::new().unwrap();
        let projects = TempDir::new().unwrap();
        let store_a = JsonlHistoryStore::with_root(cwd_a.path(), projects.path().to_path_buf())
            .await
            .unwrap();
        let store_b = JsonlHistoryStore::with_root(cwd_b.path(), projects.path().to_path_buf())
            .await
            .unwrap();
        let session_b = SessionId::new();
        store_b
            .append(
                session_b,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "cross project needle".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        let found = store_a
            .search_all_project_session_summaries("needle", 10)
            .await
            .unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, session_b);
    }

    #[tokio::test]
    async fn list_sessions_empty_dir() {
        let (store, _cwd, _proj) = make_store().await;
        let listed = store.list_sessions().await.unwrap();
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn parse_error_carries_line_number() {
        let (store, _cwd, proj) = make_store().await;
        let s = SessionId::new();
        let dir = store.project_dir_path();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = store.session_file_path(&s);

        // 第 1 行合法；第 2 行坏；第 3 行合法
        let id1 = base::id::Id::new();
        let id3 = base::id::Id::new();
        let line1 = serde_json::to_string(&json!({
            "v": 1, "id": id1, "ts": "2026-05-04T00:00:00Z",
            "session_id": s, "kind": "user", "content": []
        }))
        .unwrap();
        let line2 = "{ this is broken json";
        let line3 = serde_json::to_string(&json!({
            "v": 1, "id": id3, "ts": "2026-05-04T00:00:01Z",
            "session_id": s, "kind": "user", "content": []
        }))
        .unwrap();

        let blob = format!("{line1}\n{line2}\n{line3}\n");
        tokio::fs::write(&path, blob).await.unwrap();

        let err = store.load(s).await.unwrap_err();
        match err {
            HistoryError::Parse { line, .. } => assert_eq!(line, 2),
            other => panic!("expected Parse, got {other:?}"),
        }
        // store 仍可被 list（项目目录还在）
        let listed = store.list_sessions().await.unwrap();
        assert_eq!(listed, vec![s]);
        drop(proj);
    }

    #[tokio::test]
    async fn concurrent_appends_do_not_interleave_lines() {
        let (store, _cwd, _proj) = make_store().await;
        let s = SessionId::new();
        let store = Arc::new(store);

        let mut tasks = Vec::new();
        for i in 0..30u32 {
            let st = store.clone();
            tasks.push(tokio::spawn(async move {
                st.append(
                    s,
                    LogEntry::User {
                        content: vec![ContentBlock::Text {
                            text: format!("msg-{i}"),
                            cache_control: None,
                        }],
                    },
                )
                .await
                .unwrap();
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }

        let loaded = store.load(s).await.unwrap();
        assert_eq!(loaded.len(), 30);
        // 每行都能解析；顺序不保证（concurrent）但不应该出现 partial line 错
    }

    #[tokio::test]
    async fn delete_removes_session_file() {
        let (store, _cwd, _proj) = make_store().await;
        let s = SessionId::new();
        store
            .append(
                s,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "will be deleted".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();
        let path = store.session_file_path(&s);
        assert!(path.exists());
        store.delete(s).await.unwrap();
        assert!(!path.exists());
        // 再次删除不报错
        store.delete(s).await.unwrap();
    }

    #[tokio::test]
    async fn tool_result_roundtrip() {
        let (store, _cwd, _proj) = make_store().await;
        let s = SessionId::new();
        store
            .append(
                s,
                LogEntry::ToolResult {
                    tool_use_id: "toolu_01".into(),
                    content: ToolResultContent::Text("stdout".into()),
                    is_error: false,
                },
            )
            .await
            .unwrap();
        let loaded = store.load(s).await.unwrap();
        assert_eq!(loaded.len(), 1);
        match &loaded[0].entry {
            LogEntry::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "toolu_01");
                assert_eq!(content, &ToolResultContent::Text("stdout".into()));
                assert!(!is_error);
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn search_all_projects_finds_match() {
        let cwd_a = TempDir::new().unwrap();
        let cwd_b = TempDir::new().unwrap();
        let projects = TempDir::new().unwrap();
        let store_a =
            JsonlHistoryStore::with_root(cwd_a.path(), projects.path().to_path_buf())
                .await
                .unwrap();
        let store_b =
            JsonlHistoryStore::with_root(cwd_b.path(), projects.path().to_path_buf())
                .await
                .unwrap();

        let session_a = SessionId::new();
        store_a
            .append(
                session_a,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "project alpha unrelated".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        let session_b = SessionId::new();
        store_b
            .append(
                session_b,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "project beta with needle".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        let found = store_a
            .search_all_project_session_summaries("needle", 10)
            .await
            .unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, session_b);
    }

    #[tokio::test]
    async fn search_empty_query_returns_all() {
        let (store, _cwd, _proj) = make_store().await;
        let a = SessionId::new();
        let b = SessionId::new();
        for (s, txt) in [(&a, "first session"), (&b, "second session")] {
            store
                .append(
                    *s,
                    LogEntry::User {
                        content: vec![ContentBlock::Text {
                            text: (*txt).into(),
                            cache_control: None,
                        }],
                    },
                )
                .await
                .unwrap();
        }

        let results = store.search_session_summaries("", 10).await.unwrap();

        assert_eq!(results.len(), 2);
        let mut ids: Vec<_> = results.iter().map(|s| s.session_id).collect();
        ids.sort_by_key(|id| id.to_string());
        let mut expected = vec![a, b];
        expected.sort_by_key(|id| id.to_string());
        assert_eq!(ids, expected);
    }

    #[tokio::test]
    async fn search_same_repo_filters_correctly() {
        let repo_dir = TempDir::new().unwrap();
        let outside_dir = TempDir::new().unwrap();
        let shared_projects = TempDir::new().unwrap();
        let config_home = TempDir::new().unwrap();

        // Init git in repo_dir
        let init_output = tokio::process::Command::new("git")
            .arg("init")
            .current_dir(repo_dir.path())
            .output()
            .await
            .unwrap();
        assert!(init_output.status.success());

        let repo_store = JsonlHistoryStore::with_root(
            repo_dir.path(),
            shared_projects.path().to_path_buf(),
        )
        .await
        .unwrap();
        let outside_store = JsonlHistoryStore::with_root(
            outside_dir.path(),
            shared_projects.path().to_path_buf(),
        )
        .await
        .unwrap();

        // Create session in repo
        let session_in_repo = SessionId::new();
        repo_store
            .append(
                session_in_repo,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "inside repo work".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        // Create session outside repo
        let session_outside = SessionId::new();
        outside_store
            .append(
                session_outside,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "outside repo work".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        // Save original ATTA_CONFIG_HOME and set to temp dir
        let original = std::env::var("ATTA_CONFIG_HOME").ok();
        std::env::set_var("ATTA_CONFIG_HOME", config_home.path());

        // Create metadata for in-repo session with canonicalized cwd
        let canonical_repo = tokio::fs::canonicalize(repo_dir.path()).await.unwrap();
        let sessions_root_dir = config_home.path().join("sessions");
        let in_repo_meta_dir = sessions_root_dir.join(session_in_repo.to_string());
        tokio::fs::create_dir_all(&in_repo_meta_dir).await.unwrap();
        let in_repo_meta = SessionMetadata::new(
            &canonical_repo,
            &shared_projects.path().join("in-repo"),
            session_in_repo,
        );
        tokio::fs::write(
            in_repo_meta_dir.join("metadata.json"),
            serde_json::to_string_pretty(&in_repo_meta).unwrap(),
        )
        .await
        .unwrap();

        // Create metadata for outside session with canonicalized cwd
        let canonical_outside = tokio::fs::canonicalize(outside_dir.path()).await.unwrap();
        let outside_meta_dir = sessions_root_dir.join(session_outside.to_string());
        tokio::fs::create_dir_all(&outside_meta_dir).await.unwrap();
        let outside_meta = SessionMetadata::new(
            &canonical_outside,
            &shared_projects.path().join("outside"),
            session_outside,
        );
        tokio::fs::write(
            outside_meta_dir.join("metadata.json"),
            serde_json::to_string_pretty(&outside_meta).unwrap(),
        )
        .await
        .unwrap();

        // Search from repo store with empty query to find all matching filter
        let found = repo_store
            .search_same_repo_session_summaries("", 10)
            .await
            .unwrap();

        // Restore ATTA_CONFIG_HOME
        match original {
            Some(v) => std::env::set_var("ATTA_CONFIG_HOME", v),
            None => std::env::remove_var("ATTA_CONFIG_HOME"),
        }

        // Only the in-repo session should be found
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, session_in_repo);
    }

    // -----------------------------------------------------------------------
    // Paste store integration tests
    // -----------------------------------------------------------------------

    async fn make_store_with_paste() -> (JsonlHistoryStore, TempDir, TempDir, TempDir) {
        let cwd_tmp = TempDir::new().unwrap();
        let projects_tmp = TempDir::new().unwrap();
        let paste_base = TempDir::new().unwrap();
        let paste_store = PasteStore::new(paste_base.path());
        let store = JsonlHistoryStore::with_root(cwd_tmp.path(), projects_tmp.path().to_path_buf())
            .await
            .unwrap()
            .with_paste_store(paste_store);
        (store, cwd_tmp, projects_tmp, paste_base)
    }

    #[tokio::test]
    async fn small_content_stored_inline_not_as_pasteref() {
        let (store, _cwd, _proj, _paste_base) = make_store_with_paste().await;
        let s = SessionId::new();
        store
            .append(
                s,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "short".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        let loaded = store.load(s).await.unwrap();
        assert_eq!(loaded.len(), 1);
        // Should be a User, not a PasteRef.
        assert!(
            matches!(loaded[0].entry, LogEntry::User { .. }),
            "expected User, got {:?}",
            loaded[0].entry
        );
    }

    #[tokio::test]
    async fn large_content_stored_as_pasteref_and_hydrated() {
        let (store, _cwd, _proj, _paste_base) = make_store_with_paste().await;
        let s = SessionId::new();

        // Create a text block well over the 1024-byte threshold.
        let big_text = "X".repeat(1500);
        store
            .append(
                s,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: big_text.clone(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        // Load should transparently hydrate PasteRef back to User.
        let loaded = store.load(s).await.unwrap();
        assert_eq!(loaded.len(), 1);
        match &loaded[0].entry {
            LogEntry::User { content } => {
                match &content[0] {
                    ContentBlock::Text { text, .. } => {
                        assert_eq!(text, &big_text);
                    }
                    other => panic!("expected Text block, got {other:?}"),
                }
            }
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn large_content_raw_jsonl_has_pasteref_not_content() {
        let (store, _cwd, _proj, paste_base) = make_store_with_paste().await;
        let s = SessionId::new();

        let big_text = "Y".repeat(1500);
        store
            .append(
                s,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: big_text.clone(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        // Read the raw JSONL file — it should contain "paste_ref", not the big content.
        let path = store.session_file_path(&s);
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            raw.contains("\"kind\":\"paste_ref\""),
            "raw jsonl should have paste_ref: {raw}"
        );
        // The big text should not appear inline in the JSONL.
        assert!(
            !raw.contains(&big_text),
            "raw jsonl should not contain the large text body"
        );

        // Verify the paste file exists and has the content.
        let entries: Vec<EnvelopedEntry> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let paste_id = match &entries[0].entry {
            LogEntry::PasteRef { paste_id } => paste_id.clone(),
            other => panic!("expected PasteRef, got {other:?}"),
        };
        let paste_path = paste_base.path().join("pastes").join(&paste_id);
        assert!(paste_path.exists(), "paste file should exist");

        // The paste file should contain the full variant JSON with the content.
        let paste_json = tokio::fs::read_to_string(&paste_path).await.unwrap();
        assert!(paste_json.contains(&big_text));
    }

    #[tokio::test]
    async fn large_assistant_content_stored_and_hydrated() {
        let (store, _cwd, _proj, _paste_base) = make_store_with_paste().await;
        let s = SessionId::new();

        let big_text = "Z".repeat(1500);
        store
            .append(
                s,
                LogEntry::Assistant {
                    content: vec![ContentBlock::Text {
                        text: big_text.clone(),
                        cache_control: None,
                    }],
                    stop_reason: Some(StopReason::EndTurn),
                    usage: None,
                    model: Some("claude-sonnet-4-6".into()),
                },
            )
            .await
            .unwrap();

        // Load should transparently hydrate PasteRef back to Assistant with all fields.
        let loaded = store.load(s).await.unwrap();
        assert_eq!(loaded.len(), 1);
        match &loaded[0].entry {
            LogEntry::Assistant {
                content,
                stop_reason,
                model,
                ..
            } => {
                match &content[0] {
                    ContentBlock::Text { text, .. } => assert_eq!(text, &big_text),
                    other => panic!("expected Text block, got {other:?}"),
                }
                assert_eq!(*stop_reason, Some(StopReason::EndTurn));
                assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn load_messages_with_paste_hydration() {
        let (store, _cwd, _proj, _paste_base) = make_store_with_paste().await;
        let s = SessionId::new();

        let big_text = "A".repeat(1500);
        store
            .append(
                s,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: big_text.clone(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        // load_messages goes through load() which hydrates.
        let messages = store.load_messages(s).await.unwrap();
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            base::message::Message::User { content } => match &content[0] {
                ContentBlock::Text { text, .. } => assert_eq!(text, &big_text),
                other => panic!("expected Text block, got {other:?}"),
            },
            other => panic!("expected User message, got {other:?}"),
        }
    }
}
