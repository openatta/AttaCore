//! Model registry — multi-provider model discovery and selection.

use serde::{Deserialize, Serialize};

/// A registered model from a provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelInfo {
    pub provider_id: String,
    pub provider_name: String,
    pub model_name: String,
    pub tier: String,
}

/// Registry of available models across providers, loaded from settings.json.
#[derive(Debug, Clone, Default)]
pub struct ModelRegistry {
    models: Vec<ModelInfo>,
    default_model: Option<String>,
}

impl ModelRegistry {
    /// Load models from user and local settings directories.
    /// Reads `[providers]` section from settings.json in each directory.
    /// Priority: local overrides user.
    pub fn load(user_dir: std::path::PathBuf, local_dir: std::path::PathBuf) -> Self {
        let mut providers: Vec<ProviderConfig> = Vec::new();

        // Parse user settings
        if let Some(ps) = Self::parse_providers(&user_dir.join("settings.json")) {
            providers = ps;
        }
        // Local overrides
        if let Some(ps) = Self::parse_providers(&local_dir.join("settings.json")) {
            for p in ps {
                if let Some(existing) = providers.iter_mut().find(|x| x.id == p.id) {
                    if p.model.is_some() {
                        existing.model = p.model;
                    }
                    if p.sonnet.is_some() {
                        existing.sonnet = p.sonnet;
                    }
                    if p.opus.is_some() {
                        existing.opus = p.opus;
                    }
                    if p.haiku.is_some() {
                        existing.haiku = p.haiku;
                    }
                } else {
                    providers.push(p);
                }
            }
        }

        Self::from_providers(providers)
    }

    fn parse_providers(path: &std::path::Path) -> Option<Vec<ProviderConfig>> {
        let content = std::fs::read_to_string(path).ok()?;
        let root: serde_json::Value = serde_json::from_str(&content).ok()?;
        let arr = root.get("providers")?.as_array()?;
        let mut providers = Vec::new();
        for entry in arr {
            let id = entry.get("id")?.as_str()?.to_string();
            let name = entry
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&id)
                .to_string();
            providers.push(ProviderConfig {
                id,
                name,
                model: entry
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                sonnet: entry
                    .get("sonnet_model")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                opus: entry
                    .get("opus_model")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                haiku: entry
                    .get("haiku_model")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            });
        }
        Some(providers)
    }

    /// Build from a list of provider configs.
    pub fn from_providers(providers: Vec<ProviderConfig>) -> Self {
        let mut models = Vec::new();
        let mut default = None;
        for p in &providers {
            let pid = p.id.clone();
            let pname = p.name.clone();
            let push = |models: &mut Vec<ModelInfo>, name: &Option<String>, tier: &str| {
                if let Some(n) = name {
                    if !n.is_empty() {
                        models.push(ModelInfo {
                            provider_id: pid.clone(),
                            provider_name: pname.clone(),
                            model_name: n.clone(),
                            tier: tier.into(),
                        });
                    }
                }
            };
            push(&mut models, &p.model, "main");
            push(&mut models, &p.sonnet, "sonnet");
            push(&mut models, &p.opus, "opus");
            push(&mut models, &p.haiku, "haiku");
            if default.is_none() {
                default = p.model.clone();
            }
        }
        Self {
            models,
            default_model: default,
        }
    }

    pub fn list(&self) -> &[ModelInfo] {
        &self.models
    }
    pub fn default(&self) -> Option<&str> {
        self.default_model.as_deref()
    }
    pub fn find(&self, model_name: &str) -> Option<&ModelInfo> {
        self.models.iter().find(|m| m.model_name == model_name)
    }
}

/// Provider configuration for model registry construction.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    pub model: Option<String>,
    pub sonnet: Option<String>,
    pub opus: Option<String>,
    pub haiku: Option<String>,
}

impl ProviderConfig {
    pub fn anthropic(model: &str) -> Self {
        Self {
            id: "anthropic".into(),
            name: "Anthropic".into(),
            model: Some(model.into()),
            sonnet: None,
            opus: None,
            haiku: None,
        }
    }
    pub fn deepseek(model: &str) -> Self {
        Self {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            model: Some(model.into()),
            sonnet: None,
            opus: None,
            haiku: None,
        }
    }
}
