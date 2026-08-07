//! `Import` tool — manual, on-demand trigger for cross-tool configuration
//! import (Claude Code/Codex/Cursor). Backs the `/import` slash command.
//!
//! Unlike the automatic path (`base::interface::import_callback::ImportCallback`,
//! process-level, gated by `.atta/.imported.json`), this tool **always**
//! re-detects and never consults the "already decided" marker — a user
//! explicitly running `/import` should always see the current state, even
//! for a project that previously chose "skip" or was already imported once.
//!
//! Two-step protocol, driven by the model per the `import` bundled skill:
//! 1. Call with no `source` → lists detected candidates as text.
//! 2. Call again with `source` set to one of the listed values → executes
//!    that import and records the decision.
//!
//! See docs/design/2026-08-03-agents-config-migration.md §3.8.

use async_trait::async_trait;
use base::error::ToolError;
use base::frozen::{detect_import_sources, execute_import, mark_imported, ImportSourceKind};
use base::tool::{PermissionDecision, ProgressSender, Tool, ToolContext, ToolResult, ValidationResult};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImportInput {
    /// Which detected source to import from (`claude_code`/`codex`/`cursor`).
    /// Omit to list detected candidates instead of executing.
    #[serde(default)]
    pub source: Option<String>,
}

/// Detect or execute a cross-tool configuration import for the current project.
#[derive(Debug, Default, Clone, Copy)]
pub struct ImportTool;

#[async_trait]
impl Tool for ImportTool {
    fn name(&self) -> &str {
        "Import"
    }

    fn description(&self) -> &str {
        "Detect or execute import of Claude Code (CLAUDE.md/.claude/skills), Codex, or Cursor \
         (.cursorrules/.cursor/rules) configuration into this project's AGENTS.md/.agents/.atta \
         layout. Call with no `source` to list detected candidates; call again with `source` set \
         to one of the listed values (claude_code/codex/cursor) to execute that import. Single \
         source only — importing more than one at once is not supported."
    }

    fn is_deferred(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ImportInput)).expect("schema")
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    fn is_read_only(&self, input: &Value) -> bool {
        // Listing (no `source`) is read-only; executing an import writes files.
        input
            .get("source")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<ImportInput>(input.clone()) {
            Ok(parsed) => match parsed.source.as_deref() {
                None => ValidationResult::Ok,
                Some(s) if ImportSourceKind::try_parse(s).is_some() => ValidationResult::Ok,
                Some(s) => ValidationResult::err(
                    format!("unknown source `{s}` — expected claude_code, codex, or cursor"),
                    1,
                ),
            },
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
        let input: ImportInput = serde_json::from_value(input)?;
        let sources = detect_import_sources(&ctx.cwd).await;

        let Some(requested) = input.source.as_deref().filter(|s| !s.trim().is_empty()) else {
            // List mode.
            if sources.is_empty() {
                return Ok(ToolResult::text(
                    "No importable configuration detected (no CLAUDE.md/.claude/skills, \
                     .cursorrules/.cursor/rules, or Codex-style AGENTS.md-without-.agents/skills \
                     found in this project).",
                ));
            }
            let mut lines = vec!["Detected importable configuration:".to_string()];
            for s in &sources {
                lines.push(format!("- `{}`: {}", s.kind().as_str(), s.describe()));
            }
            lines.push(
                "Call this tool again with `source` set to one of the values above to import it."
                    .to_string(),
            );
            return Ok(ToolResult::text(lines.join("\n")));
        };

        let Some(kind) = ImportSourceKind::try_parse(requested) else {
            return Ok(ToolResult::error_text(format!(
                "unknown source `{requested}` — expected claude_code, codex, or cursor"
            )));
        };

        let Some(chosen) = sources.iter().find(|s| s.kind() == kind) else {
            return Ok(ToolResult::error_text(format!(
                "`{requested}` was requested but is no longer detected in this project \
                 (nothing to import)"
            )));
        };

        match execute_import(&ctx.cwd, chosen).await {
            Ok(summary) => {
                let _ = mark_imported(&ctx.cwd, &sources, kind).await;
                let mut lines = vec![format!("Imported from {}:", summary.kind.as_str())];
                for action in &summary.actions {
                    lines.push(format!("- {action}"));
                }
                Ok(ToolResult::text(lines.join("\n")))
            }
            Err(e) => Ok(ToolResult::error_text(format!("import failed: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;
    use tempfile::TempDir;

    fn ctx_at(cwd: &Path) -> ToolContext {
        ToolContext::for_test(cwd.to_path_buf())
    }

    #[tokio::test]
    async fn lists_nothing_for_empty_project() {
        let dir = TempDir::new().unwrap();
        let tool = ImportTool;
        let r = tool.call(json!({}), ctx_at(dir.path()), ProgressSender::noop("t")).await.unwrap();
        assert!(!r.is_error);
        match r.content {
            base::tool::ToolResultContent::Text(ref t) => assert!(t.contains("No importable")),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn lists_detected_claude_code_source() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("CLAUDE.md"), "be concise").await.unwrap();
        let tool = ImportTool;
        let r = tool.call(json!({}), ctx_at(dir.path()), ProgressSender::noop("t")).await.unwrap();
        match r.content {
            base::tool::ToolResultContent::Text(ref t) => {
                assert!(t.contains("claude_code"));
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn executes_import_for_requested_source() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("CLAUDE.md"), "be concise").await.unwrap();
        let tool = ImportTool;
        let r = tool
            .call(json!({"source": "claude_code"}), ctx_at(dir.path()), ProgressSender::noop("t"))
            .await
            .unwrap();
        assert!(!r.is_error);
        let agents_md = tokio::fs::read_to_string(dir.path().join("AGENTS.md")).await.unwrap();
        assert!(agents_md.contains("be concise"));
        // Marker should now record the decision.
        assert!(base::frozen::import_already_decided(dir.path()).await);
    }

    #[tokio::test]
    async fn manual_command_ignores_prior_skipped_marker() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("CLAUDE.md"), "be concise").await.unwrap();
        base::frozen::mark_skipped(dir.path(), &[], None).await.unwrap();

        let tool = ImportTool;
        let r = tool.call(json!({}), ctx_at(dir.path()), ProgressSender::noop("t")).await.unwrap();
        match r.content {
            base::tool::ToolResultContent::Text(ref t) => {
                assert!(t.contains("claude_code"), "must still list sources despite skipped marker");
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn rejects_unknown_source() {
        let dir = TempDir::new().unwrap();
        let tool = ImportTool;
        let r = tool
            .call(json!({"source": "bogus"}), ctx_at(dir.path()), ProgressSender::noop("t"))
            .await
            .unwrap();
        assert!(r.is_error);
    }

    #[test]
    fn is_read_only_reflects_source_presence() {
        let tool = ImportTool;
        assert!(tool.is_read_only(&json!({})));
        assert!(!tool.is_read_only(&json!({"source": "claude_code"})));
    }
}
