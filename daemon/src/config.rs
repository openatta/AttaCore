//! Daemon configuration — path abstraction + layered settings loading.
//!
//! The [`DaemonPaths`] trait decouples filesystem layout from configuration
//! loading. Implementors can swap in a tempdir for testing or a custom
//! `ATTA_CONFIG_HOME` at runtime.
//!
//! [`load_daemon_config`] merges `settings.json` layers (user → project),
//! applies CLI overrides, and returns a fully resolved [`DaemonConfig`].

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mcp::config::McpServerConfig;
use permissions::ruleset::RuleSet;

// ── Path abstraction ────────────────────────────────────────────────────

/// Controls where the daemon reads its configuration and writes its runtime
/// files (socket, lock, etc.).
pub trait DaemonPaths: Send + Sync {
    fn config_root(&self) -> PathBuf;
    fn project_root(&self) -> PathBuf;
}

/// Default path provider: `$ATTA_CONFIG_HOME` → `$HOME/.atta/code`.
#[derive(Debug, Clone)]
pub struct DefaultDaemonPaths {
    config_root: PathBuf,
    project_root: PathBuf,
}

impl DefaultDaemonPaths {
    pub fn from_env() -> Self {
        let config_root = if let Ok(p) = std::env::var("ATTA_CONFIG_HOME") {
            PathBuf::from(p)
        } else if let Some(home) = std::env::var_os("HOME") {
            PathBuf::from(home).join(".atta").join("code")
        } else {
            PathBuf::from("/tmp/attacore")
        };
        let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            config_root,
            project_root,
        }
    }

    pub fn new(config_root: PathBuf, project_root: PathBuf) -> Self {
        Self {
            config_root,
            project_root,
        }
    }
}

impl DaemonPaths for DefaultDaemonPaths {
    fn config_root(&self) -> PathBuf {
        self.config_root.clone()
    }
    fn project_root(&self) -> PathBuf {
        self.project_root.clone()
    }
}

/// Fixed-path provider for integration tests.
#[derive(Debug, Clone)]
pub struct StaticDaemonPaths {
    config_root: PathBuf,
    project_root: PathBuf,
}

impl StaticDaemonPaths {
    pub fn new(path: PathBuf) -> Self {
        Self {
            config_root: path.clone(),
            project_root: path,
        }
    }
    pub fn with_project(config_root: PathBuf, project_root: PathBuf) -> Self {
        Self {
            config_root,
            project_root,
        }
    }
}

impl DaemonPaths for StaticDaemonPaths {
    fn config_root(&self) -> PathBuf {
        self.config_root.clone()
    }
    fn project_root(&self) -> PathBuf {
        self.project_root.clone()
    }
}

// ── Config struct ────────────────────────────────────────────────────────

/// Fully resolved daemon configuration.
#[derive(Clone)]
pub struct DaemonConfig {
    pub paths: Arc<dyn DaemonPaths>,
    pub socket_path: PathBuf,
    pub lock_path: PathBuf,
    pub session_cap: usize,
    pub model: String,
    pub max_tokens: u32,
    pub mcp_servers: HashMap<String, McpServerConfig>,
    pub tcp_addr: Option<SocketAddr>,
    pub tcp_token: Option<String>,
    pub permission_rules: RuleSet,
    /// Session 空闲超时秒数，超时后自动回收（默认 3600 = 1 小时）。
    pub session_idle_timeout_secs: u64,
}

impl std::fmt::Debug for DaemonConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonConfig")
            .field("socket_path", &self.socket_path)
            .field("lock_path", &self.lock_path)
            .field("session_cap", &self.session_cap)
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("mcp_servers", &self.mcp_servers.keys().collect::<Vec<_>>())
            .field("tcp_addr", &self.tcp_addr)
            .field("tcp_token", &"...")
            .field("paths", &"...")
            .field(
                "permission_rules",
                &format!("RuleSet({})", self.permission_rules.len()),
            )
            .finish()
    }
}

impl DaemonConfig {
    pub fn minimal(paths: Arc<dyn DaemonPaths>) -> Self {
        let config_root = paths.config_root();
        Self {
            socket_path: socket_path_from_root(&config_root),
            lock_path: lock_path_from_root(&config_root),
            paths,
            session_cap: 32,
            model: "claude-sonnet-4-6".into(),
            max_tokens: 2000,
            mcp_servers: HashMap::new(),
            tcp_addr: None,
            tcp_token: None,
            permission_rules: RuleSet::empty(),
            session_idle_timeout_secs: 3600,
        }
    }
}

// ── Loading ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
struct SettingsFile {
    model: Option<String>,
    max_tokens: Option<u32>,
    #[serde(default)]
    mcp_servers: HashMap<String, McpServerConfig>,
}

