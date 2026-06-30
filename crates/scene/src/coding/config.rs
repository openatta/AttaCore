//! CodingScene configuration — two-level provider → model profile system.
//!
//! # Architecture
//!
//! Level 1: `ProviderDef` (in `base::provider`) — API endpoint, auth, supported models
//! Level 2: `ModelProfile` (here) — task-specific model selection referencing providers
//!
//! `$strong/$normal/$lite` are field-level references that resolve to the
//! corresponding built-in profile's field value.

use crate::coding::prompt::TaskProfile;
use crate::coding::task::CodingTaskKind;
use base::provider::{ProviderDef, ProviderRegistry};
use std::collections::HashMap;

// ═══════════════════════════════════════════════════════════
// ModelProfile
// ═══════════════════════════════════════════════════════════

/// A model profile — defines which model + params to use for a task.
///
/// Fields can use `$strong` / `$normal` / `$lite` references,
/// resolved at load time to concrete values.
#[derive(Debug, Clone)]
pub struct ModelProfile {
    /// Profile identifier (e.g. "strong", "debug", "review")
    pub id: String,
    /// Provider name or `$` reference (e.g. "anthropic", "$strong")
    pub provider: Option<String>,
    /// Model name or `$` reference (e.g. "claude-opus-4-8", "$normal")
    pub model: String,
    /// Temperature override (None = provider default)
    pub temperature: Option<f32>,
    /// Max tokens per request
    pub max_tokens: Option<u32>,
    /// Thinking mode override
    pub thinking_mode: Option<String>,
}

/// A fully-resolved model profile — all `$` references have been expanded.
#[derive(Debug, Clone)]
pub struct ResolvedModelProfile {
    pub provider: String,
    pub model: String,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub thinking_mode: Option<String>,
}

// ═══════════════════════════════════════════════════════════
// ModelProfileRegistry
// ═══════════════════════════════════════════════════════════

/// Registry of model profiles with `$` reference resolution.
#[derive(Debug, Clone, Default)]
pub struct ModelProfileRegistry {
    profiles: HashMap<String, ModelProfile>,
}

impl ModelProfileRegistry {
    /// Create a registry with the three built-in tier profiles.
    pub fn builtin() -> Self {
        let mut reg = Self::default();
        reg.register(ModelProfile {
            id: "strong".into(),
            provider: Some("anthropic".into()),
            model: "claude-opus-4-8".into(),
            temperature: None,
            max_tokens: Some(8192),
            thinking_mode: None,
        });
        reg.register(ModelProfile {
            id: "normal".into(),
            provider: Some("anthropic".into()),
            model: "claude-sonnet-4-6".into(),
            temperature: None,
            max_tokens: Some(4096),
            thinking_mode: None,
        });
        reg.register(ModelProfile {
            id: "lite".into(),
            provider: Some("anthropic".into()),
            model: "claude-haiku-4-5".into(),
            temperature: None,
            max_tokens: Some(2048),
            thinking_mode: None,
        });
        reg
    }

    /// Register or replace a profile.
    pub fn register(&mut self, profile: ModelProfile) {
        self.profiles.insert(profile.id.clone(), profile);
    }

    /// Register a batch of profiles from user config.
    pub fn register_all(&mut self, profiles: impl IntoIterator<Item = ModelProfile>) {
        for p in profiles {
            self.register(p);
        }
    }

    /// Resolve a profile by id, expanding all `$` references.
    ///
    /// Returns None if the profile id is not found.
    pub fn resolve(&self, id: &str) -> Option<ResolvedModelProfile> {
        let profile = self.profiles.get(id)?;

        let provider = match profile.provider.as_deref() {
            Some(p) if p.starts_with('$') => {
                let ref_id = &p[1..]; // strip '$'
                self.profiles
                    .get(ref_id)
                    .and_then(|rp| rp.provider.clone())
                    .unwrap_or_else(|| "anthropic".into())
            }
            Some(p) => p.to_string(),
            None => "anthropic".to_string(),
        };

        let model = if profile.model.starts_with('$') {
            let ref_id = &profile.model[1..];
            self.profiles
                .get(ref_id)
                .map(|rp| rp.model.clone())
                .unwrap_or_else(|| profile.model.clone())
        } else {
            profile.model.clone()
        };

        // Inherit max_tokens from the referenced profile if not set locally
        let max_tokens = profile.max_tokens.or_else(|| {
            if profile.model.starts_with('$') {
                let ref_id = &profile.model[1..];
                self.profiles.get(ref_id).and_then(|rp| rp.max_tokens)
            } else {
                None
            }
        });

        let thinking_mode = profile.thinking_mode.clone().or_else(|| {
            if profile.model.starts_with('$') {
                let ref_id = &profile.model[1..];
                self.profiles
                    .get(ref_id)
                    .and_then(|rp| rp.thinking_mode.clone())
            } else {
                None
            }
        });

        Some(ResolvedModelProfile {
            provider,
            model,
            temperature: profile.temperature,
            max_tokens,
            thinking_mode,
        })
    }

