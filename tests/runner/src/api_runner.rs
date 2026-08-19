//! AgentRunner — 通过 Agent API 执行测试用例。
//!
//! 每轮: 发送用户消息 → 收集 AgentEvent → 记录输出 → 可选 LLM 比对。

use base::interface::event::AgentEvent;
use base::interface::model::Model;
use base::interface::scene::AgentScene;
use base::interface::settings::{PermissionMode, RecorderConfig, RecorderMode, ThinkingMode};
use base::provider::ApiType;
use base::tool::InMemoryToolRegistry;
use runtime::agent::{Builder, EventReceiver, InputMessage, InputSender};
use std::path::PathBuf;
use std::sync::Arc;
use telemetry::recorder::RecorderModel;
use tokio_util::sync::CancellationToken;

use crate::script::TestCase;

pub struct AgentRunnerConfig {
    pub model: Arc<dyn Model>,
    pub recorder_mode: RecorderMode,
    pub recorder_name: String,
    pub recordings_dir: PathBuf,
    pub telemetry_path: Option<PathBuf>,
    /// 模板项目 fixture 目录（如 `tests/fixtures/template_project`）。设置后，
    /// 每次跑用例前会把它拷贝到临时工作目录，Settings 通过真实的
    /// `Settings::load()` 从拷贝出来的 `.atta/settings.json` 读 hooks/mcp/agents
    /// 配置——而不是像默认路径那样手搭一份空 Settings。None 时行为不变（兼容
    /// 不需要模板项目配置的用例，如裸 Agent 冒烟测试）。
    pub fixture_dir: Option<PathBuf>,
    /// 用哪个 `AgentScene` 跑这个用例——之前这里写死 `CodingScene`，chat/research
    /// 场景从没被端到端跑过。调用方（`main.rs`）负责按 `--scene` 解析出实例。
    pub scene: Arc<dyn AgentScene>,
}

pub struct TurnOutput {
    pub text: String,
    pub tool_uses: Vec<(String, serde_json::Value)>,
}

/// A working directory unique to this run.
///
/// This used to be a fixed `/tmp/atta_test_runner`, wiped on entry — so two
/// cases running at once deleted each other's fixtures, and the failure
/// looked like a flaky test rather than a collision.
fn run_dir() -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("atta-test-runner-{}-{unique}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub async fn run_test_case(
    config: AgentRunnerConfig,
    case: &TestCase,
) -> anyhow::Result<Vec<TurnOutput>> {
    let tmp = run_dir();

    if let Some(fixture) = &config.fixture_dir {
        let workdir = tmp.join("workdir");
        crate::fixture::copy_dir_recursive(fixture, &workdir)?;
        crate::fixture::resolve_mcp_toy_server_placeholder(&workdir)?;
    }

    let mut results = Vec::new();
    for turn in &case.turns {
        let mut agent = build_agent(&config, &tmp, format!("test-turn-{}", turn.index)).await?;
        let out = send_and_collect(&mut agent, turn, format!("turn_{}", turn.index)).await?;
        results.push(out);
    }

    // Cleanup test artifacts (temp dirs, session files)
    let _ = std::fs::remove_dir_all(&tmp);

    Ok(results)
}

