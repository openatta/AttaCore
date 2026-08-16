//! `daemon.doctor` — read-only configuration/wiring diagnostics.
//!
//! Re-inspects the same three settings.json tiers `Settings::load()` already
//! merged at startup, but reports per-tier existence/parse status instead of
//! just logging a warning and moving on — useful for a user (or an IDE
//! plugin) asking "why isn't my provider config taking effect?" without
//! grepping daemon logs.

use crate::config::DaemonPaths;
use base::interface::settings::Settings;
use std::path::Path;

/// Status of a single settings.json layer.
#[derive(serde::Serialize)]
struct TierStatus {
    tier: &'static str,
    path: String,
    exists: bool,
    parses: bool,
    error: Option<String>,
}

fn inspect_tier(tier: &'static str, dir: &Path) -> TierStatus {
    let path = dir.join("settings.json");
    let path_str = path.display().to_string();
    if !path.exists() {
        return TierStatus {
            tier,
            path: path_str,
            exists: false,
            parses: true,
            error: None,
        };
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(_) => TierStatus {
                tier,
                path: path_str,
                exists: true,
                parses: true,
                error: None,
            },
            Err(e) => TierStatus {
                tier,
                path: path_str,
                exists: true,
                parses: false,
                error: Some(format!("invalid JSON: {e}")),
            },
        },
        Err(e) => TierStatus {
            tier,
            path: path_str,
            exists: true,
            parses: false,
            error: Some(format!("read error: {e}")),
        },
    }
}

/// Build the full diagnostic report as a JSON value (the RPC result body).
pub fn run_doctor(
    paths: &dyn DaemonPaths,
    scene_id: &str,
    settings: &Settings,
    history_store_wired: bool,
    plugin_status: crate::plugins::PluginStatus,
) -> serde_json::Value {
    let global_root = paths.global_root();
    let scene_root = paths.config_root();
    let project_root = paths.project_root().join(".atta");

    let tiers = vec![
        inspect_tier("global", &global_root),
        inspect_tier("scene", &scene_root),
        inspect_tier("project", &project_root),
    ];

    // Provider / task_models routing validation — same call `main.rs` makes
    // at startup, re-run here so `doctor` reflects the live settings even if
    // this is a hot-reload/inspection call rather than a fresh process.
    let (providers_ok, provider_warnings, provider_error) = if settings.providers.is_empty() {
        (true, Vec::new(), None)
    } else {
        match base::provider::resolve_task_models(
            &settings.providers,
            settings.default_provider.as_deref(),
            &settings.task_models,
        ) {
            Ok((_, warnings)) => (true, warnings, None),
            Err(e) => (false, Vec::new(), Some(e)),
        }
    };

    let (hooks_ok, hooks_error) = match &settings.hooks_config {
        None => (true, None),
        Some(v) => match serde_json::from_value::<hooks::HooksSettings>(v.clone()) {
            Ok(_) => (true, None),
            Err(e) => (false, Some(format!("invalid hooks_config: {e}"))),
        },
    };

    serde_json::json!({
        "scene": scene_id,
        "settings_tiers": tiers,
        "providers": {
            "configured": settings.providers.keys().collect::<Vec<_>>(),
            "default_provider": settings.default_provider,
            "task_models": settings.task_models.keys().collect::<Vec<_>>(),
            "ok": providers_ok,
            "warnings": provider_warnings,
            "error": provider_error,
        },
        "hooks": {
            "configured": settings.hooks_config.is_some(),
            "ok": hooks_ok,
            "error": hooks_error,
        },
        "session_persistence": {
            "history_store_wired": history_store_wired,
        },
        // Whether this binary can load plugins at all is a deployment fact a
        // locked-down install has to be able to verify from the running
        // process, not from release notes.
        "plugins": {
            "status": plugin_status.as_str(),
        },
        "permission_rules_count": settings.permission_rules.len(),
        "model": {
            "model_name": settings.model.model_name,
            "api_type": format!("{:?}", settings.model.api_type),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StaticDaemonPaths;

    fn base_settings() -> Settings {
        Settings::defaults_for("test-model")
    }

    #[test]
    fn reports_missing_tiers_as_not_existing_but_ok() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StaticDaemonPaths::new(dir.path().to_path_buf());
        let settings = base_settings();
        let report = run_doctor(
            &paths,
            "coding",
            &settings,
            false,
            crate::plugins::PluginStatus::Enabled,
        );
        let tiers = report["settings_tiers"].as_array().unwrap();
        assert_eq!(tiers.len(), 3);
        for t in tiers {
            assert_eq!(t["exists"], false);
            assert_eq!(t["parses"], true);
        }
    }

    #[test]
    fn reports_malformed_tier_as_not_parsing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.json"), "{not json").unwrap();
        let paths = StaticDaemonPaths::new(dir.path().to_path_buf());
        let settings = base_settings();
        let report = run_doctor(
            &paths,
            "coding",
            &settings,
            false,
            crate::plugins::PluginStatus::Enabled,
        );
        let tiers = report["settings_tiers"].as_array().unwrap();
        // config_root == project_root == dir here, so both entries see the
        // malformed file — only assert the ones that actually point at `dir`.
        assert!(tiers
            .iter()
            .any(|t| t["exists"] == true && t["parses"] == false));
    }

    #[test]
    fn reports_provider_routing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StaticDaemonPaths::new(dir.path().to_path_buf());
        let mut settings = base_settings();
        settings.default_provider = Some("does-not-exist".into());
        settings.providers.insert(
            "real".into(),
            base::provider::ProviderConfig {
                api_type: Some("anthropic".into()),
                base_url: None,
                api_key: None,
                default_model: Some("m".into()),
                models: vec![],
            },
        );
        let report = run_doctor(
            &paths,
            "coding",
            &settings,
            false,
            crate::plugins::PluginStatus::Enabled,
        );
        assert_eq!(report["providers"]["ok"], false);
        assert!(report["providers"]["error"].as_str().is_some());
    }

    #[test]
    fn reports_history_store_wiring_status() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StaticDaemonPaths::new(dir.path().to_path_buf());
        let settings = base_settings();
        let report = run_doctor(
            &paths,
            "coding",
            &settings,
            true,
            crate::plugins::PluginStatus::Enabled,
        );
        assert_eq!(report["session_persistence"]["history_store_wired"], true);
    }

    /// A locked-down deployment has to be able to confirm from the running
    /// process that plugins are not merely switched off but absent.
    #[test]
    fn doctor_reports_the_plugin_subsystem_status() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StaticDaemonPaths::new(dir.path().to_path_buf());
        let settings = base_settings();

        for status in [
            crate::plugins::PluginStatus::Enabled,
            crate::plugins::PluginStatus::DisabledByPolicy,
            crate::plugins::PluginStatus::CompiledOut,
        ] {
            let report = run_doctor(&paths, "coding", &settings, false, status);
            assert_eq!(report["plugins"]["status"], status.as_str());
        }
    }
}
