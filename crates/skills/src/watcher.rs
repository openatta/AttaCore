//! # SkillWatcher — watches skill directories for file changes.
//!
//! Uses the `notify` crate to monitor SKILL.md / *.md files in skill directories.
//! When a skill file is modified, added, or removed, the path is collected.
//! Callers poll via [`SkillWatcher::check_and_reload`] to pick up changes.
//!
//! TS parity: claude-code's `loadSkillsDir.ts` + file-watching integration.

use crate::manager::SkillManager;
use notify::event::Event;
use notify::{recommended_watcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use tracing;

/// Watches skill directories for SKILL.md / *.md file changes.
///
/// Runs a background thread that collects file-system events and writes
/// changed paths into a shared list. Call [`check_and_reload`] periodically
/// (e.g. at the start of each turn) to apply pending reloads.
pub struct SkillWatcher {
    /// The notify watcher (kept alive so events keep flowing).
    #[allow(dead_code)]
    watcher_handle: Option<notify::RecommendedWatcher>,
    /// Shared list of changed skill-file paths discovered since last drain.
    changed: Arc<Mutex<Vec<PathBuf>>>,
}

impl SkillWatcher {
    pub fn new() -> Self {
        Self {
            watcher_handle: None,
            changed: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Start watching the given directories for skill-file changes.
    ///
    /// Only files named `SKILL.md` or ending in `.md` are tracked.
    /// Watches **recursively** — `skills/` directories with subdirectories
    /// like `skills/my-skill/SKILL.md` are fully covered.
    ///
    /// Returns an error if underlying notify setup fails (permissions, kernel
    /// limits, etc.).
    pub fn watch_skills(&mut self, paths: &[PathBuf]) -> Result<(), String> {
        let changed = self.changed.clone();
        let (tx, rx) = std::sync::mpsc::channel();

        let mut w = recommended_watcher(move |res: notify::Result<Event>| {
            let _ = tx.send(res);
        })
        .map_err(|e| format!("notify watcher creation failed: {e}"))?;

        for path in paths {
            if path.exists() {
                w.watch(path, RecursiveMode::Recursive)
                    .map_err(|e| format!("failed to watch '{}': {e}", path.display()))?;
            } else {
                tracing::warn!(?path, "Skill watch path does not exist, skipping");
            }
        }

        // Background thread: dequeue events and collect changed skill-file paths.
        std::thread::Builder::new()
            .name("skill-watcher".into())
            .spawn(move || {
                drain_events(rx, &changed);
            })
            .expect("skill-watcher thread");

        self.watcher_handle = Some(w);
        Ok(())
    }

    /// Drain all changed skill-file paths and reload the corresponding skills
    /// in the given [`SkillManager`].
    ///
    /// Returns the number of skills successfully reloaded.
    pub fn check_and_reload(&self, manager: &SkillManager) -> usize {
        let paths = {
            let mut lock = self.changed.lock().unwrap();
            if lock.is_empty() {
                return 0;
            }
            std::mem::take(&mut *lock)
        };

        let mut count = 0;
        for path in &paths {
            match manager.reload_skill(path) {
                Ok(()) => count += 1,
                Err(e) => tracing::warn!(?path, error = %e, "Failed to reload skill"),
            }
        }
        count
    }
}

impl Default for SkillWatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Process notify events in a loop on a background thread.
///
/// Filters for SKILL.md / *.md file changes and pushes the changed paths into
/// the shared list. The [`SkillWatcher::check_and_reload`] method later drains
/// this list and reloads the affected skills.
fn drain_events(rx: Receiver<notify::Result<Event>>, changed: &Arc<Mutex<Vec<PathBuf>>>) {
    loop {
        match rx.recv() {
            Ok(Ok(event)) => {
                let skill_changes: Vec<PathBuf> = event
                    .paths
                    .into_iter()
                    .filter(|p| is_skill_file(p))
                    .collect();

                if !skill_changes.is_empty() {
                    if let Ok(mut lock) = changed.lock() {
                        for p in skill_changes {
                            if !lock.contains(&p) {
                                lock.push(p);
                            }
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "Skill watcher notify error");
            }
            Err(_) => {
                // Channel closed — sender dropped, stop the thread.
                tracing::debug!("Skill watcher channel closed, exiting thread");
                break;
            }
        }
    }
}

/// Returns `true` if the file is a skill markdown file.
///
/// Matches:
/// - `SKILL.md` (subdirectory format: `skills/<name>/SKILL.md`)
/// - `*.md` (flat format: `<name>.md`)
fn is_skill_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e == "md")
        .unwrap_or(false)
}
