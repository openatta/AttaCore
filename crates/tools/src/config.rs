//! Config tool — runtime read/write of settings.
//!
//! TS parity: Config tool in claude-code (read/write settings at runtime).
//! Allows the model to query and modify configuration.

use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, PermissionMode, ProgressSender, PromptContext, Tool, ToolContext,
    ToolResult, ToolResultContent, ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};

/// Global mutable settings store. Maps setting names to their current runtime values.
/// Initialized by the CLI layer before any tool calls.
pub static SETTINGS_STORE: OnceLock<Mutex<Value>> = OnceLock::new();

/// Initialize the global settings store from a JSON map of current settings.
/// Must be called once at startup before ConfigTool is invoked.
pub fn initialize_settings(settings: Value) {
    SETTINGS_STORE.get_or_init(|| Mutex::new(settings));
}

/// Global model registry for model name validation.
pub static MODEL_REGISTRY: OnceLock<model::registry::ModelRegistry> = OnceLock::new();

/// Initialize the global model registry for model validation.
/// Optional — if not set, model names are accepted without validation.
pub fn initialize_model_registry(registry: model::registry::ModelRegistry) {
    MODEL_REGISTRY.get_or_init(|| registry);
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConfigInput {
    /// Setting name to get or set
    /// (e.g. "model", "permission_mode", "theme", "language", "auto_memory")
    pub setting: String,
    /// Optional new value. When provided, the setting is updated.
    /// When omitted, the current value is returned.
    #[serde(default)]
    pub value: Option<String>,
}

pub struct ConfigTool;

#[async_trait]
impl Tool for ConfigTool {
    fn name(&self) -> &str {
        "Config"
    }

    fn description(&self) -> &str {
        "Get or set configuration settings"
    }

    fn is_deferred(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ConfigInput)).expect("schema")
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        "Get or set configuration settings.\n\
         View or change settings. Use when the user requests \
         configuration changes, asks about current settings, or when adjusting \
         a setting would benefit them.\n\
         \n\
         ## Supported settings\n\
         - `model` — The AI model used for responses\n\
         - `permission_mode` — Permission mode (default, plan, acceptEdits, \
           bypassPermissions, auto, dontAsk, bubble, yolo)\n\
         - `theme` — UI theme\n\
         - `language` — Preferred response language\n\
         - `auto_memory` — Enable automatic memory management (true/false)\n\
         \n\
         ## Usage\n\
         - **Get current value:** Omit the \"value\" parameter\n\
         - **Set new value:** Include the \"value\" parameter"
            .into()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn is_read_only(&self, input: &Value) -> bool {
        // "value" key present (even if empty string) => set (mutating)
        // No "value" key => get (read-only)
        !input.get("value").is_some_and(|v| !v.is_null())
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<ConfigInput>(input.clone()) {
            Ok(p) if p.setting.trim().is_empty() => {
                ValidationResult::err("setting must not be empty", 1)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let cfg_input: ConfigInput = serde_json::from_value(input)?;
        let setting = cfg_input.setting.trim().to_lowercase();
        // value is Some (not None) => SET operation, even if the string is empty.
        // None => GET operation.
        let is_set = cfg_input.value.is_some();

        if is_set {
            let new_value = cfg_input.value.as_deref().unwrap_or("").trim().to_string();
            let old_value = get_setting_value(&setting, &ctx);

            // Validate and apply
            apply_setting(&setting, &new_value, &ctx).map_err(|e| {
                ToolError::Validation(format!("cannot set '{}': {e}", cfg_input.setting))
            })?;

            // Persist to global settings store
            if let Some(store) = SETTINGS_STORE.get() {
                if let Ok(mut guard) = store.lock() {
                    guard[&setting] = json!(new_value);
                }
            }

            Ok(ToolResult {
                content: ToolResultContent::Text(format!("{} => {}", old_value, new_value)),
                is_error: false,
                structured_content: Some(json!({
                    "success": true,
                    "operation": "set",
                    "setting": cfg_input.setting,
                    "value": new_value,
                    "previous_value": old_value
                })),
                mcp_meta: None,
                new_messages: Some(vec![]),
            })
        } else {
            let current = get_setting_value(&setting, &ctx);
            Ok(ToolResult {
                content: ToolResultContent::Text(current.clone()),
                is_error: false,
                structured_content: Some(json!({
                    "success": true,
                    "operation": "get",
                    "setting": cfg_input.setting,
                    "value": current
                })),
                mcp_meta: None,
                new_messages: Some(vec![]),
            })
        }
    }
}

/// Get the current value of a setting from available sources.
/// Extract a string value from a JSON Value without JSON-escaped quotes.
fn str_val(v: &Value) -> String {
    v.as_str()
        .map(String::from)
        .or_else(|| v.as_bool().map(|b| b.to_string()))
        .unwrap_or_else(|| v.to_string())
}

fn get_setting_value(setting: &str, ctx: &ToolContext) -> String {
    match setting {
        "model" => ctx.config.model.clone(),
        "permission_mode" => {
            let mode = ctx.session.permission_mode();
            // Serialize via serde to get camelCase output
            serde_json::to_value(mode)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| format!("{mode:?}"))
        }
        "language" => {
            // Check SystemPromptSettings first
            if let Some(lang) = &ctx.config.system_prompt.language {
                return lang.clone();
            }
            // Fall back to settings store
            if let Some(store) = SETTINGS_STORE.get() {
                if let Ok(guard) = store.lock() {
                    if let Some(v) = guard.get("language").and_then(|v| v.as_str()) {
                        return v.to_string();
                    }
                }
            }
            String::new()
        }
        "theme" => {
            if let Some(store) = SETTINGS_STORE.get() {
                if let Ok(guard) = store.lock() {
                    if let Some(v) = guard.get("theme") {
                        return str_val(v);
                    }
                }
            }
            String::new()
        }
        "auto_memory" => {
            if let Some(store) = SETTINGS_STORE.get() {
                if let Ok(guard) = store.lock() {
                    if let Some(v) = guard.get("auto_memory") {
                        return str_val(v);
                    }
                }
            }
            "true".to_string() // default
        }
        _ => {
            // Unknown setting — check store
            if let Some(store) = SETTINGS_STORE.get() {
                if let Ok(guard) = store.lock() {
                    if let Some(v) = guard.get(setting) {
                        return str_val(v);
                    }
                }
            }
            String::new()
        }
    }
}

/// Valid permission mode names (camelCase, used for display in error messages).
pub const VALID_PERMISSION_MODES: &[&str] = &[
    "default",
    "plan",
    "acceptEdits",
    "bypassPermissions",
    "auto",
    "dontAsk",
    "bubble",
    "yolo",
];

/// Apply a setting change after validation.
fn apply_setting(setting: &str, value: &str, ctx: &ToolContext) -> Result<(), String> {
    match setting {
        "model" => {
            if value.is_empty() {
                return Err("model must not be empty".into());
            }
            // Optional validation against model registry
            if let Some(registry) = MODEL_REGISTRY.get() {
                if registry.find(value).is_none() {
                    let available: Vec<String> = registry
                        .list()
                        .iter()
                        .map(|m| m.model_name.clone())
                        .collect();
                    return Err(format!(
                        "'{value}' is not a known model. Available models: [{}]",
                        available.join(", ")
                    ));
                }
            }
            Ok(())
        }
        "permission_mode" => {
            // Normalize: lowercase + strip separators
            let normalized = value.to_lowercase().replace([' ', '_', '-'], "");
            let mode = match normalized.as_str() {
                "default" => PermissionMode::Default,
                "plan" => PermissionMode::Plan,
                "acceptedits" | "accept_edits" => PermissionMode::AcceptEdits,
                "bypasspermissions" => PermissionMode::BypassPermissions,
                "auto" => PermissionMode::Auto,
                "dontask" => PermissionMode::DontAsk,
                "bubble" => PermissionMode::Bubble,
                "yolo" => PermissionMode::Yolo,
                _ => {
                    return Err(format!(
                        "'{value}' is not a valid permission mode. Valid modes: {}",
                        VALID_PERMISSION_MODES.join(", ")
                    ));
                }
            };
            ctx.session.set_permission_mode(mode);
            Ok(())
        }
        "theme" | "language" => {
            if value.is_empty() {
                return Err(format!("{setting} must not be empty"));
            }
            Ok(())
        }
        "auto_memory" => {
            let lower = value.to_lowercase();
            match lower.as_str() {
                "true" | "false" | "1" | "0" | "yes" | "no" => Ok(()),
                _ => Err("auto_memory must be a boolean (true/false)".into()),
            }
        }
        _ => {
            // Allow unknown settings to be stored for future use
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> ToolContext {
        ToolContext::for_test(PathBuf::from("/tmp"))
    }

    // ---- GET operations ----

    #[tokio::test]
    async fn config_get_model() {
        let tool = ConfigTool;
        let r = tool
            .call(
                json!({"setting": "model"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        // Default config has model="test"
        match &r.content {
            ToolResultContent::Text(s) => assert_eq!(s, "test"),
            _ => panic!("expected text content"),
        }
        if let Some(sc) = &r.structured_content {
            assert_eq!(sc["operation"], "get");
            assert_eq!(sc["success"], true);
            assert_eq!(sc["value"], "test");
        }
    }

    #[tokio::test]
    async fn config_get_permission_mode() {
        let tool = ConfigTool;
        let r = tool
            .call(
                json!({"setting": "permission_mode"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        if let Some(sc) = &r.structured_content {
            assert_eq!(sc["operation"], "get");
            assert_eq!(sc["setting"], "permission_mode");
            // Default PermissionMode serializes as "default"
            assert_eq!(sc["value"], "default");
        }
    }

    #[tokio::test]
    async fn config_get_language_default() {
        let tool = ConfigTool;
        let r = tool
            .call(
                json!({"setting": "language"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        // Results depend on whether SETTINGS_STORE was populated by parallel tests.
        // At minimum, structured_content fields are correct.
        if let Some(sc) = &r.structured_content {
            assert_eq!(sc["operation"], "get");
            assert_eq!(sc["setting"], "language");
        }
    }

    // ---- SET operations ----

    #[tokio::test]
    async fn config_set_permission_mode_valid() {
        let c = ctx();
        let tool = ConfigTool;
        let r = tool
            .call(
                json!({"setting": "permission_mode", "value": "plan"}),
                c,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        if let Some(sc) = &r.structured_content {
            assert_eq!(sc["operation"], "set");
            assert_eq!(sc["success"], true);
            assert_eq!(sc["value"], "plan");
            // Previous value should be "default"
            assert_eq!(sc["previous_value"], "default");
        }
    }

    #[tokio::test]
    async fn config_set_permission_mode_invalid() {
        let tool = ConfigTool;
        let r = tool
            .call(
                json!({"setting": "permission_mode", "value": "not_a_mode"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await;
        assert!(r.is_err(), "expected ToolError for invalid mode");
    }

    #[tokio::test]
    async fn config_set_model() {
        let tool = ConfigTool;
        let r = tool
            .call(
                json!({"setting": "model", "value": "claude-sonnet-4-20250514"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        if let Some(sc) = &r.structured_content {
            assert_eq!(sc["operation"], "set");
            assert_eq!(sc["success"], true);
            assert_eq!(sc["value"], "claude-sonnet-4-20250514");
        }
    }

    #[tokio::test]
    async fn config_set_model_empty() {
        let tool = ConfigTool;
        let r = tool
            .call(
                json!({"setting": "model", "value": ""}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await;
        assert!(r.is_err(), "expected ToolError for empty model");
    }

    #[tokio::test]
    async fn config_set_language() {
        let tool = ConfigTool;
        let r = tool
            .call(
                json!({"setting": "language", "value": "zh-CN"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        if let Some(sc) = &r.structured_content {
            assert_eq!(sc["operation"], "set");
            assert_eq!(sc["value"], "zh-CN");
        }
    }

    #[tokio::test]
    async fn config_set_unknown_setting() {
        let tool = ConfigTool;
        // Unknown settings are stored in the global store for future use
        let r = tool
            .call(
                json!({"setting": "nonexistent", "value": "something"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        if let Some(sc) = &r.structured_content {
            assert_eq!(sc["operation"], "set");
            assert_eq!(sc["value"], "something");
        }
    }

    // ---- is_read_only ----

    #[test]
    fn config_is_read_only_get() {
        let tool = ConfigTool;
        // No "value" key => get (read-only)
        assert!(tool.is_read_only(&json!({"setting": "model"})));
        // Null value treated as not provided
        assert!(tool.is_read_only(&json!({"setting": "model", "value": null})));
    }

    #[test]
    fn config_is_read_only_set() {
        let tool = ConfigTool;
        // "value" key present => set (mutating), even if empty string
        assert!(!tool.is_read_only(&json!({"setting": "model", "value": "new-model"})));
        assert!(!tool.is_read_only(&json!({"setting": "model", "value": ""})));
        assert!(!tool.is_read_only(&json!({"setting": "language", "value": "zh"})));
    }

    // ---- initialize_settings / SETTINGS_STORE ----

    #[tokio::test]
    async fn config_reads_from_settings_store() {
        // Use a unique key to avoid conflicts with parallel tests
        if SETTINGS_STORE.set(Mutex::new(json!({}))).is_err() {
            // Already initialized — that's fine
        }
        // Write to the store
        if let Some(store) = SETTINGS_STORE.get() {
            if let Ok(mut guard) = store.lock() {
                guard["_test_theme"] = json!("dark");
                guard["_test_mem"] = json!("false");
            }
        }
        let tool = ConfigTool;

        let r = tool
            .call(
                json!({"setting": "_test_theme"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        if let Some(sc) = &r.structured_content {
            assert_eq!(sc["operation"], "get");
            assert_eq!(sc["value"], "dark");
        }

        let r = tool
            .call(
                json!({"setting": "_test_mem"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        if let Some(sc) = &r.structured_content {
            assert_eq!(sc["value"], "false");
        }
    }

    #[tokio::test]
    async fn config_set_writes_to_settings_store() {
        // Use a unique key to avoid clashes
        if SETTINGS_STORE.set(Mutex::new(json!({}))).is_err() {
            // Already initialized — that's fine
        }
        let tool = ConfigTool;
        let r = tool
            .call(
                json!({"setting": "_test_write_key", "value": "written"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);

        // Verify the store was updated
        if let Some(store) = SETTINGS_STORE.get() {
            if let Ok(guard) = store.lock() {
                assert_eq!(guard["_test_write_key"], "written");
            }
        }
    }
}
