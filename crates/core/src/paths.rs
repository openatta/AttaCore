//! Data directory path abstraction.
//!
//! `ConfigPaths` provides a single source of truth for all persistent
//! directories used by the AGENT: a flat cross-scene **global** root
//! (`~/.atta/`), a **scene**-specific override root nested under it
//! (`~/.atta/scenes/<scope>/`), and a **local**/project root (`<cwd>/.atta/`).
//!
//! `scope` identifies which product built on this engine owns the
//! scene-specific override state (e.g. a coding-agent product might use
//! `"coding"`). The engine never assumes a default — callers must pass one
//! explicitly. The local/project root is flat and has no scope segment: a
//! project is assumed to be worked on by one product instance at a time, so
//! there's no collision to avoid the way there is under a shared `$HOME`.
//!
//! **2026-08-04 restructuring**: the user-level tree used to be entirely
//! scene-scoped (`~/.atta/<scope>/{memory,sessions,vcr,skills,...}`). It is
//! now split by resource type:
//!
//! - **Global + scene override** (config-shaped, keyed by name): `settings.json`,
//!   `skills/`, `plugins/`, `agents/`, `rules/`, `hooks/`. Present at both
//!   `~/.atta/<resource>/` (base) and `~/.atta/scenes/<scope>/<resource>/`
//!   (override) — a scene-specific entry with the same name wins.
//! - **Global + project only, no scene tier** (history/state-shaped):
//!   `memory/`, `sessions/`, `vcr/`, `mcp/`. Present at `~/.atta/<resource>/`
//!   (used when there's no specific project — reserved for a future
//!   no-project/desktop mode, not yet exercised by `daemon`) and
//!   `<cwd>/.atta/<resource>/` (project, used today). Deliberately **not**
//!   scene-scoped: a scene's tool/system-prompt config has no bearing on
//!   which project a session or memory file belongs to, so borrowing across
//!   scenes would be a correctness hazard (e.g. resuming a session recorded
//!   under a different scene's tool set, or VCR tapes replaying against the
//!   wrong tool config).
//! - **Removed**: `workflows/` — the named-workflow feature was dropped
//!   before it had a loader; there is no replacement path.
//!
//! Project-level skills are **not** under `ConfigPaths` at all — they live in
//! `.agents/skills/`, a sibling of `.atta/`, because that's the path Codex
//! and other external agent tools scan. `ConfigPaths` only models AttaCore's
//! own private state and extensions, which no other tool reads.

use std::path::{Path, PathBuf};

/// Unified management of AGENT persistent directory paths.
///
/// Default: `global_data_dir = $HOME/.atta/`
///          `user_data_dir   = $HOME/.atta/scenes/<scope>/`
///          `local_data_dir  = <cwd>/.atta/`
///
/// `global_data_dir` (and therefore `user_data_dir`, derived from it) can be
/// overridden via `ATTA_DATA_DIR`; `local_data_dir` via `ATTA_LOCAL_DATA_DIR`.
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    /// User-level, scene-specific override root. Default `$HOME/.atta/scenes/<scope>/`.
    pub user_data_dir: PathBuf,
    /// User-level, cross-scene global root (flat, shared by every scene).
    /// Default `$HOME/.atta/`.
    pub global_data_dir: PathBuf,
    /// Local/project data root. Default `<cwd>/.atta/`.
    pub local_data_dir: PathBuf,
}

