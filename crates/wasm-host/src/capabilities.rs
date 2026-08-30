//! Turning a WebAssembly plugin's manifest into the engine's capability
//! table.
//!
//! The table and every predicate over it live in
//! [`base::interface::capabilities`], above this carrier and shared with every
//! other one. This module is the manifest-shaped half: it converts what a
//! `plugin.toml` declares into the neutral declaration the kernel resolves,
//! and adds nothing.
//!
//! There is deliberately no allow-check here. Two carriers with two
//! allow-lists drift, and the one that drifts is found by an incident rather
//! than by a test — so the second carrier gets the same function, not its own.

use anyhow::Result;
use std::path::Path;

pub use base::interface::capabilities::{host_of, Capabilities as ResolvedCapabilities};

/// What a `plugin.toml` declared, as the kernel's neutral declaration.
fn declaration(caps: &plugin::manifest::Capabilities) -> base::interface::capabilities::CapabilityDeclaration {
    base::interface::capabilities::CapabilityDeclaration {
        fs_read: caps.fs_read.clone(),
        fs_write: caps.fs_write.clone(),
        net: caps.net.clone(),
        env: caps.env.clone(),
        max_memory_mb: caps.max_memory_mb,
        timeout_ms: caps.timeout_ms,
    }
}

/// Resolve a manifest declaration against a concrete workspace.
pub fn resolve(
    caps: &plugin::manifest::Capabilities,
    workspace: &Path,
    plugin_root: &Path,
) -> Result<ResolvedCapabilities> {
    ResolvedCapabilities::resolve(&declaration(caps), workspace, plugin_root)
        .map_err(|e| anyhow::anyhow!(e.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn caps() -> plugin::manifest::Capabilities {
        plugin::manifest::Capabilities::default()
    }

    fn resolve_for_test(c: plugin::manifest::Capabilities) -> Result<ResolvedCapabilities> {
        resolve(&c, Path::new("/ws"), Path::new("/plug"))
    }

    #[test]
    fn an_empty_declaration_grants_nothing() {
        let r = resolve_for_test(caps()).unwrap();
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
        let r = resolve_for_test(c).unwrap();
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
        let err = resolve_for_test(c).unwrap_err().to_string();
        assert!(err.contains("${workspace}"), "{err}");
    }

    #[test]
    fn a_traversal_is_refused() {
        let mut c = caps();
        c.fs_read = vec!["${workspace}/../../etc".into()];
        assert!(resolve_for_test(c).unwrap_err().to_string().contains(".."));
    }

    #[test]
    fn net_matches_the_host_exactly() {
        let mut c = caps();
        c.net = vec!["api.github.com".into()];
        let r = resolve_for_test(c).unwrap();

        assert!(r.allows_url("https://api.github.com/repos"));
        assert!(
            r.allows_url("https://API.GitHub.com/repos"),
            "host is case-insensitive"
        );
        assert!(
            r.allows_url("https://api.github.com:443/repos"),
            "port is not part of the host"
        );

        assert!(
            !r.allows_url("https://github.com/x"),
            "a declaration is not a suffix rule"
        );
        assert!(!r.allows_url("https://evil-api.github.com.attacker.test/x"));
        assert!(!r.allows_url("https://api.github.com.attacker.test/x"));
    }

    /// `https://allowed.example@evil.example/` points at `evil.example`.
    /// Reading the host off the front of the authority would get this wrong.
    #[test]
    fn userinfo_cannot_disguise_the_real_host() {
        let mut c = caps();
        c.net = vec!["allowed.example".into()];
        let r = resolve_for_test(c).unwrap();
        assert!(!r.allows_url("https://allowed.example@evil.example/x"));
        assert!(r.allows_url("https://user:pw@allowed.example/x"));
    }

    #[test]
    fn non_http_urls_are_denied_rather_than_parsed() {
        let mut c = caps();
        c.net = vec!["example.com".into()];
        let r = resolve_for_test(c).unwrap();
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
        let r = resolve_for_test(c).unwrap();
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
        let r = resolve_for_test(c).unwrap();
        assert_eq!(r.max_memory_bytes, 32 * 1024 * 1024);
        assert_eq!(r.timeout, std::time::Duration::from_millis(1500));
    }
}
