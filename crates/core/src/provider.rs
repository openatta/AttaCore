//! Multi-provider registry for LLM backends.
//!
//! Supports Anthropic Messages API and OpenAI-compatible endpoints.
//! Each provider can expose multiple API type interfaces.
//! Fallback chain is strictly within a single provider.

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

/// A single provider definition with model slot configuration.
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

impl ProviderRegistry {
    pub fn new(providers: Vec<ProviderDef>) -> Self {
        Self {
            providers,
            active_index: 0,
        }
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
