//! `plugin.toml` schema + the loaded `Plugin` it produces.
//!
//! Everything here is data. Nothing in this crate executes a plugin or knows
//! what a tool, scene or hook *is* — the translation into engine types lives
//! in `plugin-host`, which is what lets this crate stay a dependency-light
//! manifest reader that the daemon can compile out.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Plugin API contract versions this build understands.
///
/// A manifest naming anything else is refused rather than loaded on a
/// best-effort basis: the WIT world, the capability semantics and the event
/// whitelist all move together, and a plugin built against a different
/// version has no meaningful degraded mode.
pub const SUPPORTED_API_VERSIONS: &[&str] = &["0.1"];

/// Lifecycle events a plugin may subscribe to.
///
/// A deliberate subset of the engine's full event set. Two rules decide what
/// is on it: the event must be low-frequency (a plugin runs in a fresh WASM
/// store per call, which is what contains its failures — see the design doc's
/// execution model), and its payload must be small enough to cross the
/// sandbox boundary by value.
///
/// Kept as strings because this crate does not depend on the hooks crate;
/// `plugin-host` asserts each name resolves to a real event.
pub const SUBSCRIBABLE_EVENTS: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequested",
    "SessionStart",
    "SessionEnd",
];

#[derive(Debug, Deserialize, Clone)]
pub struct PluginManifest {
    pub plugin: PluginMeta,
    /// WASM component payloads.
    #[serde(default)]
    pub wasm: Vec<WasmPayload>,
    /// MCP server payloads, native or DSH-bridged.
    #[serde(default)]
    pub mcp: Vec<McpPayload>,
    /// How this plugin appears in other scenes, and the scene it owns.
    #[serde(default)]
    pub scene: SceneSection,
    /// Agent types this plugin declares.
    #[serde(default)]
    pub agent: Vec<AgentDef>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PluginMeta {
    pub name: String,
    pub version: String,
    /// Which [`SUPPORTED_API_VERSIONS`] entry this plugin was built against.
    pub api_version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub homepage: String,
    /// JSON Schema for this plugin's user-supplied configuration, validated
    /// before the plugin is initialized so a bad config fails at load time
    /// rather than inside the plugin's own parsing.
    #[serde(default)]
    pub config: ConfigSection,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ConfigSection {
    /// Path to a JSON Schema file, relative to the plugin root.
    #[serde(default)]
    pub schema: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WasmPayload {
    /// Component file, relative to the plugin root.
    pub component: PathBuf,
    /// Tool names this component is expected to export. Install-time
    /// visibility only — at runtime the component's own `list-tools` is the
    /// authority, since that is what the engine actually registers.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Events this component subscribes to; each must be in
    /// [`SUBSCRIBABLE_EVENTS`].
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub capabilities: Capabilities,
}

/// What a component is permitted to reach.
///
/// Every field defaults to nothing. A component that declares no
/// capabilities can compute and nothing else: no files, no network, no
/// environment. Whatever a plugin wants has to be written down, and what is
/// written down is what the installer shows the user.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct Capabilities {
    /// Directories exposed read-only, as WASI preopens.
    pub fs_read: Vec<String>,
    /// Directories exposed writable, as WASI preopens.
    pub fs_write: Vec<String>,
    /// Hosts the component's HTTP calls may reach.
    pub net: Vec<String>,
    /// Environment variable names readable through the host's `secret`.
    pub env: Vec<String>,
    pub max_memory_mb: u32,
    pub timeout_ms: u64,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            fs_read: Vec::new(),
            fs_write: Vec::new(),
            net: Vec::new(),
            env: Vec::new(),
            max_memory_mb: 64,
            timeout_ms: 30_000,
        }
    }
}

