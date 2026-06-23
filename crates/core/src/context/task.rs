//! 后台任务 + todo 条目类型。

/// P1-4: Task type enum — categorises all background/async task kinds.
/// TS parity: TaskType in claude-code's Task.ts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    /// Shell command executed as a background process.
    Shell,
    /// Sub-agent spawned by AgentTool.
    Agent,
    /// In-process teammate (team/swarm mode).
    Teammate,
    /// Cron/scheduled job.
    CronJob,
    /// Dream/background thinking task.
    Dream,
    /// Workflow orchestration task.
    Workflow,
    /// MCP monitor task.
    Monitor,
}

impl TaskType {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskType::Shell => "shell",
            TaskType::Agent => "agent",
            TaskType::Teammate => "teammate",
            TaskType::CronJob => "cron",
            TaskType::Dream => "dream",
            TaskType::Workflow => "workflow",
            TaskType::Monitor => "monitor",
        }
    }

    /// Prefix used in task IDs for quick type recognition.
    pub fn id_prefix(&self) -> &'static str {
        match self {
            TaskType::Shell => "sh",
            TaskType::Agent => "ag",
            TaskType::Teammate => "tm",
            TaskType::CronJob => "cj",
            TaskType::Dream => "dr",
            TaskType::Workflow => "wf",
            TaskType::Monitor => "mo",
        }
    }
}

/// P1-4: Strongly-typed task identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TaskId(pub String);

impl TaskId {
    pub fn new(task_type: TaskType) -> Self {
        let uuid = uuid::Uuid::new_v4();
        let short = &uuid.to_string()[..8];
        TaskId(format!("{}-{}", task_type.id_prefix(), short))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 后台运行的 sub-agent。output / events_log / status 在 sub-agent task loop
/// 中追加；TaskOutput 工具读快照；TaskStop 触发 cancel token。
#[derive(Debug)]
pub struct RunningTask {
    pub task_id: String,
    /// P1-4: Optional typed task identifier.
    pub typed_id: Option<TaskId>,
    /// P1-4: Task category.
    pub task_type: TaskType,
    /// 累积的 assistant 文本（流式 delta append）
    pub output: std::sync::Mutex<String>,
    /// 工具调用与结果的可读日志（每行一条）
    pub events_log: std::sync::Mutex<Vec<String>>,
    pub status: std::sync::Mutex<RunningStatus>,
    pub cancel: tokio_util::sync::CancellationToken,
}

impl RunningTask {
    /// Create a new running task with defaults.
    pub fn new(task_id: String, task_type: TaskType) -> Self {
        Self {
            task_id,
            typed_id: None,
            task_type,
            output: std::sync::Mutex::new(String::new()),
            events_log: std::sync::Mutex::new(Vec::new()),
            status: std::sync::Mutex::new(RunningStatus::Running),
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunningStatus {
    /// 还在跑
    Running,
    /// 完成（自然结束）
    Completed,
    /// TaskStop 触发取消
    Cancelled,
    /// 内部错误（API / 工具）
    Failed(String),
}

/// 一条 todo —— 模型用 TodoWrite 一次性替换整个列表。
#[derive(
    Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema, PartialEq, Eq,
)]
pub struct TodoItem {
    /// 给用户看的简短描述
    pub content: String,
    /// 状态
    pub status: TodoStatus,
    /// 现在时态描述（spinner 用），如 "Writing tests"。必填。
    /// **Q4 **: serde alias so payloads using `activeForm` (camelCase)
    /// deserialize into our `active_form` (snake_case) field. Models trained
    /// on TS schema can call our TodoWrite without contortion.
    #[serde(alias = "activeForm")]
    pub active_form: String,
}

#[derive(
    Debug, Clone, Copy, serde::Serialize, serde::Deserialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// 后台 agent 进度通知数据。AgentTool（`call_background`）在 drain
/// 循环中通过 ToolCtx.events_tx 发送，Engine 包装为 EngineEvent::BackgroundAgentProgress
/// 供 CLI/TUI 实时显示。
#[derive(Debug, Clone)]
pub struct BackgroundAgentProgressData {
    pub task_id: String,
    pub text: String,
    pub tool_name: Option<String>,
}
