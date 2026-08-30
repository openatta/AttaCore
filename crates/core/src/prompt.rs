//! Prompt assembly — pure function that stitches together multi-source content.

use crate::memory::{build_memory_prompt, MemoryStore};
use crate::rules::build_rules_prompt;
use crate::interface::scene::{AgentScene, ScenePromptContext};
use crate::settings::Settings;
use serde::{Deserialize, Serialize};

/// Protocol-agnostic prompt block.
///
/// Each Model implementation translates these into its API-specific format.
/// `cache_strategy` carries Anthropic cache_control semantics (Ephemeral/Global)
/// but is ignored by non-Anthropic models.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptBlock {
    pub role: BlockRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_strategy: Option<CacheStrategy>,
    /// What this block is — see the [`names`] module.
    ///
    /// Annotation only: no adapter reads it, so it never reaches the wire and
    /// two blocks differing only here are byte-identical to the model. It
    /// exists because block boundaries alone do not say which block is the
    /// skills inventory and which is the MCP instructions, and the count
    /// shifts with configuration (rules, MCP and `prompt_append` are each
    /// optional), so position is not a reliable answer.
    ///
    /// This is how an extension addresses a block: "put mine after
    /// `skills.catalog`", "replace `memory.session`". That only works if the
    /// names are stable, which is why the kernel's are a published contract
    /// and not a debugging label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Who put this block here.
    ///
    /// Separate from [`name`](Self::name) because they answer different
    /// questions: the name says which block this is, the origin says whose it
    /// is. Rearranging the prompt is a privileged act, and how privileged
    /// depends on where the rearranging came from — a plugin someone
    /// downloaded is not a script that same person wrote. Nothing enforces
    /// that yet; recording it truthfully is the precondition for enforcing it
    /// at all.
    #[serde(default, skip_serializing_if = "BlockOrigin::is_kernel")]
    pub origin: BlockOrigin,
}

/// The kernel's block names. **A published contract**: an extension that
/// positions itself relative to one of these is relying on the name to keep
/// meaning what it means, so these change the way any public name changes.
///
/// A scene that names its own sections keeps those names, prefixed with
/// `scene.`; [`SCENE_SKELETON`] is what a scene that names nothing gets.
pub mod names {
    /// The scene's system prompt, when the scene does not name its sections.
    pub const SCENE_SKELETON: &str = "scene.skeleton";
    /// Prefix for a scene that does name its sections.
    pub const SCENE_PREFIX: &str = "scene.";
    /// The inventory of available skills.
    pub const SKILLS_CATALOG: &str = "skills.catalog";
    /// How to use the file-based memory system.
    pub const MEMORY_SESSION: &str = "memory.session";
    /// The discovery index of `.atta/rules/` files.
    pub const RULES: &str = "rules";
    /// Instructions contributed by connected MCP servers.
    pub const MCP_INSTRUCTIONS: &str = "mcp.instructions";
    /// `settings.prompt_append`.
    pub const CONFIG_PROMPT_APPEND: &str = "config.prompt_append";
    /// `settings.prompt_override`, which replaces everything else.
    pub const CONFIG_PROMPT_OVERRIDE: &str = "config.prompt_override";
}

/// Where a prompt block came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BlockOrigin {
    /// The engine itself.
    #[default]
    Kernel,
    /// Written by whoever configured this deployment.
    Config,
    /// Contributed by an installed plugin, named by its plugin id.
    Plugin(String),
    /// Contributed by a script in this project, named by its path.
    Script(String),
}

impl BlockOrigin {
    /// Used to keep the field out of serialized output in the common case.
    pub fn is_kernel(&self) -> bool {
        matches!(self, Self::Kernel)
    }

