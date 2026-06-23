//! Demo scene — minimal agent for framework validation.

use base::interface::prompt::PromptBlock;
use base::interface::scene::{AgentScene, ScenePromptContext, TokenBudget};

pub struct DemoScene;

impl AgentScene for DemoScene {
    fn id(&self) -> &str {
        "demo"
    }

    fn name(&self) -> &str {
        "AttaCode Demo"
    }

    fn description(&self) -> &str {
        "演示场景 — 展示 AgentScene 框架的可扩展性"
    }

    fn build_system_prompt(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock> {
        vec![
            PromptBlock::system_cached("You are AttaCode Demo, a minimal agent for demonstration."),
            PromptBlock::system(format!(
                "Working directory: {}\nDate: {}\nOS: {}",
                ctx.cwd, ctx.date, ctx.os
            )),
        ]
    }

    fn tools(&self) -> Vec<String> {
        vec!["Read".into(), "Bash".into(), "Glob".into(), "Grep".into()]
    }

    fn token_budget(&self) -> TokenBudget {
        TokenBudget {
            compact_threshold: 50_000,
            compact_keep_recent: 10,
        }
    }
}
