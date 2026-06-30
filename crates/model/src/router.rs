//! ModelRouter — multi-provider Model instance management.
//!
//! v2.0.0: Holds a cache of `Arc<dyn Model>` keyed by provider name,
//! protected by `RwLock` for interior mutability. Lazily creates Model
//! instances from `ProviderDef` configurations on first access.
//!
//! This is the bridge between Level 1 (ProviderDef) and the Agent's
//! turn loop — when a task resolves to a specific profile + provider,
//! the router returns the right Model instance for the API call.

use crate::adapter::AnthropicModel;
use crate::client::{AuthMode, HttpAnthropicClient};
use base::interface::model::Model;
use base::provider::ProviderDef;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Multi-provider Model instance cache.
///
/// Each provider gets its own HTTP client and Model wrapper.
/// Instances are created lazily on first use and cached for the session.
/// Interior mutability via `RwLock` allows `&self` access for lazy creation.
#[derive(Clone)]
pub struct ModelRouter {
    instances: Arc<RwLock<HashMap<String, Arc<dyn Model>>>>,
}

impl ModelRouter {
    /// Create an empty router. Provider Model instances are created on first use.
    pub fn new() -> Self {
        Self {
            instances: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get or create a Model instance for the given provider.
    ///
    /// Uses `&self` — the internal `RwLock` provides interior mutability.
    /// Returns None if the provider has no auth token and no fallback is available.
    pub fn get_or_create(&self, provider: &ProviderDef) -> Option<Arc<dyn Model>> {
        // Fast path: read lock for cache hit
        {
            let guard = self.instances.read().unwrap_or_else(|e| e.into_inner());
            if let Some(m) = guard.get(&provider.id) {
                return Some(m.clone());
            }
        }

        // Slow path: create model under write lock
        let model = Self::create_model(provider)?;
        let mut guard = self.instances.write().unwrap_or_else(|e| e.into_inner());
        // Double-check: another thread may have created it while we were building
        if let Some(m) = guard.get(&provider.id) {
            return Some(m.clone());
        }
        guard.insert(provider.id.clone(), model.clone());
        Some(model)
    }

    /// Pre-warm the router with a specific provider's Model instance.
    /// Useful for the default provider to ensure it's ready before any turn.
    pub fn register(&self, provider_id: &str, model: Arc<dyn Model>) {
        let mut guard = self.instances.write().unwrap_or_else(|e| e.into_inner());
        guard.insert(provider_id.to_string(), model);
    }

    /// Get a cached Model instance without creating one.
    pub fn get(&self, provider_id: &str) -> Option<Arc<dyn Model>> {
        let guard = self.instances.read().unwrap_or_else(|e| e.into_inner());
        guard.get(provider_id).cloned()
    }

    /// Get the default Anthropic model (from env vars).
    /// Used as the ultimate fallback.
    pub fn default_anthropic() -> Option<Arc<dyn Model>> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").ok()?;
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
        let client = HttpAnthropicClient::with_base(
            AuthMode::ApiKey(api_key),
            url::Url::parse(&base_url).ok()?,
        )
        .ok()?;
        Some(Arc::new(AnthropicModel::new(Arc::new(client))))
    }

    /// Pre-warm from a ProviderRegistry — creates Model instances for all
    /// configured providers. Should be called during Agent construction.
    pub fn warm_from_registry(&self, registry: &base::provider::ProviderRegistry) {
        for provider in registry.iter() {
            if let Some(model) = Self::create_model(provider) {
                self.register(&provider.id, model);
            }
        }
    }

    /// Create a Model instance from a provider definition.
    fn create_model(provider: &ProviderDef) -> Option<Arc<dyn Model>> {
        let auth_token = match &provider.auth_token {
            Some(t) if !t.is_empty() => t.clone(),
            _ => {
                if provider.id == "anthropic" {
                    std::env::var("ANTHROPIC_API_KEY").ok()?
                } else {
                    return None;
                }
            }
        };

        let base_url = provider.base_url().unwrap_or("https://api.anthropic.com");

        match provider.interfaces.keys().next().cloned() {
            Some(base::provider::ApiType::Anthropic) | None => {
                let client = HttpAnthropicClient::with_base(
                    AuthMode::ApiKey(auth_token),
                    url::Url::parse(base_url).ok()?,
                )
                .ok()?;
                Some(Arc::new(AnthropicModel::new(Arc::new(client))))
            }
            Some(base::provider::ApiType::OpenAICompatible) => {
                let client = HttpAnthropicClient::with_base(
                    AuthMode::ApiKey(auth_token),
                    url::Url::parse(base_url).ok()?,
                )
                .ok()?;
                Some(Arc::new(AnthropicModel::new(Arc::new(client))))
            }
        }
    }
}

impl Default for ModelRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ModelRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let guard = self.instances.read().unwrap_or_else(|e| e.into_inner());
        f.debug_struct("ModelRouter")
            .field("providers", &guard.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::provider::{ApiType, ModelConfig};

    fn test_provider() -> ProviderDef {
        let mut interfaces = HashMap::new();
        interfaces.insert(ApiType::Anthropic, "https://api.anthropic.com".to_string());
        ProviderDef {
            id: "anthropic".into(),
            name: "Anthropic".into(),
            interfaces,
            auth_token: Some("sk-test".into()),
            models: vec!["claude-sonnet-4-6".into()],
            model_config: ModelConfig::default(),
        }
    }

    #[test]
    fn router_caches_instances() {
        let router = ModelRouter::new();
        let provider = test_provider();
        let m1 = router.get_or_create(&provider);
        let m2 = router.get_or_create(&provider);
        assert!(m1.is_some());
        assert!(m2.is_some());
        assert!(router.get("anthropic").is_some());
    }

    #[test]
    fn router_is_cloneable() {
        let router = ModelRouter::new();
        let provider = test_provider();
        router.get_or_create(&provider);
        let clone = router.clone();
        assert!(clone.get("anthropic").is_some());
    }

    #[test]
    fn empty_router_returns_none() {
        let router = ModelRouter::new();
        assert!(router.get("nonexistent").is_none());
    }
}
