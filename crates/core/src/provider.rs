//! Multi-provider registry for LLM backends.
//!
//! Supports Anthropic Messages API and OpenAI-compatible endpoints.
//! Each provider can expose multiple API type interfaces.
//! Fallback chain is strictly within a single provider.
//!
//! # v2.0.0: Two-level provider → profile system
//!
//! Level 1 — ProviderDef: "who serves the model" (API endpoint, auth, available models)
//! Level 2 — ModelProfile (in scene crate): "what model + params for this task"
//!   Profiles reference providers by name; $strong/$normal/$lite resolve to base profiles.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// API protocol type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ApiType {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai_compatible")]
    OpenAICompatible,
}

impl ApiType {
    /// Guess the API type from a base URL.
    pub fn from_base_url(url: &str) -> Self {
        let lower = url.to_lowercase();
        if lower.contains("anthropic") {
            ApiType::Anthropic
        } else {
            ApiType::OpenAICompatible
        }
    }
}

/// A single provider definition with model slot configuration.
///
/// Level 1 of the two-level config: defines "who serves the model".
/// Each provider has API endpoint(s), auth, and a list of supported model names.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderDef {
    /// Unique provider identifier (e.g. "anthropic", "deepseek")
    pub id: String,
    /// Human-readable name
    pub name: String,
    /// api_type → base_url map
    pub interfaces: HashMap<ApiType, String>,
    /// Auth token (one per provider)
    pub auth_token: Option<String>,
    /// Supported model names (whitelist). Empty = no validation.
    /// v2.0.0: used to validate ModelProfile.model references.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Model slot configuration
    pub model_config: ModelConfig,
}

/// Model slot configuration for semantic resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Primary model (required)
    pub model: String,
    /// Opus-tier model
    pub opus_model: Option<String>,
    /// Sonnet-tier model
    pub sonnet_model: Option<String>,
    /// Haiku-tier model
    pub haiku_model: Option<String>,
    /// Sub-agent model
    pub subagent_model: Option<String>,
    /// Strong reasoning model
    pub strong_model: Option<String>,
    /// Fallback model
    pub fallback_model: Option<String>,
    /// Classifier model
    pub classifier_model: Option<String>,
    /// Compact/summary model
    pub compact_model: Option<String>,
    /// Max tokens per request
    pub max_tokens: Option<u32>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-6".into(),
            opus_model: Some("claude-opus-4-8".into()),
            sonnet_model: Some("claude-sonnet-4-6".into()),
            haiku_model: Some("claude-haiku-4-5".into()),
            subagent_model: None,
            strong_model: None,
            fallback_model: None,
            classifier_model: None,
            compact_model: None,
            max_tokens: Some(4096),
        }
    }
}

/// Semantic slot identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelSlot {
    Main,
    Opus,
    Sonnet,
    Haiku,
    Subagent,
    Strong,
    Fallback,
    Classifier,
    Compact,
}

/// Multi-provider registry.
#[derive(Debug, Clone)]
pub struct ProviderRegistry {
    providers: Vec<ProviderDef>,
    active_index: usize,
}

/// Resolves semantic slot names to concrete model names within a provider.
#[derive(Debug, Clone)]
pub struct ModelResolver {
    config: ModelConfig,
}

impl ProviderDef {
    /// Build the built-in anthropic provider from environment variables.
    /// Reads `ANTHROPIC_API_KEY` (required) and `ANTHROPIC_BASE_URL` (optional).
    /// Returns None if no API key is set.
    pub fn from_env_anthropic() -> Option<Self> {
        let auth_token = std::env::var("ANTHROPIC_API_KEY").ok()?;
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".into());
        Some(Self {
            id: "anthropic".into(),
            name: "Anthropic".into(),
            interfaces: {
                let mut m = HashMap::new();
                m.insert(ApiType::Anthropic, base_url);
                m
            },
            auth_token: Some(auth_token),
            models: vec![
                "claude-opus-4-8".into(),
                "claude-sonnet-4-6".into(),
                "claude-haiku-4-5".into(),
                "claude-fable-5".into(),
            ],
            model_config: ModelConfig::default(),
        })
    }

    /// Get the primary base URL (prefers Anthropic interface).
    pub fn base_url(&self) -> Option<&str> {
        self.interfaces
            .get(&ApiType::Anthropic)
            .or_else(|| self.interfaces.get(&ApiType::OpenAICompatible))
            .map(|s| s.as_str())
    }

    /// Check if a model name is in the provider's supported models list.
    /// Returns true if models is empty (no validation) or if the model is found.
    pub fn supports_model(&self, model_name: &str) -> bool {
        self.models.is_empty() || self.models.iter().any(|m| m == model_name)
    }
}

