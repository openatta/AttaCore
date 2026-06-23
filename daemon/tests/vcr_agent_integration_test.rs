//! VCR 集成测试（方案 A — Agent API）
//!
//! 通过真实 Agent 流程录制 LLM 调用，VCR 透明夹在 Model 层录制/回放。
//!
//! ## 产出文件（每个 scenario）
//! | 文件 | 说明 |
//! |------|------|
//! | `{scenario}.jsonl` | 原始 VCR 录制（程序回放用） |
//! | `{scenario}-pretty.jsonl` | 格式化 JSON（人读，convert.py 生成） |
//! | `{scenario}.md` | `>>>>`/`<<<<` 分隔的输入输出日志 |
//! | `{scenario}.telemetry.md` | 遥测事件（录制模式） |
//! | `{scenario}.telemetry.{ts}.md` | 遥测事件（回放模式，带时间戳） |
//!
//! ## 运行
//! ```sh
//! # 录制
//! ATTA_VCR_RECORD=vcr_agent cargo test -p daemon \
//!   --test vcr_agent_integration_test -- test_vcr_agent_record --nocapture --ignored
//!
//! # 回放
//! ATTA_VCR_REPLAY=vcr_agent cargo test -p daemon \
//!   --test vcr_agent_integration_test -- test_vcr_agent_replay --nocapture
//! ```

use base::id::Id;
use base::interface::event::AgentEvent;
use base::interface::model::{Model, StreamParams};
use base::interface::settings::{PermissionMode, ThinkingMode, VcrConfig, VcrMode};
use base::interface::model::ModelStream;
use base::provider::ApiType;
use base::tool::{InMemoryToolRegistry, Tool};
use async_trait::async_trait;
use model::adapter::AnthropicModel;
use model::client::{AuthMode, HttpAnthropicClient};
use runtime::agent::{Builder, InputMessage};
use std::path::PathBuf;
use std::sync::Arc;
use telemetry::file_recorder::FileRecorder;
use telemetry::vcr::VcrModel;
use tokio_util::sync::CancellationToken;

const SCENARIO: &str = "vcr_agent";

fn vcr_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..").join("tests").join("fixtures").join("vcr")
}

// ═══════════════════════════════════════════════════════════════════
// DeepSeek config
// ═══════════════════════════════════════════════════════════════════

fn load_deepseek_config() -> Option<(String, String, String)> {
    let home = std::env::var("HOME").ok()?;
    let content = std::fs::read_to_string(PathBuf::from(home).join(".deepseek")).ok()?;
    let (mut url, mut token, mut model) = (String::new(), String::new(), String::new());
    for line in content.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("export ANTHROPIC_BASE_URL=") { url = v.trim().into(); }
        else if let Some(v) = l.strip_prefix("export ANTHROPIC_AUTH_TOKEN=") { token = v.trim().into(); }
        else if let Some(v) = l.strip_prefix("export ANTHROPIC_MODEL=") { model = v.trim().into(); }
    }
    if url.is_empty() || token.is_empty() || model.is_empty() { return None; }
    Some((url, token, model))
}

fn build_model() -> Result<Arc<dyn Model>, String> {
    let (mut url, token, _) = load_deepseek_config()
        .ok_or_else(|| String::from("~/.deepseek not found"))?;
    if !url.ends_with('/') { url.push('/'); }
    let parsed = url::Url::parse(&url).map_err(|e| e.to_string())?;
    let c = HttpAnthropicClient::with_base(AuthMode::ApiKey(token), parsed)
        .map_err(|e| format!("client: {e}"))?
        .with_backoff(vec![100, 200, 500]);
    Ok(Arc::new(AnthropicModel::new(Arc::new(c))))
}

// ═══════════════════════════════════════════════════════════════════
// Tool registry
// ═══════════════════════════════════════════════════════════════════