impl Capabilities {
    /// Does this declare access to anything outside the component itself?
    pub fn reaches_outside(&self) -> bool {
        !self.fs_read.is_empty()
            || !self.fs_write.is_empty()
            || !self.net.is_empty()
            || !self.env.is_empty()
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpKind {
    /// A standard MCP server; `config` points at its server config JSON.
    Native,
    /// A DeepSeek-Harness plugin; `entry` points at its JS entry, loaded by
    /// the `atta-dsh-bridge` process which speaks MCP on its behalf.
    Dsh,
}

#[derive(Debug, Deserialize, Clone)]
pub struct McpPayload {
    pub name: String,
    pub kind: McpKind,
    /// `kind = "native"`: server config JSON, relative to the plugin root.
    #[serde(default)]
    pub config: Option<PathBuf>,
    /// `kind = "dsh"`: JS entry module, relative to the plugin root.
    #[serde(default)]
    pub entry: Option<PathBuf>,
    /// Environment variable names to pass through to the server process.
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct SceneSection {
    /// Scenes whose explicit tool whitelist should admit this plugin's tools.
    /// Scenes with an empty whitelist ("everything registered is allowed")
    /// admit them regardless.
    #[serde(default)]
    pub visible_in: Vec<String>,
    /// The scene this plugin owns, registered as `plugin:<name>`.
    #[serde(default)]
    pub own: Option<OwnScene>,
}

/// A plugin's own scene: its system prompt, tool surface and budgets.
///
/// This is where a plugin may shape behavior. It may not shape anyone else's
/// — altering the prompt of a scene the user chose for other reasons is
/// hijacking; owning a scene the user explicitly enters is not.
#[derive(Debug, Deserialize, Clone)]
pub struct OwnScene {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Markdown system prompt, relative to the plugin root.
    pub prompt: PathBuf,
    /// Optional per-turn reminder body.
    #[serde(default)]
    pub reminder: Option<PathBuf>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    #[serde(default)]
    pub deferred_tools: Vec<String>,
    #[serde(default)]
    pub budget: SceneBudget,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct SceneBudget {
    #[serde(default)]
    pub compact_threshold: Option<usize>,
    #[serde(default)]
    pub compact_keep_recent: Option<usize>,
    #[serde(default)]
    pub max_api_calls_per_turn: Option<u32>,
}

/// A plugin-declared agent type.
///
/// `permission_mode` and `effort` stay strings here — parsing them into
/// engine enums is `plugin-host`'s job, and keeping this crate free of that
/// dependency is what keeps it compile-out-able.
#[derive(Debug, Deserialize, Clone)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// Markdown system prompt, relative to the plugin root.
    pub prompt: PathBuf,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Scene to run this agent's sub-agents in, e.g. `plugin:<name>`.
    #[serde(default)]
    pub scene: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("plugin schema: {0}")]
    Schema(String),
    #[error("homograph: {0}")]
    Homograph(String),
    #[error("checksum: {0}")]
    Checksum(String),
    #[error("unsupported api_version `{found}` (this build supports {supported})")]
    ApiVersion { found: String, supported: String },
}

#[derive(Debug, Clone)]
pub struct Plugin {
    pub root: PathBuf,
    pub manifest: PluginManifest,
}

impl Plugin {
    pub fn load(root: &Path, manifest_path: &Path) -> Result<Self, PluginError> {
        let raw = std::fs::read_to_string(manifest_path)?;
        let manifest: PluginManifest = toml::from_str(&raw)?;
        validate(&manifest)?;
        Ok(Plugin {
            root: root.to_path_buf(),
            manifest,
        })
    }

    pub fn name(&self) -> &str {
        &self.manifest.plugin.name
    }

    /// Scene id this plugin's own scene registers under, if it declares one.
    pub fn scene_id(&self) -> Option<String> {
        self.manifest
            .scene
            .own
            .as_ref()
            .map(|_| format!("plugin:{}", self.name()))
    }

    /// Resolve a manifest-relative path against this plugin's root.
    pub fn path(&self, rel: &Path) -> PathBuf {
        self.root.join(rel)
    }
}

fn validate(m: &PluginManifest) -> Result<(), PluginError> {
    if m.plugin.name.trim().is_empty() {
        return Err(PluginError::Schema("plugin.name must not be empty".into()));
    }
    if !SUPPORTED_API_VERSIONS.contains(&m.plugin.api_version.as_str()) {
        return Err(PluginError::ApiVersion {
            found: m.plugin.api_version.clone(),
            supported: SUPPORTED_API_VERSIONS.join(", "),
        });
    }
    for w in &m.wasm {
        for e in &w.events {
            if !SUBSCRIBABLE_EVENTS.contains(&e.as_str()) {
                return Err(PluginError::Schema(format!(
                    "event `{e}` is not one a plugin may subscribe to (allowed: {})",
                    SUBSCRIBABLE_EVENTS.join(", ")
                )));
            }
        }
    }
    for s in &m.mcp {
        match s.kind {
            McpKind::Native if s.config.is_none() => {
                return Err(PluginError::Schema(format!(
                    "mcp `{}` is native but declares no `config`",
                    s.name
                )));
            }
            McpKind::Dsh if s.entry.is_none() => {
                return Err(PluginError::Schema(format!(
                    "mcp `{}` is dsh but declares no `entry`",
                    s.name
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_str(toml_str: &str) -> Result<PluginManifest, PluginError> {
        let m: PluginManifest = toml::from_str(toml_str)?;
        validate(&m)?;
        Ok(m)
    }

    const MINIMAL: &str = r#"
[plugin]
name = "p"
version = "1.0.0"
api_version = "0.1"
"#;

    #[test]
    fn a_manifest_may_declare_no_payloads_at_all() {
        let m = load_str(MINIMAL).unwrap();
        assert_eq!(m.plugin.name, "p");
        assert!(m.wasm.is_empty());
        assert!(m.mcp.is_empty());
        assert!(m.agent.is_empty());
        assert!(m.scene.own.is_none());
    }

    #[test]
    fn full_manifest_round_trips() {
        let m = load_str(
            r#"
[plugin]
name = "github-tools"
version = "1.2.0"
api_version = "0.1"
description = "GitHub tools"

[plugin.config]
schema = "config.schema.json"

[[wasm]]
component = "gh.wasm"
tools = ["diff"]
events = ["PreToolUse", "PostToolUse"]

[wasm.capabilities]
fs_read = ["${workspace}"]
net = ["api.github.com"]
env = ["GITHUB_TOKEN"]
max_memory_mb = 128
timeout_ms = 5000

[[mcp]]
name = "github"
kind = "native"
config = "mcp/github.json"

[[mcp]]
name = "pr-helper"
kind = "dsh"
entry = "dist/index.js"

[scene]
visible_in = ["coding"]

[scene.own]
name = "GitHub workflow"
prompt = "scene/prompt.md"
tools = ["Read", "Grep"]
disallowed_tools = ["Bash"]

[scene.own.budget]
compact_threshold = 120000
max_api_calls_per_turn = 40

[[agent]]
name = "pr-reviewer"
description = "Reviews PRs"
prompt = "agents/reviewer.md"
allowed_tools = ["Read"]
permission_mode = "plan"
max_turns = 30
scene = "plugin:github-tools"
"#,
        )
        .unwrap();

        assert_eq!(m.plugin.config.schema.unwrap().to_str().unwrap(), "config.schema.json");
        assert_eq!(m.wasm.len(), 1);
        assert_eq!(m.wasm[0].capabilities.net, ["api.github.com"]);
        assert_eq!(m.wasm[0].capabilities.max_memory_mb, 128);
        assert_eq!(m.mcp.len(), 2);
        assert_eq!(m.mcp[0].kind, McpKind::Native);
        assert_eq!(m.mcp[1].kind, McpKind::Dsh);
        assert_eq!(m.scene.visible_in, ["coding"]);
        let own = m.scene.own.unwrap();
        assert_eq!(own.disallowed_tools, ["Bash"]);
        assert_eq!(own.budget.compact_threshold, Some(120_000));
        assert_eq!(own.budget.compact_keep_recent, None);
        assert_eq!(m.agent[0].scene.as_deref(), Some("plugin:github-tools"));
        assert_eq!(m.agent[0].permission_mode.as_deref(), Some("plan"));
    }

    /// Nothing declared means nothing reachable — that default is the whole
    /// point of the capability list, so it is worth pinning.
    #[test]
    fn undeclared_capabilities_reach_nothing() {
        let c = Capabilities::default();
        assert!(!c.reaches_outside());
        assert!(c.fs_read.is_empty() && c.net.is_empty() && c.env.is_empty());
    }

    #[test]
    fn empty_name_is_refused() {
        let err = load_str(
            r#"
[plugin]
name = ""
version = "1.0.0"
api_version = "0.1"
"#,
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::Schema(_)));
    }

    /// No silent downgrade: a plugin built against a version this binary
    /// doesn't implement is refused with the reason, not loaded on a
    /// best-effort basis.
    #[test]
    fn an_unsupported_api_version_is_refused_with_the_supported_set() {
        let err = load_str(
            r#"
[plugin]
name = "p"
version = "1.0.0"
api_version = "9.9"
"#,
        )
        .unwrap_err();
        match err {
            PluginError::ApiVersion { found, supported } => {
                assert_eq!(found, "9.9");
                assert!(supported.contains("0.1"));
            }
            other => panic!("expected ApiVersion, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_api_version_is_a_parse_error_not_a_default() {
        let err = load_str(
            r#"
[plugin]
name = "p"
version = "1.0.0"
"#,
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::Toml(_)));
    }

    #[test]
    fn subscribing_to_an_event_outside_the_whitelist_is_refused() {
        let err = load_str(
            r#"
[plugin]
name = "p"
version = "1.0.0"
api_version = "0.1"

[[wasm]]
component = "p.wasm"
events = ["PreCompact"]
"#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("PreCompact"), "{msg}");
        assert!(msg.contains("PreToolUse"), "the error should list what is allowed: {msg}");
    }

    #[test]
    fn each_mcp_kind_requires_its_own_path_field() {
        for (body, missing) in [
            (
                r#"
[[mcp]]
name = "s"
kind = "native"
"#,
                "config",
            ),
            (
                r#"
[[mcp]]
name = "s"
kind = "dsh"
"#,
                "entry",
            ),
        ] {
            let err = load_str(&format!("{MINIMAL}{body}")).unwrap_err();
            assert!(err.to_string().contains(missing), "{err}");
        }
    }

    #[test]
    fn scene_id_is_namespaced_and_only_present_when_a_scene_is_declared() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plugin.toml"), MINIMAL).unwrap();
        let p = Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();
        assert_eq!(p.scene_id(), None);

        std::fs::write(
            dir.path().join("plugin.toml"),
            format!(
                "{MINIMAL}\n[scene.own]\nname = \"Own\"\nprompt = \"scene/prompt.md\"\n"
            ),
        )
        .unwrap();
        let p = Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();
        assert_eq!(p.scene_id().as_deref(), Some("plugin:p"));
    }
}