    /// Resolve a profile id, falling back to "normal" if not found.
    pub fn resolve_or_default(&self, id: &str) -> ResolvedModelProfile {
        self.resolve(id).unwrap_or_else(|| {
            self.resolve("normal").unwrap_or(ResolvedModelProfile {
                provider: "anthropic".into(),
                model: "claude-sonnet-4-6".into(),
                temperature: None,
                max_tokens: Some(4096),
                thinking_mode: None,
            })
        })
    }

    /// List all registered profile ids.
    pub fn ids(&self) -> Vec<&str> {
        self.profiles.keys().map(|s| s.as_str()).collect()
    }

    /// Number of registered profiles.
    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    /// Whether any profiles are registered.
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }
}

// ═══════════════════════════════════════════════════════════
// CodingSceneConfig
// ═══════════════════════════════════════════════════════════

/// Complete CodingScene configuration.
///
/// Parsed from `Settings.scene_config` JSON field, or built from defaults.
#[derive(Debug, Clone)]
pub struct CodingSceneConfig {
    /// Master switch: enable task routing (default: true).
    pub enable_task_routing: bool,
    /// Enable ContextPack construction (default: true).
    pub enable_context_pack: bool,
    /// Enable verification loop (default: false — Phase 3).
    pub enable_verification_loop: bool,
    /// Enable policy hooks (default: false — Phase 4).
    pub enable_policy_hooks: bool,

    /// Provider registry (Level 1: API endpoints, auth).
    pub provider_registry: ProviderRegistry,
    /// Model profile registry (Level 2: task→model mapping).
    pub model_profiles: ModelProfileRegistry,
    /// Per-task-kind profiles (task → model_profile + tool_policy + verification_policy).
    pub task_profiles: HashMap<String, TaskProfile>,
    /// Default model profile id to use when a task doesn't specify one.
    pub default_model_profile: String,
}

impl Default for CodingSceneConfig {
    fn default() -> Self {
        Self {
            enable_task_routing: true,
            enable_context_pack: true,
            enable_verification_loop: false,
            enable_policy_hooks: false,
            provider_registry: ProviderRegistry::from_env(),
            model_profiles: ModelProfileRegistry::builtin(),
            task_profiles: HashMap::new(),
            default_model_profile: "normal".into(),
        }
    }
}

impl CodingSceneConfig {
    /// Create with defaults, optionally seeded with providers from env.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from a JSON value (from Settings.scene_config).
    pub fn from_json(value: &serde_json::Value) -> Result<Self, ConfigError> {
        let mut config = Self::default();

        if let Some(v) = value.get("enable_task_routing") {
            config.enable_task_routing = v.as_bool().unwrap_or(true);
        }
        if let Some(v) = value.get("enable_context_pack") {
            config.enable_context_pack = v.as_bool().unwrap_or(true);
        }
        if let Some(v) = value.get("enable_verification_loop") {
            config.enable_verification_loop = v.as_bool().unwrap_or(false);
        }
        if let Some(v) = value.get("enable_policy_hooks") {
            config.enable_policy_hooks = v.as_bool().unwrap_or(false);
        }

        // Parse providers
        if let Some(providers_val) = value.get("providers") {
            if let Some(providers_obj) = providers_val.as_object() {
                for (id, prov_val) in providers_obj {
                    let def = parse_provider_def(id, prov_val)?;
                    config.provider_registry.register(def);
                }
            }
        }

        // Parse model_profiles
        if let Some(profiles_val) = value.get("model_profiles") {
            if let Some(profiles_obj) = profiles_val.as_object() {
                for (id, prof_val) in profiles_obj {
                    let profile = parse_model_profile(id, prof_val)?;
                    config.model_profiles.register(profile);
                }
            }
        }

        // Parse task_profiles
        if let Some(tasks_val) = value.get("task_profiles") {
            if let Some(tasks_obj) = tasks_val.as_object() {
                for (kind, task_val) in tasks_obj {
                    let tp = parse_task_profile(kind, task_val)?;
                    config.task_profiles.insert(kind.clone(), tp);
                }
            }
        }

        if let Some(v) = value.get("default_model_profile") {
            config.default_model_profile = v.as_str().unwrap_or("normal").to_string();
        }

        Ok(config)
    }

