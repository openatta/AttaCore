//! Durable memory extraction — structured, cross-session memories extracted from
//! session transcripts.
//!
//! **DEPRECATED**: This module uses JSON-based storage. New code should use
//! `interface::memory::MemoryStore` which stores memories as Markdown files
//! with YAML frontmatter, matching the Claude Code TS format.
//!
//! The lifecycle:
//! 1. During session memory extraction, the engine produces `Vec<DurableMemory>`.
//! 2. `DurableMemoryStore` persists them under `~/.atta/code/memories/` as individual
//!    JSON files, deduplicating by topic.
//! 3. On resume or new session, relevant memories are surfaced via the cross-
//!    session memory system prompt section.
//!
//! See P2.1 in the cross-session memory analysis document.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A single structured memory extracted from a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DurableMemory {
    /// Short, unique-ish label for dedup matching (e.g. "api-error-handling-pattern").
    pub topic: String,
    /// Human-readable summary (1-3 sentences).
    pub summary: String,
    /// Specific quotes or observations that support this memory.
    pub evidence: Vec<String>,
    /// Session ID the memory was extracted from.
    pub source_session_id: String,
    /// Confidence 0.0–1.0. Entries below 0.3 are dropped during merge.
    pub confidence: f64,
    /// ISO-8601 timestamp of when this fact was last observed.
    pub last_seen: String,
}

impl DurableMemory {
    pub fn new(
        topic: impl Into<String>,
        summary: impl Into<String>,
        source_session_id: impl Into<String>,
        confidence: f64,
        last_seen: impl Into<String>,
    ) -> Self {
        Self {
            topic: topic.into(),
            summary: summary.into(),
            evidence: Vec::new(),
            source_session_id: source_session_id.into(),
            confidence,
            last_seen: last_seen.into(),
        }
    }

    pub fn with_evidence(mut self, evidence: Vec<String>) -> Self {
        self.evidence = evidence;
        self
    }

    /// Merge another memory with the same topic into this one.
    /// Keeps the higher confidence, appends new evidence, updates last_seen.
    pub fn merge(&mut self, other: &DurableMemory) {
        if other.confidence > self.confidence {
            self.confidence = other.confidence;
        }
        for e in &other.evidence {
            if !self.evidence.contains(e) {
                self.evidence.push(e.clone());
            }
        }
        if other.last_seen > self.last_seen {
            self.last_seen = other.last_seen.clone();
        }
    }
}

/// Error type for `DurableMemoryStore` operations.
#[derive(Debug, thiserror::Error)]
pub enum MemoryStoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Manages reading/writing structured durable memories to disk.
///
/// Layout:
/// ```text
/// ~/.atta/code/memories/
/// ├── <topic-hash>.json   # one per unique topic
/// └── index.json          # topic → filename mapping (optional, for fast listing)
/// ```
///
/// Each file contains a single `DurableMemory` in JSON. The filename is derived
/// from a hash of the topic so that same-topic memories naturally collide.
pub struct DurableMemoryStore {
    root: PathBuf,
}