/// Like `run_test_case`, but builds **one** `Agent` for the whole case and
/// sends every turn to it in sequence — no rebuild between turns, same
/// `input_tx`/`event_rx` throughout.
///
/// Why this exists as a separate function rather than a flag on
/// `run_test_case`: that function (and every case recorded against it so
/// far) builds a *fresh* `Agent` per turn, each with its own fresh
/// `SkillManager`/`AgentTool` — so mutating the fixture between turns and
/// seeing turn 2 pick it up proves nothing about the Skills/Agent-type
/// `notify` watchers added this session (a cold `Builder::build()` sees
/// current disk state regardless of whether a watcher exists — that was
/// never in question). Only a *shared* `Agent` across turns can actually
/// exercise "the watcher fired and updated already-running state without a
/// rebuild," which is the specific guarantee those watchers exist to
/// provide. Kept as a separate entry point (not a retrofit of
/// `run_test_case`) so every existing recorded cassette — whose request hash
/// depends on the exact per-turn-isolated message history — stays valid
/// unchanged.
///
/// `mutations`, if given, describes filesystem edits to apply to the
/// fixture's copied workdir between specific turns (see `crate::mutations`)
/// — applied after the named turn completes, with a short grace period
/// before the next turn's message goes out so the watcher's background
/// thread has time to observe and react to the change.
pub async fn run_test_case_same_session(
    config: AgentRunnerConfig,
    case: &TestCase,
    mutations: Option<&crate::mutations::MutationManifest>,
) -> anyhow::Result<Vec<TurnOutput>> {
    let tmp = run_dir();

    if let Some(fixture) = &config.fixture_dir {
        let workdir = tmp.join("workdir");
        crate::fixture::copy_dir_recursive(fixture, &workdir)?;
        crate::fixture::resolve_mcp_toy_server_placeholder(&workdir)?;
    }

    let mut agent = build_agent(&config, &tmp, "test-session".to_string()).await?;

    let mut results = Vec::new();
    for turn in &case.turns {
        let out = send_and_collect(&mut agent, turn, format!("turn_{}", turn.index)).await?;
        results.push(out);

        if let Some(manifest) = mutations {
            if let Some(m) = manifest
                .mutations
                .iter()
                .find(|m| m.after_turn == turn.index)
            {
                let workdir = tmp.join("workdir");
                crate::mutations::apply(&workdir, m)?;
                eprintln!(
                    "Applied {} mutation(s) after turn {} — waiting for watcher pickup",
                    m.ops.len(),
                    turn.index
                );
                // `notify` delivers file events on a background thread; the
                // Skills/Agent-type watchers have no debounce, but the OS
                // event itself isn't synchronous with this process. A fixed
                // grace period (not a poll — there's nothing in this process
                // to poll, the watcher lives inside the spawned `Agent` task)
                // is the only externally-observable option; generous since
                // this only runs during a one-off recording, never on a hot
                // path.
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            }
        }
    }

    let _ = std::fs::remove_dir_all(&tmp);

    Ok(results)
}

/// A running `Agent` task plus everything needed to drive it and to explain
/// how it died.
///
/// `join` used to be dropped on the floor: when the agent task *panicked*, the
/// only externally visible effect was the event channel closing, which the
/// harness reported identically to "the agent exited without a TurnComplete".
/// A panic (a bug in the engine) and a clean turn error (a bug in the model
/// interaction) are very different findings and must not read the same in a
/// test report — keeping the handle lets `send_and_collect` say which one
/// happened, and surface the panic message.
struct RunningAgent {
    input_tx: InputSender,
    event_rx: EventReceiver,
    cancel: CancellationToken,
    join: tokio::task::JoinHandle<()>,
}

/// Build one `Agent`, spawn its run loop, and hand back the handle to drive
/// it — shared by `run_test_case` (fresh agent per turn) and
/// `run_test_case_same_session` (one agent for the whole case). `session_id`
/// is the only thing that varies between those two callers' usage.
async fn build_agent(
    config: &AgentRunnerConfig,
    tmp: &std::path::Path,
    session_id: String,
) -> anyhow::Result<RunningAgent> {
    let _ = std::fs::create_dir_all(tmp);

    // Passing an explicit config bypasses the recorder's own env-var
    // resolution, so this harness asks the function that owns the divergence
    // policy rather than re-deriving it — re-deriving it is exactly how the
    // equivalent flag once shipped as a silent no-op.
    let recorder_model = Arc::new(RecorderModel::new(
        config.model.clone(),
        Some(RecorderConfig {
            mode: config.recorder_mode,
            name: Some(config.recorder_name.clone()),
            root: config.recordings_dir.clone(),
            on_divergence: RecorderModel::default_divergence(),
        }),
        env!("CARGO_PKG_VERSION"),
    ));

    let model_name =
        std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".into());

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
        api_type: ApiType::Anthropic,
        base_url: String::new(),
        auth_token: String::new(),
        model_name: model_name.clone(),
        max_tokens: 2000,
        thinking_mode: ThinkingMode::Off,
        fallback_model: None,
    };
    if config.fixture_dir.is_none() {
        settings.paths = base::interface::settings::PathSettings {
            user_data_dir: tmp.join("user"),
            global_data_dir: tmp.join("global"),
            local_data_dir: tmp.join("local"),
            scope: "code".into(),
        };
    }
    settings.memory_enabled = false;
    // 测试非交互运行,始终旁路权限确认——和 daemon 的生产选择一致（IDE/测试
    // 宿主自己管沙盒）。
    settings.permission_mode = PermissionMode::BypassPermissions;
    let settings = Arc::new(settings);

    let tools_registry = make_tools();

    let mut builder = Builder::new()
        .scene(config.scene.clone())
        .model(recorder_model)
        .tools(tools_registry)
        .settings(settings.clone())
        .session_id(session_id)
        .skip_warmup(true);

    // Connect any MCP servers the fixture's settings.json configured — without
    // this, `Settings.mcp_servers` gets parsed but never actually connected, so
    // no MCP tool (e.g. `mcp__demo__ping`) is ever registered/callable and the
    // model never even sees it as an option. `daemon` does the equivalent via
    // `SessionPool::connect_mcp_servers_in_background` (main.rs); this harness
    // had no equivalent at all. Synchronous here (not fire-and-forget like
    // daemon) since the test needs the tools registered before the turn starts.
    if !settings.mcp_servers.is_empty() {
        let mut parsed = std::collections::HashMap::new();
        for (name, v) in &settings.mcp_servers {
            match serde_json::from_value::<mcp::config::McpServerConfig>(v.clone()) {
                Ok(cfg) => {
                    parsed.insert(name.clone(), cfg);
                }
                Err(e) => eprintln!("invalid mcp_servers.{name} config, skipping: {e}"),
            }
        }
        if !parsed.is_empty() {
            let requested: Vec<String> = parsed.keys().cloned().collect();
            let manager = mcp::manager::McpManager::connect_all(parsed).await;
            let connected: std::collections::HashSet<String> = manager
                .server_statuses()
                .iter()
                .map(|s| s.name.clone())
                .collect();
            for name in &requested {
                eprintln!(
                    "MCP server '{name}': {}",
                    if connected.contains(name) {
                        "connected"
                    } else {
                        "failed to connect"
                    }
                );
            }
            builder = builder.mcp_manager(manager);
        }
    }

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

    let (agent, event_rx, input_tx) = builder
        .build()
        .map_err(|e| anyhow::anyhow!("build agent: {e}"))?;

    let cancel = CancellationToken::new();
    let agent_cancel = cancel.clone();
    let join = tokio::spawn(async move {
        let mut agent = agent;
        let _ = agent.run(agent_cancel).await;
    });

    Ok(RunningAgent {
        input_tx,
        event_rx,
        cancel,
        join,
    })
}

