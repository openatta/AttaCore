//! Configuration injected by the application layer.
//!
//! `Settings::load()` is the single canonical settings.json loader (global →
//! scene → project, generic recursive JSON merge — see its doc comment).
//! Everything downstream (the AGENT itself, `Builder`) receives the already-
//! merged result; it does not perform its own multi-layer config merging.

use crate::provider::{ApiType, ProviderConfig, TaskModelOverride};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Complete AGENT configuration. Merged by `Settings::load()` before injection.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Settings {
    pub model: ModelSettings,
    pub paths: PathSettings,
    #[serde(default)]
    pub execution: ExecutionSettings,
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,

    /// Path to an instruction file (e.g. AGENTS.md, CLAUDE.md).
    /// The AGENT reads the file at its discretion (every turn, on change, etc.).
    #[serde(default)]
    pub instruction_file: Option<PathBuf>,

    /// Appended to the end of the system prompt.
    #[serde(default)]
    pub prompt_append: Option<String>,
    /// Overrides the entire system prompt if set.
    #[serde(default)]
    pub prompt_override: Option<String>,

    // ── Internal component configuration ──
    /// VCR record/replay configuration. None = pass-through.
    #[serde(default)]
    pub vcr: Option<VcrConfig>,
    /// Telemetry endpoint URL. None = noop.
    #[serde(default)]
    pub telemetry_url: Option<String>,
    /// Session persistence directory. None = no persistence.
    /// Default: `Some(user_data_dir/sessions/)`.
    #[serde(default)]
    pub session_dir: Option<PathBuf>,

    /// Enable the file-based memory system (MEMORY.md + .md files).
    /// Default: true. Set to false to disable memory prompt injection and file-based memory.
    #[serde(default = "default_memory_enabled")]
    pub memory_enabled: bool,

    /// Permission mode for tool execution.
    /// TS parity: `PermissionMode` in `types/permissions.ts`.
    #[serde(default)]
    pub permission_mode: PermissionMode,

    /// Allow/deny/ask rules for specific tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission_rules: Vec<PermissionRule>,

    /// Hooks configuration (merged from global/scene/project layers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks_config: Option<serde_json::Value>,

    /// MCP server configurations, keyed by server name (merged from
    /// global/scene/project layers). Kept untyped (`serde_json::Value`
    /// rather than `mcp::config::McpServerConfig`) — `core` cannot depend on
    /// `mcp` (which already depends on `core`, for `base::paths::ConfigPaths`)
    /// without a circular dependency; downstream consumers deserialize each
    /// value into `McpServerConfig` themselves.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub mcp_servers: HashMap<String, serde_json::Value>,

    /// Multi-provider LLM registry, keyed by provider id. See
    /// `docs/LLM_PROVIDERS.md`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub providers: HashMap<String, ProviderConfig>,
    /// Provider id used for any task with no `task_models` entry, and as the
    /// fallback target when an entry's provider/model turns out invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    /// Per-task-type provider/model routing — see `base::provider::resolve_task_models`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub task_models: HashMap<String, TaskModelOverride>,

    /// User language preference (e.g. "zh-CN", "ja").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,

    /// Feature flags — compile-time + runtime gate for experimental features.
    /// TS parity: GrowthBook feature flags in claude-code.
    #[serde(default)]
    pub feature_flags: crate::features::FeatureFlags,
}

fn default_memory_enabled() -> bool {
    true
}

/// Permission mode for tool execution (TS parity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Prompt user for each tool call that isn't explicitly allowed.
    #[default]
    Default,
    /// Auto-accept edits to files (Write/Edit), prompt for others.
    AcceptEdits,
    /// Bypass all permission checks.
    BypassPermissions,
    /// Plan mode — only allow read-only tools.
    Plan,
    /// Auto mode — skip prompts for known-safe operations.
    /// **Program-only**: requires transcript classifier; cannot be set by user.
    Auto,
    /// Don't ask — deny any tool not explicitly allowed (no prompt). TS parity: dontAsk.
    DontAsk,
    /// Bubble mode — forward permission requests up to a parent agent.
    /// **Program-only**: set by team/coordinator runtime; cannot be set by user.
    /// TS parity: bubble.
    Bubble,
    /// YOLO mode — aggressive auto-approval for power users.
    Yolo,
}

