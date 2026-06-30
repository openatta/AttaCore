//! AgentRunner — 通过 Agent API 执行测试用例。
//!
//! 每轮: 发送用户消息 → 收集 AgentEvent → 记录输出 → 可选 LLM 比对。

use base::frozen::FrozenContext;
use base::id::Id;
use base::interface::event::AgentEvent;
use base::interface::model::{Model, StreamParams};
use base::interface::settings::{PermissionMode, ThinkingMode, VcrConfig, VcrMode};
use base::provider::ApiType;
use base::tool::{InMemoryToolRegistry, Tool};
use runtime::agent::{Builder, InputMessage};
use std::path::PathBuf;
use std::sync::Arc;
use telemetry::vcr::VcrModel;
use tokio_util::sync::CancellationToken;

use crate::script::TestCase;

pub struct AgentRunnerConfig {
    pub model: Arc<dyn Model>,
    pub vcr_mode: VcrMode,
    pub vcr_scenario: String,
    pub vcr_dir: PathBuf,
    pub telemetry_path: Option<PathBuf>,
}

pub struct TurnOutput {
    pub text: String,
    pub tool_uses: Vec<(String, serde_json::Value)>,
}

pub async fn run_test_case(
    config: AgentRunnerConfig,
    case: &TestCase,
) -> anyhow::Result<Vec<TurnOutput>> {
    let mut results = Vec::new();

    for turn in &case.turns {
        let out = run_one_turn(&config, turn).await?;
        results.push(out);
    }

    // Cleanup test artifacts (temp dirs, session files)
    let tmp = PathBuf::from("/tmp/atta_test_runner");
    let _ = std::fs::remove_dir_all(&tmp);

    Ok(results)
}

async fn run_one_turn(
    config: &AgentRunnerConfig,
    turn: &crate::script::Turn,
) -> anyhow::Result<TurnOutput> {
    let tmp = PathBuf::from("/tmp/atta_test_runner");
    let _ = std::fs::create_dir_all(&tmp);

    let vcr_model = Arc::new(VcrModel::new(
        config.model.clone(),
        Some(VcrConfig {
            mode: config.vcr_mode,
            scenario: config.vcr_scenario.clone(),
            fallback_on_miss: true,
        }),
        PathBuf::from("/tmp/atta_vcr_nonexistent"),
        config.vcr_dir.clone(),
    ));

    let settings = Arc::new(base::interface::settings::Settings {
        model: base::interface::settings::ModelSettings {
            api_type: ApiType::Anthropic,
            base_url: String::new(),
            auth_token: String::new(),
            model_name: std::env::var("ANTHROPIC_MODEL")
                .unwrap_or_else(|_| "claude-sonnet-4-6".into()),
            max_tokens: 2000,
            thinking_mode: ThinkingMode::Off,
            fallback_model: None,
        },
        paths: base::interface::settings::PathSettings {
            user_data_dir: tmp.join("user"),
            local_data_dir: tmp.join("local"),
        },
        execution: Default::default(),
        compaction: Default::default(),
        sandbox: Default::default(),
        instruction_file: None,
        prompt_append: None,
        prompt_override: None,
        vcr: None,
        telemetry_url: None,
        session_dir: None,
        memory_enabled: false,
        permission_mode: PermissionMode::BypassPermissions,
        permission_rules: vec![],
        hooks_config: None,
        mcp_servers: vec![],
        language: None,
        feature_flags: Default::default(),
    });

    let tools_registry = make_tools();

    let mut builder = Builder::new()
        .scene(Arc::new(scene::scene::coding::CodingScene::default_scene()))
        .model(vcr_model)
        .tools(tools_registry)
        .settings(settings)
        .session_id(format!("test-turn-{}", turn.index))
        .skip_warmup(true);

    // Inject telemetry if configured
    if let Some(ref tp) = config.telemetry_path {
        let rec = Arc::new(telemetry::FileRecorder::new(tp)?);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<telemetry::events::TelemetryEvent>(1024);
        let rec2 = rec.clone();
        tokio::spawn(async move {
            use telemetry::TelemetryRecorder;
            while let Some(event) = rx.recv().await {
                let _ = rec2.record(event);
            }
        });
        builder = builder.telemetry_handle(telemetry::TelemetryHandle::new(tx));
    }

    let (agent, mut event_rx, input_tx) = builder
        .build()
        .map_err(|e| anyhow::anyhow!("build agent: {e}"))?;

    let cancel = CancellationToken::new();
    tokio::spawn(async move {
        let mut agent = agent;
        let _ = agent.run(cancel).await;
    });

    let _ = input_tx.send(InputMessage::User {
        content: turn.input.clone(),
        attachments: vec![],
        turn_id: format!("turn_{}", turn.index),
    });

    let mut text = String::new();
    let mut tool_uses: Vec<(String, serde_json::Value)> = vec![];
    while let Some(event) = event_rx.recv().await {
        match event {
            AgentEvent::TextDelta { text: t, .. } => text.push_str(&t),
            AgentEvent::ToolUse { name, input, .. } => tool_uses.push((name, input)),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }

    Ok(TurnOutput { text, tool_uses })
}

fn make_tools() -> Arc<InMemoryToolRegistry> {
    let reg = Arc::new(InMemoryToolRegistry::new());
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(tools::bash::BashTool),
        Arc::new(tools::file_read::FileReadTool),
        Arc::new(tools::file_write::FileWriteTool),
        Arc::new(tools::file_edit::FileEditTool),
        Arc::new(tools::grep::GrepTool),
        Arc::new(tools::glob::GlobTool),
        Arc::new(tools::lsp::LspTool::ephemeral()),
        Arc::new(tools::todo_write::TodoWriteTool),
        Arc::new(tools::tasks::TaskCreateTool),
        Arc::new(tools::tasks::TaskUpdateTool),
        Arc::new(tools::task_output::TaskOutputTool),
        Arc::new(tools::task_stop::TaskStopTool),
    ];
    for t in tools {
        reg.register(t);
    }
    reg
}