pub fn load_daemon_config(
    cli_model: &str,
    cli_max_tokens: u32,
    cli_socket: Option<&Path>,
    paths: &dyn DaemonPaths,
) -> DaemonConfig {
    let config_root = paths.config_root();
    let project_root = paths.project_root();

    let user_path = config_root.join("settings.json");
    let mut merged = load_single(&user_path).unwrap_or_default();

    let project_path = project_root
        .join(".atta")
        .join("code")
        .join("settings.json");
    if let Some(proj) = load_single(&project_path) {
        merged = merge_settings(merged, proj);
    }

    let socket_path = cli_socket
        .map(PathBuf::from)
        .unwrap_or_else(|| socket_path_from_root(&config_root));
    let lock_path = lock_path_from_root(&config_root);

    DaemonConfig {
        socket_path,
        lock_path,
        paths: Arc::new(StaticDaemonPaths::with_project(config_root, project_root)),
        session_cap: 32,
        model: merged.model.unwrap_or_else(|| cli_model.to_string()),
        max_tokens: merged.max_tokens.unwrap_or(cli_max_tokens),
        mcp_servers: merged.mcp_servers,
        tcp_addr: None,
        tcp_token: None,
        permission_rules: RuleSet::empty(),
        session_idle_timeout_secs: 3600,
    }
}

fn load_single(path: &Path) -> Option<SettingsFile> {
    if !path.exists() {
        return None;
    }
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

fn merge_settings(mut base: SettingsFile, over: SettingsFile) -> SettingsFile {
    if over.model.is_some() {
        base.model = over.model;
    }
    if over.max_tokens.is_some() {
        base.max_tokens = over.max_tokens;
    }
    if !over.mcp_servers.is_empty() {
        base.mcp_servers = over.mcp_servers;
    }
    base
}

pub fn socket_path_from_root(root: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(r"\\.\pipe\attacore-daemon")
    }
    #[cfg(not(windows))]
    {
        root.join("daemon.sock")
    }
}

pub fn lock_path_from_root(root: &Path) -> PathBuf {
    root.join("daemon.lock")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_settings(dir: &Path, content: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("settings.json"), content).unwrap();
    }

    fn write_project_settings(project: &Path, content: &str) {
        let atta_dir = project.join(".atta").join("code");
        std::fs::create_dir_all(&atta_dir).unwrap();
        std::fs::write(atta_dir.join("settings.json"), content).unwrap();
    }

    #[test]
    fn default_paths_from_env_falls_back_to_home() {
        let paths = DefaultDaemonPaths::from_env();
        let root = paths.config_root();
        assert!(!root.as_os_str().is_empty());
    }

    #[test]
    fn static_paths_returns_configured_dirs() {
        let paths = StaticDaemonPaths::new(PathBuf::from("/test/config"));
        assert_eq!(paths.config_root(), PathBuf::from("/test/config"));
        assert_eq!(paths.project_root(), PathBuf::from("/test/config"));
    }

    #[test]
    fn socket_path_derived_from_config_root() {
        #[cfg(not(windows))]
        {
            let p = socket_path_from_root(Path::new("/home/user/.atta/code"));
            assert_eq!(p, PathBuf::from("/home/user/.atta/code/daemon.sock"));
        }
    }

    #[test]
    fn lock_path_derived_from_config_root() {
        let p = lock_path_from_root(Path::new("/home/user/.atta/code"));
        assert_eq!(p, PathBuf::from("/home/user/.atta/code/daemon.lock"));
    }

    #[test]
    fn load_single_returns_none_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_single(&dir.path().join("nonexistent.json")).is_none());
    }

    #[test]
    fn load_single_parses_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        write_settings(
            dir.path(),
            r#"{"model": "claude-sonnet-4-6", "max_tokens": 4096}"#,
        );
        let s = load_single(&dir.path().join("settings.json")).unwrap();
        assert_eq!(s.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(s.max_tokens, Some(4096));
    }

    #[test]
    fn load_daemon_config_cli_fallback_when_no_settings() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StaticDaemonPaths::new(dir.path().to_path_buf());
        let config = load_daemon_config("cli-model", 5000, None, &paths);
        assert_eq!(config.model, "cli-model");
        assert_eq!(config.max_tokens, 5000);
    }

    #[test]
    fn load_daemon_config_project_overrides_user() {
        let config_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        write_settings(
            config_dir.path(),
            r#"{"model": "user-model", "max_tokens": 1000}"#,
        );
        write_project_settings(project_dir.path(), r#"{"model": "project-model"}"#);
        let paths = StaticDaemonPaths::with_project(
            config_dir.path().to_path_buf(),
            project_dir.path().to_path_buf(),
        );
        let config = load_daemon_config("cli-model", 2000, None, &paths);
        assert_eq!(config.model, "project-model");
        assert_eq!(config.max_tokens, 1000);
    }

    #[test]
    fn minimal_config_uses_sensible_defaults() {
        let paths = Arc::new(StaticDaemonPaths::new(PathBuf::from("/tmp/test")));
        let config = DaemonConfig::minimal(paths.clone());
        assert_eq!(config.session_cap, 32);
        assert!(config.mcp_servers.is_empty());
        assert!(config.tcp_addr.is_none());
    }
}