impl PermissionMode {
    /// Whether the user can set this mode via settings.json / CLI.
    /// TS parity: `EXTERNAL_PERMISSION_MODES` in `types/permissions.ts`.
    /// `Auto` and `Bubble` are program-only — they are set by the runtime
    /// (classifier activation, team coordinator) and rejected from user config.
    pub fn is_user_settable(self) -> bool {
        !matches!(self, Self::Auto | Self::Bubble)
    }
}

/// A single permission rule: allow/deny/ask a tool matching a pattern.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PermissionRule {
    /// Tool name pattern (e.g. "Bash", "Bash(git push:*)", "FileWrite").
    pub tool: String,
    /// Action: "allow", "deny", or "ask".
    pub action: PermissionAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    Allow,
    Deny,
    Ask,
}

/// LLM model configuration for a single provider.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelSettings {
    pub api_type: ApiType,
    pub base_url: String,
    pub auth_token: String,
    /// Resolved model name (upper layer already resolved slot → name).
    pub model_name: String,
    pub max_tokens: u32,
    pub thinking_mode: ThinkingMode,
    /// Fallback model for persistent Overloaded/529 errors (e.g. Opus → Sonnet).
    /// None = no fallback.
    #[serde(default)]
    pub fallback_model: Option<String>,
}

/// Path configuration for data directories.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PathSettings {
    /// User-level, scene-specific override root (e.g. `~/.atta/scenes/<scope>/`).
    pub user_data_dir: PathBuf,
    /// User-level, cross-scene global root — flat, shared by every scene
    /// (e.g. `~/.atta/`). Used by resources that don't have a scene tier
    /// (`memory`/`sessions`/`vcr`/`mcp`) and as the base layer for resources
    /// that do (`settings.json`/`skills`/`plugins`/`agents`/`rules`/`hooks`).
    /// See `base::paths::ConfigPaths` module docs for the full breakdown.
    #[serde(default)]
    pub global_data_dir: PathBuf,
    /// Local/project data root (e.g. `<cwd>/.atta/`)
    pub local_data_dir: PathBuf,
    /// Which product instance's user-level state `user_data_dir` was built
    /// for (see `base::paths::ConfigPaths`). Carried alongside the resolved
    /// dirs so downstream code (e.g. `FrozenContext::collect`) doesn't have
    /// to re-derive it from the path. Defaults to `"code"` on deserialize
    /// for settings.json files predating this field — `Settings::merge`
    /// never copies a deserialized `paths` value over the one `Settings::load`
    /// sets explicitly, so this default only matters for standalone
    /// (de)serialization, not for the daemon's actual startup path.
    #[serde(default = "default_scope")]
    pub scope: String,
}

fn default_scope() -> String {
    "code".to_string()
}

impl PathSettings {
    /// The actual project working directory — **not** `local_data_dir`
    /// itself, which is `<project_root>/.atta` (flat, no scope segment; see
    /// `base::paths::ConfigPaths`). Several call sites used to pass
    /// `local_data_dir` straight to `FrozenContext::collect` as if it were
    /// the project root, which meant `AGENTS.md`/git-status discovery was
    /// scanning the `.atta/` directory instead of the real project — this is
    /// the fixed, single place that derives it correctly (falls back to
    /// `local_data_dir` itself if it has no parent, e.g. `.atta` sitting at
    /// filesystem root — not expected in practice).
    pub fn project_root(&self) -> PathBuf {
        self.local_data_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.local_data_dir.clone())
    }
}

