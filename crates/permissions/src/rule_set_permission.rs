//! `RuleSetPermission` — the real `base::interface::permission::Permission`
//! implementation, wrapping `PermissionGate`/`RuleSet`.
//!
//! Named to match the doc comment on the `Permission` trait itself
//! ("AttaCode uses `RuleSetPermission`"), which described this exact shape
//! before any implementation existed — every production caller
//! (`daemon::main`, `Builder::build()`'s default) previously used a no-op
//! `AllowAllPermission`/`AllowAll` instead, so `permission_rules` in
//! `settings.json` and `PermissionMode` (`BypassPermissions`/`DontAsk`/
//! `Plan`/...) had zero effect on any real tool call. This is what makes
//! them real, plus gives `Skill.allowed_tools` (see `SkillTool`) somewhere
//! to inject temporary Allow rules into via `add_rules`.

use crate::gate::PermissionGate;
use async_trait::async_trait;
use base::context::SessionState;
use base::interface::permission::{Permission, PermissionOutcome};
use base::permission::{PermissionDecision as GateDecision, PermissionMode};
use base::tool::{InMemoryToolRegistry, ToolContext};
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct RuleSetPermission {
    gate: Arc<PermissionGate>,
    tools: Arc<InMemoryToolRegistry>,
    /// Static per construction — matches `Agent.config`'s own "computed once
    /// at `Builder::build()`, not live-updated by `config.reload`" scope;
    /// widening either to track runtime changes is a separate, later change.
    permission_mode: PermissionMode,
}

impl RuleSetPermission {
    pub fn new(
        gate: Arc<PermissionGate>,
        tools: Arc<InMemoryToolRegistry>,
        permission_mode: PermissionMode,
    ) -> Self {
        Self {
            gate,
            tools,
            permission_mode,
        }
    }
}

#[async_trait]
impl Permission for RuleSetPermission {
    async fn check(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        cwd: &Path,
        session_id: &str,
    ) -> PermissionOutcome {
        // Unknown tool name: not this layer's problem to report — dispatch's
        // own "Tool not found" error already covers it downstream. Permit
        // here so that real error surfaces instead of a confusing permission
        // denial masking it.
        let Some(tool) = self.tools.get(tool_name) else {
            return PermissionOutcome::Permit;
        };

        let mut ctx = ToolContext::from_engine_ctx(cwd.to_path_buf(), CancellationToken::new());
        ctx.session_id = session_id.to_string();
        ctx.session = Arc::new(
            SessionState::new(cwd.to_path_buf()).with_permission_mode(self.permission_mode),
        );

        match self.gate.check(tool.as_ref(), tool_input, &ctx).await {
            Ok(GateDecision::Allow { .. }) => PermissionOutcome::Permit,
            Ok(GateDecision::Deny { message, .. }) => PermissionOutcome::Deny { reason: message },
            Ok(GateDecision::Ask { message, .. }) => PermissionOutcome::Prompt {
                prompt_id: uuid::Uuid::new_v4().to_string(),
                message,
                paths: Vec::new(),
            },
            // Gate errors (bad tool input, etc.) are conservative — deny
            // rather than silently let a malformed call through.
            Err(e) => PermissionOutcome::Deny {
                reason: format!("permission check error: {e}"),
            },
        }
    }

    fn add_temporary_allow(&self, tool_name: &str) {
        self.gate.add_rules(vec![base::permission::PermissionRule {
            source: base::permission::RuleSource::Command,
            behavior: base::permission::RuleBehavior::Allow,
            tool_name: tool_name.to_string(),
            rule_content: None,
        }]);
    }

    fn clear_temporary_allows(&self) {
        self.gate
            .remove_rules_by_source(base::permission::RuleSource::Command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ruleset::RuleSet;
    use async_trait::async_trait as async_trait2;
    use base::error::ToolError;
    use base::tool::{PermissionDecision as ToolPermissionDecision, Tool};
    use serde_json::json;

    struct FakeTool {
        name: &'static str,
        read_only: bool,
    }

    #[async_trait2]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn prompt(&self, _: &base::tool::PromptContext) -> String {
            "fake".into()
        }
        fn is_read_only(&self, _: &serde_json::Value) -> bool {
            self.read_only
        }
        async fn check_permissions(
            &self,
            _: &serde_json::Value,
            _: &ToolContext,
        ) -> ToolPermissionDecision {
            ToolPermissionDecision::ask("?")
        }
        async fn call(
            &self,
            _: serde_json::Value,
            _: ToolContext,
            _: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, ToolError> {
            Ok(base::tool::ToolResult::text("ok"))
        }
    }

    fn registry_with(tool: FakeTool) -> Arc<InMemoryToolRegistry> {
        let reg = Arc::new(InMemoryToolRegistry::new());
        reg.register(Arc::new(tool));
        reg
    }

    #[tokio::test]
    async fn unknown_tool_name_permits() {
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            Arc::new(InMemoryToolRegistry::new()),
            PermissionMode::Default,
        );
        let outcome = perm
            .check("Nonexistent", &json!({}), Path::new("/tmp"), "s1")
            .await;
        assert!(matches!(outcome, PermissionOutcome::Permit));
    }

    #[tokio::test]
    async fn bypass_mode_permits_without_rules() {
        let tools = registry_with(FakeTool {
            name: "Bash",
            read_only: false,
        });
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            tools,
            PermissionMode::BypassPermissions,
        );
        let outcome = perm
            .check("Bash", &json!({}), Path::new("/tmp"), "s1")
            .await;
        assert!(matches!(outcome, PermissionOutcome::Permit));
    }

    #[tokio::test]
    async fn dontask_mode_denies_without_rules() {
        let tools = registry_with(FakeTool {
            name: "Bash",
            read_only: false,
        });
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            tools,
            PermissionMode::DontAsk,
        );
        let outcome = perm
            .check("Bash", &json!({}), Path::new("/tmp"), "s1")
            .await;
        assert!(matches!(outcome, PermissionOutcome::Deny { .. }));
    }

    #[tokio::test]
    async fn default_mode_with_no_rule_prompts() {
        let tools = registry_with(FakeTool {
            name: "Bash",
            read_only: false,
        });
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            tools,
            PermissionMode::Default,
        );
        let outcome = perm
            .check("Bash", &json!({}), Path::new("/tmp"), "s1")
            .await;
        assert!(matches!(outcome, PermissionOutcome::Prompt { .. }));
    }

    #[tokio::test]
    async fn allow_rule_permits_even_in_default_mode() {
        let tools = registry_with(FakeTool {
            name: "Bash",
            read_only: false,
        });
        let rules = RuleSet::new(vec![base::permission::PermissionRule {
            source: base::permission::RuleSource::UserSettings,
            behavior: base::permission::RuleBehavior::Allow,
            tool_name: "Bash".into(),
            rule_content: None,
        }]);
        let perm =
            RuleSetPermission::new(Arc::new(PermissionGate::new(rules)), tools, PermissionMode::Default);
        let outcome = perm
            .check("Bash", &json!({}), Path::new("/tmp"), "s1")
            .await;
        assert!(matches!(outcome, PermissionOutcome::Permit));
    }
}