    /// Resolve the effective TaskProfile for a given task kind.
    ///
    /// Fallback chain: user-configured task_profile → built-in default for the kind.
    pub fn resolve_task_profile(&self, kind: CodingTaskKind) -> TaskProfile {
        let kind_str = kind.as_str();
        self.task_profiles
            .get(kind_str)
            .cloned()
            .unwrap_or_else(|| TaskProfile::builtin(kind))
    }

    /// Resolve the effective model profile for a task.
    pub fn resolve_model_for_task(&self, kind: CodingTaskKind) -> ResolvedModelProfile {
        let task_profile = self.resolve_task_profile(kind);
        let profile_id = task_profile
            .model_profile
            .unwrap_or_else(|| self.default_model_profile.clone());
        self.model_profiles.resolve_or_default(&profile_id)
    }

    /// Validate that all configured model_profiles reference known providers.
    pub fn validate(&self) -> Vec<ConfigError> {
        let mut errors = Vec::new();
        for id in self.model_profiles.ids() {
            if let Some(resolved) = self.model_profiles.resolve(id) {
                if self.provider_registry.find(&resolved.provider).is_none() {
                    errors.push(ConfigError::UnknownProvider {
                        profile: id.to_string(),
                        provider: resolved.provider,
                    });
                }
            }
        }
        errors
    }
}

// ═══════════════════════════════════════════════════════════
// JSON parsing helpers
// ═══════════════════════════════════════════════════════════

fn parse_provider_def(id: &str, value: &serde_json::Value) -> Result<ProviderDef, ConfigError> {
    use base::provider::{ApiType, ModelConfig};
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(id)
        .to_string();
    let base_url = value
        .get("base_url")
        .and_then(|v| v.as_str())
        .unwrap_or("https://api.anthropic.com")
        .to_string();
    let auth_token = value
        .get("auth_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let models: Vec<String> = value
        .get("models")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let api_type = ApiType::from_base_url(&base_url);
    let mut interfaces = std::collections::HashMap::new();
    interfaces.insert(api_type, base_url);

    Ok(ProviderDef {
        id: id.to_string(),
        name,
        interfaces,
        auth_token,
        models,
        model_config: ModelConfig::default(),
    })
}

fn parse_model_profile(id: &str, value: &serde_json::Value) -> Result<ModelProfile, ConfigError> {
    let model = value
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ConfigError::MissingField {
            profile: id.to_string(),
            field: "model".to_string(),
        })?
        .to_string();

    Ok(ModelProfile {
        id: id.to_string(),
        provider: value
            .get("provider")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        model,
        temperature: value
            .get("temperature")
            .and_then(|v| v.as_f64())
            .map(|f| f as f32),
        max_tokens: value
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32),
        thinking_mode: value
            .get("thinking_mode")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

fn parse_task_profile(kind: &str, value: &serde_json::Value) -> Result<TaskProfile, ConfigError> {
    Ok(TaskProfile {
        kind: kind.to_string(),
        model_profile: value
            .get("model_profile")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        tool_policy: value
            .get("tool_policy")
            .and_then(|v| v.as_str())
            .unwrap_or("read_write")
            .to_string(),
        verification_policy: value
            .get("verification_policy")
            .and_then(|v| v.as_str())
            .unwrap_or("none")
            .to_string(),
        prompt_profile: value
            .get("prompt_profile")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        require_plan: value
            .get("require_plan")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        require_review: value
            .get("require_review")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        require_verification: value
            .get("require_verification")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    })
}

// ═══════════════════════════════════════════════════════════
// ConfigError
// ═══════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub enum ConfigError {
    UnknownProvider { profile: String, provider: String },
    MissingField { profile: String, field: String },
    Invalid(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::UnknownProvider { profile, provider } => {
                write!(
                    f,
                    "unknown provider '{provider}' referenced by profile '{profile}'"
                )
            }
            ConfigError::MissingField { profile, field } => {
                write!(f, "profile '{profile}' is missing required field '{field}'")
            }
            ConfigError::Invalid(msg) => write!(f, "invalid config: {msg}"),
        }
    }
}

