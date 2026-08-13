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
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;

pub struct RuleSetPermission {
    gate: Arc<PermissionGate>,
    /// The registry `check` resolves tool names against.
    ///
    /// Interior-mutable because the registry this handler is *constructed*
    /// with is frequently not the registry the session ends up dispatching
    /// against — see `Permission::bind_tool_registry`, which swaps this to
    /// the real one once `Builder::build()` has finished populating it. A
    /// stale registry here is not a cosmetic problem: every tool missing from
    /// it used to be waved straight through.
    tools: RwLock<Arc<InMemoryToolRegistry>>,
    /// The session's live state, once `bind_session_state` has supplied it.
    ///
    /// `None` (embedders that never bind) falls back to a throwaway
    /// `SessionState` carrying `permission_mode` below — the old behavior.
    /// When bound, `check` reads the *live* permission mode, so
    /// `EnterPlanMode`/`ExitPlanMode` actually move the gate.
    session: RwLock<Option<Arc<SessionState>>>,
    /// The mode to assume when no live `SessionState` is bound. Also the mode
    /// a freshly-bound session starts in — `Builder::build()` seeds
    /// `SessionState` from the same `Settings` value.
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
            tools: RwLock::new(tools),
            session: RwLock::new(None),
            permission_mode,
        }
    }

    /// The registry currently bound — snapshot of the `Arc`, so a concurrent
    /// `bind_tool_registry` can't hold the lock across an `.await`.
    fn tools(&self) -> Arc<InMemoryToolRegistry> {
        self.tools.read().unwrap_or_else(|e| e.into_inner()).clone()
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
        // Unknown tool name — fail closed.
        //
        // This used to `Permit`, on the reasoning that dispatch's own "Tool
        // not found" error would surface right afterwards and a permission
        // denial here would only mask it. That reasoning holds exactly as
        // long as this registry *is* the dispatch registry. It was not: the
        // handler was built over a daemon's pool-level template while
        // dispatch ran against a per-session registry holding `Skill`,
        // `Agent`, `Team*`, `WebSearch`, `EnterWorktree`, `Cron*`, `Task*`,
        // `Import` and every `mcp__*` adapter — all of which landed here,
        // matched nothing, and executed unchecked. `bind_tool_registry` fixes
        // the binding; this branch stops the same class of bug from ever
        // being silent again. A genuinely nonexistent tool now surfaces as a
        // prompt whose message says so, and dispatch's own error follows if
        // the host approves it.
        let Some(tool) = self.tools().get(tool_name) else {
            return PermissionOutcome::Prompt {
                prompt_id: uuid::Uuid::new_v4().to_string(),
                message: format!(
                    "`{tool_name}` is not registered in this session's permission registry. \
                     It is either an unknown tool, or a tool that reached dispatch without \
                     being registered for permission checks — approve only if you expected it."
                ),
                paths: Vec::new(),
            };
        };

        let mut ctx = ToolContext::from_engine_ctx(cwd.to_path_buf(), CancellationToken::new());
        ctx.session_id = session_id.to_string();
        // Prefer the session's live state (so a runtime `set_permission_mode`
        // — plan mode — is honored); fall back to a throwaway carrying the
        // construction-time mode when nothing was bound.
        ctx.session = match self
            .session
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            Some(live) => live,
            None => Arc::new(
                SessionState::new(cwd.to_path_buf()).with_permission_mode(self.permission_mode),
            ),
        };
        ctx.permission_mode = ctx.session.permission_mode();

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

    fn bind_tool_registry(&self, tools: Arc<InMemoryToolRegistry>) {
        *self.tools.write().unwrap_or_else(|e| e.into_inner()) = tools;
    }

    fn bind_session_state(&self, session: Arc<SessionState>) {
        *self.session.write().unwrap_or_else(|e| e.into_inner()) = Some(session);
    }

    fn add_temporary_allow(&self, tool_name: &str, rule_content: Option<&str>) {
        self.gate.add_rules(vec![base::permission::PermissionRule {
            source: base::permission::RuleSource::Command,
            behavior: base::permission::RuleBehavior::Allow,
            tool_name: tool_name.to_string(),
            rule_content: rule_content.map(|s| s.to_string()),
        }]);
    }

    fn clear_temporary_allows(&self) {
        self.gate
            .remove_rules_by_source(base::permission::RuleSource::Command);
    }

    fn add_persistent_allow(&self, tool_name: &str, rule_content: Option<&str>) {
        // `RuleSource::Session` (50), not `Command` (45) — `Command`-sourced
        // rules get bulk-deleted by `clear_temporary_allows` (called from
        // `turn.rs` per-turn skill cleanup via `RuleSet::remove_by_source`)
        // whenever a skill goes inactive, and a "permit always" decision
        // from a user's interactive prompt answer must survive that
        // unrelated cleanup for the rest of the session.
        self.gate.add_rules(vec![base::permission::PermissionRule {
            source: base::permission::RuleSource::Session,
            behavior: base::permission::RuleBehavior::Allow,
            tool_name: tool_name.to_string(),
            rule_content: rule_content.map(|s| s.to_string()),
        }]);
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

    /// Like `FakeTool`, but with a settable `permission_match_content` —
    /// needed to exercise `add_persistent_allow`'s content-aware rule
    /// (`FakeTool` always derives `None`, which only a blanket/no-content
    /// rule would match).
    struct ContentAwareFakeTool {
        name: &'static str,
        content: Option<&'static str>,
    }

    #[async_trait2]
    impl Tool for ContentAwareFakeTool {
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
            false
        }
        async fn check_permissions(
            &self,
            _: &serde_json::Value,
            _: &ToolContext,
        ) -> ToolPermissionDecision {
            ToolPermissionDecision::ask("?")
        }
        fn permission_match_content(&self, _: &serde_json::Value) -> Option<String> {
            self.content.map(|s| s.to_string())
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

    fn registry_with_content_tool(tool: ContentAwareFakeTool) -> Arc<InMemoryToolRegistry> {
        let reg = Arc::new(InMemoryToolRegistry::new());
        reg.register(Arc::new(tool));
        reg
    }

    /// N-1: an unresolvable tool name must **not** be waved through. This
    /// used to `Permit`, which is what let every per-session tool
    /// (`Skill`/`Agent`/`Team*`/`mcp__*`/...) execute unchecked while this
    /// handler held only the daemon's pool-level built-in template.
    #[tokio::test]
    async fn unknown_tool_name_fails_closed() {
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            Arc::new(InMemoryToolRegistry::new()),
            PermissionMode::Default,
        );
        let outcome = perm
            .check("Nonexistent", &json!({}), Path::new("/tmp"), "s1")
            .await;
        match outcome {
            PermissionOutcome::Prompt { ref message, .. } => {
                assert!(message.contains("Nonexistent"), "{message}");
            }
            other => panic!("expected a Prompt for an unregistered tool, got {other:?}"),
        }
    }

    /// N-1: rebinding makes a tool registered *after* construction visible,
    /// which is exactly the `Builder::build()` ordering — the handler is
    /// built from a template, the session registry is populated afterwards.
    #[tokio::test]
    async fn bind_tool_registry_makes_late_registered_tools_checkable() {
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            Arc::new(InMemoryToolRegistry::new()),
            PermissionMode::DontAsk,
        );
        // Before binding: not in the registry at all → fails closed as a prompt.
        assert!(matches!(
            perm.check("Skill", &json!({}), Path::new("/tmp"), "s1")
                .await,
            PermissionOutcome::Prompt { .. }
        ));

        let session_registry = registry_with(FakeTool {
            name: "Skill",
            read_only: false,
        });
        perm.bind_tool_registry(session_registry);

        // After binding: a real tool, so `DontAsk` with no matching rule denies.
        assert!(
            matches!(
                perm.check("Skill", &json!({}), Path::new("/tmp"), "s1")
                    .await,
                PermissionOutcome::Deny { .. }
            ),
            "after rebinding, the tool must go through real mode dispatch"
        );
    }

    /// N-7: a bound `SessionState` is read live, so switching the mode at
    /// runtime (what `EnterPlanMode`/`ExitPlanMode` do) moves the gate.
    #[tokio::test]
    async fn bound_session_state_supplies_the_live_permission_mode() {
        let tools = registry_with(FakeTool {
            name: "Write",
            read_only: false,
        });
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            tools,
            PermissionMode::Default,
        );
        let session = Arc::new(
            SessionState::new(std::path::PathBuf::from("/tmp"))
                .with_permission_mode(PermissionMode::Default),
        );
        perm.bind_session_state(session.clone());

        assert!(matches!(
            perm.check("Write", &json!({}), Path::new("/tmp"), "s1")
                .await,
            PermissionOutcome::Prompt { .. }
        ));

        session.set_permission_mode(PermissionMode::Plan);
        assert!(
            matches!(
                perm.check("Write", &json!({}), Path::new("/tmp"), "s1")
                    .await,
                PermissionOutcome::Deny { .. }
            ),
            "entering plan mode must deny a non-read-only tool"
        );

        session.set_permission_mode(PermissionMode::BypassPermissions);
        assert!(matches!(
            perm.check("Write", &json!({}), Path::new("/tmp"), "s1")
                .await,
            PermissionOutcome::Permit
        ));
    }

    /// N-5: a temporary allow carrying `rule_content` must only match calls
    /// whose content matches — a blanket grant is what made a project-supplied
    /// skill's `allowed-tools` able to switch off confirmation wholesale.
    #[tokio::test]
    async fn temporary_allow_with_content_does_not_grant_everything() {
        let tools = registry_with_content_tool(ContentAwareFakeTool {
            name: "Bash",
            content: Some("cargo test"),
        });
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            tools,
            PermissionMode::Default,
        );

        // Grant only `git status`; the tool reports `cargo test` as its
        // content, so the grant must not apply.
        perm.add_temporary_allow("Bash", Some("git status"));
        assert!(
            matches!(
                perm.check("Bash", &json!({}), Path::new("/tmp"), "s1")
                    .await,
                PermissionOutcome::Prompt { .. }
            ),
            "a content-scoped grant must not cover a different command"
        );

        perm.add_temporary_allow("Bash", Some("cargo test"));
        assert!(matches!(
            perm.check("Bash", &json!({}), Path::new("/tmp"), "s1")
                .await,
            PermissionOutcome::Permit
        ));
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
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::new(rules)),
            tools,
            PermissionMode::Default,
        );
        let outcome = perm
            .check("Bash", &json!({}), Path::new("/tmp"), "s1")
            .await;
        assert!(matches!(outcome, PermissionOutcome::Permit));
    }

    #[tokio::test]
    async fn add_persistent_allow_permits_matching_content_without_prompting() {
        // The tool derives "git status" as its match content for any input
        // (a stand-in for e.g. Bash's command-line extraction) — matching
        // what a real "always allow" prompt answer would persist for the
        // specific call the user just approved.
        let tools = registry_with_content_tool(ContentAwareFakeTool {
            name: "Bash",
            content: Some("git status"),
        });
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            tools,
            PermissionMode::Default,
        );

        // Before: no rule, Default mode with a real tool asks.
        let outcome = perm
            .check("Bash", &json!({}), Path::new("/tmp"), "s1")
            .await;
        assert!(matches!(outcome, PermissionOutcome::Prompt { .. }));

        perm.add_persistent_allow("Bash", Some("git status"));

        let outcome = perm
            .check("Bash", &json!({}), Path::new("/tmp"), "s1")
            .await;
        assert!(
            matches!(outcome, PermissionOutcome::Permit),
            "expected immediate Permit after add_persistent_allow, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn add_persistent_allow_survives_clear_temporary_allows() {
        // Regression guard for the priority/source choice: a Session-sourced
        // persistent allow must NOT be wiped by `clear_temporary_allows`
        // (which only removes `Command`-sourced rules — see
        // `RuleSetPermission::clear_temporary_allows` and the skill
        // deactivation cleanup in `turn.rs` that calls it).
        let tools = registry_with(FakeTool {
            name: "Bash",
            read_only: false,
        });
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            tools,
            PermissionMode::Default,
        );
        perm.add_persistent_allow("Bash", None);
        perm.clear_temporary_allows();

        let outcome = perm
            .check("Bash", &json!({}), Path::new("/tmp"), "s1")
            .await;
        assert!(
            matches!(outcome, PermissionOutcome::Permit),
            "persistent allow must survive clear_temporary_allows, got {outcome:?}"
        );
    }
}
