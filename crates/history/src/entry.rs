//! jsonl 行级数据模型。
//!
//! `EnvelopedEntry` = 顶层字段（v / id / ts / session_id）+ flatten 进 `LogEntry`。
//! 见 docs/DATA_FORMATS.md §A.2 / §A.3。

use base::id::Id;
use base::message::{ContentBlock, Message, StopReason, ToolResultContent};
use base::session::SessionId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// 当前 schema 版本。旧历史文件无 `v` 字段 → 反序列化时按 0 解读
/// （由 default 兜底）。
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// 完整一行 = envelope + 内嵌的具体 entry。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvelopedEntry {
    /// schema 版本。default = 0（兼容旧格式）
    #[serde(default)]
    pub v: u32,

    /// 行级 id（dedup / resume 用）
    pub id: Id,

    /// 行写入时刻（UTC RFC3339）
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,

    /// 与 jsonl 文件名相同；冗余存供跨文件聚合
    pub session_id: SessionId,

    /// Optional transcript topology pointer. Old logs do not have this field.
    /// Writers can fill it when preserving a branch/sidechain conversation
    /// graph matters; the default linear replay path ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<Id>,

    /// Marks entries written by subagents or side conversations. Kept optional
    /// for backward compatibility with older linear transcripts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_sidechain: bool,

    #[serde(flatten)]
    pub entry: LogEntry,
}

impl EnvelopedEntry {
    /// Create a new enveloped entry with the current schema version, a fresh
    /// ID, and the current timestamp. No parent linkage or sidechain marker
    /// is set by default — use [`with_parent`] / [`as_sidechain`] for that.
    pub fn new(session_id: SessionId, entry: LogEntry) -> Self {
        Self {
            v: CURRENT_SCHEMA_VERSION,
            id: Id::new(),
            ts: OffsetDateTime::now_utc(),
            session_id,
            parent_id: None,
            is_sidechain: false,
            entry,
        }
    }

    /// Link this entry to a parent entry, establishing a causal relationship
    /// in the history graph (e.g. a tool result that was spawned by a
    /// specific assistant turn).
    pub fn with_parent(mut self, parent_id: Id) -> Self {
        self.parent_id = Some(parent_id);
        self
    }

    /// Mark this entry as a sidechain entry — one that branches off the main
    /// conversation timeline (e.g. a sub-agent's background work) rather
    /// than belonging to the primary turn sequence.
    pub fn as_sidechain(mut self) -> Self {
        self.is_sidechain = true;
        self
    }
}

/// 行的具体 kind。`#[serde(tag = "kind")]` 让 JSON 用 `"kind": "user"` 区分。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogEntry {
    /// 会话起始；每个 jsonl 文件首行都是这个。
    Meta {
        cwd: String,
        #[serde(with = "time::serde::rfc3339")]
        started_at: OffsetDateTime,
        model: String,
        permission_mode: String,
        engine_version: String,
        attacode_version: String,
        /// The session ID that spawned this session (parent-child relationship).
        /// Set when a sub-agent or forked conversation is created from an existing session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_session_id: Option<String>,
    },

    /// 用户消息（含粘贴 / 图像；进 API）
    User { content: Vec<ContentBlock> },

    /// 模型消息（含 thinking / tool_use；进 API 和 transcript）
    Assistant {
        content: Vec<ContentBlock>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<StopReason>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<UsageRecord>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },

    /// 工具结果（送 API 时被引擎包到 user message 里）
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },

    /// UI-only 系统消息：本地命令输出 / 提醒 / 通知 —— **不**送 API
    System {
        subkind: SystemSubkind,
        text: String,
    },

    /// 一次压缩动作的标记
    Compact {
        before_tokens: u64,
        after_tokens: u64,
        /// 压缩生成的 summary 块所属 assistant 行的 envelope id
        summary_block_id: Option<Id>,
        /// 压缩后的完整替换历史。优先于 summary 用于重建投影视图。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replacement_history: Option<Vec<Message>>,
        /// 压缩产物的消息内容。通常是单条带 summary marker 的 user message。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<Vec<ContentBlock>>,
        /// Snip metadata: UUIDs of messages removed by snip compaction.
        /// Used during resume to filter out removed messages (TS parity: `snipMetadata.removedUuids`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snip_removed_uuids: Option<Vec<String>>,
    },

    /// 周期性 cost 快照
    UsageSnapshot {
        total_input: u64,
        total_output: u64,
        total_cache_creation: u64,
        total_cache_read: u64,
        total_cost_usd: f64,
    },

    /// Content stored externally in the paste store (SHA-256 hex, first 16
    /// chars). Written when the serialized content exceeds 1024 bytes, to
    /// avoid bloating the conversation JSONL. Transparently hydrated back to
    /// the original variant by [`JsonlHistoryStore::load`].
    PasteRef { paste_id: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SystemSubkind {
    LocalCommand,
    Reminder,
    Notice,
}

/// API 返回的 usage 字段在 jsonl 里的形状（与 Anthropic SSE 的 Usage 对齐）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageRecord {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
}

