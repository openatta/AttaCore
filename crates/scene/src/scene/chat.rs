//! ChatScene — 对话场景。
//!
//! 与 CodingScene 不同，ChatScene 面向通用对话，支持自动 session 命名。

use base::interface::prompt::PromptBlock;
use base::interface::scene::{
    AgentScene, ExecutionParams, ReminderContext, ScenePromptContext, TokenBudget,
};

pub struct ChatScene;

impl AgentScene for ChatScene {
    fn id(&self) -> &str {
        "chat"
    }
    fn name(&self) -> &str {
        "Chat"
    }
    fn description(&self) -> &str {
        "通用对话助手"
    }

    fn build_system_prompt(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock> {
        vec![PromptBlock::system(format!(
            "You are a helpful assistant. Today is {date}. OS: {os}. Shell: {shell}.",
            date = ctx.date,
            os = ctx.os,
            shell = ctx.shell,
        ))]
    }

    fn tools(&self) -> Vec<String> {
        vec![] // 空 = 全部工具
    }

    fn token_budget(&self) -> TokenBudget {
        TokenBudget {
            compact_threshold: 150_000,
            compact_keep_recent: 20,
        }
    }

    fn build_system_reminder(&self, _ctx: &ReminderContext) -> String {
        String::new()
    }

    fn execution_params(&self) -> ExecutionParams {
        ExecutionParams::default()
    }

    /// CHAT 场景支持自动生成 session 名称。
    fn auto_name_session(&self) -> bool {
        true
    }

    /// 用 3-5 个词概括对话主题。
    fn session_name_prompt(&self, first_message: &str) -> Option<String> {
        Some(format!("用 3-5 个词概括以下对话的主题，只输出标题不要任何解释：\n{first_message}"))
    }
}
