//! Data directory path abstraction.
//!
//! `ConfigPaths` provides a single source of truth for all persistent
//! directories used by the AGENT: user-level (`~/.atta/code/`) and
//! local-level (`<cwd>/.atta/code/`).

use std::path::{Path, PathBuf};

/// Unified management of AGENT persistent directory paths.
///
/// Default: `user_data_dir = $HOME/.atta/code/`
///          `local_data_dir = <cwd>/.atta/code/`
/// Override via `ATTA_DATA_DIR` env var.
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    /// User-level data root directory. Default `$HOME/.atta/code/`
    pub user_data_dir: PathBuf,
    /// Local/project data root directory. Default `<cwd>/.atta/code/`
    pub local_data_dir: PathBuf,
}

impl ConfigPaths {
    /// Build from environment. Respects `ATTA_DATA_DIR` override.
    pub fn from_env(cwd: &Path) -> Self {
        let user_default = dirs_home().join(".atta").join("code");
        let local_default = cwd.join(".atta").join("code");

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

    pub fn local_skills_dir(&self) -> PathBuf {
        self.local_data_dir.join("skills")
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

/// Returns the user-level `.atta/code/` directory.
///
/// Equivalent to `$HOME/.atta/code/`. Relies on `HOME` env var.
pub fn atta_code_dir() -> PathBuf {
    dirs_home().join(".atta").join("code")
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
        let paths = ConfigPaths::from_env(cwd);
        assert!(paths.user_data_dir.to_string_lossy().contains(".atta"));
        assert!(paths.local_data_dir.to_string_lossy().contains(".atta"));
    }

    #[test]
    fn convenience_methods_derive_from_roots() {
        let cwd = Path::new("/tmp/test");
        let paths = ConfigPaths::from_env(cwd);
        assert!(paths.user_settings_path().ends_with("settings.json"));
        assert!(paths.user_sessions_dir().ends_with("sessions"));
    }
}
