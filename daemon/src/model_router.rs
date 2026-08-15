//! Builds the `Arc<dyn Model>` instances multi-provider LLM routing needs,
//! from parsed `settings.json` provider config — the piece
//! `base::provider::resolve_task_models` deliberately leaves undone (it only
//! resolves *which* provider/model a task should use, as pure data; it
//! can't construct a `model::adapter::AnthropicModel` itself without `base`
//! depending on `model`, inverting the crate layering).

use base::provider::{ProviderConfig, ResolvedModel, TaskRouter};
use model::adapter::AnthropicModel;
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};
use std::collections::HashMap;
use std::sync::Arc;

/// Construct the `Arc<dyn Model>` a single provider config describes.
///
/// Both `api_type` values now have a runtime implementation: `anthropic`
/// (unset means this too — same default as `base::provider::ApiType`'s serde
/// attribute) builds an `HttpAnthropicClient`, `openai_compatible` builds a
/// `model::OpenAICompatibleModel` speaking OpenAI Chat Completions.
///
/// `openai_compatible` used to be a valid *config* value with no `Model` impl
/// behind it, so configuring one was a hard startup error — which made the
/// multi-provider story Anthropic-only in practice
/// (OpenAI, Gemini-via-proxy, vLLM and Ollama were all unreachable).
///
/// Anything else is still a hard error surfaced at startup (via `Err`, which
/// `main.rs` turns into `anyhow::bail!`), not a silent downgrade or a runtime
/// panic on first use.
fn build_provider_model(
    provider_id: &str,
    cfg: &ProviderConfig,
) -> Result<Arc<dyn base::interface::model::Model>, String> {
    match cfg.api_type.as_deref() {
        Some("openai_compatible") => {
            // `base_url` is required here, unlike the Anthropic branch below:
            // there is no default OpenAI-compatible host to fall back to, and
            // guessing one would produce a confusing auth failure at first
            // use rather than a clear config error at startup.
            let base_url = cfg
                .base_url
                .clone()
                .filter(|u| !u.is_empty())
                .ok_or_else(|| {
                    format!(
                        "provider '{provider_id}': api_type 'openai_compatible' requires base_url"
                    )
                })?;
            let api_key = cfg
                .api_key
                .clone()
                .filter(|k| !k.is_empty())
                .ok_or_else(|| format!("provider '{provider_id}': missing api_key"))?;
            let default_model = cfg.default_model.clone().unwrap_or_default();
            Ok(Arc::new(
                model::OpenAICompatibleModel::new(&base_url, api_key, default_model)
                    .map_err(|e| format!("provider '{provider_id}': {e}"))?,
            ))
        }
        Some(other) if other != "anthropic" => Err(format!(
            "provider '{provider_id}': unrecognized api_type '{other}' (expected 'anthropic' \
             or 'openai_compatible')"
        )),
        _ => {
            let api_key = cfg
                .api_key
                .clone()
                .filter(|k| !k.is_empty())
                .ok_or_else(|| format!("provider '{provider_id}': missing api_key"))?;
            let auth = AuthMode::ApiKey(api_key);
            let client: Arc<dyn AnthropicClient> = match cfg.base_url.as_deref() {
                Some(url) if !url.is_empty() => {
                    let mut url = url.to_string();
                    if !url.ends_with('/') {
                        url.push('/');
                    }
                    let base = reqwest::Url::parse(&url).map_err(|e| {
                        format!("provider '{provider_id}': invalid base_url '{url}': {e}")
                    })?;
                    Arc::new(
                        HttpAnthropicClient::with_base(auth, base)
                            .map_err(|e| format!("provider '{provider_id}': {e}"))?,
                    )
                }
                _ => Arc::new(
                    HttpAnthropicClient::new(auth)
                        .map_err(|e| format!("provider '{provider_id}': {e}"))?,
                ),
            };
            Ok(Arc::new(AnthropicModel::new(client)))
        }
    }
}