/// Execution constraints.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExecutionSettings {
    pub max_parallelism: usize,
    pub max_api_calls_per_turn: u32,
    /// T3.2: Maximum USD budget per session. None = unlimited.
    pub max_budget_usd: Option<f64>,
}

impl Default for ExecutionSettings {
    fn default() -> Self {
        Self {
            max_parallelism: 10,
            max_api_calls_per_turn: 200,
            max_budget_usd: None,
        }
    }
}

/// Context compaction configuration.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CompactionConfig {
    pub threshold_tokens: usize,
    pub keep_recent: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            threshold_tokens: 150_000,
            keep_recent: 20,
        }
    }
}

/// Sandbox/security configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SandboxConfig {
    pub deny_read: Vec<PathBuf>,
    pub allowed_domains: Vec<String>,
    /// Bypass sandbox entirely (TS parity: `dangerouslyDisableSandbox`).
    /// Default: false. Only for trusted environments.
    #[serde(default)]
    pub dangerously_disable_sandbox: bool,
}

/// VCR (record/replay) configuration.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VcrConfig {
    /// "record" or "replay"
    pub mode: VcrMode,
    /// Scenario name (JSONL filename without extension)
    pub scenario: String,
    /// On replay, fall back to real API when no match? (default true)
    #[serde(default = "default_true")]
    pub fallback_on_miss: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VcrMode {
    Record,
    Replay,
}

fn default_true() -> bool {
    true
}

/// Model thinking/reasoning mode.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    Auto,
    Off,
    On,
    OnBudget(u32),
}