impl ConfigPaths {
    /// Build from environment. Respects `ATTA_DATA_DIR` / `ATTA_LOCAL_DATA_DIR`
    /// overrides.
    ///
    /// `scope` has no default at this layer — it identifies which product
    /// instance's scene-specific override state this is. Callers (e.g.
    /// `daemon`) decide what to pass, and may apply their own default before
    /// calling this.
    pub fn from_env(cwd: &Path, scope: &str) -> Self {
        let global_data_dir = std::env::var("ATTA_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs_home().join(".atta"));
        let user_data_dir = global_data_dir.join("scenes").join(scope);
        let local_data_dir = std::env::var("ATTA_LOCAL_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| cwd.join(".atta"));

        Self {
            user_data_dir,
            global_data_dir,
            local_data_dir,
        }
    }

    // ── Scene-tier convenience methods (override; falls back to the
    //    matching `global_*` method when the caller finds nothing here) ──

    pub fn user_settings_path(&self) -> PathBuf {
        self.user_data_dir.join("settings.json")
    }

    pub fn user_skills_dir(&self) -> PathBuf {
        self.user_data_dir.join("skills")
    }

    /// Scene-specific installed plugins: `~/.atta/scenes/<scope>/plugins/`.
    pub fn user_plugins_dir(&self) -> PathBuf {
        self.user_data_dir.join("plugins")
    }

    /// Scene-specific subagent definitions: `~/.atta/scenes/<scope>/agents/`.
    /// AttaCore extension, no external standard. Loader not yet implemented.
    pub fn user_agents_dir(&self) -> PathBuf {
        self.user_data_dir.join("agents")
    }

    /// Scene-specific rule documents: `~/.atta/scenes/<scope>/rules/`.
    /// AttaCore extension, no external standard. Loader not yet implemented.
    pub fn user_rules_dir(&self) -> PathBuf {
        self.user_data_dir.join("rules")
    }

    /// Scene-specific hook scripts: `~/.atta/scenes/<scope>/hooks/`.
    /// AttaCore extension, no external standard. Loader not yet implemented.
    pub fn user_hooks_dir(&self) -> PathBuf {
        self.user_data_dir.join("hooks")
    }

    // ── Global-tier convenience methods (flat, cross-scene) ──

    pub fn global_settings_path(&self) -> PathBuf {
        self.global_data_dir.join("settings.json")
    }

    pub fn global_skills_dir(&self) -> PathBuf {
        self.global_data_dir.join("skills")
    }

    pub fn global_plugins_dir(&self) -> PathBuf {
        self.global_data_dir.join("plugins")
    }

    pub fn global_agents_dir(&self) -> PathBuf {
        self.global_data_dir.join("agents")
    }

    pub fn global_rules_dir(&self) -> PathBuf {
        self.global_data_dir.join("rules")
    }

    pub fn global_hooks_dir(&self) -> PathBuf {
        self.global_data_dir.join("hooks")
    }

    /// Cross-scene memory root — used when there's no specific project open
    /// (reserved for future no-project/desktop use; today sessions always
    /// have a project, so `local_memory_dir()` is what's actually read).
    pub fn global_memory_dir(&self) -> PathBuf {
        self.global_data_dir.join("memory")
    }

    pub fn global_sessions_dir(&self) -> PathBuf {
        self.global_data_dir.join("sessions")
    }

    pub fn global_mcp_dir(&self) -> PathBuf {
        self.global_data_dir.join("mcp")
    }

    pub fn global_vcr_dir(&self) -> PathBuf {
        self.global_data_dir.join("vcr")
    }

    // ── Local/project-tier convenience methods ──

    pub fn local_settings_path(&self) -> PathBuf {
        self.local_data_dir.join("settings.json")
    }

    pub fn local_memory_dir(&self) -> PathBuf {
        self.local_data_dir.join("memory")
    }

    pub fn local_sessions_dir(&self) -> PathBuf {
        self.local_data_dir.join("sessions")
    }

    pub fn local_mcp_dir(&self) -> PathBuf {
        self.local_data_dir.join("mcp")
    }

    pub fn local_vcr_dir(&self) -> PathBuf {
        self.local_data_dir.join("vcr")
    }
}

/// Returns the user-level, scene-specific override directory:
/// `$HOME/.atta/scenes/<scope>/`.
///
/// `scope` has no default here — see module docs.
pub fn atta_scope_dir(scope: &str) -> PathBuf {
    dirs_home().join(".atta").join("scenes").join(scope)
}

/// Returns the user-level, cross-scene global directory: `$HOME/.atta/`.
pub fn atta_global_dir() -> PathBuf {
    dirs_home().join(".atta")
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_use_user_home_and_cwd() {
        let cwd = Path::new("/tmp/test-project");
        let paths = ConfigPaths::from_env(cwd, "code");
        assert!(paths.user_data_dir.to_string_lossy().contains(".atta"));
        assert!(paths.user_data_dir.to_string_lossy().contains("scenes"));
        assert!(paths.user_data_dir.to_string_lossy().ends_with("code"));
        assert!(paths.global_data_dir.ends_with(".atta"));
        assert!(paths.local_data_dir.to_string_lossy().contains(".atta"));
        // Local/project root is flat: no scope segment appended.
        assert!(paths.local_data_dir.ends_with(".atta"));
    }

    #[test]
    fn scene_dir_nests_under_global_dir() {
        let cwd = Path::new("/tmp/test-project");
        let paths = ConfigPaths::from_env(cwd, "code");
        assert_eq!(
            paths.user_data_dir,
            paths.global_data_dir.join("scenes").join("code")
        );
    }

    #[test]
    fn different_scopes_get_different_user_roots_but_same_global_root() {
        let cwd = Path::new("/tmp/test-project");
        let code_paths = ConfigPaths::from_env(cwd, "code");
        let ops_paths = ConfigPaths::from_env(cwd, "ops");
        assert_ne!(code_paths.user_data_dir, ops_paths.user_data_dir);
        assert_eq!(code_paths.global_data_dir, ops_paths.global_data_dir);
        // Local root is scope-independent (project-level is flat).
        assert_eq!(code_paths.local_data_dir, ops_paths.local_data_dir);
    }

    #[test]
    fn convenience_methods_derive_from_roots() {
        let cwd = Path::new("/tmp/test");
        let paths = ConfigPaths::from_env(cwd, "code");
        assert!(paths.user_settings_path().ends_with("settings.json"));
        assert!(paths.user_skills_dir().ends_with("skills"));
        assert!(paths.user_plugins_dir().ends_with("plugins"));
        assert!(paths.user_agents_dir().ends_with("agents"));
        assert!(paths.user_rules_dir().ends_with("rules"));
        assert!(paths.user_hooks_dir().ends_with("hooks"));
        assert!(paths.global_settings_path().ends_with("settings.json"));
        assert!(paths.global_skills_dir().ends_with("skills"));
        assert!(paths.global_memory_dir().ends_with("memory"));
        assert!(paths.global_sessions_dir().ends_with("sessions"));
        assert!(paths.global_vcr_dir().ends_with("vcr"));
        assert!(paths.global_mcp_dir().ends_with("mcp"));
        assert!(paths.local_memory_dir().ends_with("memory"));
        assert!(paths.local_sessions_dir().ends_with("sessions"));
        assert!(paths.local_vcr_dir().ends_with("vcr"));
        assert!(paths.local_mcp_dir().ends_with("mcp"));
    }

    #[test]
    fn atta_scope_dir_respects_scope_param_and_nests_under_scenes() {
        let code_dir = atta_scope_dir("code");
        let ops_dir = atta_scope_dir("ops");
        assert!(code_dir.ends_with("code"));
        assert!(ops_dir.ends_with("ops"));
        assert_ne!(code_dir, ops_dir);
        assert!(code_dir.to_string_lossy().contains("scenes"));
    }

    #[test]
    fn atta_global_dir_is_flat() {
        let dir = atta_global_dir();
        assert!(dir.ends_with(".atta"));
    }
}
