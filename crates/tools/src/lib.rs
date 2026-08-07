//! Built-in tool implementations — all concrete tools the agent can invoke.

pub mod aliases;
pub mod ask_user;
pub mod bash;
pub mod cancel;
pub mod config;
pub mod cron;
pub mod file_edit;
pub mod file_read;
pub mod file_write;
pub mod glob;
pub mod grep;
pub mod import_tool;
pub mod lsp;
pub mod monitor;
pub mod notebook_edit;
pub mod ping;
pub mod plan_mode;
pub mod plan_verify;
pub mod push_notification;
pub mod remote_trigger;
pub mod saas_stubs;
pub mod schedule_wakeup;
pub mod security;
pub mod skill_tool;
pub mod sleep;
pub mod structured_output;
pub mod task_output;
pub mod task_stop;
pub mod tasks;
pub mod todo_write;
pub mod tool_search;
pub mod web_fetch;
pub mod web_search;
pub mod worktree;
pub mod worktree_tools;

// From anthropic/ — tool-side logic
pub mod native_search;
pub mod secondary_llm;

// Agent tool

use base::tool::Tool;
use std::sync::Arc;

/// Assemble the final tool pool: deduplicate built-in and MCP tools by name.
/// Built-in tools take priority on name conflict.
///
/// TS parity: `assembleToolPool()` in tools.ts.
pub fn assemble_tool_pool(
    builtin: Vec<Arc<dyn Tool>>,
    mcp: Vec<Arc<dyn Tool>>,
) -> Vec<Arc<dyn Tool>> {
    use std::collections::BTreeMap;
    let mut pool: BTreeMap<String, Arc<dyn Tool>> = BTreeMap::new();
    for t in mcp {
        pool.insert(t.name().to_string(), t);
    }
    for t in builtin {
        pool.insert(t.name().to_string(), t);
    }
    pool.into_values().collect()
}

/// Reference-based variant — used in `build_tool_defs()` where tools are behind Arc.
pub fn assemble_tool_pool_refs<'a>(
    builtin: Vec<&'a dyn Tool>,
    mcp: Vec<&'a dyn Tool>,
) -> Vec<&'a dyn Tool> {
    use std::collections::BTreeMap;
    let mut pool: BTreeMap<String, &dyn Tool> = BTreeMap::new();
    for t in mcp {
        pool.insert(t.name().to_string(), t);
    }
    for t in builtin {
        pool.insert(t.name().to_string(), t);
    }
    pool.into_values().collect()
}

pub fn register_skill_tool(
    r: &base::tool::InMemoryToolRegistry,
    m: std::sync::Arc<skills::manager::SkillManager>,
    spawner: Option<std::sync::Arc<dyn base::interface::agent_spawner::AgentSpawner>>,
    permission: std::sync::Arc<dyn base::interface::permission::Permission>,
) {
    let mut tool = crate::skill_tool::SkillTool::new(m).with_permission(permission);
    if let Some(spawner) = spawner {
        tool = tool.with_spawner(spawner);
    }
    r.register(std::sync::Arc::new(tool));
}