/// Send one turn's input to an already-built agent and collect its response
/// until `TurnComplete`/`Error`/timeout. Reusable across multiple turns
/// against the same `event_rx` (see `run_test_case_same_session`) — each
/// call only drains events belonging to its own turn because `Agent::run()`
/// only ever has one turn in flight at a time.
async fn send_and_collect(
    agent: &mut RunningAgent,
    turn: &crate::script::Turn,
    turn_id: String,
) -> anyhow::Result<TurnOutput> {
    let RunningAgent {
        input_tx,
        event_rx,
        cancel,
        join,
    } = agent;
    let _ = input_tx.send(InputMessage::User {
        content: turn.input.clone(),
        attachments: vec![],
        turn_id,
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
                    return Err(anyhow::anyhow!(
                        "turn {} failed: [{code}] {message}",
                        turn.index
                    ));
                }
                _ => {}
            }
        }
        // The channel closing means the agent task is gone. *Why* it's gone is
        // the interesting part, and only the JoinHandle knows: a panic inside
        // the engine and a task that returned normally without completing the
        // turn are the same event from here, and reporting them the same way
        // hides real crashes behind a generic "channel closed".
        anyhow::bail!(
            "turn {} ended: event channel closed before TurnComplete/Error — {}",
            turn.index,
            describe_agent_exit(join).await
        );
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

/// Turn a dead agent task into a diagnosis: panic (with its message), external
/// cancellation, or a clean exit. Bounded wait — if the task is somehow still
/// alive after the event channel closed, say so rather than hanging the run.
async fn describe_agent_exit(join: &mut tokio::task::JoinHandle<()>) -> String {
    match tokio::time::timeout(std::time::Duration::from_secs(5), join).await {
        Ok(Ok(())) => "the agent task exited normally without emitting TurnComplete".to_string(),
        Ok(Err(e)) if e.is_panic() => {
            let payload = e.into_panic();
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            format!("the agent task PANICKED: {msg}")
        }
        Ok(Err(e)) => format!("the agent task was cancelled/aborted: {e}"),
        Err(_) => {
            "the agent task is still running (event channel closed independently)".to_string()
        }
    }
}

fn make_tools() -> Arc<InMemoryToolRegistry> {
    // Was a hand-rolled duplicate of this exact list — now the single source
    // of truth also used by `daemon` (see `tools::register_builtin_tools`'s
    // doc comment for why that duplication used to matter: daemon had none
    // of this registered at all).
    let reg = Arc::new(InMemoryToolRegistry::new());
    tools::register_builtin_tools(&reg);
    reg
}
