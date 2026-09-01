//! `ModelFactory` — how a wire protocol gets built from configuration.
//!
//! [`Model`] is open: anyone can implement another protocol. Constructing one
//! was not. The composition root matched on `api_type` and knew exactly two
//! answers, so a third protocol could be written but never reached — the only
//! way to configure it was to edit the match, in the kernel, in a release.
//!
//! # What is registered, and by what key
//!
//! The key is the `api_type` **string** from `settings.json`, not the
//! [`ApiType`] enum. That enum names the protocols the engine itself
//! implements and is what a [`Model`] reports about itself; the registry is
//! about what a *host* can add, and a host adding a protocol has no way to
//! add a variant to an enum in this crate. A third-party model reports
//! whichever `ApiType` describes its wire shape most closely, which affects
//! how it is labeled in telemetry and nothing else.
//!
//! # Unknown means unknown at startup
//!
//! [`ModelFactoryRegistry::build`] fails for an `api_type` nobody claimed,
//! and it fails where provider config is read — at startup — rather than on
//! the first call that happens to route there. A typo in a provider name
//! should cost a clear error at launch, not a mid-conversation failure in
//! whichever session first uses that task type.

use std::collections::HashMap;
use std::sync::Arc;

use crate::interface::credentials::{ConfigCredentials, CredentialSource};
use crate::interface::model::Model;
use crate::provider::ProviderConfig;

/// Builds a live model from one provider's configuration.
pub trait ModelFactory: Send + Sync {
    /// The `api_type` value in `settings.json` this factory answers to.
    fn api_type(&self) -> &str;

    /// Construct the model, or say what is wrong with the config.
    ///
    /// `provider_id` is only for the error message — it is the name the user
    /// wrote, and an error that does not name it makes them guess which of
    /// their providers is broken.
    ///
    /// The credential comes from `credentials`, never from `config.api_key`
    /// directly: a factory that reads the field itself silently opts its
    /// protocol out of whatever the host arranged, which is exactly the bug
    /// the source exists to prevent.
    fn build(
        &self,
        provider_id: &str,
        config: &ProviderConfig,
        credentials: &dyn CredentialSource,
    ) -> Result<Arc<dyn Model>, String>;
}

/// Every wire protocol this process can construct.
#[derive(Clone)]
pub struct ModelFactoryRegistry {
    by_api_type: HashMap<String, Arc<dyn ModelFactory>>,
    credentials: Arc<dyn CredentialSource>,
}

impl Default for ModelFactoryRegistry {
    fn default() -> Self {
        Self {
            by_api_type: HashMap::new(),
            credentials: Arc::new(ConfigCredentials),
        }
    }
}

impl ModelFactoryRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a factory, replacing any previous one for the same `api_type` and
    /// returning it.
    ///
    /// Replacing is allowed on purpose: a host that wants its own client
    /// behind `anthropic` — a proxy, a recording layer, a different retry
    /// policy — should not have to invent a new `api_type` its users then
    /// have to write in their settings.
    pub fn register(&mut self, factory: Arc<dyn ModelFactory>) -> Option<Arc<dyn ModelFactory>> {
        self.by_api_type
            .insert(factory.api_type().to_string(), factory)
    }

    /// Ask `source` for every credential instead of reading `settings.json`.
    ///
    /// One source for the whole registry, not one per protocol: "where do
    /// this deployment's credentials come from" is a property of the
    /// deployment, and letting it vary per protocol would mean a host could
    /// secure one and forget the other.
    pub fn with_credentials(mut self, source: Arc<dyn CredentialSource>) -> Self {
        self.credentials = source;
        self
    }

    pub fn get(&self, api_type: &str) -> Option<&Arc<dyn ModelFactory>> {
        self.by_api_type.get(api_type)
    }

    /// The registered `api_type` values, sorted — for error messages, so what
    /// the user is told they may write is generated from what actually works.
    pub fn api_types(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.by_api_type.keys().map(String::as_str).collect();
        out.sort_unstable();
        out
    }

    pub fn is_empty(&self) -> bool {
        self.by_api_type.is_empty()
    }

    /// Build the model one provider config describes.
    ///
    /// An absent `api_type` means `anthropic`, matching
    /// [`ApiType`](crate::provider::ApiType)'s serde default and the behavior
    /// every existing settings file relies on.
    pub fn build(
        &self,
        provider_id: &str,
        config: &ProviderConfig,
    ) -> Result<Arc<dyn Model>, String> {
        let api_type = config.api_type.as_deref().unwrap_or("anthropic");
        match self.by_api_type.get(api_type) {
            Some(f) => f.build(provider_id, config, self.credentials.as_ref()),
            None => Err(format!(
                "provider '{provider_id}': unrecognized api_type '{api_type}' (expected {})",
                self.api_types()
                    .iter()
                    .map(|t| format!("'{t}'"))
                    .collect::<Vec<_>>()
                    .join(" or ")
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub(&'static str);

    impl ModelFactory for Stub {
        fn api_type(&self) -> &str {
            self.0
        }
        fn build(
            &self,
            _id: &str,
            _cfg: &ProviderConfig,
            _credentials: &dyn CredentialSource,
        ) -> Result<Arc<dyn Model>, String> {
            Err("stub".into())
        }
    }

    #[test]
    fn an_unknown_api_type_names_the_provider_and_what_is_available() {
        let mut registry = ModelFactoryRegistry::new();
        registry.register(Arc::new(Stub("anthropic")));
        registry.register(Arc::new(Stub("openai_compatible")));

        let Err(err) = registry.build(
            "my-provider",
            &ProviderConfig {
                api_type: Some("gemini".into()),
                ..Default::default()
            },
        ) else {
            panic!("an unregistered api_type must not build");
        };
        assert!(err.contains("my-provider"), "{err}");
        assert!(err.contains("'gemini'"), "{err}");
        assert!(
            err.contains("'anthropic' or 'openai_compatible'"),
            "the list of what works must come from what is registered: {err}"
        );
    }

    #[test]
    fn no_api_type_means_anthropic() {
        let mut registry = ModelFactoryRegistry::new();
        registry.register(Arc::new(Stub("anthropic")));
        let Err(err) = registry.build("p", &ProviderConfig::default()) else {
            panic!("the stub factory never succeeds");
        };
        assert_eq!(
            err, "stub",
            "an absent api_type must reach the anthropic factory"
        );
    }

    #[test]
    fn a_host_can_take_over_a_built_in_api_type() {
        let mut registry = ModelFactoryRegistry::new();
        registry.register(Arc::new(Stub("anthropic")));
        let replaced = registry.register(Arc::new(Stub("anthropic")));
        assert!(
            replaced.is_some(),
            "replacing must hand back what it replaced, not silently stack"
        );
        assert_eq!(registry.api_types(), ["anthropic"]);
    }
}
