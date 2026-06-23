//! **S1-f **: append a permission rule to a settings.json file
//! atomically, preserving unknown fields. Used by the TUI's "always allow"
//! shortcut on the ask dialog.
//!
//! Atomicity: write to `<path>.tmp-<pid>` then rename. If the target dir
//! doesn't exist, it's created.

use std::fs;
use std::io;
use std::path::Path;

/// Which behavior bucket to append to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendTarget {
    Allow,
    Deny,
    Ask,
}

impl AppendTarget {
    fn key(&self) -> &'static str {
        match self {
            AppendTarget::Allow => "allow",
            AppendTarget::Deny => "deny",
            AppendTarget::Ask => "ask",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AppendError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("settings.json malformed: {0}")]
    BadJson(#[from] serde_json::Error),
    #[error("settings.json root must be an object")]
    NotObject,
}

/// Append `rule_string` to `permissions.<target>` in the given settings.json
/// file. If the file doesn't exist, it's created with `{}` first. If the
/// rule is already present (string-equal), the file is left untouched.
///
/// Returns `Ok(true)` if the file was modified, `Ok(false)` if the rule was
/// already present.
pub fn append_permission_rule(
    settings_path: &Path,
    target: AppendTarget,
    rule_string: &str,
) -> Result<bool, AppendError> {
    if let Some(parent) = settings_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    // Load or initialize
    let mut value: serde_json::Value = if settings_path.exists() {
        let bytes = fs::read(settings_path)?;
        if bytes.iter().all(|b| b.is_ascii_whitespace()) {
            serde_json::json!({})
        } else {
            serde_json::from_slice(&bytes)?
        }
    } else {
        serde_json::json!({})
    };

    let root = value.as_object_mut().ok_or(AppendError::NotObject)?;

    // permissions: object
    let perms = root
        .entry("permissions".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let perms_obj = perms.as_object_mut().ok_or(AppendError::NotObject)?;

    // permissions.<target>: array
    let arr = perms_obj
        .entry(target.key().to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let arr = arr.as_array_mut().ok_or(AppendError::NotObject)?;

    // Idempotency
    let already = arr
        .iter()
        .any(|v| v.as_str().map(|s| s == rule_string).unwrap_or(false));
    if already {
        return Ok(false);
    }
    arr.push(serde_json::Value::String(rule_string.to_string()));

    // Atomic write
    let tmp = settings_path.with_extension(format!("tmp-{}", std::process::id()));
    let body = serde_json::to_string_pretty(&value)?;
    fs::write(&tmp, body)?;
    fs::rename(&tmp, settings_path)?;
    Ok(true)
}

/// Build a canonical rule string `<tool_name>(<content>)`. Returns None when
/// content is empty (no useful pattern can be derived).
pub fn build_rule_string(tool_name: &str, match_content: Option<&str>) -> Option<String> {
    let content = match_content?.trim();
    if content.is_empty() {
        return None;
    }
    Some(format!("{tool_name}({content})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn append_creates_file_if_absent() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("subdir").join("settings.json");
        let modified = append_permission_rule(&p, AppendTarget::Allow, "Bash(ls)").unwrap();
        assert!(modified);
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        let arr = v["permissions"]["allow"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0], "Bash(ls)");
    }

    #[test]
    fn append_idempotent_when_rule_present() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        fs::write(&p, r#"{"permissions":{"allow":["Bash(git status)"]}}"#).unwrap();
        let modified = append_permission_rule(&p, AppendTarget::Allow, "Bash(git status)").unwrap();
        assert!(!modified);
    }

    #[test]
    fn append_preserves_other_keys() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        fs::write(
            &p,
            r#"{"model":"x","weird":42,"permissions":{"deny":["Bash(rm:*)"]}}"#,
        )
        .unwrap();
        append_permission_rule(&p, AppendTarget::Allow, "Bash(ls)").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["model"], "x");
        assert_eq!(v["weird"], 42);
        assert_eq!(v["permissions"]["deny"][0], "Bash(rm:*)");
        assert_eq!(v["permissions"]["allow"][0], "Bash(ls)");
    }

    #[test]
    fn append_can_target_deny_or_ask() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        append_permission_rule(&p, AppendTarget::Deny, "Bash(rm -rf:*)").unwrap();
        append_permission_rule(&p, AppendTarget::Ask, "Bash(git push:*)").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["permissions"]["deny"][0], "Bash(rm -rf:*)");
        assert_eq!(v["permissions"]["ask"][0], "Bash(git push:*)");
    }

    #[test]
    fn build_rule_string_returns_none_for_empty() {
        assert!(build_rule_string("Bash", None).is_none());
        assert!(build_rule_string("Bash", Some("")).is_none());
        assert!(build_rule_string("Bash", Some("   ")).is_none());
    }

    #[test]
    fn build_rule_string_basic() {
        assert_eq!(
            build_rule_string("Bash", Some("git status")).as_deref(),
            Some("Bash(git status)")
        );
    }
}