impl ProviderRegistry {
    pub fn new(providers: Vec<ProviderDef>) -> Self {
        Self {
            providers,
            active_index: 0,
        }
    }

    /// v2.0.0: Create a registry seeded with the built-in anthropic provider from env.
    /// Falls back to an empty registry if no credentials are available.
    pub fn from_env() -> Self {
        let mut providers = Vec::new();
        if let Some(anthropic) = ProviderDef::from_env_anthropic() {
            providers.push(anthropic);
        }
        Self {
            providers,
            active_index: 0,
        }
    }

    /// v2.0.0: Register or replace a provider by id.
    pub fn register(&mut self, def: ProviderDef) {
        if let Some(idx) = self.providers.iter().position(|p| p.id == def.id) {
            self.providers[idx] = def;
        } else {
            self.providers.push(def);
        }
    }

    /// v2.0.0: Look up a provider by id.
    pub fn find(&self, id: &str) -> Option<&ProviderDef> {
        self.providers.iter().find(|p| p.id == id)
    }

    /// v2.0.0: Look up a provider by id (mutable).
    pub fn find_mut(&mut self, id: &str) -> Option<&mut ProviderDef> {
        self.providers.iter_mut().find(|p| p.id == id)
    }

    pub fn activate(&mut self, provider_id: &str) -> Result<(), ProviderError> {
        let idx = self
            .providers
            .iter()
            .position(|p| p.id == provider_id)
            .ok_or_else(|| ProviderError::NotFound {
                id: provider_id.to_string(),
                available: self.providers.iter().map(|p| p.id.clone()).collect(),
            })?;
        self.active_index = idx;
        Ok(())
    }

    pub fn active(&self) -> &ProviderDef {
        &self.providers[self.active_index]
    }

    /// v2.0.0: Iterate all registered providers.
    pub fn iter(&self) -> impl Iterator<Item = &ProviderDef> {
        self.providers.iter()
    }

    pub fn find_by_model(&self, model_name: &str) -> Option<&ProviderDef> {
        self.providers.iter().find(|p| {
            let mc = &p.model_config;
            mc.model == model_name
                || mc.opus_model.as_deref() == Some(model_name)
                || mc.sonnet_model.as_deref() == Some(model_name)
                || mc.haiku_model.as_deref() == Some(model_name)
                || mc.fallback_model.as_deref() == Some(model_name)
                || mc.compact_model.as_deref() == Some(model_name)
        })
    }

    pub fn available_models(&self) -> Vec<AvailableModel> {
        self.providers
            .iter()
            .flat_map(|p| {
                let mut models = vec![AvailableModel {
                    name: p.model_config.model.clone(),
                    provider_id: p.id.clone(),
                    slot: ModelSlot::Main,
                }];
                for (name, slot) in [
                    (&p.model_config.opus_model, ModelSlot::Opus),
                    (&p.model_config.sonnet_model, ModelSlot::Sonnet),
                    (&p.model_config.haiku_model, ModelSlot::Haiku),
                    (&p.model_config.fallback_model, ModelSlot::Fallback),
                    (&p.model_config.compact_model, ModelSlot::Compact),
                ] {
                    if let Some(n) = name {
                        models.push(AvailableModel {
                            name: n.clone(),
                            provider_id: p.id.clone(),
                            slot,
                        });
                    }
                }
                models
            })
            .collect()
    }
}

impl ModelResolver {
    pub fn new(config: ModelConfig) -> Self {
        Self { config }
    }

    /// Resolve the main model (sonnet → model).
    pub fn main(&self) -> &str {
        self.config
            .sonnet_model
            .as_deref()
            .unwrap_or(&self.config.model)
    }

    /// Resolve the strong model (strong → opus → model).
    pub fn strong(&self) -> &str {
        self.config
            .strong_model
            .as_deref()
            .or(self.config.opus_model.as_deref())
            .unwrap_or(&self.config.model)
    }

    /// Resolve the fallback model (fallback → opus → model).
    pub fn fallback(&self) -> &str {
        self.config
            .fallback_model
            .as_deref()
            .or(self.config.opus_model.as_deref())
            .unwrap_or(&self.config.model)
    }

    /// Resolve the compact model (compact → haiku → model).
    pub fn compact(&self) -> &str {
        self.config
            .compact_model
            .as_deref()
            .or(self.config.haiku_model.as_deref())
            .unwrap_or(&self.config.model)
    }
}

#[derive(Debug, Clone)]
pub struct AvailableModel {
    pub name: String,
    pub provider_id: String,
    pub slot: ModelSlot,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider '{id}' not found; available: {available:?}")]
    NotFound { id: String, available: Vec<String> },
    #[error("no auth token configured")]
    NoCredentials,
    #[error("unsupported API type: {0:?}")]
    UnsupportedApiType(ApiType),
}