fn make_tools() -> Arc<InMemoryToolRegistry> {
    let reg = Arc::new(InMemoryToolRegistry::new());
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(tools::bash::BashTool),
        Arc::new(tools::file_read::FileReadTool),
        Arc::new(tools::file_write::FileWriteTool),
        Arc::new(tools::file_edit::FileEditTool),
        Arc::new(tools::grep::GrepTool),
        Arc::new(tools::glob::GlobTool),
        Arc::new(tools::lsp::LspTool),
        Arc::new(tools::todo_write::TodoWriteTool),
        Arc::new(tools::tasks::TaskCreateTool),
        Arc::new(tools::tasks::TaskUpdateTool),
        Arc::new(tools::task_output::TaskOutputTool),
        Arc::new(tools::task_stop::TaskStopTool),
    ];
    for t in tools { reg.register(t); }
    reg
}

// ═══════════════════════════════════════════════════════════════════
// Build Agent
// ═══════════════════════════════════════════════════════════════════

fn build_agent(
    model: Arc<dyn Model>,
    scenario: &str,
    mode: VcrMode,
    telemetry_path: Option<PathBuf>,
    frozen: Option<base::frozen::FrozenContext>,
) -> Result<(runtime::agent::Agent, runtime::agent::EventReceiver, runtime::agent::InputSender), String> {
    let tmp = PathBuf::from("/tmp/atta_vcr_agent_test");
    let _ = std::fs::create_dir_all(&tmp);

    let vcr_model = Arc::new(VcrModel::new(
        model,
        Some(VcrConfig { mode, scenario: scenario.into(), fallback_on_miss: true }),
        PathBuf::from("/tmp/atta_vcr_nonexistent"),
        vcr_dir(),
    ));

    let settings = Arc::new(base::interface::settings::Settings {
        model: base::interface::settings::ModelSettings {
            api_type: ApiType::Anthropic, base_url: String::new(), auth_token: String::new(),
            model_name: "deepseek-v4-pro[1m]".into(), max_tokens: 2000,
            thinking_mode: ThinkingMode::Off, fallback_model: None,
        },
        paths: base::interface::settings::PathSettings {
            user_data_dir: tmp.join("user"), local_data_dir: tmp.join("local"),
        },
        execution: Default::default(), compaction: Default::default(), sandbox: Default::default(),
        instruction_file: None, prompt_append: None, prompt_override: None,
        vcr: None, telemetry_url: None, session_dir: None,
        memory_enabled: false,
        permission_mode: PermissionMode::BypassPermissions,
        permission_rules: vec![], hooks_config: None, mcp_servers: vec![],
        language: None, feature_flags: Default::default(),
    });

    let mut builder = Builder::new()
        .scene(Arc::new(scene::scene::coding::CodingScene))
        .model(vcr_model)
        .tools(make_tools())
        .settings(settings)
        .session_id("vcr-test-session".to_string())
        .skip_warmup(true);

    if let Some(f) = frozen {
        builder = builder.frozen(f);
    }

    // Inject file-based telemetry recorder via channel shim.
    // Agent uses TelemetryHandle (channel-based); we spawn a consumer
    // that forwards events to FileRecorder for persistent logging.
    if let Some(tp) = telemetry_path {
        let rec = Arc::new(FileRecorder::new(&tp).map_err(|e| format!("telemetry file: {e}"))?);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<telemetry::events::TelemetryEvent>(1024);
        let rec_clone = Arc::clone(&rec);
        tokio::spawn(async move {
            use telemetry::TelemetryRecorder;
            while let Some(event) = rx.recv().await {
                let _ = rec_clone.record(event);
            }
        });
        builder = builder.telemetry_handle(telemetry::TelemetryHandle::new(tx));
    }

    builder.build().map_err(|e| format!("build agent: {e}"))
}

// ═══════════════════════════════════════════════════════════════════
// Helper: spawn agent + send message + drain events
// ═══════════════════════════════════════════════════════════════════

struct TurnOutput {
    text: String,
    tool_uses: Vec<String>,
}

