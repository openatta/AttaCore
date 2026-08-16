//! Validating a plugin's user configuration before the plugin sees it.
//!
//! A plugin declares a JSON Schema for what it accepts, the user writes a
//! `config.json` beside it, and the two are checked against each other here.
//!
//! Doing it at load rather than inside the plugin is what makes the failure
//! legible: a plugin left to parse its own configuration reports the problem
//! in its own words, at whatever moment it first looks — which for a badly
//! written one is the middle of a tool call.

use anyhow::{anyhow, Result};
use std::path::Path;

/// Filename a plugin's user configuration is read from, inside the plugin's
/// installed directory.
pub const CONFIG_FILE: &str = "config.json";

/// Read and validate `plugin`'s configuration.
///
/// Returns the JSON to hand `init`. A plugin with no configuration and no
/// schema gets `{}`, which is what "nothing to configure" serialises to.
pub fn load_config(plugin: &plugin::manifest::Plugin) -> Result<serde_json::Value> {
    let path = plugin.root.join(CONFIG_FILE);
    let config: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|e| anyhow!("{CONFIG_FILE} is not valid JSON: {e}"))?,
        // Absent is not empty-and-wrong: a plugin whose schema requires
        // nothing is correctly configured by having no file at all.
        Err(_) => serde_json::json!({}),
    };

    if let Some(rel) = &plugin.manifest.plugin.config.schema {
        validate_against(&plugin.path(rel), &config)?;
    }
    Ok(config)
}

/// Check `config` against the schema at `schema_path`.
///
/// A schema the plugin author wrote badly is the author's problem and is
/// reported as such; it does not silently become "no validation", because a
/// schema that never rejects anything is indistinguishable from one that was
/// never applied.
pub fn validate_against(schema_path: &Path, config: &serde_json::Value) -> Result<()> {
    let raw = std::fs::read_to_string(schema_path)
        .map_err(|e| anyhow!("config schema {}: {e}", schema_path.display()))?;
    let schema: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| anyhow!("config schema {} is not valid JSON: {e}", schema_path.display()))?;
    let validator = jsonschema::validator_for(&schema)
        .map_err(|e| anyhow!("config schema {} is not a valid JSON Schema: {e}", schema_path.display()))?;

    let problems: Vec<String> = validator
        .iter_errors(config)
        .map(|e| {
            let at = e.instance_path().to_string();
            if at.is_empty() {
                e.to_string()
            } else {
                format!("{at}: {e}")
            }
        })
        .collect();

    if problems.is_empty() {
        return Ok(());
    }
    // Every problem, not just the first: a user fixing one at a time,
    // reinstalling between each, is a worse experience than one list.
    Err(anyhow!("{CONFIG_FILE} does not match this plugin's schema: {}", problems.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "[plugin]\nname = \"demo\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n";

    fn plugin_with_schema(dir: &Path, schema: &str) -> plugin::manifest::Plugin {
        std::fs::write(dir.join("config.schema.json"), schema).unwrap();
        std::fs::write(
            dir.join("plugin.toml"),
            format!("{HEAD}\n[plugin.config]\nschema = \"config.schema.json\"\n"),
        )
        .unwrap();
        plugin::manifest::Plugin::load(dir, &dir.join("plugin.toml")).unwrap()
    }

    const SCHEMA: &str = r#"{
      "type": "object",
      "properties": {"token": {"type": "string"}, "retries": {"type": "integer"}},
      "required": ["token"]
    }"#;

    #[test]
    fn a_plugin_with_no_schema_accepts_anything() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plugin.toml"), HEAD).unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), r#"{"whatever": true}"#).unwrap();
        let p = plugin::manifest::Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();
        assert_eq!(load_config(&p).unwrap()["whatever"], true);
    }

    /// A plugin whose schema requires nothing is correctly configured by
    /// having no file at all.
    #[test]
    fn an_absent_config_is_an_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        let p = plugin_with_schema(dir.path(), r#"{"type": "object"}"#);
        assert_eq!(load_config(&p).unwrap(), serde_json::json!({}));
    }

    #[test]
    fn a_conforming_config_is_returned_as_written() {
        let dir = tempfile::tempdir().unwrap();
        let p = plugin_with_schema(dir.path(), SCHEMA);
        std::fs::write(dir.path().join(CONFIG_FILE), r#"{"token": "abc", "retries": 3}"#).unwrap();
        let c = load_config(&p).unwrap();
        assert_eq!(c["token"], "abc");
        assert_eq!(c["retries"], 3);
    }

    #[test]
    fn a_missing_required_field_is_refused_with_the_field_named() {
        let dir = tempfile::tempdir().unwrap();
        let p = plugin_with_schema(dir.path(), SCHEMA);
        std::fs::write(dir.path().join(CONFIG_FILE), r#"{"retries": 3}"#).unwrap();
        let err = load_config(&p).unwrap_err().to_string();
        assert!(err.contains("token"), "{err}");
    }

    #[test]
    fn a_wrongly_typed_field_is_refused_with_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let p = plugin_with_schema(dir.path(), SCHEMA);
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            r#"{"token": "abc", "retries": "three"}"#,
        )
        .unwrap();
        let err = load_config(&p).unwrap_err().to_string();
        assert!(err.contains("/retries"), "the path is what a user needs: {err}");
    }

    /// One list beats fixing one problem at a time with a reinstall between
    /// each.
    #[test]
    fn every_problem_is_reported_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let p = plugin_with_schema(dir.path(), SCHEMA);
        std::fs::write(dir.path().join(CONFIG_FILE), r#"{"retries": "three"}"#).unwrap();
        let err = load_config(&p).unwrap_err().to_string();
        assert!(err.contains("token"), "{err}");
        assert!(err.contains("retries"), "{err}");
    }

    #[test]
    fn a_config_that_is_not_json_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let p = plugin_with_schema(dir.path(), SCHEMA);
        std::fs::write(dir.path().join(CONFIG_FILE), "not json").unwrap();
        assert!(load_config(&p).unwrap_err().to_string().contains("valid JSON"));
    }

    /// A schema that never rejects anything is indistinguishable from one
    /// that was never applied, so a broken schema is an error rather than a
    /// silent pass.
    #[test]
    fn a_malformed_schema_is_the_authors_error_not_a_free_pass() {
        let dir = tempfile::tempdir().unwrap();
        let p = plugin_with_schema(dir.path(), r#"{"type": 12345}"#);
        std::fs::write(dir.path().join(CONFIG_FILE), "{}").unwrap();
        assert!(load_config(&p).is_err());
    }
}
