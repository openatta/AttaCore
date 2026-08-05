//! Multi-provider LLM config: `ApiType` + provider registry + per-task-type
//! model routing, parsed from the `providers`/`default_provider`/
//! `task_models` sections of `settings.json`.
//!
//! **2026-08-04, second round**: moved here from `daemon::model_routing` so
//! `core::Settings` (not just `daemon`'s separate `SettingsFile`) can carry
//! these fields directly — see `docs/design/2026-08-04-multi-provider-llm-migration.md`
//! for why the two-representation split was a structural risk worth fixing.
//!
//! This module only resolves *which provider/model a task should use* from
//! already-merged config; it does not yet dispatch actual LLM requests
//! through the resolved provider (Phase 4 in the design doc above — wiring
//! `SessionPool`/`AgentTool`/etc. to consume this is future work).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// API protocol type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub enum ApiType {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai_compatible")]
    OpenAICompatible,
}

/// A single provider's connection + model config, as written in settings.json.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ProviderConfig {
    /// "anthropic" | "openai_compatible". Kept as a plain string (not
    /// `ApiType`) so an unrecognized value degrades to a warning instead of
    /// a hard parse failure of the whole settings.json.
    pub api_type: Option<String>,
    pub base_url: Option<String>,
    /// Stored in plaintext, same as every other settings.json field — see
    /// the "安全提示" section in `docs/LLM_PROVIDERS.md`.
    pub api_key: Option<String>,
    /// Model used when a task_models override doesn't name one explicitly,
    /// or when a named model fails the `models` allow-list check below.
    pub default_model: Option<String>,
    /// Allow-list used purely to *filter* an explicitly-requested model name;
    /// it is not a live catalog fetched from the provider. Empty = no
    /// filtering, any model name is accepted as-is.
    #[serde(default)]
    pub models: Vec<String>,
}

/// A `task_models.<task>` entry. Accepts either the shorthand `"provider_id"`
/// string form or the detailed `{ "provider": ..., "model": ... }` object form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum TaskModelOverride {
    ProviderOnly(String),
    Detailed {
        provider: String,
        #[serde(default)]
        model: Option<String>,
    },
}

impl TaskModelOverride {
    fn provider(&self) -> &str {
        match self {
            TaskModelOverride::ProviderOnly(p) => p,
            TaskModelOverride::Detailed { provider, .. } => provider,
        }
    }

    fn model(&self) -> Option<&str> {
        match self {
            TaskModelOverride::ProviderOnly(_) => None,
            TaskModelOverride::Detailed { model, .. } => model.as_deref(),
        }
    }
}

/// The fully-resolved (provider, model) pair a task type will actually use.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModel {
    pub provider_id: String,
    pub model: String,
}

