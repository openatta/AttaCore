//! Prompt assembly — pure function that stitches together multi-source content.

use crate::interface::memory::{build_memory_prompt, MemoryStore};
use crate::interface::scene::{AgentScene, ScenePromptContext};
use crate::interface::settings::Settings;

/// Protocol-agnostic prompt block.
///
/// Each Model implementation translates these into its API-specific format.
/// `cache_strategy` carries Anthropic cache_control semantics (Ephemeral/Global)
/// but is ignored by non-Anthropic models.
#[derive(Debug, Clone)]
pub struct PromptBlock {
    pub role: BlockRole,
    pub content: String,
    pub cache_strategy: Option<CacheStrategy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStrategy {
    Ephemeral,
    Global,
}

impl PromptBlock {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: BlockRole::System,
            content: content.into(),
            cache_strategy: None,
        }
    }

    pub fn system_cached(content: impl Into<String>) -> Self {
        Self {
            role: BlockRole::System,
            content: content.into(),
            cache_strategy: Some(CacheStrategy::Ephemeral),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: BlockRole::User,
            content: content.into(),
            cache_strategy: None,
        }
    }
}

/// Assemble the full system prompt from all sources.
///
/// Concatenation order:
/// 1. Scene skeleton (AgentScene::build_system_prompt)
/// 2. Skills loaded from skills/ directories
/// 3. Memory loaded from MemoryStore
/// 4. Runtime state (plan, todos, current turn info)
/// 5. settings.prompt_append
/// 6. [if set] settings.prompt_override → replaces all of the above
///
/// Note: CLAUDE.md / instruction file is NOT in the system prompt — it is injected
/// as a synthetic `<system-reminder>` user message (TS userContext parity).
pub fn assemble_prompt(
    scene: &dyn AgentScene,
    settings: &Settings,
    _memory_store: &MemoryStore,
    ctx: &ScenePromptContext,
    skills_text: Option<&str>,
    mcp_instructions: Option<&str>,
) -> Vec<PromptBlock> {
    // If override is set, return it as the sole system block.
    if let Some(ref ov) = settings.prompt_override {
        return vec![PromptBlock::system(ov.clone())];
    }

    let mut blocks = Vec::new();

    // 1. Scene skeleton
    blocks.extend(scene.build_system_prompt(ctx));

    // 2. Skills
    if let Some(text) = skills_text {
        if !text.is_empty() {
            blocks.push(PromptBlock::system(text.to_string()));
        }
    }

    // 3. Memory — inject memory system prompt instructions (TS parity).
    // The memory prompt tells the model how to use the file-based memory system.
    // Actual memory content is loaded via MEMORY.md by the system reminder mechanism.
    // Gated by settings.memory_enabled (default: true).
    if settings.memory_enabled {
        let mem_dir = &settings.paths.user_data_dir.join("memory");
        blocks.push(PromptBlock::system(build_memory_prompt(mem_dir)));
    }

    // 4. MCP instructions
    if let Some(text) = mcp_instructions {
        if !text.is_empty() {
            blocks.push(PromptBlock::system(text.to_string()));
        }
    }

    // 5. User append
    if let Some(ref append) = settings.prompt_append {
        blocks.push(PromptBlock::system(append.clone()));
    }

    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interface::memory::MemoryStore;
    use crate::interface::scene::TokenBudget;
    use crate::interface::settings::{
        ExecutionSettings, ModelSettings, PathSettings, PermissionMode, Settings, ThinkingMode,
    };
    use crate::provider::ApiType;
    use std::borrow::Cow;
    use tempfile::TempDir;

    struct TestScene;
    impl AgentScene for TestScene {
        fn id(&self) -> &str {
            "test"
        }
        fn name(&self) -> &str {
            "Test"
        }
        fn description(&self) -> &str {
            "Test scene"
        }
        fn build_system_prompt(&self, _ctx: &ScenePromptContext) -> Vec<PromptBlock> {
            vec![PromptBlock::system_cached("You are a test agent.")]
        }
        fn tools(&self) -> Vec<String> {
            vec![]
        }
        fn token_budget(&self) -> TokenBudget {
            TokenBudget {
                compact_threshold: 1000,
                compact_keep_recent: 5,
            }
        }
    }

    fn test_settings() -> Settings {
        Settings {
            model: ModelSettings {
                api_type: ApiType::Anthropic,
                base_url: "https://api.example.com".into(),
                auth_token: "test".into(),
                model_name: "test-model".into(),
                max_tokens: 4096,
                thinking_mode: ThinkingMode::Auto,
                fallback_model: None,
            },
            paths: PathSettings {
                user_data_dir: "/tmp/atta/user".into(),
                local_data_dir: "/tmp/atta/local".into(),
            },
            execution: ExecutionSettings::default(),
            compaction: Default::default(),
            sandbox: Default::default(),
            instruction_file: None,
            prompt_append: None,
            prompt_override: None,
            vcr: None,
            telemetry_url: None,
            memory_enabled: true,
            permission_mode: PermissionMode::default(),
            permission_rules: Vec::new(),
            hooks_config: None,
            mcp_servers: Vec::new(),
            language: None,
            feature_flags: Default::default(),
            session_dir: None,
        }
    }

    #[test]
    fn assemble_basic_prompt() {
        let scene = TestScene;
        let settings = test_settings();
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::new(tmp.path().join("user"), tmp.path().join("local"));
        let ctx = ScenePromptContext {
            cwd: Cow::Borrowed("/tmp"),
            os: Cow::Borrowed("linux"),
            shell: Cow::Borrowed("bash"),
            home_dir: Cow::Borrowed("/home/user"),
            date: Cow::Borrowed("2026-06-10"),
            model_name: Cow::Borrowed("test-model"),
            skills_text: None,
            mcp_instructions: None,
            session_memory: None,
            is_git: false,
            git_branch: None,
            is_worktree: false,
            git_status: None,
            language: None,
            scratchpad_dir: None,
            output_style_content: None,
            available_tools: None,
            user_message: None,
        };

        let blocks = assemble_prompt(&scene, &settings, &store, &ctx, None, None);
        assert!(!blocks.is_empty());
        assert_eq!(blocks[0].role, BlockRole::System);
    }

    #[test]
    fn override_replaces_all() {
        let scene = TestScene;
        let mut settings = test_settings();
        settings.prompt_override = Some("OVERRIDE".into());
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::new(tmp.path().join("user"), tmp.path().join("local"));
        let ctx = ScenePromptContext {
            cwd: Cow::Borrowed("/tmp"),
            os: Cow::Borrowed("linux"),
            shell: Cow::Borrowed("bash"),
            home_dir: Cow::Borrowed("/home/user"),
            date: Cow::Borrowed("2026-06-10"),
            model_name: Cow::Borrowed("test-model"),
            skills_text: None,
            mcp_instructions: None,
            session_memory: None,
            is_git: false,
            git_branch: None,
            is_worktree: false,
            git_status: None,
            language: None,
            scratchpad_dir: None,
            output_style_content: None,
            available_tools: None,
            user_message: None,
        };

        let blocks = assemble_prompt(&scene, &settings, &store, &ctx, None, None);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].content, "OVERRIDE");
    }
}
