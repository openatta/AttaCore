//! Configuration injected by the application layer.
//!
//! The AGENT receives fully-merged settings; it does not perform
//! its own multi-layer config merging.

use crate::provider::ApiType;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Complete AGENT configuration. Merged by the application layer before injection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub model: ModelSettings,
    pub paths: PathSettings,
    pub execution: ExecutionSettings,
    pub compaction: CompactionConfig,
    pub sandbox: SandboxConfig,

    /// Path to an instruction file (e.g. CLAUDE.md, ATTA.md).
    /// The AGENT reads the file at its discretion (every turn, on change, etc.).
    pub instruction_file: Option<PathBuf>,

    /// Appended to the end of the system prompt.
    pub prompt_append: Option<String>,
    /// Overrides the entire system prompt if set.
    pub prompt_override: Option<String>,

    // ── Internal component configuration ──
    /// VCR record/replay configuration. None = pass-through.
    pub vcr: Option<VcrConfig>,
    /// Telemetry endpoint URL. None = noop.
    pub telemetry_url: Option<String>,
    /// Session persistence directory. None = no persistence.
    /// Default: `Some(user_data_dir/sessions/)`.
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

    /// Hooks configuration (merged from user/project layers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks_config: Option<serde_json::Value>,

    /// MCP server configurations (merged from user/project layers).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<serde_json::Value>,

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    /// Tool name pattern (e.g. "Bash", "Bash(git push:*)", "FileWrite").
    pub tool: String,
    /// Action: "allow", "deny", or "ask".
    pub action: PermissionAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    Allow,
    Deny,
    Ask,
}

/// Multi-layer config merge: user → project → local → flags → policy.
/// TS parity: 5-layer merge in `settings/constants.ts`.
///
/// Currently implements user + local; other layers are stubs for future use.
#[derive(Debug, Clone, Default)]
pub struct SettingsMerge {
    pub user: Option<Settings>,
    pub project: Option<Settings>,
    /// CLI flags / env var overrides (highest priority).
    pub flags: Option<Settings>,
    /// Organization policy layer (lowest priority, foundation).
    pub policy: Option<Settings>,
}

/// LLM model configuration for a single provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathSettings {
    /// User-level data root (e.g. `~/.atta/code/`)
    pub user_data_dir: PathBuf,
    /// Local/project data root (e.g. `<cwd>/.atta/code/`)
    pub local_data_dir: PathBuf,
}

/// Execution constraints.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SandboxConfig {
    pub deny_read: Vec<PathBuf>,
    pub allowed_domains: Vec<String>,
    /// Bypass sandbox entirely (TS parity: `dangerouslyDisableSandbox`).
    /// Default: false. Only for trusted environments.
    #[serde(default)]
    pub dangerously_disable_sandbox: bool,
}

/// VCR (record/replay) configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VcrConfig {
    /// "record" or "replay"
    pub mode: VcrMode,
    /// Scenario name (JSONL filename without extension)
    pub scenario: String,
    /// On replay, fall back to real API when no match? (default true)
    #[serde(default = "default_true")]
    pub fallback_on_miss: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VcrMode {
    Record,
    Replay,
}

fn default_true() -> bool {
    true
}

/// Model thinking/reasoning mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    Auto,
    Off,
    On,
    OnBudget(u32),
}

impl Settings {
    /// Load settings from user and local directories, with ENV override.
    /// Priority (low→high): user_dir/settings.json → local_dir/settings.json → ENV
    pub fn load(user_dir: PathBuf, local_dir: PathBuf) -> Result<Self, SettingsError> {
        let mut base = Self::defaults_for("claude-sonnet-4-6");
        base.paths = PathSettings {
            user_data_dir: user_dir.clone(),
            local_data_dir: local_dir.clone(),
        };

        // 1. Load user settings
        let user_path = user_dir.join("settings.json");
        if user_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&user_path) {
                if let Ok(s) = serde_json::from_str::<Settings>(&content) {
                    base = base.merge(s);
                }
            }
        }

        // 2. Override with local settings
        let local_path = local_dir.join("settings.json");
        if local_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&local_path) {
                if let Ok(s) = serde_json::from_str::<Settings>(&content) {
                    base = base.merge(s);
                }
            }
        }

        // 3. ENV override: ATTA_MODEL or ANTHROPIC_MODEL
        if let Ok(m) = std::env::var("ATTA_MODEL").or_else(|_| std::env::var("ANTHROPIC_MODEL")) {
            base.model.model_name = m;
        }

        // 4. Validate: reject program-only permission modes from user config.
        // TS parity: EXTERNAL_PERMISSION_MODES vs INTERNAL in types/permissions.ts.
        if let Err(reason) = base.validate() {
            tracing::warn!("Settings validation: {reason}");
        }

        Ok(base)
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
                user_data_dir: PathBuf::from("~/.atta/agent"),
                local_data_dir: PathBuf::from("."),
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
            mcp_servers: Vec::new(),
            language: None,
            feature_flags: crate::features::FeatureFlags::default(),
        }
    }

    fn merge(mut self, other: Settings) -> Self {
        if !other.model.model_name.is_empty() {
            self.model.model_name = other.model.model_name;
        }
        if other.model.max_tokens != 2000 {
            self.model.max_tokens = other.model.max_tokens;
        }
        if !other.model.base_url.is_empty() {
            self.model.base_url = other.model.base_url;
        }
        if !other.model.auth_token.is_empty() {
            self.model.auth_token = other.model.auth_token;
        }
        if other.instruction_file.is_some() {
            self.instruction_file = other.instruction_file;
        }
        if other.prompt_append.is_some() {
            self.prompt_append = other.prompt_append;
        }
        if other.prompt_override.is_some() {
            self.prompt_override = other.prompt_override;
        }
        if other.telemetry_url.is_some() {
            self.telemetry_url = other.telemetry_url;
        }
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Parse error: {0}")]
    Parse(#[from] serde_json::Error),
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
        let s = Settings {
            permission_mode: PermissionMode::Bubble,
            ..Settings::defaults_for("test")
        };
        assert!(s.validate().is_err());

        // Validation: Auto in settings is rejected.
        let s = Settings {
            permission_mode: PermissionMode::Auto,
            ..Settings::defaults_for("test")
        };
        assert!(s.validate().is_err());
    }
}
