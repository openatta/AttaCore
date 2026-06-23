//! Discovery lock file.
//!
//! The daemon writes `~/.atta/code/daemon.lock` (mode 0600) on startup
//! with `{pid, socket_path, version, started_at}`. IDE plugins read
//! this file to find a running daemon — no env var hunting, no port scan.
//!
//! On graceful shutdown the file is removed. On crash, the next daemon
//! startup detects the stale file (pid no longer alive) and overwrites.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonLock {
    /// Daemon PID (for liveness check by clients).
    pub pid: u32,
    /// Absolute path of the unix socket the daemon is listening on.
    pub socket_path: PathBuf,
    /// Daemon binary version (Cargo).
    pub version: String,
    /// Unix timestamp (seconds) the lock was written.
    pub started_at: i64,
    /// Protocol version the daemon speaks; clients refuse mismatched majors.
    pub protocol_version: String,
}

/// Write the lock file with `0600` perms on Unix. If a lock file already
/// exists, examine its `pid`: alive → return Err so the new daemon refuses
/// to start; dead → overwrite with our info.
pub fn write_lock_file(lock_path: &Path, socket_path: &Path) -> Result<DaemonLock, LockFileError> {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(LockFileError::Io)?;
    }
    if lock_path.exists() {
        match fs::read(lock_path) {
            Ok(bytes) => {
                if let Ok(prev) = serde_json::from_slice::<DaemonLock>(&bytes) {
                    if pid_alive(prev.pid) {
                        return Err(LockFileError::AnotherDaemonRunning {
                            pid: prev.pid,
                            socket_path: prev.socket_path,
                        });
                    }
                    tracing::info!(
                        stale_pid = prev.pid,
                        "stale daemon.lock from dead pid; overwriting"
                    );
                }
            }
            Err(_) => { /* unreadable — overwrite */ }
        }
    }

    let lock = DaemonLock {
        pid: std::process::id(),
        socket_path: socket_path.to_path_buf(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        protocol_version: "1".into(),
    };
    let body = serde_json::to_string_pretty(&lock).map_err(LockFileError::Json)?;
    let tmp = lock_path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, body).map_err(LockFileError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&tmp).map_err(LockFileError::Io)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&tmp, perms).map_err(LockFileError::Io)?;
    }
    fs::rename(&tmp, lock_path).map_err(LockFileError::Io)?;
    Ok(lock)
}

/// Read an existing lock file.
#[allow(dead_code)]
pub fn read_lock_file(lock_path: &Path) -> Result<Option<DaemonLock>, LockFileError> {
    if !lock_path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(lock_path).map_err(LockFileError::Io)?;
    let lock: DaemonLock = serde_json::from_slice(&bytes).map_err(LockFileError::Json)?;
    Ok(Some(lock))
}

/// Best-effort liveness probe — `kill -0 pid` semantics on Unix.
#[allow(unsafe_code)]
fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if r == 0 {
            return true;
        }
        let err = std::io::Error::last_os_error();
        err.raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LockFileError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("malformed daemon.lock: {0}")]
    Json(#[from] serde_json::Error),
    #[error("another daemon is already running (pid={pid}, socket={})", socket_path.display())]
    AnotherDaemonRunning { pid: u32, socket_path: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_then_read_round_trips() {
        let dir = TempDir::new().unwrap();
        let lock = dir.path().join("daemon.lock");
        let sock = dir.path().join("daemon.sock");
        let written = write_lock_file(&lock, &sock).unwrap();
        let read = read_lock_file(&lock).unwrap().unwrap();
        assert_eq!(read.pid, written.pid);
        assert_eq!(read.socket_path, sock);
        assert_eq!(read.protocol_version, "1");
    }

    #[test]
    fn write_overwrites_stale_lock() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("daemon.lock");
        let stale = DaemonLock {
            pid: 999_999_999,
            socket_path: PathBuf::from("/tmp/whatever.sock"),
            version: "0.0.0".into(),
            started_at: 0,
            protocol_version: "1".into(),
        };
        std::fs::write(&lock_path, serde_json::to_vec(&stale).unwrap()).unwrap();
        let sock = dir.path().join("new.sock");
        let written = write_lock_file(&lock_path, &sock).unwrap();
        assert_ne!(written.pid, 999_999_999);
        assert_eq!(written.socket_path, sock);
    }

    #[test]
    fn read_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        let r = read_lock_file(&dir.path().join("does-not-exist")).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn write_rejects_when_existing_pid_is_alive() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("daemon.lock");
        let alive = DaemonLock {
            pid: std::process::id(),
            socket_path: PathBuf::from("/tmp/x.sock"),
            version: "9.9.9".into(),
            started_at: 0,
            protocol_version: "1".into(),
        };
        std::fs::write(&lock_path, serde_json::to_vec(&alive).unwrap()).unwrap();
        let r = write_lock_file(&lock_path, &dir.path().join("new.sock"));
        assert!(matches!(r, Err(LockFileError::AnotherDaemonRunning { .. })));
    }
}