impl std::error::Error for ConfigError {}

// ═══════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_has_three_tiers() {
        let reg = ModelProfileRegistry::builtin();
        assert_eq!(reg.len(), 3);
        assert!(reg.resolve("strong").is_some());
        assert!(reg.resolve("normal").is_some());
        assert!(reg.resolve("lite").is_some());
    }

    #[test]
    fn resolve_strong_profile() {
        let reg = ModelProfileRegistry::builtin();
        let resolved = reg.resolve("strong").unwrap();
        assert_eq!(resolved.provider, "anthropic");
        assert_eq!(resolved.model, "claude-opus-4-8");
        assert_eq!(resolved.max_tokens, Some(8192));
    }

    #[test]
    fn dollar_reference_resolves_model() {
        let mut reg = ModelProfileRegistry::builtin();
        // User defines a "debug" profile that references $strong
        reg.register(ModelProfile {
            id: "debug".into(),
            provider: Some("$strong".into()),
            model: "$strong".into(),
            temperature: None,
            max_tokens: None, // inherits from strong
            thinking_mode: Some("extended".into()),
        });
        let resolved = reg.resolve("debug").unwrap();
        assert_eq!(resolved.provider, "anthropic"); // $strong.provider → "anthropic"
        assert_eq!(resolved.model, "claude-opus-4-8"); // $strong.model → "claude-opus-4-8"
        assert_eq!(resolved.max_tokens, Some(8192)); // inherited from strong
        assert_eq!(resolved.thinking_mode, Some("extended".into())); // local override
    }

    #[test]
    fn dollar_reference_across_tiers() {
        let mut reg = ModelProfileRegistry::builtin();
        reg.register(ModelProfile {
            id: "review".into(),
            provider: None,
            model: "$normal".into(),
            temperature: None,
            max_tokens: None,
            thinking_mode: None,
        });
        let resolved = reg.resolve("review").unwrap();
        assert_eq!(resolved.model, "claude-sonnet-4-6");
        assert_eq!(resolved.max_tokens, Some(4096));
    }

    #[test]
    fn resolve_or_default_falls_back_to_normal() {
        let reg = ModelProfileRegistry::builtin();
        let resolved = reg.resolve_or_default("nonexistent");
        assert_eq!(resolved.model, "claude-sonnet-4-6");
    }

    #[test]
    fn default_config_has_routing_enabled() {
        let config = CodingSceneConfig::default();
        assert!(config.enable_task_routing);
        assert!(config.enable_context_pack);
        assert!(!config.enable_verification_loop);
        assert!(!config.enable_policy_hooks);
    }

    #[test]
    fn config_from_json_parses_profiles() {
        let json = serde_json::json!({
            "model_profiles": {
                "debug": {
                    "model": "$strong",
                    "max_tokens": 8192,
                    "thinking_mode": "extended"
                },
                "explain": {
                    "model": "$lite"
                }
            },
            "default_model_profile": "normal"
        });
        let config = CodingSceneConfig::from_json(&json).unwrap();
        assert_eq!(config.model_profiles.len(), 5); // 3 builtin + 2 user
        let debug = config.model_profiles.resolve("debug").unwrap();
        assert_eq!(debug.model, "claude-opus-4-8");
        let explain = config.model_profiles.resolve("explain").unwrap();
        assert_eq!(explain.model, "claude-haiku-4-5");
    }

    #[test]
    fn config_from_json_parses_providers() {
        let json = serde_json::json!({
            "providers": {
                "deepseek": {
                    "name": "DeepSeek",
                    "base_url": "https://api.deepseek.com",
                    "auth_token": "sk-test",
                    "models": ["deepseek-v4-pro", "deepseek-chat"]
                }
            }
        });
        let config = CodingSceneConfig::from_json(&json).unwrap();
        let ds = config.provider_registry.find("deepseek").unwrap();
        assert_eq!(ds.name, "DeepSeek");
        assert_eq!(ds.models.len(), 2);
    }
}