/// Build a [`TaskRouter`] from parsed provider config + the result of
/// `base::provider::resolve_task_models` (already validated — this is the
/// only place that turns config into live client instances).
///
/// Builds one model instance per entry in `providers_cfg`, not just the
/// ones actually referenced by `resolved` — the map is expected to be small
/// (a handful of providers at most), and building all of them up front
/// means a later `config.setProvider` call that starts routing traffic to a
/// previously-unused provider doesn't need any extra plumbing. A
/// misconfigured provider (bad `api_key`/`base_url`/unsupported
/// `api_type`) fails the whole call — same "fail at startup, not on first
/// use" rationale as `resolve_task_models`'s hard-error paths.
pub fn build_task_router(
    providers_cfg: &HashMap<String, ProviderConfig>,
    default_provider: &str,
    resolved: HashMap<String, ResolvedModel>,
) -> Result<TaskRouter, String> {
    let mut providers = HashMap::with_capacity(providers_cfg.len());
    for (id, cfg) in providers_cfg {
        providers.insert(id.clone(), build_provider_model(id, cfg)?);
    }
    let default = providers
        .get(default_provider)
        .cloned()
        .ok_or_else(|| format!("default_provider '{default_provider}' not present in providers"))?;
    Ok(TaskRouter::new(providers, resolved, default))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Result::unwrap_err()` requires the `Ok` type to be `Debug` (for the
    /// panic message if it turns out to be `Ok`) — `Arc<dyn Model>` and
    /// `TaskRouter` aren't, so this stands in.
    fn err_of<T>(r: Result<T, String>) -> String {
        match r {
            Ok(_) => panic!("expected Err"),
            Err(e) => e,
        }
    }

    fn anthropic_provider(api_key: &str) -> ProviderConfig {
        ProviderConfig {
            api_type: Some("anthropic".into()),
            base_url: None,
            api_key: Some(api_key.into()),
            default_model: Some("claude-sonnet-4-6".into()),
            models: vec![],
        }
    }

    #[test]
    fn build_provider_model_succeeds_for_anthropic() {
        let cfg = anthropic_provider("sk-ant-test");
        assert!(build_provider_model("anthropic", &cfg).is_ok());
    }

    #[test]
    fn build_provider_model_defaults_missing_api_type_to_anthropic() {
        let mut cfg = anthropic_provider("sk-ant-test");
        cfg.api_type = None;
        assert!(build_provider_model("anthropic", &cfg).is_ok());
    }

    /// N-16: `openai_compatible` builds a real model now. It needs an
    /// explicit `base_url` (there is no sensible default host), and that
    /// requirement has to be reported clearly rather than as an auth failure
    /// on the first call.
    #[test]
    fn build_provider_model_accepts_openai_compatible_with_a_base_url() {
        let mut cfg = anthropic_provider("key");
        cfg.api_type = Some("openai_compatible".into());
        cfg.base_url = Some("https://api.example.com/v1".into());
        let built = build_provider_model("oa", &cfg).expect("should build");
        assert_eq!(built.api_type(), base::provider::ApiType::OpenAICompatible);
    }

    #[test]
    fn build_provider_model_requires_base_url_for_openai_compatible() {
        let mut cfg = anthropic_provider("key");
        cfg.api_type = Some("openai_compatible".into());
        cfg.base_url = None;
        let err = err_of(build_provider_model("oa", &cfg));
        assert!(err.contains("base_url"), "{err}");
    }

    #[test]
    fn build_provider_model_rejects_unknown_api_type() {
        let mut cfg = anthropic_provider("key");
        cfg.api_type = Some("bedrock".into());
        let err = err_of(build_provider_model("b", &cfg));
        assert!(err.contains("bedrock"));
    }

    #[test]
    fn build_provider_model_requires_api_key() {
        let mut cfg = anthropic_provider("");
        cfg.api_key = None;
        let err = err_of(build_provider_model("anthropic", &cfg));
        assert!(err.contains("api_key"));

        cfg.api_key = Some(String::new());
        let err = err_of(build_provider_model("anthropic", &cfg));
        assert!(err.contains("api_key"));
    }

    #[test]
    fn build_provider_model_rejects_invalid_base_url() {
        let mut cfg = anthropic_provider("key");
        cfg.base_url = Some("not a url".into());
        let err = err_of(build_provider_model("anthropic", &cfg));
        assert!(err.contains("invalid base_url"));
    }

    #[test]
    fn build_task_router_succeeds_with_valid_config() {
        let mut providers = HashMap::new();
        providers.insert("anthropic".to_string(), anthropic_provider("key1"));
        providers.insert("deepseek".to_string(), {
            let mut p = anthropic_provider("key2");
            p.default_model = Some("deepseek-pro".into());
            p
        });

        let router = build_task_router(&providers, "anthropic", HashMap::new()).unwrap();
        // model_for on an unresolved task falls back to the default
        // provider — just confirming this doesn't panic/error is the point
        // (identity checks live in `base::provider`'s own tests).
        let _ = router.model_for("main");
    }

    #[test]
    fn build_task_router_fails_when_default_provider_missing() {
        let providers = HashMap::new();
        let err = err_of(build_task_router(&providers, "anthropic", HashMap::new()));
        assert!(err.contains("anthropic"));
    }

    #[test]
    fn build_task_router_propagates_a_single_bad_providers_error() {
        let mut providers = HashMap::new();
        providers.insert("anthropic".to_string(), anthropic_provider("key1"));
        providers.insert("broken".to_string(), {
            let mut p = anthropic_provider("key2");
            p.api_type = Some("bedrock".into());
            p
        });
        let err = err_of(build_task_router(&providers, "anthropic", HashMap::new()));
        assert!(err.contains("broken"));
        assert!(err.contains("bedrock"));
    }
}