impl Settings {
    /// Load settings from user and local directories, with ENV override.
    /// Priority (low → high): `global_dir/settings.json` (shared by every
    /// scene) → `scene_dir/settings.json` → `local_dir/settings.json`
    /// (project). This is the **single canonical settings.json loader** —
    /// `daemon` and any other embedder should call this rather than parsing
    /// settings.json themselves. See `docs/CONFIG_LAYOUT.md`.
    ///
    /// Merging is a **generic recursive JSON merge** (later layer's object
    /// keys override/extend the earlier layer's, non-object values fully
    /// replace) applied directly to the JSON representation, not a
    /// hand-written field-by-field `Settings` merge — this is why adding a
    /// new field to `Settings` needs no changes here: it flows through
    /// automatically. `paths` is deliberately excluded from every layer
    /// before merging — a settings.json file cannot override where its own
    /// layers live, that's decided by the caller's `global_dir`/`scene_dir`/
    /// `local_dir`/`scope` arguments alone.
    ///
    /// **Never fails** — a layer that doesn't exist is skipped; a layer that
    /// fails to parse is skipped with a `tracing::warn!` (path + error)
    /// rather than aborting startup or silently losing the problem. If the
    /// fully-merged JSON somehow doesn't deserialize into `Settings` (e.g. a
    /// layer put a string where an object was expected), the merge is
    /// discarded with a warning and plain defaults are returned instead.
    ///
    /// `scope` identifies which product instance `scene_dir` belongs to (see
    /// `base::paths::ConfigPaths`) — no default at this layer; callers decide.
    ///
    /// `default_model` seeds `model.model_name` **before** any layer is
    /// merged in — same priority tier as `defaults_for`'s own hardcoded
    /// default, i.e. lower priority than every settings.json layer. This
    /// lets a caller's CLI `--model` flag act as a fallback (only takes
    /// effect when no settings.json layer specifies one) rather than a hard
    /// override.
    pub fn load(
        global_dir: PathBuf,
        scene_dir: PathBuf,
        local_dir: PathBuf,
        scope: &str,
        default_model: &str,
    ) -> Self {
        let base = Self::defaults_for(default_model);
        let mut merged_json = serde_json::to_value(&base).unwrap_or_else(|_| serde_json::json!({}));

        for (layer_name, dir) in [
            ("global", &global_dir),
            ("scene", &scene_dir),
            ("project", &local_dir),
        ] {
            let path = dir.join("settings.json");
            if !path.exists() {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(layer = layer_name, path = %path.display(), error = %e, "failed to read settings.json, skipping this layer");
                    continue;
                }
            };
            let mut layer_json: serde_json::Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(layer = layer_name, path = %path.display(), error = %e, "failed to parse settings.json, skipping this layer");
                    continue;
                }
            };
            // `paths` is resolved from this function's own arguments, never
            // from settings.json content.
            if let Some(obj) = layer_json.as_object_mut() {
                obj.remove("paths");
            }
            merge_json_values(&mut merged_json, layer_json);
        }

        let mut settings: Settings = match serde_json::from_value(merged_json) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "merged settings.json failed to deserialize into Settings; falling back to defaults");
                base
            }
        };

        settings.paths = PathSettings {
            user_data_dir: scene_dir,
            global_data_dir: global_dir,
            local_data_dir: local_dir,
            scope: scope.to_string(),
        };

        // Validate: reject program-only permission modes from user config.
        // TS parity: EXTERNAL_PERMISSION_MODES vs INTERNAL in types/permissions.ts.
        if let Err(reason) = settings.validate() {
            tracing::warn!("Settings validation: {reason}");
            settings.permission_mode = PermissionMode::default();
        }

        settings
    }

    /// Validate settings consistency. Returns Err on invalid combinations.
    pub fn validate(&self) -> Result<(), String> {
        if !self.permission_mode.is_user_settable() {
            return Err(format!(
                "Permission mode '{:?}' is program-only and cannot be set in settings.json. \
                 Defaulting to Default.",
                self.permission_mode
            ));
        }
        Ok(())
    }

    /// Quick default for a given model name.
    pub fn defaults_for(model_name: &str) -> Self {
        Self {
            model: ModelSettings {
                api_type: ApiType::Anthropic,
                base_url: String::new(),
                auth_token: String::new(),
                model_name: model_name.to_string(),
                max_tokens: 2000,
                thinking_mode: ThinkingMode::Auto,
                fallback_model: None,
            },
            paths: PathSettings {
                user_data_dir: PathBuf::from("~/.atta/scenes/agent"),
                global_data_dir: PathBuf::from("~/.atta"),
                local_data_dir: PathBuf::from("."),
                scope: default_scope(),
            },
            execution: ExecutionSettings::default(),
            compaction: CompactionConfig::default(),
            sandbox: SandboxConfig::default(),
            instruction_file: None,
            prompt_append: None,
            prompt_override: None,
            vcr: None,
            telemetry_url: None,
            session_dir: None,
            memory_enabled: true,
            permission_mode: PermissionMode::default(),
            permission_rules: Vec::new(),
            hooks_config: None,
            mcp_servers: HashMap::new(),
            providers: HashMap::new(),
            default_provider: None,
            task_models: HashMap::new(),
            language: None,
            feature_flags: crate::features::FeatureFlags::default(),
        }
    }
}

/// JSON Schema for settings.json, generated from `Settings`'s own type
/// definitions (`#[derive(schemars::JsonSchema)]` on `Settings` and every
/// nested type) — not hand-maintained, so it can never drift from what
/// `Settings::load()` actually accepts. Published as `docs/schemas/settings.schema.json`
/// for editor autocomplete/validation (reference it from a settings.json file
/// via a `"$schema"` key). Regenerate with:
/// `cargo test -p base settings_schema_matches_committed_file -- --ignored`
/// after changing any `Settings`-reachable type (see that test for the exact
/// write-back invocation).
pub fn settings_json_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(Settings)).unwrap_or(serde_json::Value::Null)
}