impl DurableMemoryStore {
    /// Create a store rooted at `root`. Creates the directory if it doesn't exist.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Path to the memory directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Load all durable memories from disk.
    pub fn load_all(&self) -> Result<Vec<DurableMemory>, MemoryStoreError> {
        let mut memories = Vec::new();
        if !self.root.exists() {
            return Ok(memories);
        }
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json")
                && path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| !s.is_empty())
            {
                let content = std::fs::read_to_string(&path)?;
                match serde_json::from_str::<DurableMemory>(&content) {
                    Ok(m) => memories.push(m),
                    Err(e) => {
                        eprintln!(
                            "[attacode-core] corrupt memory file {}: {e}",
                            path.display()
                        );
                    }
                }
            }
        }
        // Sort by last_seen descending so most recent memories come first
        memories.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        Ok(memories)
    }

    /// Persist a batch of extracted memories, deduplicating against existing ones.
    ///
    /// Merges same-topic entries, drops low-confidence ones, and overwrites the
    /// on-disk file with the merged result.
    pub fn persist_batch(
        &self,
        new_memories: Vec<DurableMemory>,
    ) -> Result<usize, MemoryStoreError> {
        if new_memories.is_empty() {
            return Ok(0);
        }
        std::fs::create_dir_all(&self.root)?;

        // Load existing
        let existing = self.load_all()?;
        let mut by_topic: HashMap<String, DurableMemory> = HashMap::new();
        for m in existing {
            by_topic.insert(m.topic.clone(), m);
        }

        // Merge new into existing
        for memory in new_memories {
            // Drop low-confidence
            if memory.confidence < 0.3 {
                continue;
            }
            let topic = memory.topic.clone();
            if let Some(existing) = by_topic.get_mut(&topic) {
                existing.merge(&memory);
            } else {
                by_topic.insert(topic, memory);
            }
        }

        // Write each memory to its file
        let mut written = 0usize;
        for (topic, memory) in &by_topic {
            let filename = topic_hash_filename(topic);
            let path = self.root.join(&filename);
            let json = serde_json::to_string_pretty(&memory)?;
            std::fs::write(&path, json)?;
            written += 1;
        }

        Ok(written)
    }

    /// Remove a memory by topic (e.g., for user-requested deletion).
    pub fn remove(&self, topic: &str) -> Result<bool, MemoryStoreError> {
        let filename = topic_hash_filename(topic);
        let path = self.root.join(&filename);
        if path.exists() {
            std::fs::remove_file(&path)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Search memories by keyword in topic or summary.
    pub fn search(&self, query: &str) -> Result<Vec<DurableMemory>, MemoryStoreError> {
        let q = query.to_lowercase();
        let memories = self.load_all()?;
        Ok(memories
            .into_iter()
            .filter(|m| {
                m.topic.to_lowercase().contains(&q) || m.summary.to_lowercase().contains(&q)
            })
            .collect())
    }
}

/// Deterministic filename for a topic: hash the topic to avoid filesystem issues
/// with special characters, but prefix with first 20 chars of topic for readability.
fn topic_hash_filename(topic: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    topic.hash(&mut hasher);
    let hash = hasher.finish();
    // Sanitize: take first 30 alphanumeric chars of topic
    let prefix: String = topic
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .take(30)
        .collect();
    format!("memory_{}_{:016x}.json", prefix, hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn durable_memory_new_and_merge() {
        let mut m1 = DurableMemory::new("errors", "handle API 401s", "sess-1", 0.8, "2026-05-01");
        m1 = m1.with_evidence(vec!["user got 401".into()]);

        let m2 = DurableMemory {
            topic: "errors".into(),
            summary: "handle API 401s and refresh tokens".into(),
            evidence: vec!["we added retry".into()],
            source_session_id: "sess-2".into(),
            confidence: 0.9,
            last_seen: "2026-05-15".into(),
        };

        m1.merge(&m2);
        assert_eq!(m1.confidence, 0.9);
        assert_eq!(m1.evidence.len(), 2);
        assert_eq!(m1.last_seen, "2026-05-15");
    }

    #[test]
    fn durable_memory_does_not_duplicate_evidence_during_merge() {
        let mut m1 = DurableMemory::new("test", "desc", "s1", 0.8, "now");
        m1 = m1.with_evidence(vec!["same".into()]);
        let m2 = DurableMemory {
            evidence: vec!["same".into()],
            ..DurableMemory::new("test", "desc", "s1", 0.8, "now")
        };
        m1.merge(&m2);
        assert_eq!(m1.evidence.len(), 1);
    }

    #[test]
    fn store_persist_batch_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let store = DurableMemoryStore::new(dir.path().join("memories"));

        let memories = vec![
            DurableMemory::new("auth", "handle OAuth", "s1", 0.9, "2026-05-01"),
            DurableMemory::new("cache", "redis caching", "s1", 0.7, "2026-05-01"),
        ];
        let written = store.persist_batch(memories).unwrap();
        assert_eq!(written, 2);

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn store_dedups_by_topic_and_merges() {
        let dir = tempfile::tempdir().unwrap();
        let store = DurableMemoryStore::new(dir.path().join("memories"));

        // First batch
        store
            .persist_batch(vec![DurableMemory::new(
                "auth",
                "original",
                "s1",
                0.8,
                "2026-05-01",
            )])
            .unwrap();

        // Second batch — same topic, higher confidence
        store
            .persist_batch(vec![DurableMemory {
                summary: "improved".into(),
                evidence: vec!["new evidence".into()],
                confidence: 0.95,
                last_seen: "2026-05-10".into(),
                ..DurableMemory::new("auth", "improved", "s2", 0.95, "2026-05-10")
            }])
            .unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1, "should still be 1 entry after dedup");
        assert_eq!(loaded[0].confidence, 0.95, "should take higher confidence");
        assert_eq!(loaded[0].evidence.len(), 1, "should have new evidence");
    }

    #[test]
    fn store_drops_low_confidence() {
        let dir = tempfile::tempdir().unwrap();
        let store = DurableMemoryStore::new(dir.path().join("memories"));

        store
            .persist_batch(vec![DurableMemory::new(
                "low",
                "not useful",
                "s1",
                0.2,
                "now",
            )])
            .unwrap();

        let loaded = store.load_all().unwrap();
        assert!(
            loaded.is_empty(),
            "low-confidence entries should be dropped"
        );
    }

    #[test]
    fn store_remove_deletes_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = DurableMemoryStore::new(dir.path().join("memories"));

        store
            .persist_batch(vec![DurableMemory::new(
                "toremove",
                "delete me",
                "s1",
                0.9,
                "now",
            )])
            .unwrap();
        assert_eq!(store.load_all().unwrap().len(), 1);

        let removed = store.remove("toremove").unwrap();
        assert!(removed);
        assert!(store.load_all().unwrap().is_empty());
    }

    #[test]
    fn store_search_finds_by_topic_and_summary() {
        let dir = tempfile::tempdir().unwrap();
        let store = DurableMemoryStore::new(dir.path().join("memories"));

        store
            .persist_batch(vec![
                DurableMemory::new("auth", "OAuth tokens", "s1", 0.9, "now"),
                DurableMemory::new("cache", "Redis strategy", "s1", 0.8, "now"),
            ])
            .unwrap();

        assert_eq!(store.search("auth").unwrap().len(), 1);
        assert_eq!(store.search("redis").unwrap().len(), 1);
        assert_eq!(store.search("tokens").unwrap().len(), 1);
        assert_eq!(store.search("nope").unwrap().len(), 0);
    }

    #[test]
    fn store_empty_on_nonexistent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = DurableMemoryStore::new(dir.path().join("does-not-exist-yet"));
        let loaded = store.load_all().unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn topic_hash_filename_includes_prefix_and_hash() {
        let name = topic_hash_filename("my-memory-topic");
        assert!(name.starts_with("memory_my-memory-topic_"));
        assert!(name.ends_with(".json"));
        assert!(name.len() > 30);
    }

    #[test]
    fn topic_hash_filename_sanitizes_special_chars() {
        let name = topic_hash_filename("path/../traversal?attempt!");
        assert!(!name.contains('/'), "path separators should be removed");
        assert!(!name.contains(".."), "double dots should be removed");
    }

    #[test]
    fn store_handles_corrupt_file_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let mem_dir = dir.path().join("memories");
        fs::create_dir_all(&mem_dir).unwrap();
        fs::write(mem_dir.join("corrupt.json"), "not valid json").unwrap();
        fs::write(
            mem_dir.join("valid.json"),
            serde_json::to_string(&DurableMemory::new("valid", "this works", "s1", 0.9, "now"))
                .unwrap(),
        )
        .unwrap();

        let store = DurableMemoryStore::new(mem_dir);
        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].topic, "valid");
    }
}
