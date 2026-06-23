//! Cron job storage — in-memory store with optional file persistence.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

use super::parser::cron_matches;

// ---- Cron types ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub cron: String,
    pub prompt: String,
    /// true = fire on each cron match; false = fire once then auto-delete
    pub recurring: bool,
    /// true = persisted to disk and survives restarts
    pub durable: bool,
    /// Unix millis when the job was created
    pub created_ms: i64,
    /// Optional agent id (for teammate-owned crons)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Unix millis of last fire (prevents double-fire within the same minute)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_ms: Option<i64>,
}

/// Thread-safe cron job store. Shared between cron tools and engine via `Arc`.
pub struct CronStore {
    inner: Mutex<Vec<CronJob>>,
    file_path: Option<PathBuf>,
}

impl CronStore {
    /// Create an empty in-memory store.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
            file_path: None,
        }
    }

    /// Create or load from a file path. If the file exists, loads jobs from it.
    pub fn load_or_default(file_path: Option<PathBuf>) -> Self {
        let jobs = file_path
            .as_ref()
            .and_then(|p| Self::load_from_disk(p).ok())
            .unwrap_or_default();
        Self {
            inner: Mutex::new(jobs),
            file_path,
        }
    }

    /// Add a new cron job. Returns the assigned ID.
    pub fn add(&self, cron: String, prompt: String, recurring: bool, durable: bool) -> String {
        let id = generate_id();
        let job = CronJob {
            id: id.clone(),
            cron,
            prompt,
            recurring,
            durable,
            created_ms: now_ms(),
            agent_id: None,
            last_fired_ms: None,
        };
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).push(job);
        if durable {
            self.save();
        }
        id
    }

    /// Remove a job by ID. Returns true if found and removed.
    pub fn remove(&self, id: &str) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let len_before = inner.len();
        inner.retain(|j| j.id != id);
        let removed = inner.len() < len_before;
        if removed {
            self.save();
        }
        removed
    }

    /// List all active jobs.
    pub fn list(&self) -> Vec<CronJob> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Pop all jobs whose cron expression matches the current time.
    ///
    /// Recurring jobs have `last_fired_ms` updated so they won't re-fire
    /// within the same minute. Non-recurring jobs are removed after firing.
    /// Returns due jobs (oldest first).
    pub fn pop_due(&self) -> Vec<CronJob> {
        let now =
            time::OffsetDateTime::now_utc();
        let now_ms = now_ms();
        let current_minute = now.minute();

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut due: Vec<CronJob> = Vec::new();
        let mut keep: Vec<CronJob> = Vec::new();

        for mut job in inner.drain(..) {
            if cron_matches(&job.cron, &now) {
                // Prevent double-fire within the same clock minute
                let already_fired_this_minute = job
                    .last_fired_ms
                    .map(|lf| {
                        let last = time::OffsetDateTime::from_unix_timestamp(lf / 1000).ok();
                        last.map(|t| t.minute() == current_minute).unwrap_or(false)
                    })
                    .unwrap_or(false);

                if already_fired_this_minute {
                    keep.push(job);
                } else if job.recurring {
                    job.last_fired_ms = Some(now_ms);
                    due.push(job.clone());
                    keep.push(job);
                } else {
                    due.push(job);
                }
            } else {
                keep.push(job);
            }
        }

        *inner = keep;
        drop(inner); // release lock before save (std::sync::Mutex is not reentrant)
        self.save();
        due
    }

    fn save(&self) {
        let Some(ref path) = self.file_path else {
            return;
        };
        let jobs = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(json) = serde_json::to_string(&*jobs) {
            let _ = std::fs::write(path, &json);
        }
    }

    fn load_from_disk(path: &std::path::Path) -> std::io::Result<Vec<CronJob>> {
        let data = std::fs::read_to_string(path)?;
        serde_json::from_str(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

impl Default for CronStore {
    fn default() -> Self {
        Self::new()
    }
}

// ---- helpers ----

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn generate_id() -> String {
    let id = uuid::Uuid::new_v4();
    id.to_string()[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_store_new_empty() {
        let store = CronStore::new();
        assert!(store.list().is_empty());
    }

    #[test]
    fn cron_add_and_list() {
        let store = CronStore::new();
        let id = store.add("0 9 * * *".into(), "daily check".into(), true, false);
        assert!(!id.is_empty());
        assert_eq!(id.len(), 8);
        let jobs = store.list();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].cron, "0 9 * * *");
        assert_eq!(jobs[0].prompt, "daily check");
    }

    #[test]
    fn cron_add_and_remove() {
        let store = CronStore::new();
        let id = store.add("*/5 * * * *".into(), "ping".into(), true, false);
        assert!(store.remove(&id));
        assert!(store.list().is_empty());
    }

    #[test]
    fn cron_remove_nonexistent() {
        let store = CronStore::new();
        assert!(!store.remove("nonexistent"));
    }

    #[test]
    fn cron_pop_due_non_recurring_is_removed() {
        let store = CronStore::new();
        // Add a job that matches every minute
        store.add("* * * * *".into(), "every-min".into(), false, false);
        let due = store.pop_due();
        assert!(!due.is_empty(), "should fire at least one job");
        // After popping, the non-recurring job should be gone
        assert!(store.list().is_empty());
    }

    #[test]
    fn cron_pop_due_recurring_stays() {
        let store = CronStore::new();
        store.add("* * * * *".into(), "every-min".into(), true, false);
        let due = store.pop_due();
        assert!(!due.is_empty());
        // After popping, the recurring job should still be there
        let remaining = store.list();
        assert_eq!(remaining.len(), 1);
        assert!(remaining[0].last_fired_ms.is_some());
    }

    #[test]
    fn cron_durability_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron_test.json");

        // Create store with path, add a durable job
        let store = CronStore::load_or_default(Some(path.clone()));
        store.add("0 9 * * *".into(), "durable task".into(), true, true);
        drop(store); // drops the Mutex, file is written

        // Load from same path
        let store2 = CronStore::load_or_default(Some(path.clone()));
        let jobs = store2.list();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].prompt, "durable task");
        assert!(jobs[0].durable);
    }
}
