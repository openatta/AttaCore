//! Tool security helpers — path validation, sandbox detection, safe guards.

use std::path::{Component, Path, PathBuf};

/// Validate that a file path is within one of the allowed working directories.
/// Returns Ok(()) if the path is safe, or Err with a description.
pub fn validate_path_within_bounds(
    target: &Path,
    cwd: &Path,
    additional_dirs: &[PathBuf],
) -> Result<(), String> {
    // Canonicalize if possible, fall back to absolute form
    let resolved = canonicalize_best_effort(target);

    // Check against primary cwd
    let cwd_resolved = canonicalize_best_effort(cwd);
    if resolved.starts_with(&cwd_resolved) {
        return Ok(());
    }

    // Check against additional dirs
    for dir in additional_dirs {
        let dir_resolved = canonicalize_best_effort(dir);
        if resolved.starts_with(&dir_resolved) {
            return Ok(());
        }
    }

    Err(format!(
        "Path {:?} is outside allowed working directories. \
         Primary: {:?}",
        target, cwd
    ))
}

/// Detect path traversal attempts via `../` sequences.
pub fn contains_path_traversal(path: &Path) -> bool {
    path.components().any(|c| matches!(c, Component::ParentDir))
}

/// Check if the current platform supports sandbox execution.
/// macOS: sandbox-exec available; Linux: bwrap available.
pub fn platform_sandbox_available() -> bool {
    if cfg!(target_os = "macos") {
        std::process::Command::new("which")
            .arg("sandbox-exec")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    } else if cfg!(target_os = "linux") {
        std::process::Command::new("which")
            .arg("bwrap")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    } else {
        false
    }
}

/// Canonicalize a path, falling back to the input path on error.
pub fn canonicalize_best_effort(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| {
        // On non-existent paths, resolve parent and append
        if let Some(parent) = p.parent() {
            let parent_ok = parent
                .canonicalize()
                .unwrap_or_else(|_| parent.to_path_buf());
            parent_ok.join(p.file_name().unwrap_or_default())
        } else {
            p.to_path_buf()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_path_is_allowed() {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp"));
        let target = cwd.join("test.txt");
        assert!(validate_path_within_bounds(&target, &cwd, &[]).is_ok());
    }

    #[test]
    fn detects_traversal() {
        assert!(contains_path_traversal(Path::new("../etc/passwd")));
        assert!(!contains_path_traversal(Path::new("foo/bar.txt")));
    }
}

// ── Write policy types ──

#[derive(Debug, Clone)]
pub enum PathSafetyError {
    OutsideAllowedRoots {
        path: PathBuf,
        allowed: Vec<PathBuf>,
    },
    Other(String),
}

#[derive(Debug, Clone)]
pub struct WritePolicy {
    roots: Vec<PathBuf>,
}
impl WritePolicy {
    pub fn new(cwd: PathBuf) -> Self {
        Self { roots: vec![cwd] }
    }
    pub fn with_additional_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.roots.extend(roots);
        self
    }
}

pub fn check_write(path: &Path, policy: &WritePolicy) -> Result<(), PathSafetyError> {
    let resolved = canonicalize_best_effort(path);
    for root in &policy.roots {
        if resolved.starts_with(canonicalize_best_effort(root)) {
            return Ok(());
        }
    }
    Err(PathSafetyError::OutsideAllowedRoots {
        path: path.to_path_buf(),
        allowed: policy.roots.clone(),
    })
}

pub fn is_path_within_root(path: &Path, root: &Path) -> bool {
    canonicalize_best_effort(path).starts_with(canonicalize_best_effort(root))
}

pub fn normalize_path_lexically(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => {
                out.push(other);
            }
        }
    }
    out
}

/// Whether `url`'s host is loopback (`127.0.0.1`, `::1`, `localhost`).
/// Used to decide whether an HTTP client should bypass the ambient
/// `HTTP_PROXY`/`HTTPS_PROXY` for this specific request — see callers
/// (`web_fetch.rs`, `ping.rs`) for why: those env vars mean "reach the real
/// internet through this egress proxy," not "also proxy calls back into my
/// own machine," but reqwest has no built-in way to express that distinction
/// short of the `NO_PROXY` env var, which callers of this tool don't control.
/// Malformed URLs are treated as non-loopback (fail open to the existing,
/// proxy-respecting behavior — never silently bypass a proxy for a URL we
/// couldn't even parse).
pub fn is_loopback_url(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        // IPv6 hosts come back bracketed (`"[::1]"`), unlike v4/hostnames.
        .is_some_and(|h| {
            let h = h.trim_start_matches('[').trim_end_matches(']');
            h == "localhost" || h == "127.0.0.1" || h == "::1"
        })
}

#[cfg(test)]
mod loopback_tests {
    use super::*;

    #[test]
    fn recognizes_loopback_hosts() {
        assert!(is_loopback_url("http://127.0.0.1:8080/health"));
        assert!(is_loopback_url("http://localhost/"));
        assert!(is_loopback_url("https://localhost:9000"));
        assert!(is_loopback_url("http://[::1]:3000"));
    }

    #[test]
    fn does_not_flag_real_external_hosts() {
        assert!(!is_loopback_url("https://example.com"));
        assert!(!is_loopback_url("https://10.211.55.2:8001"));
        assert!(!is_loopback_url("https://api.anthropic.com/v1/messages"));
    }

    #[test]
    fn malformed_url_fails_open_to_non_loopback() {
        // Never silently bypass the proxy for a URL we couldn't even parse.
        assert!(!is_loopback_url("not a url at all"));
        assert!(!is_loopback_url(""));
    }
}
