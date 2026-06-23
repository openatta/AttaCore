//! SessionPool —— daemon 多 session 实例管理器。
//!
//! 每个 session 对应一个独立的 Agent 实例（后台 run loop + 独立 event channel）。
//! 支持：
//! - 按 session_id 查找/创建
//! - 容量上限 + LRU 驱逐
//! - 空闲超时回收
//! - session.list 合并活跃 + 历史

use crate::rpc::{codes, RpcResponse, SessionOptions, StreamFrame};
use base::context::EngineConfig;
use base::id::Id;
use base::interface::event::AgentEvent;
use base::interface::memory::MemoryStore;
use base::interface::permission::Permission;
use base::interface::scene::AgentScene;
use base::interface::settings::Settings;
use model::adapter::AnthropicModel;
use model::client::AnthropicClient;
use telemetry::file_recorder::FileRecorder;
use telemetry::vcr::VcrModel;
use base::interface::settings::{VcrConfig, VcrMode};
use runtime::agent::{Builder, EventReceiver, InputMessage, InputSender};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

type Writer = Arc<AsyncMutex<Box<dyn AsyncWrite + Send + Unpin + 'static>>>;

// ── LiveSession ─────────────────────────────────────────────────────────

struct LiveSession {
    input_tx: InputSender,
    event_rx: Arc<AsyncMutex<Option<EventReceiver>>>,
    cancel: CancellationToken,
    /// Session name（CHAT 场景首轮后由 LLM 生成；CODING 为 None）。
    name: Option<String>,
    created_at: Instant,
    last_active: Instant,
    /// 用于首轮命名判断。
    is_first_turn: bool,
}

// ── SessionPool ─────────────────────────────────────────────────────────

pub struct SessionPool {
    sessions: AsyncMutex<HashMap<String, LiveSession>>,
    cap: usize,
    idle_timeout: Duration,
    /// 共享的 LLM client（用于 session 命名等）。
    _client: Arc<dyn AnthropicClient>,
    model: Arc<dyn base::interface::model::Model>,
    settings: Arc<Settings>,
    scene_coding: Arc<dyn AgentScene>,
    scene_chat: Arc<dyn AgentScene>,
    permission: Arc<dyn Permission>,
    memory_store: Arc<MemoryStore>,
    _cwd: PathBuf,
    engine_config: EngineConfig,
    history_store: Option<Arc<dyn history::store::HistoryStore>>,
}

/// session.list 返回的单条记录。
#[derive(serde::Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub name: Option<String>,
    pub preview: Option<String>,
    pub message_count: u32,
    pub created_at: String,
    pub last_active: String,
    pub status: SessionStatus,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Inactive,
}

