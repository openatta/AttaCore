//! The one capability table, and the one function that answers from it.
//!
//! An extension carrier — WebAssembly today, a script engine next to it —
//! needs to know what its extensions may reach. The obvious way to build the
//! second carrier is to give it its own table and its own checks, and that is
//! the mistake this module exists to prevent: two allow-lists drift, and the
//! one that drifts is discovered by an incident rather than by a test.
//!
//! So resolution and the predicates live here, above every carrier, and a
//! carrier's job is to convert its own manifest format into a
//! [`CapabilityDeclaration`] and then ask.
//!
//! # Nothing is granted by omission
//!
//! An undeclared capability is denied. A plugin that declares nothing can
//! compute and no more — no files, no network, no environment.
//!
//! # Resolution happens once, before anything runs
//!
//! `${workspace}` and `${plugin}` are expanded and paths checked at load, so
//! no decision at call time depends on state an extension could influence
//! between the check and the use.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Which carrier loaded an extension.
///
/// Carriers do not talk to each other — an extension reaches another one only
/// through the host's contracts, never through a direct call across memory
/// models — so this is for reporting and policy, not for dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderKind {
    /// A WebAssembly component.
    Wasm,
    /// A script run in this process.
    Script,
    /// Compiled into the host.
    Native,
}

impl LoaderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Wasm => "wasm",
            Self::Script => "script",
            Self::Native => "native",
        }
    }
}

/// What an extension declared it needs, in whatever manifest its carrier uses,
/// flattened into the one shape the engine reasons about.
#[derive(Debug, Clone, Default)]
pub struct CapabilityDeclaration {
    pub fs_read: Vec<String>,
    pub fs_write: Vec<String>,
    pub net: Vec<String>,
    pub env: Vec<String>,
    pub max_memory_mb: u32,
    pub timeout_ms: u64,
}

/// A declaration resolved against a concrete workspace, ready to enforce.
#[derive(Debug, Clone)]
pub struct Capabilities {
    pub fs_read: Vec<PathBuf>,
    pub fs_write: Vec<PathBuf>,
    net: HashSet<String>,
    env: HashSet<String>,
    pub max_memory_bytes: usize,
    pub timeout: std::time::Duration,
}

/// Why a declaration could not be resolved.
///
/// A refusal, never a downgrade: a capability path that cannot be understood
/// is not silently narrowed to something safe, because "something safe" is a
/// guess about intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityError(pub String);

impl std::fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CapabilityError {}

impl Capabilities {
    /// Resolve a declaration. `workspace` backs `${workspace}`; `plugin_root`
    /// backs `${plugin}`.
    ///
    /// A path anchored to neither is rejected rather than taken literally: an
    /// absolute `/` in `fs_read` would otherwise read as a grant of the whole
    /// filesystem, which is exactly the kind of line a reviewer skims past.
    pub fn resolve(
        decl: &CapabilityDeclaration,
        workspace: &Path,
        plugin_root: &Path,
    ) -> Result<Self, CapabilityError> {
        let expand = |list: &[String]| -> Result<Vec<PathBuf>, CapabilityError> {
            list.iter()
                .map(|raw| resolve_path(raw, workspace, plugin_root))
                .collect()
        };
        Ok(Self {
            fs_read: expand(&decl.fs_read)?,
            fs_write: expand(&decl.fs_write)?,
            net: decl.net.iter().map(|h| h.to_ascii_lowercase()).collect(),
            env: decl.env.iter().cloned().collect(),
            max_memory_bytes: (decl.max_memory_mb as usize).saturating_mul(1024 * 1024),
            timeout: std::time::Duration::from_millis(decl.timeout_ms),
        })
    }

    /// May this extension fetch this URL?
    ///
    /// Matched on host, exactly. No suffix matching: `evil-github.com` must
    /// not satisfy a declaration of `github.com`, and an extension that wants
    /// a subdomain can name it.
    pub fn allows_url(&self, url: &str) -> bool {
        match host_of(url) {
            Some(host) => self.net.contains(&host),
            None => false,
        }
    }

    /// May it read this environment variable?
    pub fn allows_env(&self, key: &str) -> bool {
        self.env.contains(key)
    }