    /// Whether this block came from something the user wrote themselves, as
    /// opposed to something they installed. The distinction trust decisions
    /// will be made on.
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Kernel | Self::Config | Self::Script(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
            name: None,
            origin: BlockOrigin::Kernel,
        }
    }

    pub fn system_cached(content: impl Into<String>) -> Self {
        Self {
            role: BlockRole::System,
            content: content.into(),
            cache_strategy: Some(CacheStrategy::Ephemeral),
            name: None,
            origin: BlockOrigin::Kernel,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: BlockRole::User,
            content: content.into(),
            cache_strategy: None,
            name: None,
            origin: BlockOrigin::Kernel,
        }
    }

    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn from_origin(mut self, origin: BlockOrigin) -> Self {
        self.origin = origin;
        self
    }
}

/// Assemble the full system prompt from all sources.
///
/// Concatenation order:
/// 1. Scene skeleton (AgentScene::build_system_prompt)
/// 2. Skills loaded from skills/ directories
/// 3. Memory loaded from MemoryStore
///    3b. Rules discovery index (filenames + first line only; see `crate::rules`)
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
        return vec![PromptBlock::system(ov.clone())
            .named(names::CONFIG_PROMPT_OVERRIDE)
            .from_origin(BlockOrigin::Config)];
    }

    let mut blocks = Vec::new();

    // 1. Scene skeleton — a scene names its own sections; only fill in for one
    // that doesn't, so a scene's own labels are never overwritten here.
    blocks.extend(scene.build_system_prompt(ctx).into_iter().map(|b| match &b.name {
        // A scene that labels its own sections keeps those labels, under the
        // `scene.` prefix so a name can never collide with a kernel block's.
        Some(section) if !section.starts_with(names::SCENE_PREFIX) => {
            let prefixed = format!("{}{section}", names::SCENE_PREFIX);
            b.named(prefixed)
        }
        Some(_) => b,
        None => b.named(names::SCENE_SKELETON),
    }));

    // 2. Skills
    if let Some(text) = skills_text {
        if !text.is_empty() {
            blocks.push(PromptBlock::system(text.to_string()).named(names::SKILLS_CATALOG));
        }
    }

    // 3. Memory — inject memory system prompt instructions.
    // The memory prompt tells the model how to use the file-based memory system.
    // Actual memory content is loaded via MEMORY.md by the system reminder mechanism.
    // Gated by settings.memory_enabled (default: true).
    if settings.memory_enabled {
        let mem_dir = &settings.paths.global_data_dir.join("memory");
        blocks
            .push(PromptBlock::system(build_memory_prompt(mem_dir)).named(names::MEMORY_SESSION));
    }

    // 3b. Rules — lightweight discovery index only (filenames + first-line
    // description), not full content. Omitted entirely when no `.atta/rules/`
    // files exist anywhere, so sessions that don't use this feature pay zero
    // extra tokens. See `crate::rules` module docs.
    if let Some(text) = build_rules_prompt(&settings.paths) {
        blocks.push(PromptBlock::system(text).named(names::RULES));
    }

    // 4. MCP instructions
    if let Some(text) = mcp_instructions {
        if !text.is_empty() {
            blocks.push(PromptBlock::system(text.to_string()).named(names::MCP_INSTRUCTIONS));
        }
    }

    // 5. User append
    if let Some(ref append) = settings.prompt_append {
        blocks.push(
            PromptBlock::system(append.clone())
                .named(names::CONFIG_PROMPT_APPEND)
                .from_origin(BlockOrigin::Config),
        );
    }

    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;
    use crate::interface::scene::TokenBudget;
    use crate::settings::{
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

    fn test_ctx() -> ScenePromptContext<'static> {
        ScenePromptContext {
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
            tool_results_ever_cleared: false,
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
                user_data_dir: "/tmp/atta/scenes/code".into(),
                global_data_dir: "/tmp/atta".into(),
                local_data_dir: "/tmp/atta/local".into(),
                scope: "code".into(),
            },
            execution: ExecutionSettings::default(),
            compaction: Default::default(),
            sandbox: Default::default(),
            plugins: Default::default(),
            instruction_file: None,
            prompt_append: None,
            prompt_override: None,
            recorder: None,
            telemetry_url: None,
            memory_enabled: true,
            disable_skill_shell_execution: false,
            permission_mode: PermissionMode::default(),
            permission_rules: Vec::new(),
            local_permission_rules: Vec::new(),
            allow_client_permission_override: false,
            telemetry_enabled: false,
            hooks_config: None,
            mcp_servers: Default::default(),
            providers: Default::default(),
            default_provider: None,
            task_models: Default::default(),
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

            tool_results_ever_cleared: false,
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

            tool_results_ever_cleared: false,
        };

        let blocks = assemble_prompt(&scene, &settings, &store, &ctx, None, None);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].content, "OVERRIDE");
    }

    /// The names are a contract an extension positions itself against, so a
    /// rename is a breaking change and has to look like one. This is the test
    /// that makes it look like one.
    #[test]
    fn the_kernel_block_names_are_what_they_are_published_as() {
        assert_eq!(names::SCENE_SKELETON, "scene.skeleton");
        assert_eq!(names::SKILLS_CATALOG, "skills.catalog");
        assert_eq!(names::MEMORY_SESSION, "memory.session");
        assert_eq!(names::RULES, "rules");
        assert_eq!(names::MCP_INSTRUCTIONS, "mcp.instructions");
        assert_eq!(names::CONFIG_PROMPT_APPEND, "config.prompt_append");
        assert_eq!(names::CONFIG_PROMPT_OVERRIDE, "config.prompt_override");
    }

    /// Every block the kernel assembles must be addressable. An unnamed block
    /// is one an extension can neither position against nor replace, and it
    /// would be invisible in exactly the configuration that produced it.
    #[test]
    fn every_assembled_block_is_named_and_owned() {
        let scene = TestScene;
        let mut settings = test_settings();
        settings.prompt_append = Some("appended".into());
        settings.memory_enabled = true;
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::new(tmp.path().join("user"), tmp.path().join("local"));
        let ctx = test_ctx();

        let blocks = assemble_prompt(
            &scene,
            &settings,
            &store,
            &ctx,
            Some("skills inventory"),
            Some("mcp says hello"),
        );

        for b in &blocks {
            let name = b
                .name
                .as_deref()
                .unwrap_or_else(|| panic!("an unnamed block cannot be addressed: {b:?}"));
            assert!(!name.is_empty());
        }

        let named: Vec<&str> = blocks.iter().filter_map(|b| b.name.as_deref()).collect();
        assert!(named.contains(&names::SKILLS_CATALOG), "{named:?}");
        assert!(named.contains(&names::MEMORY_SESSION), "{named:?}");
        assert!(named.contains(&names::MCP_INSTRUCTIONS), "{named:?}");
        assert!(named.contains(&names::CONFIG_PROMPT_APPEND), "{named:?}");

        // A scene that names nothing gets the skeleton name; a scene that
        // names its sections keeps them, under the prefix.
        assert_eq!(blocks[0].name.as_deref(), Some(names::SCENE_SKELETON));

        let appended = blocks
            .iter()
            .find(|b| b.name.as_deref() == Some(names::CONFIG_PROMPT_APPEND))
            .unwrap();
        assert_eq!(
            appended.origin,
            BlockOrigin::Config,
            "a block the operator wrote is not the kernel's"
        );
        assert!(blocks[0].origin.is_kernel());
    }

    #[test]
    fn a_scene_that_names_its_sections_keeps_the_names_under_the_prefix() {
        struct SectionedScene;
        impl AgentScene for SectionedScene {
            fn id(&self) -> &str {
                "sectioned"
            }
            fn name(&self) -> &str {
                "Sectioned"
            }
            fn description(&self) -> &str {
                "names its own sections"
            }
            fn build_system_prompt(&self, _ctx: &ScenePromptContext) -> Vec<PromptBlock> {
                vec![
                    PromptBlock::system("who you are").named("identity"),
                    PromptBlock::system("what you may do").named("tools"),
                ]
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

        let settings = test_settings();
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::new(tmp.path().join("user"), tmp.path().join("local"));
        let blocks = assemble_prompt(&SectionedScene, &settings, &store, &test_ctx(), None, None);

        assert_eq!(blocks[0].name.as_deref(), Some("scene.identity"));
        assert_eq!(blocks[1].name.as_deref(), Some("scene.tools"));
    }
}