impl SessionPool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cap: usize,
        idle_timeout_secs: u64,
        client: Arc<dyn AnthropicClient>,
        settings: Arc<Settings>,
        scene_coding: Arc<dyn AgentScene>,
        scene_chat: Arc<dyn AgentScene>,
        permission: Arc<dyn Permission>,
        memory_store: Arc<MemoryStore>,
        cwd: PathBuf,
        engine_config: EngineConfig,
        history_store: Option<Arc<dyn history::store::HistoryStore>>,
    ) -> Self {
        let model = Arc::new(AnthropicModel::new(client.clone()));
        Self {
            sessions: AsyncMutex::new(HashMap::new()),
            cap,
            idle_timeout: Duration::from_secs(idle_timeout_secs),
            _client: client,
            model,
            settings,
            scene_coding,
            scene_chat,
            permission,
            memory_store,
            _cwd: cwd,
            engine_config,
            history_store,
        }
    }

    /// 创建新 session 并启动 Agent 后台 run loop。
    /// `options` 仅在新创建 session 时生效；已有 session 时忽略。
    async fn create(
        &self,
        session_id: String,
        scene: Arc<dyn AgentScene>,
        options: Option<&SessionOptions>,
    ) -> Result<String, String> {
        let mut config = self.engine_config.clone();
        config.permission_mode = base::permission::PermissionMode::BypassPermissions;

        // Apply VCR wrapping if configured
        let model: Arc<dyn base::interface::model::Model> = match options.and_then(|o| o.vcr.as_ref()) {
            Some(vcr) => {
                let mode = match vcr.mode.as_str() {
                    "record" => VcrMode::Record,
                    _ => VcrMode::Replay,
                };
                Arc::new(VcrModel::new(
                    self.model.clone(),
                    Some(VcrConfig { mode, scenario: vcr.scenario.clone(), fallback_on_miss: true }),
                    std::path::PathBuf::from("/tmp/atta_vcr_nonexistent"),
                    std::path::PathBuf::from(&vcr.dir),
                ))
            }
            None => self.model.clone(),
        };

        let mut builder = Builder::new()
            .scene(scene)
            .model(model)
            .settings(self.settings.clone())
            .permission(self.permission.clone())
            .memory_store(self.memory_store.clone())
            .session_id(session_id.clone());

        // Apply telemetry file recorder if configured
        if let Some(telemetry_path) = options.and_then(|o| o.telemetry.as_ref()).map(|t| t.output.clone()) {
            if let Ok(rec) = FileRecorder::new(&telemetry_path) {
                let rec = std::sync::Arc::new(rec);
                let (tx, mut rx) = tokio::sync::mpsc::channel::<telemetry::events::TelemetryEvent>(1024);
                let rec_clone = rec.clone();
                tokio::spawn(async move {
                    use telemetry::TelemetryRecorder;
                    while let Some(event) = rx.recv().await {
                        let _ = rec_clone.record(event);
                    }
                });
                builder = builder.telemetry_handle(telemetry::TelemetryHandle::new(tx));
            }
        }

        let (agent, event_rx, input_tx) = builder
            .build()
            .map_err(|e| format!("build agent: {e}"))?;

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        // 启动 Agent 后台事件循环
        tokio::spawn(async move {
            let mut agent = agent;
            let _ = agent.run(cancel_clone).await;
        });

        let mut sessions = self.sessions.lock().await;

        // 容量检查：驱逐最久未活跃的 session
        if sessions.len() >= self.cap {
            self.evict_lru(&mut sessions).await;
        }

        let live = LiveSession {
            input_tx,
            event_rx: Arc::new(AsyncMutex::new(Some(event_rx))),
            cancel,
            name: None,
            created_at: Instant::now(),
            last_active: Instant::now(),
            is_first_turn: true,
        };
        sessions.insert(session_id.clone(), live);
        info!(%session_id, "session created");
        Ok(session_id)
    }

    /// 执行一个 turn：发送消息 → 流式返回事件 → 返回结果。
    /// session_id 为 None 时自动创建新 session。
    pub async fn run_turn(
        &self,
        session_id: Option<String>,
        message: String,
        turn_id: String,
        writer: Writer,
        id: serde_json::Value,
        options: Option<SessionOptions>,
    ) -> RpcResponse {
        // ── 解析或创建 session ──
        let sid = match session_id {
            Some(ref sid) => {
                let sessions = self.sessions.lock().await;
                if sessions.contains_key(sid) {
                    sid.clone()
                } else {
                    drop(sessions);
                    self.resume_or_create(sid.clone(), options.as_ref()).await
                }
            }
            None => {
                let sid = Id::new().to_string();
                match self.create(sid.clone(), self.scene_coding.clone(), options.as_ref()).await {
                    Ok(sid) => sid,
                    Err(e) => {
                        return RpcResponse::err(id, codes::INTERNAL_ERROR, e);
                    }
                }
            }
        };

        // ── 获取 session 的 input_tx 和 event_rx ──
        let (input_tx, event_rx_mutex) = {
            let mut sessions = self.sessions.lock().await;
            let live = match sessions.get_mut(&sid) {
                Some(s) => s,
                None => {
                    return RpcResponse::err(
                        id,
                        codes::SESSION_NOT_FOUND,
                        format!("session not found: {sid}"),
                    );
                }
            };
            live.last_active = Instant::now();
            (live.input_tx.clone(), live.event_rx.clone())
        };

        // 发送用户消息
        let _ = input_tx.send(InputMessage::User {
            content: message.clone(),
            attachments: vec![],
            turn_id: turn_id.clone(),
        });

        // 取出 event_rx（独占 drain）
        let mut event_rx = match event_rx_mutex.lock().await.take() {
            Some(rx) => rx,
            None => {
                return RpcResponse::err(id, codes::INTERNAL_ERROR, "event channel busy");
            }
        };

        let mut api_calls = 0u32;
        let mut writer_broken = false;
        loop {
            match event_rx.recv().await {
                Some(AgentEvent::SystemInit { .. }) => continue,
                Some(AgentEvent::TextDelta { text, .. }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({"kind":"text_delta","text":text}),
                    );
                    if let Ok(mut b) = serde_json::to_vec(&f) {
                        b.push(b'\n');
                        if writer.lock().await.write_all(&b).await.is_err() {
                            writer_broken = true;
                            break;
                        }
                    }
                }
                Some(AgentEvent::ToolUse { id: tid, name, input, .. }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({"kind":"tool_use","id":tid,"name":name,"input":input}),
                    );
                    if let Ok(mut b) = serde_json::to_vec(&f) {
                        b.push(b'\n');
                        if writer.lock().await.write_all(&b).await.is_err() {
                            writer_broken = true;
                            break;
                        }
                    }
                }
                Some(AgentEvent::ToolResult { id: tid, name, content, is_error, .. }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({"kind":"tool_result","id":tid,"name":name,"content":content,"is_error":is_error}),
                    );
                    if let Ok(mut b) = serde_json::to_vec(&f) {
                        b.push(b'\n');
                        if writer.lock().await.write_all(&b).await.is_err() {
                            writer_broken = true;
                            break;
                        }
                    }
                }
                Some(AgentEvent::TurnComplete { stop_reason, api_calls: ac, usage, .. }) => {
                    api_calls = ac;
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({
                            "kind":"turn_complete","stop_reason":stop_reason,"api_calls":api_calls,
                            "usage":{"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens}
                        }),
                    );
                    if let Ok(mut b) = serde_json::to_vec(&f) {
                        b.push(b'\n');
                        let _ = writer.lock().await.write_all(&b).await;
                    }
                    break;
                }
                Some(AgentEvent::Error { code, message, .. }) => {
                    // 归还 event_rx
                    *event_rx_mutex.lock().await = Some(event_rx);
                    return RpcResponse::err(id, codes::ENGINE_ERROR, format!("{code}: {message}"));
                }
                _ => continue,
            }
        }

        // Client disconnected during turn — cancel the session immediately so
        // the Agent stops processing and any child processes (e.g. BashTool)
        // are killed, rather than waiting up to 5 minutes for the janitor.
        if writer_broken {
            drop(event_rx);
            self.shutdown_session(&sid).await;
            return RpcResponse::ok(
                id,
                serde_json::json!({
                    "session_id": sid,
                    "turn_id": turn_id,
                    "disconnected": true,
                }),
            );
        }

        // 归还 event_rx
        *event_rx_mutex.lock().await = Some(event_rx);

        // ── 首轮自动命名 ──
        let mut session_name = None;
        {
            let mut sessions = self.sessions.lock().await;
            if let Some(live) = sessions.get_mut(&sid) {
                if live.is_first_turn {
                    live.is_first_turn = false;
                    // 尝试通过场景判断是否需要命名
                    // CODING 场景不需要；CHAT 场景需要额外 LLM 调用
                    if self.scene_chat.auto_name_session() {
                        if let Some(prompt) = self
                            .scene_chat
                            .session_name_prompt(&message)
                        {
                            match self.generate_session_name(&prompt).await {
                                Ok(name) => {
                                    live.name = Some(name.clone());
                                    session_name = Some(name);
                                }
                                Err(e) => {
                                    warn!(%sid, error=%e, "session name generation failed");
                                }
                            }
                        }
                    }
                } else {
                    session_name = live.name.clone();
                }
            }
        }

        RpcResponse::ok(
            id,
            serde_json::json!({
                "session_id": sid,
                "turn_id": turn_id,
                "name": session_name,
                "api_calls": api_calls,
            }),
        )
    }

    /// 列出所有 session（活跃的 + 磁盘上历史的），合并去重。
    pub async fn list_all(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.lock().await;
        let mut out: Vec<SessionInfo> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // 活跃 session
        for (sid, live) in sessions.iter() {
            seen.insert(sid.clone());
            out.push(SessionInfo {
                session_id: sid.clone(),
                name: live.name.clone(),
                preview: None,
                message_count: 0,
                created_at: format_instant(live.created_at),
                last_active: format_instant(live.last_active),
                status: SessionStatus::Active,
            });
        }

        // 从 HistoryStore 查磁盘历史（inactive sessions）
        if let Some(ref store) = self.history_store {
            if let Ok(sids) = store.list_sessions().await {
                for sid in sids {
                    let sid_str = sid.to_string();
                    if seen.contains(&sid_str) {
                        continue;
                    }
                    out.push(SessionInfo {
                        session_id: sid_str,
                        name: None,
                        preview: None,
                        message_count: 0,
                        created_at: String::new(),
                        last_active: String::new(),
                        status: SessionStatus::Inactive,
                    });
                }
            }
        }

        out
    }

    /// 获取活跃 session 数量。
    pub async fn active_count(&self) -> usize {
        self.sessions.lock().await.len()
    }

    /// 关闭指定 session。
    pub async fn shutdown_session(&self, session_id: &str) {
        let mut sessions = self.sessions.lock().await;
        if let Some(live) = sessions.remove(session_id) {
            live.cancel.cancel();
            info!(%session_id, "session removed");
        }
    }

    /// 关闭所有 session。
    pub async fn shutdown_all(&self) {
        let mut sessions = self.sessions.lock().await;
        for (sid, live) in sessions.drain() {
            live.cancel.cancel();
            info!(%sid, "session removed (shutdown)");
        }
    }

    /// 后台回收任务：定期驱逐超时 idle session。
    pub fn start_janitor(self: &Arc<Self>) {
        let pool = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(300)).await; // 每 5 分钟
                let mut sessions = pool.sessions.lock().await;
                let timeout = pool.idle_timeout;
                let now = Instant::now();
                let to_evict: Vec<String> = sessions
                    .iter()
                    .filter(|(_, live)| now.duration_since(live.last_active) > timeout)
                    .map(|(sid, _)| sid.clone())
                    .collect();
                for sid in &to_evict {
                    if let Some(live) = sessions.remove(sid) {
                        live.cancel.cancel();
                        info!(%sid, "session evicted (idle timeout)");
                    }
                }
                if !to_evict.is_empty() {
                    debug!(evicted = to_evict.len(), remaining = sessions.len(), "janitor run");
                }
            }
        });
    }

    // ── 内部方法 ──

    /// 从 HistoryStore 恢复 session 或创建新的。
    async fn resume_or_create(&self, sid: String, options: Option<&SessionOptions>) -> String {
        // 尝试从 HistoryStore 加载历史消息
        let has_history = if let Some(ref store) = self.history_store {
            match store.load(base::session::SessionId::parse(&sid).unwrap()).await {
                Ok(entries) => !entries.is_empty(),
                Err(_) => false,
            }
        } else {
            false
        };

        if has_history {
            match self.create(sid.clone(), self.scene_coding.clone(), options).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(%sid, error=%e, "resume failed, creating new");
                    let new_sid = Id::new().to_string();
                    self.create(new_sid.clone(), self.scene_coding.clone(), options)
                        .await
                        .unwrap_or_else(|e| panic!("create session: {e}"))
                }
            }
        } else {
            match self.create(sid.clone(), self.scene_coding.clone(), options).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(sid, error=%e, "create with given sid failed");
                    let new_sid = Id::new().to_string();
                    self.create(new_sid.clone(), self.scene_coding.clone(), options)
                        .await
                        .unwrap_or_else(|e| panic!("create session: {e}"))
                }
            }
        }
    }

    /// LRU 驱逐：移除最久未活跃的 session。
    async fn evict_lru(&self, sessions: &mut HashMap<String, LiveSession>) {
        if let Some((sid, _)) = sessions
            .iter()
            .min_by_key(|(_, live)| live.last_active)
            .map(|(k, v)| (k.clone(), v.last_active))
        {
            if let Some(live) = sessions.remove(&sid) {
                live.cancel.cancel();
                info!(%sid, "session evicted (LRU, pool full)");
            }
        }
    }

    /// 调用 LLM 生成 session 名称。
    async fn generate_session_name(&self, prompt: &str) -> Result<String, String> {
        use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage, StreamParams};
        use base::interface::prompt::PromptBlock;
        use futures::StreamExt;

        let system = PromptBlock::system("你是一个简洁的标题生成器。只输出 3-5 个词的中文标题，不要任何解释。");
        let messages = vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: prompt.to_string(),
            }],
        }];

        let stream = self
            .model
            .stream(
                vec![system],
                vec![],
                messages,
                StreamParams {
                    model: "claude-haiku-4-5-20251001".into(),
                    max_tokens: 50,
                    thinking_mode: base::interface::settings::ThinkingMode::Off,
                    fallback_model: None,
                    cache_edits: vec![],
                },
                CancellationToken::new(),
            )
            .await
            .map_err(|e| format!("LLM name error: {e}"))?;

        tokio::pin!(stream);
        let mut name = String::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(base::interface::model::ModelEvent::TextDelta { text }) => {
                    name.push_str(&text);
                }
                Ok(base::interface::model::ModelEvent::EndTurn { .. }) => break,
                Err(e) => return Err(format!("LLM name stream error: {e}")),
                _ => {}
            }
        }

        let name = name.trim().trim_matches('"').trim().to_string();
        if name.is_empty() {
            Err("empty name generated".into())
        } else {
            Ok(name)
        }
    }
}

fn format_instant(t: Instant) -> String {
    let ago = Instant::now().duration_since(t);
    let secs_ago = ago.as_secs();
    let now = time::OffsetDateTime::now_utc();
    let abs = now - time::Duration::seconds(secs_ago as i64);
    abs.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
}