/// External content store for deduplicating large content in the JSONL.
///
/// Files are stored under `<base>/pastes/` keyed by the first 16 hex
/// characters of the SHA-256 hash of the content. Content is deduplicated:
/// identical content produces the same paste ID and is written once.
#[derive(Debug, Clone)]
pub struct PasteStore {
    dir: PathBuf,
}

impl PasteStore {
    /// Create a new paste store rooted at `base` (e.g. `~/.atta/code`).
    /// The actual paste files live in `<base>/pastes/`.
    pub fn new(base: &Path) -> Self {
        Self {
            dir: base.join("pastes"),
        }
    }

    /// Store `content` and return the paste ID (SHA-256 hex, first 16 chars).
    /// If the same content was previously stored, the existing paste ID is
    /// returned and no additional file is written (deduplication).
    pub fn store(&self, content: &str) -> Result<String, std::io::Error> {
        let hash = Sha256::digest(content.as_bytes());
        let hex_str = hex::encode(hash);
        let paste_id = hex_str[..16].to_string();

        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(&paste_id);
        if !path.exists() {
            std::fs::write(&path, content)?;
        }
        Ok(paste_id)
    }

    /// Load content by `paste_id`. Returns `None` if the paste file does not
    /// exist (e.g. if it was cleaned up or never written).
    pub fn load(&self, paste_id: &str) -> Result<Option<String>, std::io::Error> {
        let path = self.dir.join(paste_id);
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Remove paste files whose last modification time is older than 7 days.
    /// Returns the number of files removed.
    pub fn cleanup(&self) -> Result<usize, std::io::Error> {
        let now = std::time::SystemTime::now();
        let max_age = std::time::Duration::from_secs(7 * 24 * 3600);
        let mut removed = 0;

        if !self.dir.exists() {
            return Ok(0);
        }

        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if !meta.is_file() {
                continue;
            }
            if let Ok(modified) = meta.modified() {
                if now
                    .duration_since(modified)
                    .unwrap_or(std::time::Duration::ZERO)
                    > max_age
                {
                    std::fs::remove_file(entry.path())?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_entry_roundtrip() {
        let env = EnvelopedEntry::new(
            SessionId::new(),
            LogEntry::User {
                content: vec![ContentBlock::Text {
                    text: "hi".into(),
                    cache_control: None,
                }],
            },
        );
        let s = serde_json::to_string(&env).unwrap();
        let back: EnvelopedEntry = serde_json::from_str(&s).unwrap();
        match back.entry {
            LogEntry::User { content } => {
                assert_eq!(content.len(), 1);
            }
            _ => panic!(),
        }
        assert_eq!(back.v, CURRENT_SCHEMA_VERSION);
        assert!(back.parent_id.is_none());
        assert!(!back.is_sidechain);
    }

    #[test]
    fn assistant_with_stop_reason_and_usage() {
        let env = EnvelopedEntry::new(
            SessionId::new(),
            LogEntry::Assistant {
                content: vec![ContentBlock::Text {
                    text: "ok".into(),
                    cache_control: None,
                }],
                stop_reason: Some(StopReason::EndTurn),
                usage: Some(UsageRecord {
                    input_tokens: 100,
                    output_tokens: 5,
                    ..Default::default()
                }),
                model: Some("claude-sonnet-4-6".into()),
            },
        );
        let s = serde_json::to_string(&env).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"], "assistant");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["usage"]["input_tokens"], 100);
    }

    #[test]
    fn meta_entry_uses_kind_tag() {
        let now = OffsetDateTime::now_utc();
        let env = EnvelopedEntry::new(
            SessionId::new(),
            LogEntry::Meta {
                cwd: "/tmp".into(),
                started_at: now,
                model: "claude-sonnet-4-6".into(),
                permission_mode: "default".into(),
                engine_version: "0.0.1".into(),
                attacode_version: "0.0.1".into(),
                parent_session_id: None,
            },
        );
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["kind"], "meta");
        assert_eq!(v["cwd"], "/tmp");
        assert!(v["v"].is_number());
    }

    #[test]
    fn missing_v_treated_as_zero() {
        let session = SessionId::new();
        let id = Id::new();
        let raw = json!({
            "id": id,
            "ts": "2026-05-04T00:00:00Z",
            "session_id": session,
            "kind": "user",
            "content": []
        });
        let env: EnvelopedEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(env.v, 0);
        assert!(env.parent_id.is_none());
        assert!(!env.is_sidechain);
    }

    #[test]
    fn envelope_topology_fields_roundtrip() {
        let parent = Id::new();
        let env = EnvelopedEntry::new(
            SessionId::new(),
            LogEntry::User {
                content: vec![ContentBlock::Text {
                    text: "side".into(),
                    cache_control: None,
                }],
            },
        )
        .with_parent(parent)
        .as_sidechain();

        let s = serde_json::to_string(&env).unwrap();
        let back: EnvelopedEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(back.parent_id, Some(parent));
        assert!(back.is_sidechain);
    }

    #[test]
    fn tool_result_string_form_decodes() {
        let session = SessionId::new();
        let raw = json!({
            "v": 1,
            "id": Id::new(),
            "ts": "2026-05-04T00:00:00Z",
            "session_id": session,
            "kind": "tool_result",
            "tool_use_id": "toolu_01",
            "content": "stdout",
        });
        let env: EnvelopedEntry = serde_json::from_value(raw).unwrap();
        match env.entry {
            LogEntry::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "toolu_01");
                assert_eq!(content, ToolResultContent::Text("stdout".into()));
                assert!(!is_error);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn compact_with_summary_roundtrips() {
        let session = SessionId::new();
        let env = EnvelopedEntry::new(
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
        );
        let s = serde_json::to_string(&env).unwrap();
        let back: EnvelopedEntry = serde_json::from_str(&s).unwrap();
        match back.entry {
            LogEntry::Compact {
                summary,
                replacement_history,
                ..
            } => {
                assert!(replacement_history.is_some());
                assert!(summary.is_some());
            }
            _ => panic!(),
        }
    }

    // -----------------------------------------------------------------------
    // PasteStore tests
    // -----------------------------------------------------------------------

    #[test]
    fn paste_store_store_and_load_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = PasteStore::new(dir.path());
        let content = "hello paste store";

        let id = store.store(content).unwrap();
        // ID is first 16 hex chars of SHA-256
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));

        let loaded = store.load(&id).unwrap().expect("should find paste file");
        assert_eq!(loaded, content);
    }

    #[test]
    fn paste_store_dedup_returns_same_id() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = PasteStore::new(dir.path());
        let content = "this content should have a stable hash";

        let id1 = store.store(content).unwrap();
        let id2 = store.store(content).unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn paste_store_load_missing_returns_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = PasteStore::new(dir.path());
        let result = store.load("nonexistentpasteid").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn paste_store_cleanup_skips_fresh_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = PasteStore::new(dir.path());
        store.store("fresh content").unwrap();
        // No files should be cleaned up — they were just written.
        let removed = store.cleanup().unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn paste_store_cleanup_handles_empty_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = PasteStore::new(dir.path());
        let removed = store.cleanup().unwrap();
        assert_eq!(removed, 0);
    }
}
