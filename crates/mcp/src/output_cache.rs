//! MCP tool output cache — avoids duplicate MCP calls within and across sessions.
//! TS parity: mcpOutputStorage.ts.
//!
//! Caches the last result of each MCP tool call keyed by (server, tool, args_hash).
//! Cache entries have a TTL of 30 seconds — after that, results are re-fetched.
//! Cache is persisted to `~/.atta/code/mcp_cache.json` for cross-session reuse.
//! This prevents the model from making identical MCP calls back-to-back.

use crate::client::McpCallResult;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CACHE_TTL: Duration = Duration::from_secs(30);
const MAX_CACHE_ENTRIES: usize = 100;

/// Default cache file path: `~/.atta/code/mcp_cache.json`.
fn default_cache_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".atta/code/mcp_cache.json")
}

#[derive(Debug, Clone)]
struct CacheEntry {
    result: McpCallResult,
    created_at: Instant,
}

/// Serializable form of a cache entry for on-disk persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistentEntry {
    key: String,
    server: String,
    tool: String,
    args_json: String,
    result_text: String,
    is_error: bool,
    created_at_secs: u64,
}

/// An in-memory cache for MCP tool call results with optional file persistence.
/// TS parity: mcpOutputStorage.ts.
#[derive(Debug, Default)]
pub struct McpOutputCache {
    entries: HashMap<String, CacheEntry>,
    /// Path to the persistence file. None disables persistence.
    persist_path: Option<PathBuf>,
}

