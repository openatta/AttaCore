//! Turning a manifest's capability declaration into something the host
//! enforces.
//!
//! The manifest is a promise about what a plugin will reach; this module is
//! where that promise becomes the only thing it *can* reach. Two rules shape
//! everything here:
//!
//! - **Nothing is granted by omission.** An undeclared capability is denied,
//!   so a plugin that declares nothing can compute and no more.
//! - **Resolution happens once, at load.** Variables like `${workspace}` are
//!   expanded and paths canonicalized before the component runs, so no check
//!   at call time depends on state a plugin could influence.

use anyhow::{bail, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A capability declaration with its paths resolved against a concrete
/// workspace, ready to enforce.
#[derive(Debug, Clone)]
pub struct ResolvedCapabilities {
    pub fs_read: Vec<PathBuf>,
    pub fs_write: Vec<PathBuf>,
    net: HashSet<String>,
    env: HashSet<String>,
    pub max_memory_bytes: usize,
    pub timeout: std::time::Duration,
}

impl ResolvedCapabilities {
    /// Resolve a manifest declaration. `workspace` backs `${workspace}`;
    /// `plugin_root` backs `${plugin}`.
    ///
    /// A path that escapes neither anchor is rejected rather than accepted
    /// literally: an absolute `/` in `fs_read` would otherwise read as a
    /// grant of the whole filesystem, which is exactly the kind of thing a
    /// reviewer skims past.
    pub fn resolve(
        caps: &plugin::manifest::Capabilities,
        workspace: &Path,
        plugin_root: &Path,
    ) -> Result<Self> {
        let expand = |list: &[String]| -> Result<Vec<PathBuf>> {
            list.iter()
                .map(|raw| resolve_path(raw, workspace, plugin_root))
                .collect()
        };
        Ok(Self {
            fs_read: expand(&caps.fs_read)?,
            fs_write: expand(&caps.fs_write)?,
            net: caps.net.iter().map(|h| h.to_ascii_lowercase()).collect(),
            env: caps.env.iter().cloned().collect(),
            max_memory_bytes: (caps.max_memory_mb as usize).saturating_mul(1024 * 1024),
            timeout: std::time::Duration::from_millis(caps.timeout_ms),
        })
    }

    /// May the component fetch this URL?
    ///
    /// Matched on host, exactly. No suffix matching: `evil-github.com` must
    /// not satisfy a declaration of `github.com`, and a plugin that wants a
    /// subdomain can name it.
    pub fn allows_url(&self, url: &str) -> bool {
        match host_of(url) {
            Some(host) => self.net.contains(&host),
            None => false,
        }
    }

    /// May the component read this environment variable?
    pub fn allows_env(&self, key: &str) -> bool {
        self.env.contains(key)
    }

    /// Does this grant anything beyond pure computation? Drives what the
    /// installer has to put in front of the user.
    pub fn reaches_outside(&self) -> bool {
        !self.fs_read.is_empty()
            || !self.fs_write.is_empty()
            || !self.net.is_empty()
            || !self.env.is_empty()
    }
}

/// Lowercased host of an absolute http(s) URL.
///
/// Public because an error message must be able to name what was refused
/// *without* echoing the URL: a plugin may have built it from a secret it
/// fetched through `host.secret`, and a refusal that quotes the whole thing
/// puts that secret into the model's context and the session transcript.
///
/// Deliberately narrow: anything that isn't plainly `http://host/...` or
/// `https://host/...` yields `None`, and `None` means denied. A parser that
/// tries to be clever about malformed input is a parser an attacker gets to
/// negotiate with.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    // Strip userinfo, which is the classic way to make a URL look like it
    // points somewhere it doesn't (`https://github.com@evil.example/`).
    let authority = authority.rsplit('@').next()?;
    let host = authority.split(':').next()?;
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

fn resolve_path(raw: &str, workspace: &Path, plugin_root: &Path) -> Result<PathBuf> {
    let expanded = if let Some(rest) = raw.strip_prefix("${workspace}") {
        workspace.join(rest.trim_start_matches('/'))
    } else if let Some(rest) = raw.strip_prefix("${plugin}") {
        plugin_root.join(rest.trim_start_matches('/'))
    } else {
        bail!(
            "capability path `{raw}` must start with ${{workspace}} or ${{plugin}} — \
             an unanchored path would grant more than a reviewer can judge"
        );
    };
    if expanded.components().any(|c| c == std::path::Component::ParentDir) {
        bail!("capability path `{raw}` may not contain `..`");
    }
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> plugin::manifest::Capabilities {
        plugin::manifest::Capabilities::default()
    }

    fn resolve(c: plugin::manifest::Capabilities) -> Result<ResolvedCapabilities> {
        ResolvedCapabilities::resolve(&c, Path::new("/ws"), Path::new("/plug"))
    }

    #[test]
    fn an_empty_declaration_grants_nothing() {
        let r = resolve(caps()).unwrap();
        assert!(!r.reaches_outside());
        assert!(!r.allows_url("https://example.com/x"));
        assert!(!r.allows_env("PATH"));
        assert!(r.fs_read.is_empty() && r.fs_write.is_empty());
    }

    #[test]
    fn variables_expand_against_their_anchors() {
        let mut c = caps();
        c.fs_read = vec!["${workspace}/src".into()];
        c.fs_write = vec!["${plugin}/scratch".into()];
        let r = resolve(c).unwrap();
        assert_eq!(r.fs_read, [PathBuf::from("/ws/src")]);
        assert_eq!(r.fs_write, [PathBuf::from("/plug/scratch")]);
    }

    /// An absolute path in a capability list reads as a grant of that path,
    /// which for `/` is the whole machine. Requiring an anchor keeps the
    /// declaration reviewable.
    #[test]
    fn an_unanchored_path_is_refused() {
        let mut c = caps();
        c.fs_read = vec!["/".into()];
        let err = resolve(c).unwrap_err().to_string();
        assert!(err.contains("${workspace}"), "{err}");
    }

    #[test]
    fn a_traversal_is_refused() {
        let mut c = caps();
        c.fs_read = vec!["${workspace}/../../etc".into()];
        assert!(resolve(c).unwrap_err().to_string().contains(".."));
    }

    #[test]
    fn net_matches_the_host_exactly() {
        let mut c = caps();
        c.net = vec!["api.github.com".into()];
        let r = resolve(c).unwrap();

        assert!(r.allows_url("https://api.github.com/repos"));
        assert!(r.allows_url("https://API.GitHub.com/repos"), "host is case-insensitive");
        assert!(r.allows_url("https://api.github.com:443/repos"), "port is not part of the host");

        assert!(!r.allows_url("https://github.com/x"), "a declaration is not a suffix rule");
        assert!(!r.allows_url("https://evil-api.github.com.attacker.test/x"));
        assert!(!r.allows_url("https://api.github.com.attacker.test/x"));
    }

    /// `https://allowed.example@evil.example/` points at `evil.example`.
    /// Reading the host off the front of the authority would get this wrong.
    #[test]
    fn userinfo_cannot_disguise_the_real_host() {
        let mut c = caps();
        c.net = vec!["allowed.example".into()];
        let r = resolve(c).unwrap();
        assert!(!r.allows_url("https://allowed.example@evil.example/x"));
        assert!(r.allows_url("https://user:pw@allowed.example/x"));
    }

    #[test]
    fn non_http_urls_are_denied_rather_than_parsed() {
        let mut c = caps();
        c.net = vec!["example.com".into()];
        let r = resolve(c).unwrap();
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "example.com/x",
            "",
            "https://",
        ] {
            assert!(!r.allows_url(url), "{url} should be denied");
        }
    }

    #[test]
    fn env_matches_exactly_and_is_case_sensitive() {
        let mut c = caps();
        c.env = vec!["GITHUB_TOKEN".into()];
        let r = resolve(c).unwrap();
        assert!(r.allows_env("GITHUB_TOKEN"));
        assert!(!r.allows_env("github_token"));
        assert!(!r.allows_env("GITHUB_TOKEN_2"));
        assert!(!r.allows_env("AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn memory_and_timeout_come_from_the_declaration() {
        let mut c = caps();
        c.max_memory_mb = 32;
        c.timeout_ms = 1500;
        let r = resolve(c).unwrap();
        assert_eq!(r.max_memory_bytes, 32 * 1024 * 1024);
        assert_eq!(r.timeout, std::time::Duration::from_millis(1500));
    }
}