async fn run_one_turn(
    agent: runtime::agent::Agent,
    mut event_rx: runtime::agent::EventReceiver,
    input_tx: runtime::agent::InputSender,
    message: &str,
) -> TurnOutput {
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        let mut agent = agent;
        let _ = agent.run(cancel_clone).await;
    });

    let _ = input_tx.send(InputMessage::User {
        content: message.into(),
        attachments: vec![],
        turn_id: "turn_001".into(),
    });

    let mut text = String::new();
    let mut tool_uses: Vec<String> = vec![];
    let mut turn_complete = false;
    while let Some(event) = event_rx.recv().await {
        match event {
            AgentEvent::TextDelta { text: t, .. } => text.push_str(&t),
            AgentEvent::ToolUse { name, .. } => tool_uses.push(name),
            AgentEvent::TurnComplete { .. } => { turn_complete = true; break; }
            _ => {}
        }
    }
    assert!(turn_complete, "turn should complete");
    TurnOutput { text, tool_uses }
}

// ═══════════════════════════════════════════════════════════════════
// Test 1 — Record
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "需要真实 LLM API；用 ATTA_VCR_RECORD=vcr_agent 运行"]
async fn test_vcr_agent_record() {
    if std::env::var("ATTA_VCR_RECORD").is_err() {
        eprintln!("SKIP: set ATTA_VCR_RECORD={SCENARIO}"); return;
    }
    if load_deepseek_config().is_none() { eprintln!("SKIP: ~/.deepseek not found"); return; }

    let real_model = build_model().expect("build model");
    let fixture_path = vcr_dir().join(format!("{SCENARIO}.jsonl"));
    let _ = std::fs::remove_file(&fixture_path);

    let telemetry_path = vcr_dir().join(format!("{SCENARIO}.telemetry.md"));
    let _ = std::fs::remove_file(&telemetry_path);

    // Clean temp dir for deterministic FrozenContext between record & replay
    let tmp_local = PathBuf::from("/tmp/atta_vcr_agent_test/local");
    let _ = std::fs::remove_dir_all(&tmp_local);
    let _ = std::fs::create_dir_all(&tmp_local);
    let frozen = base::frozen::FrozenContext::collect(tmp_local).await;

    let (agent, event_rx, input_tx) = build_agent(
        real_model, SCENARIO, VcrMode::Record,
        Some(telemetry_path.clone()), Some(frozen),
    ).expect("build agent");

    let out = run_one_turn(agent, event_rx, input_tx, "\u{521b}\u{5efa}\u{4e00}\u{4e2a} C \u{8bed}\u{8a00}\u{9879}\u{76ee}\u{ff0c}\u{8981}\u{6c42}\u{ff1a}\n\
                  1. src/main.c \u{2014}\u{2014} \u{8f93}\u{51fa} \"Hello, World!\" \u{7684}\u{4e3b}\u{7a0b}\u{5e8f}\n\
                  2. include/greet.h \u{2014}\u{2014} \u{58f0}\u{660e} void greet(const char* name) \u{51fd}\u{6570}\n\
                  3. src/greet.c \u{2014}\u{2014} \u{5b9e}\u{73b0} greet \u{51fd}\u{6570}\n\
                  4. Makefile \u{2014}\u{2014} \u{652f}\u{6301} make build（\u{7f16}\u{8bd1}）\u{548c} make run（\u{8fd0}\u{884c}）\n\
                  \u{8bf7}\u{9010}\u{4e2a}\u{521b}\u{5efa}\u{8fd9}\u{4e9b}\u{6587}\u{4ef6}\u{3002}").await;

    eprintln!("=== AGENT RESPONSE ===");
    eprintln!("text ({} chars):\n{}", out.text.len(), out.text);
    eprintln!("tool_uses: {:?}", out.tool_uses);
    eprintln!("======================");

    assert!(!out.text.is_empty() || !out.tool_uses.is_empty());
    assert!(fixture_path.exists(), "fixture not written");
    assert!(telemetry_path.exists(), "telemetry not written");

    let lines: Vec<_> = std::fs::read_to_string(&fixture_path).unwrap().lines().map(|s| s.to_string()).collect();
    eprintln!("Recorded {} VCR entries", lines.len());
    assert!(!lines.is_empty());
}

