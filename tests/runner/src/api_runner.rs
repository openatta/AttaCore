//! AgentRunner — 通过 Agent API 执行测试用例。
//!
//! 每轮: 发送用户消息 → 收集 AgentEvent → 记录输出 → 可选 LLM 比对。

use base::interface::event::AgentEvent;
use base::interface::model::Model;
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
    /// 模板项目 fixture 目录（如 `tests/fixtures/template_project`）。设置后，
    /// 每次跑用例前会把它拷贝到临时工作目录，Settings 通过真实的
    /// `Settings::load()` 从拷贝出来的 `.atta/settings.json` 读 hooks/mcp/agents
    /// 配置——而不是像默认路径那样手搭一份空 Settings。None 时行为不变（兼容
    /// 不需要模板项目配置的用例，如裸 Agent 冒烟测试）。
    pub fixture_dir: Option<PathBuf>,
}

pub struct TurnOutput {
    pub text: String,
    pub tool_uses: Vec<(String, serde_json::Value)>,
}

pub async fn run_test_case(config: AgentRunnerConfig, case: &TestCase) -> anyhow::Result<Vec<TurnOutput>> {
    let tmp = PathBuf::from("/tmp/atta_test_runner");
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::create_dir_all(&tmp);

    if let Some(fixture) = &config.fixture_dir {
        let workdir = tmp.join("workdir");
        crate::fixture::copy_dir_recursive(fixture, &workdir)?;
        crate::fixture::resolve_mcp_toy_server_placeholder(&workdir)?;
    }

    let mut results = Vec::new();
    for turn in &case.turns {
        let out = run_one_turn(&config, turn).await?;
        results.push(out);
    }

    // Cleanup test artifacts (temp dirs, session files)
    let _ = std::fs::remove_dir_all(&tmp);

    Ok(results)
}

async fn run_one_turn(config: &AgentRunnerConfig, turn: &crate::script::Turn) -> anyhow::Result<TurnOutput> {
    let tmp = PathBuf::from("/tmp/atta_test_runner");
    let _ = std::fs::create_dir_all(&tmp);

    // ATTA_VCR_STRICT=1 disables the network fallback on a replay miss — fails
    // immediately with the hash that didn't match instead of silently eating a
    // real API call. Explicitly passing a VcrConfig here (rather than None)
    // bypasses VcrModel's own env-var resolution, so this harness has to read
    // the flag itself to actually respect it.
    let strict = std::env::var("ATTA_VCR_STRICT").is_ok_and(|v| v != "0" && !v.is_empty());
    let vcr_model = Arc::new(VcrModel::new(
        config.model.clone(),
        Some(VcrConfig { mode: config.vcr_mode, scenario: config.vcr_scenario.clone(), fallback_on_miss: !strict }),
        PathBuf::from("/tmp/atta_vcr_nonexistent"),
        config.vcr_dir.clone(),
    ));

    let model_name = std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".into());

    let mut settings = if config.fixture_dir.is_some() {
        // 真实的 Settings::load() 路径：从拷贝出来的模板项目读
        // .atta/settings.json（hooks_config/mcp_servers/...），project_root()
        // 落在 workdir 本身，工具（Bash/Read/...)、skills/rules 发现都对着这份
        // 拷贝操作,不是手搭的空 Settings。
        let workdir = tmp.join("workdir");
        base::interface::settings::Settings::load(
            tmp.join("global_empty"),
            tmp.join("scene_empty"),
            workdir.join(".atta"),
            "code",
            &model_name,
        )
    } else {
        base::interface::settings::Settings::defaults_for(&model_name)
    };
    settings.model = base::interface::settings::ModelSettings {
        api_type: ApiType::Anthropic, base_url: String::new(), auth_token: String::new(),
        model_name: model_name.clone(), max_tokens: 2000,
        thinking_mode: ThinkingMode::Off, fallback_model: None,
    };
    if config.fixture_dir.is_none() {
        settings.paths = base::interface::settings::PathSettings {
            user_data_dir: tmp.join("user"), global_data_dir: tmp.join("global"), local_data_dir: tmp.join("local"),
            scope: "code".into(),
        };
    }
    settings.memory_enabled = false;
    // 测试非交互运行,始终旁路权限确认——和 daemon 的生产选择一致（IDE/测试
    // 宿主自己管沙盒，见 docs/CONFIG_LAYOUT.md §13.1）。
    settings.permission_mode = PermissionMode::BypassPermissions;
    let settings = Arc::new(settings);

    let tools_registry = make_tools();

    let mut builder = Builder::new()
        .scene(Arc::new(scene::scene::coding::CodingScene))
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

    let (agent, mut event_rx, input_tx) = builder.build()
        .map_err(|e| anyhow::anyhow!("build agent: {e}"))?;

    let cancel = CancellationToken::new();
    let agent_cancel = cancel.clone();
    tokio::spawn(async move {
        let mut agent = agent;
        let _ = agent.run(agent_cancel).await;
    });

    let _ = input_tx.send(InputMessage::User {
        content: turn.input.clone(),
        attachments: vec![],
        turn_id: format!("turn_{}", turn.index),
    });

    // 没有这层超时的话，网络卡住（比如沙箱环境出不了网、endpoint 无响应）会导致
    // event_rx.recv() 永远 pending——不报错、不退出，只是安静地挂着。加一层
    // 可配置的整轮超时，超时就主动 cancel 掉 agent 任务并报出明确错误。
    let turn_timeout = std::time::Duration::from_secs(
        std::env::var("ATTA_TEST_TURN_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(180),
    );

    // `Agent::run()` doesn't stop on a turn error — it sends `AgentEvent::Error`
    // and goes back to waiting for the *next* input message (correct for a
    // long-lived chat session, wrong for this harness which only ever sends one
    // message per turn). A collector that silently drops `Error` events (old
    // `_ => {}` catch-all) wedges forever waiting for a `TurnComplete` that will
    // never come — this is what actually produced the multi-minute "hangs" this
    // was chasing; the underlying network layer had already failed cleanly and
    // reported it, nothing downstream was listening for that report.
    let collect = async {
        let mut text = String::new();
        let mut tool_uses: Vec<(String, serde_json::Value)> = vec![];
        while let Some(event) = event_rx.recv().await {
            match event {
                AgentEvent::TextDelta { text: t, .. } => text.push_str(&t),
                AgentEvent::ToolUse { name, input, .. } => tool_uses.push((name, input)),
                AgentEvent::TurnComplete { .. } => return Ok((text, tool_uses)),
                AgentEvent::Error { code, message, .. } => {
                    return Err(anyhow::anyhow!("turn {} failed: [{code}] {message}", turn.index));
                }
                _ => {}
            }
        }
        anyhow::bail!("turn {} ended: event channel closed before TurnComplete/Error", turn.index);
    };

    let (text, tool_uses) = match tokio::time::timeout(turn_timeout, collect).await {
        Ok(result) => result?,
        Err(_) => {
            cancel.cancel();
            anyhow::bail!(
                "turn {} timed out after {turn_timeout:?} waiting for TurnComplete/Error — no event at \
                 all arrived in that window (as opposed to a reported turn failure, which surfaces \
                 immediately above). Override with ATTA_TEST_TURN_TIMEOUT_SECS if a turn genuinely needs \
                 longer.",
                turn.index
            );
        }
    };

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
    for t in tools { reg.register(t); }
    reg
}
