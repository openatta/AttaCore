//! Data directory path abstraction.
//!
//! `ConfigPaths` provides a single source of truth for all persistent
//! directories used by the AGENT: user-level (`~/.atta/<scope>/`) and
//! local-level (`<cwd>/.atta/`).
//!
//! `scope` identifies which product built on this engine owns the user-level
//! state (e.g. a coding-agent product might use `"code"`). The engine never
//! assumes a default — callers must pass one explicitly. The local/project
//! root is flat and has no scope segment: a project is assumed to be worked
//! on by one product instance at a time, so there's no collision to avoid
//! the way there is under a shared `$HOME`.
//!
//! Project-level skills are **not** under `ConfigPaths` at all — they live in
//! `.agents/skills/`, a sibling of `.atta/`, because that's the path Codex
//! and other external agent tools scan. `ConfigPaths` only models AttaCore's
//! own private state and extensions (workflows/agents/rules/hooks), which no
//! other tool reads.

use std::path::{Path, PathBuf};

/// Unified management of AGENT persistent directory paths.
///
/// Default: `user_data_dir = $HOME/.atta/<scope>/`
///          `local_data_dir = <cwd>/.atta/`
/// Override via `ATTA_DATA_DIR` env var.
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    /// User-level data root directory. Default `$HOME/.atta/<scope>/`
    pub user_data_dir: PathBuf,
    /// Local/project data root directory. Default `<cwd>/.atta/`
    pub local_data_dir: PathBuf,
}

impl ConfigPaths {
    /// Build from environment. Respects `ATTA_DATA_DIR` override.
    ///
    /// `scope` has no default at this layer — it identifies which product
    /// instance's user-level state this is. Callers (e.g. `daemon`) decide
    /// what to pass, and may apply their own default before calling this.
    pub fn from_env(cwd: &Path, scope: &str) -> Self {
        let user_default = dirs_home().join(".atta").join(scope);
        let local_default = cwd.join(".atta");

        let user_data_dir = std::env::var("ATTA_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or(user_default);
        let local_data_dir = std::env::var("ATTA_LOCAL_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or(local_default);

        Self {
            user_data_dir,
            local_data_dir,
        }
    }

    // ── Convenience methods ──

    pub fn user_settings_path(&self) -> PathBuf {
        self.user_data_dir.join("settings.json")
    }

    pub fn local_settings_path(&self) -> PathBuf {
        self.local_data_dir.join("settings.json")
    }

    pub fn user_skills_dir(&self) -> PathBuf {
        self.user_data_dir.join("skills")
    }

    /// User-level workflows dir: `~/.atta/<scope>/workflows/`. AttaCore
    /// extension, no external standard.
    pub fn user_workflows_dir(&self) -> PathBuf {
        self.user_data_dir.join("workflows")
    }

    /// User-level subagent definitions: `~/.atta/<scope>/agents/`. AttaCore
    /// extension, no external standard.
    pub fn user_agents_dir(&self) -> PathBuf {
        self.user_data_dir.join("agents")
    }

    /// User-level rule documents: `~/.atta/<scope>/rules/`. AttaCore
    /// extension, no external standard.
    pub fn user_rules_dir(&self) -> PathBuf {
        self.user_data_dir.join("rules")
    }

    /// User-level hook scripts: `~/.atta/<scope>/hooks/`. AttaCore
    /// extension, no external standard.
    pub fn user_hooks_dir(&self) -> PathBuf {
        self.user_data_dir.join("hooks")
    }

    pub fn user_memory_dir(&self) -> PathBuf {
        self.user_data_dir.join("memory")
    }

    pub fn local_memory_dir(&self) -> PathBuf {
        self.local_data_dir.join("memory")
    }

    pub fn user_mcp_dir(&self) -> PathBuf {
        self.user_data_dir.join("mcp")
    }

    pub fn local_mcp_dir(&self) -> PathBuf {
        self.local_data_dir.join("mcp")
    }

    pub fn user_sessions_dir(&self) -> PathBuf {
        self.user_data_dir.join("sessions")
    }

    pub fn local_vcr_dir(&self) -> PathBuf {
        self.local_data_dir.join("vcr")
    }

    pub fn user_vcr_dir(&self) -> PathBuf {
        self.user_data_dir.join("vcr")
    }
}

/// Returns the user-level `.atta/<scope>/` directory.
///
/// Equivalent to `$HOME/.atta/<scope>/`. Relies on `HOME` env var. `scope`
/// has no default here — see module docs.
pub fn atta_scope_dir(scope: &str) -> PathBuf {
    dirs_home().join(".atta").join(scope)
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
        assert!(paths.user_data_dir.to_string_lossy().ends_with("code"));
        assert!(paths.local_data_dir.to_string_lossy().contains(".atta"));
        // Local/project root is flat: no scope segment appended.
        assert!(paths.local_data_dir.ends_with(".atta"));
    }

    #[test]
    fn different_scopes_get_different_user_roots() {
        let cwd = Path::new("/tmp/test-project");
        let code_paths = ConfigPaths::from_env(cwd, "code");
        let ops_paths = ConfigPaths::from_env(cwd, "ops");
        assert_ne!(code_paths.user_data_dir, ops_paths.user_data_dir);
        // Local root is scope-independent (project-level is flat).
        assert_eq!(code_paths.local_data_dir, ops_paths.local_data_dir);
    }

    #[test]
    fn convenience_methods_derive_from_roots() {
        let cwd = Path::new("/tmp/test");
        let paths = ConfigPaths::from_env(cwd, "code");
        assert!(paths.user_settings_path().ends_with("settings.json"));
        assert!(paths.user_sessions_dir().ends_with("sessions"));
        assert!(paths.user_workflows_dir().ends_with("workflows"));
        assert!(paths.user_agents_dir().ends_with("agents"));
        assert!(paths.user_rules_dir().ends_with("rules"));
        assert!(paths.user_hooks_dir().ends_with("hooks"));
    }

    #[test]
    fn atta_scope_dir_respects_scope_param() {
        let code_dir = atta_scope_dir("code");
        let ops_dir = atta_scope_dir("ops");
        assert!(code_dir.ends_with("code"));
        assert!(ops_dir.ends_with("ops"));
        assert_ne!(code_dir, ops_dir);
    }
}
