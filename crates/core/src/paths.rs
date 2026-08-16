//! Data directory path abstraction.
//!
//! `ConfigPaths` provides a single source of truth for all persistent
//! directories used by the AGENT: a flat cross-scene **global** root
//! (conventionally `~/.atta/`), a **scene**-specific override root nested
//! under it (`<global>/scenes/<scope>/`), and a **local**/project root
//! (`<cwd>/.atta/`).
//!
//! **The roots are given, never discovered.** Nothing in this module reads
//! the environment: the process entry point decides which directory the
//! instance owns and passes it down. `~/.atta` is what that entry point
//! usually picks, not something the library assumes — which is what lets a
//! test, a service account, or a deployment with its state elsewhere work
//! without every module having to agree separately.
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

use std::path::PathBuf;

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
    /// Build from explicit roots.
    ///
    /// There is no constructor that consults the environment, and that is the
    /// point: a process serves one instance, and which directory that
    /// instance owns is the caller's to decide. Reading `$HOME` here would
    /// make every downstream module silently agree on the invoking user's
    /// home, which is wrong for a test, wrong for a service account, and
    /// wrong for any deployment that keeps its state somewhere else.
    ///
    /// `scope` identifies the product instance whose scene-specific override
    /// tree lives under `global_data_dir/scenes/<scope>/`.
    pub fn new(global_data_dir: PathBuf, local_data_dir: PathBuf, scope: &str) -> Self {
        Self {
            user_data_dir: global_data_dir.join("scenes").join(scope),
            global_data_dir,
            local_data_dir,
        }
    }

    /// The same roots [`crate::interface::settings::Settings`] already
    /// carries — for the call sites that hold `Settings` and want these
    /// accessors without re-deriving anything.
    pub fn from_settings(paths: &crate::interface::settings::PathSettings) -> Self {
        Self {
            user_data_dir: paths.user_data_dir.clone(),
            global_data_dir: paths.global_data_dir.clone(),
            local_data_dir: paths.local_data_dir.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> ConfigPaths {
        ConfigPaths::new(
            PathBuf::from("/state/.atta"),
            PathBuf::from("/tmp/test-project/.atta"),
            "code",
        )
    }

    #[test]
    fn roots_are_exactly_what_the_caller_passed() {
        let p = paths();
        assert_eq!(p.global_data_dir, PathBuf::from("/state/.atta"));
        assert_eq!(p.local_data_dir, PathBuf::from("/tmp/test-project/.atta"));
    }

    #[test]
    fn scene_dir_nests_under_global_dir() {
        let p = paths();
        assert_eq!(
            p.user_data_dir,
            p.global_data_dir.join("scenes").join("code")
        );
    }

    #[test]
    fn different_scopes_get_different_user_roots_but_same_global_root() {
        let global = PathBuf::from("/state/.atta");
        let local = PathBuf::from("/tmp/p/.atta");
        let code = ConfigPaths::new(global.clone(), local.clone(), "code");
        let ops = ConfigPaths::new(global, local, "ops");
        assert_ne!(code.user_data_dir, ops.user_data_dir);
        assert_eq!(code.global_data_dir, ops.global_data_dir);
        // Local root is scope-independent (project-level is flat).
        assert_eq!(code.local_data_dir, ops.local_data_dir);
    }

    /// The roots must survive the trip through `Settings` unchanged —
    /// `PathSettings` is how they actually reach most call sites.
    #[test]
    fn settings_roots_round_trip() {
        let p = paths();
        let carried = crate::interface::settings::PathSettings {
            user_data_dir: p.user_data_dir.clone(),
            global_data_dir: p.global_data_dir.clone(),
            local_data_dir: p.local_data_dir.clone(),
            scope: "code".into(),
        };
        let back = ConfigPaths::from_settings(&carried);
        assert_eq!(back.user_data_dir, p.user_data_dir);
        assert_eq!(back.global_data_dir, p.global_data_dir);
        assert_eq!(back.local_data_dir, p.local_data_dir);
    }

    #[test]
    fn convenience_methods_derive_from_roots() {
        let p = paths();
        assert_eq!(
            p.user_settings_path(),
            p.user_data_dir.join("settings.json")
        );
        assert_eq!(p.user_skills_dir(), p.user_data_dir.join("skills"));
        assert_eq!(p.user_plugins_dir(), p.user_data_dir.join("plugins"));
        assert_eq!(p.user_agents_dir(), p.user_data_dir.join("agents"));
        assert_eq!(p.user_rules_dir(), p.user_data_dir.join("rules"));
        assert_eq!(p.user_hooks_dir(), p.user_data_dir.join("hooks"));
        assert_eq!(
            p.global_settings_path(),
            p.global_data_dir.join("settings.json")
        );
        assert_eq!(p.global_skills_dir(), p.global_data_dir.join("skills"));
        assert_eq!(p.global_memory_dir(), p.global_data_dir.join("memory"));
        assert_eq!(p.global_sessions_dir(), p.global_data_dir.join("sessions"));
        assert_eq!(p.global_vcr_dir(), p.global_data_dir.join("vcr"));
        assert_eq!(p.global_mcp_dir(), p.global_data_dir.join("mcp"));
        assert_eq!(p.local_memory_dir(), p.local_data_dir.join("memory"));
        assert_eq!(p.local_sessions_dir(), p.local_data_dir.join("sessions"));
        assert_eq!(p.local_vcr_dir(), p.local_data_dir.join("vcr"));
        assert_eq!(p.local_mcp_dir(), p.local_data_dir.join("mcp"));
    }
}