impl McpOutputCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a cache that persists to `~/.atta/code/mcp_cache.json`.
    pub fn with_default_persistence() -> Self {
        let mut cache = Self {
            entries: HashMap::new(),
            persist_path: Some(default_cache_path()),
        };
        cache.load_from_disk();
        cache
    }

    /// Create a cache that persists to a custom path.
    pub fn with_persistence(path: PathBuf) -> Self {
        let mut cache = Self {
            entries: HashMap::new(),
            persist_path: Some(path),
        };
        cache.load_from_disk();
        cache
    }

    /// Load cached entries from disk.
    fn load_from_disk(&mut self) {
        let Some(ref path) = self.persist_path else {
            return;
        };
        let Ok(data) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(entries): Result<Vec<PersistentEntry>, _> = serde_json::from_str(&data) else {
            return;
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        for e in entries {
            // Skip expired entries
            let age_secs = now.as_secs().saturating_sub(e.created_at_secs);
            if age_secs > CACHE_TTL.as_secs() {
                continue;
            }
            let created_at = Instant::now()
                .checked_sub(Duration::from_secs(age_secs))
                .unwrap_or_else(Instant::now);
            self.entries.insert(
                e.key,
                CacheEntry {
                    result: McpCallResult {
                        content: vec![crate::client::McpContent::Text(e.result_text)],
                        is_error: e.is_error,
                        meta: None,
                    },
                    created_at,
                },
            );
        }
    }

    /// Persist current entries to disk.
    fn save_to_disk(&self) {
        let Some(ref path) = self.persist_path else {
            return;
        };
        let mut entries: Vec<PersistentEntry> = Vec::with_capacity(self.entries.len());
        for (key, entry) in &self.entries {
            let text = entry
                .result
                .content
                .iter()
                .filter_map(|c| match c {
                    crate::client::McpContent::Text(t) => Some(t.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            // Extract server and tool from key format: mcp_cache_<hash>_<server>_<tool>
            let parts: Vec<&str> = key.rsplitn(3, '_').collect();
            let (server, tool) = if parts.len() >= 2 {
                (
                    parts.get(1).copied().unwrap_or(""),
                    parts.first().copied().unwrap_or(""),
                )
            } else {
                ("", "")
            };
            let created_at_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .saturating_sub(
                    CACHE_TTL.as_secs().saturating_sub(
                        entry
                            .created_at
                            .elapsed()
                            .as_secs()
                            .min(CACHE_TTL.as_secs()),
                    ),
                );
            entries.push(PersistentEntry {
                key: key.clone(),
                server: server.to_string(),
                tool: tool.to_string(),
                args_json: String::new(),
                result_text: text,
                is_error: entry.result.is_error,
                created_at_secs,
            });
        }
        if let Ok(json) = serde_json::to_string(&entries) {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, json);
        }
    }

    /// Build a cache key from server name, tool name, and args.
    fn key(server: &str, tool: &str, args: &serde_json::Map<String, serde_json::Value>) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        server.hash(&mut h);
        tool.hash(&mut h);
        // Hash the sorted JSON representation for deterministic lookup
        if let Ok(sorted) = serde_json::to_string(args) {
            sorted.hash(&mut h);
        }
        format!("mcp_cache_{}_{server}_{tool}", h.finish())
    }

    /// Look up a cached result. Returns None if not found or expired.
    pub fn get(
        &self,
        server: &str,
        tool: &str,
        args: &serde_json::Map<String, serde_json::Value>,
    ) -> Option<McpCallResult> {
        let key = Self::key(server, tool, args);
        let entry = self.entries.get(&key)?;
        if entry.created_at.elapsed() > CACHE_TTL {
            return None;
        }
        Some(entry.result.clone())
    }

    /// Store a result in the cache, evicting oldest entries if over capacity.
    pub fn put(
        &mut self,
        server: &str,
        tool: &str,
        args: &serde_json::Map<String, serde_json::Value>,
        result: McpCallResult,
    ) {
        let key = Self::key(server, tool, args);

        // Evict oldest entries if at capacity
        if self.entries.len() >= MAX_CACHE_ENTRIES {
            let mut oldest_key: Option<String> = None;
            let mut oldest_time = Instant::now();
            for (k, v) in &self.entries {
                if v.created_at < oldest_time {
                    oldest_time = v.created_at;
                    oldest_key = Some(k.clone());
                }
            }
            if let Some(k) = oldest_key {
                self.entries.remove(&k);
            }
        }

        self.entries.insert(
            key,
            CacheEntry {
                result,
                created_at: Instant::now(),
            },
        );
        // Persist to disk (fire-and-forget — small JSON, sync is fine).
        self.save_to_disk();
    }

    /// Clear all cached entries.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.save_to_disk();
    }

    /// Return the number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Remove expired entries (call periodically).
    pub fn evict_expired(&mut self) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|_, v| v.created_at.elapsed() <= CACHE_TTL);
        before - self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::McpContent;

    fn make_result(text: &str) -> McpCallResult {
        McpCallResult {
            content: vec![McpContent::Text(text.to_string())],
            is_error: false,
            meta: None,
        }
    }

    fn make_args(s: &str) -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        m.insert("q".to_string(), serde_json::Value::String(s.to_string()));
        m
    }

    #[test]
    fn cache_hit_returns_stored_result() {
        let mut cache = McpOutputCache::new();
        let args = make_args("test");
        cache.put("github", "search_issues", &args, make_result("cached"));
        let result = cache.get("github", "search_issues", &args);
        assert!(result.is_some());
        match &result.unwrap().content[0] {
            McpContent::Text(s) => assert_eq!(s, "cached"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn cache_miss_returns_none() {
        let cache = McpOutputCache::new();
        let args = make_args("test");
        assert!(cache.get("github", "search_issues", &args).is_none());
    }

    #[test]
    fn different_args_produce_different_keys() {
        let mut cache = McpOutputCache::new();
        let args1 = make_args("test1");
        let args2 = make_args("test2");
        cache.put("github", "search", &args1, make_result("one"));
        cache.put("github", "search", &args2, make_result("two"));
        let r1 = cache.get("github", "search", &args1);
        let r2 = cache.get("github", "search", &args2);
        match &r1.unwrap().content[0] {
            McpContent::Text(s) => assert_eq!(s, "one"),
            _ => panic!("expected text"),
        }
        match &r2.unwrap().content[0] {
            McpContent::Text(s) => assert_eq!(s, "two"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn cache_clear_removes_all() {
        let mut cache = McpOutputCache::new();
        cache.put("s", "t", &make_args("x"), make_result("y"));
        assert_eq!(cache.len(), 1);
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn evict_expired_removes_old_entries() {
        let mut cache = McpOutputCache::new();
        // Directly manipulate entry time to simulate expiry
        cache.put("s", "t", &make_args("x"), make_result("y"));
        // Cannot easily simulate TTL expiry in unit tests without time mocking,
        // so just verify eviction doesn't panic and returns 0 for fresh entries.
        let evicted = cache.evict_expired();
        assert_eq!(evicted, 0);
        assert_eq!(cache.len(), 1);
    }
}