/// Recursively merge `over` into `base` in place: objects merge key-by-key
/// (recursing into shared keys), anything else (arrays, scalars, null) fully
/// replaces the corresponding `base` value. This is the same algorithm
/// `Settings::load()` uses to merge global/scene/project tiers, exposed here
/// so other crates (e.g. `daemon`'s `config.setProvider` RPC) can apply a
/// partial patch to a single settings.json tier with identical semantics.
pub fn merge_json_values(base: &mut serde_json::Value, over: serde_json::Value) {
    match (base, over) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(over_map)) => {
            for (k, v) in over_map {
                match base_map.get_mut(&k) {
                    Some(existing) => merge_json_values(existing, v),
                    None => {
                        base_map.insert(k, v);
                    }
                }
            }
        }
        (base_slot, over_val) => {
            *base_slot = over_val;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_execution_settings() {
        let s = ExecutionSettings::default();
        assert_eq!(s.max_parallelism, 10);
        assert_eq!(s.max_api_calls_per_turn, 200);
    }

    #[test]
    fn permission_mode_deserializes_bubble_and_dontask() {
        // TS parity: types/permissions.ts — dontAsk is EXTERNAL (user-settable),
        // bubble and auto are program-only. Settings must deserialize all but
        // validation rejects non-user-settable modes.
        let b: PermissionMode = serde_json::from_str("\"bubble\"").unwrap();
        assert_eq!(b, PermissionMode::Bubble);
        assert!(!b.is_user_settable(), "Bubble is program-only");

        let a: PermissionMode = serde_json::from_str("\"auto\"").unwrap();
        assert_eq!(a, PermissionMode::Auto);
        assert!(!a.is_user_settable(), "Auto is program-only");

        let d: PermissionMode = serde_json::from_str("\"dontAsk\"").unwrap();
        assert_eq!(d, PermissionMode::DontAsk);
        assert!(d.is_user_settable(), "DontAsk is user-settable");

        // Validation: Bubble in settings is rejected.
        let s = Settings { permission_mode: PermissionMode::Bubble, ..Settings::defaults_for("test") };
        assert!(s.validate().is_err());

        // Validation: Auto in settings is rejected.
        let s = Settings { permission_mode: PermissionMode::Auto, ..Settings::defaults_for("test") };
        assert!(s.validate().is_err());
    }

    #[test]
    fn merge_json_values_recurses_into_nested_objects() {
        let mut base = serde_json::json!({
            "providers": {
                "deepseek": { "api_key": "old-key", "default_model": "deepseek-pro" }
            }
        });
        let over = serde_json::json!({
            "providers": {
                "deepseek": { "api_key": "new-key" }
            }
        });
        merge_json_values(&mut base, over);
        assert_eq!(base["providers"]["deepseek"]["api_key"], "new-key");
        // Untouched sibling field survives — this is the whole point of a
        // recursive merge over a whole-value replace.
        assert_eq!(base["providers"]["deepseek"]["default_model"], "deepseek-pro");
    }

    #[test]
    fn merge_json_values_non_object_values_fully_replace() {
        let mut base = serde_json::json!({ "mcp_servers": {"a": 1} });
        let over = serde_json::json!({ "mcp_servers": {"b": 2} });
        merge_json_values(&mut base, over);
        // Both are objects, so this recurses (keeps "a", adds "b") — objects
        // always merge key-by-key, never fully replace each other.
        assert_eq!(base["mcp_servers"]["a"], 1);
        assert_eq!(base["mcp_servers"]["b"], 2);

        let mut base2 = serde_json::json!({ "language": "en" });
        let over2 = serde_json::json!({ "language": "zh-CN" });
        merge_json_values(&mut base2, over2);
        assert_eq!(base2["language"], "zh-CN");
    }

    fn write_settings(dir: &std::path::Path, content: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("settings.json"), content).unwrap();
    }

    #[test]
    fn load_merges_three_tiers_low_to_high() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        let scene = root.path().join("scene");
        let project = root.path().join("project");
        write_settings(&global, r#"{"model": {"model_name": "global-model"}}"#);
        write_settings(&scene, r#"{"model": {"model_name": "scene-model"}}"#);
        write_settings(&project, r#"{}"#);

        let settings = Settings::load(global, scene, project, "code", "cli-default");
        assert_eq!(settings.model.model_name, "scene-model");
    }

    #[test]
    fn load_falls_back_to_cli_default_model_when_no_layer_sets_it() {
        let root = tempfile::tempdir().unwrap();
        let settings = Settings::load(
            root.path().join("global"),
            root.path().join("scene"),
            root.path().join("project"),
            "code",
            "cli-default-model",
        );
        assert_eq!(settings.model.model_name, "cli-default-model");
    }

    #[test]
    fn load_lets_project_override_just_one_provider_field() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        let project = root.path().join("project");
        write_settings(
            &global,
            r#"{"providers": {"deepseek": {"api_key": "global-key", "default_model": "deepseek-pro"}}}"#,
        );
        write_settings(&project, r#"{"providers": {"deepseek": {"api_key": "project-key"}}}"#);

        let settings = Settings::load(global, root.path().join("scene"), project, "code", "m");
        let deepseek = &settings.providers["deepseek"];
        assert_eq!(deepseek.api_key.as_deref(), Some("project-key"));
        assert_eq!(deepseek.default_model.as_deref(), Some("deepseek-pro"));
    }

    #[test]
    fn load_never_lets_settings_json_override_paths() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        write_settings(&global, r#"{"paths": {"scope": "hijacked"}}"#);

        let settings = Settings::load(global, root.path().join("scene"), root.path().join("project"), "real-scope", "m");
        assert_eq!(settings.paths.scope, "real-scope");
    }

    #[test]
    fn load_malformed_layer_warns_and_is_skipped_not_fatal() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        write_settings(&global, "not valid json {{{");
        // Must not panic — malformed layer is skipped, defaults are used.
        let settings = Settings::load(global, root.path().join("scene"), root.path().join("project"), "code", "fallback-model");
        assert_eq!(settings.model.model_name, "fallback-model");
    }

    #[test]
    fn settings_json_schema_has_expected_top_level_properties() {
        let schema = settings_json_schema();
        let props = schema["properties"]
            .as_object()
            .expect("schema should have a top-level `properties` object");
        for key in ["model", "paths", "providers", "default_provider", "task_models", "hooks_config"] {
            assert!(props.contains_key(key), "schema missing property `{key}`");
        }
    }

    /// Regenerates `docs/schemas/settings.schema.json` from the current
    /// `Settings` type definitions and asserts it matches what's committed —
    /// fails (rather than silently drifting) if a `Settings`-reachable type
    /// changed without regenerating the schema. `#[ignore]`d because it
    /// writes to the repo tree; not run by default `cargo test`.
    ///
    /// To regenerate after a real schema change: run this test once with
    /// `cargo test -p base settings_schema_matches_committed_file -- --ignored`
    /// (it writes the file even on "mismatch" before asserting), then commit
    /// the updated `docs/schemas/settings.schema.json`.
    #[test]
    #[ignore]
    fn settings_schema_matches_committed_file() {
        let schema = settings_json_schema();
        let pretty = serde_json::to_string_pretty(&schema).unwrap() + "\n";
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/schemas/settings.schema.json");
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if existing != pretty {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &pretty).unwrap();
        }
        assert_eq!(existing, pretty, "docs/schemas/settings.schema.json was out of date and has been regenerated — review the diff and commit it");
    }

    #[test]
    fn load_missing_directories_returns_plain_defaults() {
        let root = tempfile::tempdir().unwrap();
        let settings = Settings::load(
            root.path().join("nonexistent-global"),
            root.path().join("nonexistent-scene"),
            root.path().join("nonexistent-project"),
            "code",
            "fallback-model",
        );
        assert_eq!(settings.model.model_name, "fallback-model");
        assert!(settings.providers.is_empty());
    }
}