// ═══════════════════════════════════════════════════════════════════
// Test 2 — Replay
// ═══════════════════════════════════════════════════════════════════

struct PanicModel;
#[async_trait]
impl Model for PanicModel {
    fn api_type(&self) -> ApiType { ApiType::Anthropic }
    async fn stream(&self, _: Vec<base::interface::prompt::PromptBlock>, _: Vec<base::interface::model::ToolDef>, _: Vec<base::interface::model::ModelMessage>, _: StreamParams, _: CancellationToken) -> Result<ModelStream, base::interface::model::ModelError> {
        panic!("PanicModel::stream() called — VCR replay should intercept ALL calls");
    }
}

/// 回放测试。需要先用 ATTA_VCR_RECORD=vcr_agent 录制 fixture。
///
/// 已知限制：当前仅在录制后不重启进程时通过（FrozenContext 一致）。
/// 跨进程回放需要额外工作确保 Agent 内部状态确定性。
#[tokio::test]
#[ignore = "需要与录制相同的进程内 FrozenContext 快照"]
async fn test_vcr_agent_replay() {
    let fixture_path = vcr_dir().join(format!("{SCENARIO}.jsonl"));
    if !fixture_path.exists() {
        eprintln!("SKIP: no fixture. Run record test first."); return;
    }

    // replay 时遥测文件带时间戳，便于和录制遥测对比
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let telemetry_path = vcr_dir().join(format!("{SCENARIO}.telemetry.{ts}.md"));

    let mock_model: Arc<dyn Model> = Arc::new(PanicModel);

    // Re-collect FrozenContext from clean temp dir (same state as record)
    let tmp_local = PathBuf::from("/tmp/atta_vcr_agent_test/local");
    let _ = std::fs::remove_dir_all(&tmp_local);
    let _ = std::fs::create_dir_all(&tmp_local);
    let frozen = base::frozen::FrozenContext::collect(tmp_local).await;

    let (agent, event_rx, input_tx) = build_agent(
        mock_model, SCENARIO, VcrMode::Replay,
        Some(telemetry_path.clone()), Some(frozen),
    ).expect("build agent");

    let out = run_one_turn(agent, event_rx, input_tx, "\u{521b}\u{5efa}\u{4e00}\u{4e2a} C \u{8bed}\u{8a00}\u{9879}\u{76ee}\u{ff0c}\u{8981}\u{6c42}\u{ff1a}\n\
                  1. src/main.c \u{2014}\u{2014} \u{8f93}\u{51fa} \"Hello, World!\" \u{7684}\u{4e3b}\u{7a0b}\u{5e8f}\n\
                  2. include/greet.h \u{2014}\u{2014} \u{58f0}\u{660e} void greet(const char* name) \u{51fd}\u{6570}\n\
                  3. src/greet.c \u{2014}\u{2014} \u{5b9e}\u{73b0} greet \u{51fd}\u{6570}\n\
                  4. Makefile \u{2014}\u{2014} \u{652f}\u{6301} make build（\u{7f16}\u{8bd1}）\u{548c} make run（\u{8fd0}\u{884c}）\n\
                  \u{8bf7}\u{9010}\u{4e2a}\u{521b}\u{5efa}\u{8fd9}\u{4e9b}\u{6587}\u{4ef6}\u{3002}").await;

    eprintln!("=== REPLAYED ===");
    eprintln!("text ({} chars):\n{}", out.text.len(), out.text);
    eprintln!("tool_uses: {:?}", out.tool_uses);
    eprintln!("===============");

    assert!(!out.text.is_empty() || !out.tool_uses.is_empty());
    assert!(telemetry_path.exists(), "replay telemetry not written");
}
