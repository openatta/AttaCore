//! SkillTool — invokes user-invocable skills by name.
//! TS parity: Claude Code's SkillTool dispatches skill files as expanded prompts.
//! v4: Returns expanded skill content via new_messages (injected as user messages),
//!     checks disable_model_invocation, uses full expand_skill_vars substitution.
//!     Supports forked execution via AgentSpawner when skill.context = "fork".

use base::error::ToolError;
use base::interface::agent_spawner::AgentSpawner;
use base::tool::{ProgressSender, PromptContext, Tool, ToolContext, ToolResult, ToolResultContent};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// A tool that invokes skills loaded from disk.
/// Skills are .md files in `~/.atta/skills/` (global default),
/// `~/.atta/scenes/<scene>/skills/` (scene override), and
/// `<project>/.agents/skills/` (project-level).
///
/// Any skill the `SkillManager` loaded is invocable — the only gate is each
/// skill's own `disable_model_invocation` frontmatter flag (checked in
/// `call()` below). There used to be a second, coarser gate here too: a
/// scene-supplied allow-list (plus a hardcoded duplicate of it), which meant
/// a project's own `.agents/skills/*.md` could never be invoked via this
/// tool no matter what — `disable_model_invocation` already covers "should
/// the model be allowed to call this," per-skill, which is the right
/// granularity; the scene-level list was redundant with it and actively
/// blocked legitimate project skills, so it's gone.
pub struct SkillTool {
    /// Reference to the skill manager for looking up and expanding skills.
    skill_manager: std::sync::Arc<skills::manager::SkillManager>,
    /// P2-10: Agent spawner for forked skill execution (context: "fork").
    spawner: Option<Arc<dyn AgentSpawner>>,
}

impl SkillTool {
    pub fn new(skill_manager: std::sync::Arc<skills::manager::SkillManager>) -> Self {
        Self { skill_manager, spawner: None }
    }

