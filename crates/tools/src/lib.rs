//! Built-in tool implementations — all concrete tools the agent can invoke.

pub mod aliases;
pub mod ask_user;
pub mod config;
pub mod bash;
pub mod cancel;
pub mod cron;
pub mod file_edit;
pub mod file_read;
pub mod file_write;
pub mod glob;
pub mod grep;
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

pub fn register_skill_tool(r: &base::tool::InMemoryToolRegistry, m: std::sync::Arc<skills::manager::SkillManager>, u: Vec<String>) {
    r.register(std::sync::Arc::new(crate::skill_tool::SkillTool::new(m, u)));
}