/// Register the standard set of self-contained built-in tools — no host-specific
/// wiring (bridges, external providers, registries) required to construct any of
/// them. Every embedder (daemon, test harness, ...) that wants a working
/// Bash/Read/Write/Edit/... agent needs to call this on the registry it hands to
/// `runtime::agent::Builder::tools()` — `Builder::build()` does NOT populate these
/// itself. It only auto-registers a handful of tools that need engine-internal
/// state to construct (Skill, TaskStop, TaskOutput, Import, the "Agent" spawner,
/// MCP adapters) — see `crates/runtime/src/agent.rs::Builder::build()`.
///
/// Found via: `daemon` never called `Builder::tools()` at all (confirmed by its
/// own startup log — `"startup: tools registered (noop)"`,
/// `daemon/src/main.rs`), so every daemon session ran with only that
/// engine-internal handful and nothing else — no Bash, no file edits, no search.
/// `tests/runner/src/api_runner.rs` had independently hand-rolled a near-identical
/// list for the same reason; this is now the single source of truth both use.
///
/// Deliberately excluded (needs host-specific construction this crate can't
/// supply, or is a legacy/duplicate-named alias — see call sites for why):
/// `WebSearchTool` (needs a `SearchProvider` impl; none exists in this repo yet —
/// only a test `MockProvider` — wiring a real one, e.g. a native-search sub-call
/// or an MCP search server, is separate follow-up work), `RemoteTriggerTool`
/// (needs a bridge), `EnterWorktreeTool`/`ExitWorktreeTool` (needs a
/// `WorktreeRegistry`), `saas_stubs::McpAuthTool` (needs SaaS auth
/// infrastructure), cron tools (needs a scheduler bridge), and everything in
/// `aliases.rs`/`config.rs` (several duplicate the name of an already-registered
/// tool — e.g. `aliases::TaskOutputTool`/`aliases::ConfigTool` vs.
/// `tasks::TaskOutputTool`/`config::ConfigTool` — picking the "right" one is a
/// separate cleanup, not bundled into this fix).
pub fn register_builtin_tools(reg: &std::sync::Arc<base::tool::InMemoryToolRegistry>) {
    let simple: Vec<Arc<dyn Tool>> = vec![
        Arc::new(crate::bash::BashTool),
        Arc::new(crate::file_read::FileReadTool),
        Arc::new(crate::file_write::FileWriteTool),
        Arc::new(crate::file_edit::FileEditTool),
        Arc::new(crate::grep::GrepTool),
        Arc::new(crate::glob::GlobTool),
        Arc::new(crate::lsp::LspTool::ephemeral()),
        Arc::new(crate::notebook_edit::NotebookEditTool),
        Arc::new(crate::todo_write::TodoWriteTool),
        Arc::new(crate::tasks::TaskCreateTool),
        Arc::new(crate::tasks::TaskGetTool),
        Arc::new(crate::tasks::TaskListTool),
        Arc::new(crate::tasks::TaskUpdateTool),
        Arc::new(crate::web_fetch::WebFetchTool::new()),
        Arc::new(crate::monitor::MonitorTool),
        Arc::new(crate::sleep::SleepTool),
        Arc::new(crate::ping::PingTool),
        Arc::new(crate::plan_mode::EnterPlanModeTool),
        Arc::new(crate::plan_mode::ExitPlanModeTool),
        Arc::new(crate::plan_verify::VerifyPlanExecutionTool),
        Arc::new(crate::schedule_wakeup::ScheduleWakeupTool),
        Arc::new(crate::push_notification::PushNotificationTool),
        Arc::new(crate::ask_user::AskUserQuestionTool),
        Arc::new(crate::structured_output::StructuredOutputTool),
    ];
    for t in simple {
        reg.register(t);
    }
    // Needs the same registry it searches over — registered last so it sees
    // every tool registered above (it reads `reg.list()` at call time via the
    // `Arc`, so registration order doesn't actually matter for correctness,
    // but "last" documents the dependency clearly).
    reg.register(Arc::new(crate::tool_search::ToolSearchTool::new(
        reg.clone(),
    )));
}

#[cfg(test)]
mod register_builtin_tools_tests {
    use super::*;

    /// Regression: `daemon` used to never call `Builder::tools()` at all, so
    /// every daemon session ran with zero of these — only the handful
    /// `Builder::build()` registers internally (Skill/TaskStop/TaskOutput/
    /// Import/Agent/MCP). Pin down that the tools every built-in scene
    /// actually lists (CodingScene/ChatScene/ResearchScene) end up
    /// registered by this single function.
    #[test]
    fn registers_the_tools_every_built_in_scene_lists() {
        let reg = Arc::new(base::tool::InMemoryToolRegistry::new());
        register_builtin_tools(&reg);
        let names: std::collections::HashSet<String> = reg
            .list()
            .into_iter()
            .map(|t| t.name().to_string())
            .collect();

        // CodingScene's tools() is an empty allow-list ("everything
        // registered is allowed") — it depends on these actually being
        // registered, not just declared in a scene's tools() list.
        for expected in [
            "Bash",
            "Read",
            "Write",
            "Edit",
            "Grep",
            "Glob",
            "TodoWrite",
            "WebFetch",
        ] {
            assert!(
                names.contains(expected),
                "missing built-in tool: {expected}"
            );
        }
    }

    #[test]
    fn tool_search_tool_sees_the_other_registered_tools() {
        // ToolSearchTool needs the *same* registry Arc it searches over —
        // regression for constructing it with a registry snapshotted before
        // the rest were registered (would make it permanently blind).
        let reg = Arc::new(base::tool::InMemoryToolRegistry::new());
        register_builtin_tools(&reg);
        let names: Vec<String> = reg
            .list()
            .into_iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.contains(&"ToolSearch".to_string()));
        assert!(names.contains(&"Bash".to_string()));
    }
}