/// Resolve every `task_models` entry (plus an implicit `default_provider`
/// resolution used by any task with no entry) against the `providers` map.
///
/// Degrades softly wherever possible — this is deliberate: a config typo
/// should produce a startup warning, not a crash. See
/// `docs/design/2026-08-04-multi-provider-llm-migration.md` §3.3 for the
/// two-tier failure semantics:
///
/// - `default_provider` missing, empty, or not present in `providers` →
///   hard error (`Err`); there is no lower fallback left.
/// - A `task_models` entry naming an unknown provider → falls back to
///   `default_provider`, with a warning.
/// - A `task_models` entry naming a model absent from that provider's
///   (non-empty) `models` allow-list → falls back to *that provider's own*
///   `default_model`, with a warning (not to `default_provider` — the
///   provider choice itself was valid, only the model name wasn't).
pub fn resolve_task_models(
    providers: &HashMap<String, ProviderConfig>,
    default_provider: Option<&str>,
    task_models: &HashMap<String, TaskModelOverride>,
) -> Result<(HashMap<String, ResolvedModel>, Vec<String>), String> {
    let mut warnings = Vec::new();

    let default_provider = default_provider
        .filter(|p| !p.is_empty())
        .ok_or_else(|| "default_provider is not set but providers are configured".to_string())?;
    let default_model = providers
        .get(default_provider)
        .ok_or_else(|| {
            format!(
                "default_provider '{default_provider}' is not present in providers \
                 ({:?})",
                providers.keys().collect::<Vec<_>>()
            )
        })?
        .default_model
        .clone()
        .ok_or_else(|| format!("default_provider '{default_provider}' has no default_model"))?;

    let mut resolved = HashMap::new();

    for (task, ov) in task_models {
        let requested_provider = ov.provider();
        let Some(provider_cfg) = providers.get(requested_provider) else {
            warnings.push(format!(
                "task_models.{task} 引用的 provider '{requested_provider}' 不存在，\
                 已降级到 default_provider '{default_provider}'"
            ));
            resolved.insert(
                task.clone(),
                ResolvedModel {
                    provider_id: default_provider.to_string(),
                    model: default_model.clone(),
                },
            );
            continue;
        };

        let model = match ov.model() {
            None => provider_cfg.default_model.clone().unwrap_or_else(|| {
                warnings.push(format!(
                    "task_models.{task} 的 provider '{requested_provider}' 没有配置 \
                     default_model，回退到 default_provider '{default_provider}'"
                ));
                default_model.clone()
            }),
            Some(m) if provider_cfg.models.is_empty() || provider_cfg.models.iter().any(|x| x == m) => {
                m.to_string()
            }
            Some(m) => {
                let fallback = provider_cfg.default_model.clone().unwrap_or_else(|| {
                    warnings.push(format!(
                        "task_models.{task} 指定的模型 '{m}' 不在 provider \
                         '{requested_provider}' 的 models 列表 {:?} 中，且该 provider 没有 \
                         default_model，回退到 default_provider '{default_provider}'",
                        provider_cfg.models
                    ));
                    default_model.clone()
                });
                warnings.push(format!(
                    "task_models.{task} 指定的模型 '{m}' 不在 provider '{requested_provider}' \
                     的 models 列表 {:?} 中，已回退到该 provider 的默认模型 '{fallback}'",
                    provider_cfg.models
                ));
                fallback
            }
        };

        resolved.insert(
            task.clone(),
            ResolvedModel {
                provider_id: requested_provider.to_string(),
                model,
            },
        );
    }

    Ok((resolved, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(default_model: &str, models: &[&str]) -> ProviderConfig {
        ProviderConfig {
            api_type: Some("openai_compatible".into()),
            base_url: Some("https://example.invalid".into()),
            api_key: Some("key".into()),
            default_model: Some(default_model.into()),
            models: models.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn providers_fixture() -> HashMap<String, ProviderConfig> {
        let mut p = HashMap::new();
        p.insert(
            "anthropic".to_string(),
            provider("claude-sonnet-4-6", &[]),
        );
        p.insert(
            "deepseek".to_string(),
            provider("deepseek-pro", &["deepseek-pro", "deepseek-flash"]),
        );
        p
    }

    #[test]
    fn no_override_uses_default_provider_and_its_default_model() {
        let providers = providers_fixture();
        let task_models = HashMap::new();
        let (resolved, warnings) =
            resolve_task_models(&providers, Some("anthropic"), &task_models).unwrap();
        assert!(resolved.is_empty()); // nothing to resolve; callers fall back to default themselves
        assert!(warnings.is_empty());
    }

    #[test]
    fn shorthand_override_uses_that_providers_default_model() {
        let providers = providers_fixture();
        let mut task_models = HashMap::new();
        task_models.insert(
            "subagent".to_string(),
            TaskModelOverride::ProviderOnly("deepseek".into()),
        );
        let (resolved, warnings) =
            resolve_task_models(&providers, Some("anthropic"), &task_models).unwrap();
        assert_eq!(
            resolved["subagent"],
            ResolvedModel {
                provider_id: "deepseek".into(),
                model: "deepseek-pro".into(),
            }
        );
        assert!(warnings.is_empty());
    }

    /// Mirrors the user's own example: DS supports PRO/FLASH; asking for MAX
    /// warns and falls back to DS's own default (PRO), not to the global
    /// default_provider.
    #[test]
    fn model_outside_allow_list_falls_back_to_same_providers_default_and_warns() {
        let providers = providers_fixture();
        let mut task_models = HashMap::new();
        task_models.insert(
            "subagent".to_string(),
            TaskModelOverride::Detailed {
                provider: "deepseek".into(),
                model: Some("deepseek-max".into()),
            },
        );
        let (resolved, warnings) =
            resolve_task_models(&providers, Some("anthropic"), &task_models).unwrap();
        assert_eq!(
            resolved["subagent"],
            ResolvedModel {
                provider_id: "deepseek".into(),
                model: "deepseek-pro".into(),
            }
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("deepseek-max"));
        assert!(warnings[0].contains("deepseek-pro"));
    }

    #[test]
    fn model_inside_allow_list_is_used_as_is() {
        let providers = providers_fixture();
        let mut task_models = HashMap::new();
        task_models.insert(
            "subagent".to_string(),
            TaskModelOverride::Detailed {
                provider: "deepseek".into(),
                model: Some("deepseek-flash".into()),
            },
        );
        let (resolved, warnings) =
            resolve_task_models(&providers, Some("anthropic"), &task_models).unwrap();
        assert_eq!(
            resolved["subagent"],
            ResolvedModel {
                provider_id: "deepseek".into(),
                model: "deepseek-flash".into(),
            }
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn empty_allow_list_accepts_any_model_name() {
        let providers = providers_fixture();
        let mut task_models = HashMap::new();
        task_models.insert(
            "main".to_string(),
            TaskModelOverride::Detailed {
                provider: "anthropic".into(),
                model: Some("claude-opus-4-8".into()),
            },
        );
        let (resolved, warnings) =
            resolve_task_models(&providers, Some("anthropic"), &task_models).unwrap();
        assert_eq!(
            resolved["main"],
            ResolvedModel {
                provider_id: "anthropic".into(),
                model: "claude-opus-4-8".into(),
            }
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn unknown_provider_falls_back_to_default_provider_and_warns() {
        let providers = providers_fixture();
        let mut task_models = HashMap::new();
        task_models.insert(
            "web_fetch".to_string(),
            TaskModelOverride::ProviderOnly("nonexistent".into()),
        );
        let (resolved, warnings) =
            resolve_task_models(&providers, Some("anthropic"), &task_models).unwrap();
        assert_eq!(
            resolved["web_fetch"],
            ResolvedModel {
                provider_id: "anthropic".into(),
                model: "claude-sonnet-4-6".into(),
            }
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("nonexistent"));
    }

    #[test]
    fn missing_default_provider_is_a_hard_error() {
        let providers = providers_fixture();
        let task_models = HashMap::new();
        let err = resolve_task_models(&providers, None, &task_models).unwrap_err();
        assert!(err.contains("default_provider"));
    }

    #[test]
    fn default_provider_not_in_providers_is_a_hard_error() {
        let providers = providers_fixture();
        let task_models = HashMap::new();
        let err = resolve_task_models(&providers, Some("ghost"), &task_models).unwrap_err();
        assert!(err.contains("ghost"));
    }

    #[test]
    fn task_model_override_deserializes_both_forms() {
        let shorthand: TaskModelOverride = serde_json::from_str("\"deepseek\"").unwrap();
        assert_eq!(shorthand, TaskModelOverride::ProviderOnly("deepseek".into()));

        let detailed: TaskModelOverride =
            serde_json::from_str(r#"{"provider":"deepseek","model":"deepseek-flash"}"#).unwrap();
        assert_eq!(
            detailed,
            TaskModelOverride::Detailed {
                provider: "deepseek".into(),
                model: Some("deepseek-flash".into())
            }
        );
    }
}