    /// P2-10: Set the agent spawner for forked skill execution.
    pub fn with_spawner(mut self, spawner: Arc<dyn AgentSpawner>) -> Self {
        self.spawner = Some(spawner);
        self
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "Skill"
    }
    fn description(&self) -> &str {
        "Execute a skill within the main conversation. Skills are loaded from ~/.atta/skills/ (global), ~/.atta/scenes/<scene>/skills/ (scene override), and <project>/.agents/skills/ (project-level). Only skills listed in the user-invocable section are available."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "The name of the skill to invoke"
                },
                "args": {
                    "type": "string",
                    "description": "Optional arguments to pass to the skill"
                }
            },
            "required": ["skill"]
        })
    }
    fn is_read_only(&self, _input: &Value) -> bool {
        false // skills may do anything
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/skill_tool.prompt.md").to_string()
    }

    async fn call(&self, input: Value, _ctx: ToolContext, _progress: ProgressSender) -> Result<ToolResult, ToolError> {
        let skill_name = input["skill"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("skill name required".into()))?;
        let args = input["args"].as_str().unwrap_or("");

        // Look up the skill info to check disable_model_invocation
        let skills = self.skill_manager.list();
        let skill_info = skills.iter().find(|s| s.name == skill_name);

        // Check disable_model_invocation: if true, model cannot invoke this skill
        let context_mode = skill_info.and_then(|s| s.context.clone());
        if let Some(info) = skill_info {
            if info.disable_model_invocation {
                return Err(ToolError::Denied(format!(
                    "Skill '{skill_name}' has disable_model_invocation set. \
                     It can only be invoked by the user via slash command."
                )));
            }
        }

        // P2-10: Forked skill execution. When context="fork" and spawner is available,
        // run the skill in a sub-agent with isolated context.
        if context_mode.as_deref() == Some("fork") {
            if let Some(ref spawner) = self.spawner {
                let body = self.skill_manager.get_skill_content(skill_name)
                    .ok_or_else(|| ToolError::NotFound(format!(
                        "Skill '{skill_name}' not found."
                    )))?;
                let expanded = base::frozen::skill::expand_skill_vars(&body, args);
                let cancel = _ctx.cancel.clone();
                match spawner.spawn_agent(expanded, vec![], _ctx.cwd.clone(), cancel).await {
                    Ok(output) => {
                        return Ok(ToolResult {
                            content: ToolResultContent::Text(format!(
                                "Skill '{skill_name}' executed in forked agent.\n\n{output}"
                            )),
                            is_error: false,
                            ..Default::default()
                        });
                    }
                    Err(e) => {
                        return Ok(ToolResult {
                            content: ToolResultContent::Text(format!(
                                "Skill '{skill_name}' fork failed: {e}"
                            )),
                            is_error: true,
                            ..Default::default()
                        });
                    }
                }
            }
            // No spawner: fall through to inline execution below.
        }

        // Get the skill body content
        let body = self.skill_manager.get_skill_content(skill_name)
            .ok_or_else(|| ToolError::NotFound(format!(
                "Skill '{skill_name}' not found."
            )))?;

        // Use full expand_skill_vars (TS parity: $1..$9, $@, $ARGUMENTS, {ARGS}, etc.)
        let expanded = base::frozen::skill::expand_skill_vars(&body, args);

        // Truncate at 8000 chars to prevent context explosion
        let truncated = if expanded.len() > 8000 {
            format!("{}...[truncated]", &expanded[..8000])
        } else {
            expanded
        };

        // Build a user message with the expanded skill content wrapped in command-message tags.
        // TS parity: Claude Code's SkillTool injects expanded skill content as user messages
        // so the model sees them as new instructions, not as tool output.
        let skill_msg = format!(
            "<command-name>{skill_name}</command-name>\n<command-args>{args}</command-args>\n\n{truncated}"
        );

        Ok(ToolResult {
            content: ToolResultContent::Text(format!(
                "Skill '{skill_name}' invoked. The skill instructions have been loaded."
            )),
            is_error: false,
            new_messages: Some(vec![serde_json::json!({
                "role": "user",
                "content": skill_msg,
            })]),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::frozen::skill::{SkillEntry, SkillSource as FrozenSkillSource};

    fn register_test_skill(
        mgr: &skills::manager::SkillManager,
        name: &str,
        body: &str,
        disable_model_invocation: bool,
    ) {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir so the file survives past this function — fine for
        // a short-lived test process.
        let path = dir.keep().join(format!("{name}.md"));
        std::fs::write(&path, body).unwrap();
        mgr.register_bundled(SkillEntry {
            name: name.into(),
            description: format!("{name} skill"),
            source: FrozenSkillSource::Project,
            path,
            disable_model_invocation,
            ..Default::default()
        });
    }

    /// Regression: a project-defined skill (e.g. loaded from
    /// `.agents/skills/`) used to be unconditionally denied by a hardcoded
    /// scene-level allow-list, regardless of its own frontmatter — this is
    /// exactly the case that was broken.
    #[tokio::test]
    async fn project_skill_without_disable_flag_is_invocable() {
        let mgr = Arc::new(skills::manager::SkillManager::new());
        register_test_skill(&mgr, "code-review", "Review the changed files.", false);
        let tool = SkillTool::new(mgr);

        let result = tool
            .call(
                serde_json::json!({"skill": "code-review"}),
                ToolContext::for_test(std::env::temp_dir()),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");

        assert!(!result.is_error);
        assert!(result.new_messages.is_some(), "expanded skill content should be injected as a new message");
    }

    /// A skill's own `disable_model_invocation: true` is still respected —
    /// this is the one gate that survived (per-skill, not per-scene).
    #[tokio::test]
    async fn skill_with_disable_model_invocation_is_denied() {
        let mgr = Arc::new(skills::manager::SkillManager::new());
        register_test_skill(&mgr, "manual-only", "Only run via slash command.", true);
        let tool = SkillTool::new(mgr);

        let err = tool
            .call(
                serde_json::json!({"skill": "manual-only"}),
                ToolContext::for_test(std::env::temp_dir()),
                ProgressSender::noop("t"),
            )
            .await
            .expect_err("disable_model_invocation should deny the call");

        assert!(matches!(err, ToolError::Denied(_)));
    }

    #[tokio::test]
    async fn unknown_skill_returns_not_found() {
        let mgr = Arc::new(skills::manager::SkillManager::new());
        let tool = SkillTool::new(mgr);

        let err = tool
            .call(
                serde_json::json!({"skill": "does-not-exist"}),
                ToolContext::for_test(std::env::temp_dir()),
                ProgressSender::noop("t"),
            )
            .await
            .expect_err("unknown skill should error");

        assert!(matches!(err, ToolError::NotFound(_)));
    }
}