    /// May it read this path?
    pub fn allows_read(&self, path: &Path) -> bool {
        self.fs_read.iter().any(|granted| path.starts_with(granted))
            || self.fs_write.iter().any(|granted| path.starts_with(granted))
    }

    /// May it write this path? Write does not imply read's grants, and read
    /// does not imply write.
    pub fn allows_write(&self, path: &Path) -> bool {
        self.fs_write.iter().any(|granted| path.starts_with(granted))
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
/// *without* echoing the URL: an extension may have built it from a secret it
/// fetched, and a refusal that quotes the whole thing puts that secret into
/// the model's context and the session transcript.
///
/// Deliberately narrow: anything that is not plainly `http://host/…` or
/// `https://host/…` yields `None`, and `None` means denied. A parser that
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

fn resolve_path(
    raw: &str,
    workspace: &Path,
    plugin_root: &Path,
) -> Result<PathBuf, CapabilityError> {
    let expanded = if let Some(rest) = raw.strip_prefix("${workspace}") {
        workspace.join(rest.trim_start_matches('/'))
    } else if let Some(rest) = raw.strip_prefix("${plugin}") {
        plugin_root.join(rest.trim_start_matches('/'))
    } else {
        return Err(CapabilityError(format!(
            "capability path `{raw}` must start with ${{workspace}} or ${{plugin}} — \
             an unanchored path would grant more than a reviewer can judge"
        )));
    };
    if expanded
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(CapabilityError(format!(
            "capability path `{raw}` may not contain `..`"
        )));
    }
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(d: CapabilityDeclaration) -> Result<Capabilities, CapabilityError> {
        Capabilities::resolve(&d, Path::new("/ws"), Path::new("/plug"))
    }

    #[test]
    fn an_empty_declaration_grants_nothing() {
        let r = resolve(CapabilityDeclaration::default()).unwrap();
        assert!(!r.reaches_outside());
        assert!(!r.allows_url("https://example.com/x"));
        assert!(!r.allows_env("PATH"));
        assert!(!r.allows_read(Path::new("/ws/src/main.rs")));
        assert!(!r.allows_write(Path::new("/ws/src/main.rs")));
    }

    #[test]
    fn a_read_grant_is_not_a_write_grant() {
        let r = resolve(CapabilityDeclaration {
            fs_read: vec!["${workspace}/src".into()],
            ..Default::default()
        })
        .unwrap();
        assert!(r.allows_read(Path::new("/ws/src/main.rs")));
        assert!(
            !r.allows_write(Path::new("/ws/src/main.rs")),
            "reading is not writing"
        );
    }

    #[test]
    fn a_write_grant_carries_read_of_the_same_place() {
        let r = resolve(CapabilityDeclaration {
            fs_write: vec!["${plugin}/scratch".into()],
            ..Default::default()
        })
        .unwrap();
        assert!(r.allows_write(Path::new("/plug/scratch/f")));
        assert!(
            r.allows_read(Path::new("/plug/scratch/f")),
            "a plugin that may write a file it cannot read cannot check its own work"
        );
    }

    #[test]
    fn an_unanchored_path_is_refused() {
        let err = resolve(CapabilityDeclaration {
            fs_read: vec!["/".into()],
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("${workspace}"), "{err}");
    }

    #[test]
    fn a_traversal_is_refused() {
        let err = resolve(CapabilityDeclaration {
            fs_read: vec!["${workspace}/../../etc".into()],
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains(".."), "{err}");
    }

    #[test]
    fn net_matches_the_host_exactly() {
        let r = resolve(CapabilityDeclaration {
            net: vec!["api.github.com".into()],
            ..Default::default()
        })
        .unwrap();
        assert!(r.allows_url("https://api.github.com/repos"));
        assert!(r.allows_url("https://API.GitHub.com/repos"), "case-insensitive");
        assert!(r.allows_url("https://api.github.com:443/repos"), "port is not the host");
        assert!(!r.allows_url("https://github.com/x"), "not a suffix rule");
        assert!(!r.allows_url("https://api.github.com.attacker.test/x"));
        assert!(
            !r.allows_url("https://api.github.com@evil.example/x"),
            "userinfo must not disguise the host"
        );
    }
}
