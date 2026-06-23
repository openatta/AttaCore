# AttaCore 库模式开发者指南 — 嵌入式 API

> 面向将 AttaCore 作为 Rust 库嵌入自己应用的开发者。
> 适用场景：桌面应用、服务端、自定义 CLI 工具 —— 直接操作 Agent 对象，获得完全控制权。

---

## 1. 逻辑架构

AttaCore 是一个 **18-crate workspace**，采用分层设计：

```
┌──────────────────────────────────────────────────────────┐
│  你的应用（桌面 / CLI / 服务）                             │
│                                                          │
│  构造 Settings → 构造 Agent(Builder) → 启动 run(cancel)   │
│  通过 InputSender 发消息 → 通过 EventReceiver 收事件       │
│                                                          │
│  SessionManager 只管磁盘历史（增删查），不管实例生命周期      │
│  用户自己管理一个或多个 Agent 实例                          │
└──────────┬───────────────────────────────────────────────┘
           │
┌──────────▼───────────────────────────────────────────────┐
│  L4   runtime crate                                       │
│       Agent (turn loop), Builder, InputMessage/AgentEvent │
│       dispatch, streaming, request                         │
├──────────────────────────────────────────────────────────┤
│  L3   tools / skills / scene / task / team                 │
│       全部内置工具 + skill 加载 + 场景定义                  │
│       (CodingScene, ChatScene, DemoScene)                  │
├──────────────────────────────────────────────────────────┤
│  L2   model / history / permissions / mcp / compaction     │
│       session（对话状态管理 + HistoryStore 持久化）         │
├──────────────────────────────────────────────────────────┤
│  L1   core crate                                          │
│       所有 trait 定义 (Model, Scene, Permission, Memory)   │
│       基础类型 (PromptBlock, ModelMessage, Settings)       │
│       Id (BASE58 UUID), error, provider, context           │
├──────────────────────────────────────────────────────────┤
│  L0   auth / hooks / plugin / telemetry                   │
│       零依赖叶节点 crate                                   │
└──────────────────────────────────────────────────────────┘
```

### 核心设计原则

- **一个 Agent = 一个 session**：`Builder::session_id(id)` 绑定 session，或多个 Agent 实例管理多个 session。SessionManager 不管理实例。
- **trait 注入**：应用层实现 `Model`、`Permission`、`AgentScene` 等 trait，通过 `Builder` 注入 Agent。
- **channel 通信**：Agent 通过 `InputSender` / `EventReceiver`（mpsc unbounded channel）与外部通信。
- **ID 体系**：所有 ID 均为 BASE58(UUID v4) 文本，22 字符，唯一生成入口 `core::id::Id::new()`。

---

## 2. 依赖

在你的 `Cargo.toml` 中：

```toml
[dependencies]
runtime     = { path = "../AttaCore/crates/runtime" }
core        = { path = "../AttaCore/crates/core" }
model       = { path = "../AttaCore/crates/model" }
scene       = { path = "../AttaCore/crates/scene" }
session     = { path = "../AttaCore/crates/session" }
history     = { path = "../AttaCore/crates/history" }
tools       = { path = "../AttaCore/crates/tools" }
skills      = { path = "../AttaCore/crates/skills" }

tokio       = { version = "1", features = ["full"] }
tokio-util  = { version = "0.7" }
serde_json  = "1"
anyhow      = "1"
```

---

## 3. 启动一个最小 Agent

```rust
use std::path::PathBuf;
use std::sync::Arc;

use core::id::Id;
use core::interface::memory::MemoryStore;
use core::interface::permission::PermissionOutcome;
use core::interface::settings::{
    CompactionConfig, ExecutionSettings, ModelSettings, PathSettings, SandboxConfig,
    Settings, ThinkingMode,
};
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};
use model::adapter::AnthropicModel;
use runtime::agent::{Agent, EventReceiver, InputSender, Builder, InputMessage};
use scene::scene::coding::CodingScene;
use tokio_util::sync::CancellationToken;

/// 全部允许的权限处理器（生产环境应替换为真实实现）。
struct AllowAllPermission;

#[async_trait::async_trait]
impl core::interface::permission::Permission for AllowAllPermission {
    async fn check(
        &self, _tool: &str, _input: &serde_json::Value,
        _cwd: &std::path::Path, _session_id: &str,
    ) -> PermissionOutcome {
        PermissionOutcome::Permit
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. 准备路径
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let user_dir = PathBuf::from(&home).join(".atta").join("code");
    let local_dir = PathBuf::from(".").join(".atta").join("code");

    // 2. 组装 Settings
    let settings = Arc::new(Settings {
        model: ModelSettings {
            api_type: core::provider::ApiType::Anthropic,
            base_url: String::new(),
            auth_token: String::new(),
            model_name: "claude-sonnet-4-6".into(),
            max_tokens: 2000,
            thinking_mode: ThinkingMode::Auto,
            fallback_model: None,
        },
        paths: PathSettings {
            user_data_dir: user_dir.clone(),
            local_data_dir: local_dir.clone(),
        },
        execution: ExecutionSettings::default(),
        compaction: CompactionConfig::default(),
        sandbox: SandboxConfig::default(),
        instruction_file: None,
        prompt_append: None,
        prompt_override: None,
        vcr: None,
        telemetry_url: None,
        session_dir: Some(local_dir.clone()), // 启用会话持久化
    });

    // 3. 创建 Model 客户端
    let api_key = std::env::var("ANTHROPIC_API_KEY")?;
    let auth = AuthMode::ApiKey(api_key);
    let client: Arc<dyn AnthropicClient> = Arc::new(HttpAnthropicClient::new(auth)?);
    let model = Arc::new(AnthropicModel::new(client));

    // 4. 选择 Scene
    let scene: Arc<dyn core::interface::scene::AgentScene> = Arc::new(CodingScene);

    // 5. 构建 Agent（Builder 模式），一个实例 = 一个 session
    let session_id = Id::new().to_string(); // BASE58 UUID, 22 字符
    let (mut agent, event_rx, input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(settings.clone())
        .permission(Arc::new(AllowAllPermission))
        .memory_store(Arc::new(MemoryStore::new(
            user_dir.join("memory"),
            local_dir.join("memory"),
        )))
        .session_id(session_id.clone())
        .build()?;

    // 6. 启动事件循环（后台任务）
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        let mut agent = agent;
        let _ = agent.run(cancel_clone).await;
    });

    // 7. 发送用户消息
    let turn_id = Id::new().to_string();
    let _ = input_tx.send(InputMessage::User {
        content: "用 Rust 写一个 hello world".into(),
        attachments: vec![],
        turn_id: turn_id.clone(),
    });

    // 8. 接收流事件（所有事件携带 turn_id）
    use core::interface::event::AgentEvent;
    let mut event_rx = event_rx;
    while let Some(event) = event_rx.recv().await {
        match event {
            AgentEvent::TextDelta { text, .. } => print!("{text}"),
            AgentEvent::ToolUse { name, input, .. } =>
                eprintln!("\n🔧 {name}: {input}"),
            AgentEvent::TurnComplete { stop_reason, api_calls, .. } => {
                println!("\n✓ turn 完成 [stop={stop_reason}, calls={api_calls}, turn={turn_id:.8}]");
                break;
            }
            AgentEvent::Error { code, message, .. } => {
                eprintln!("✗ [{code}]: {message}");
                break;
            }
            _ => {}
        }
    }

    cancel.cancel();
    Ok(())
}
```

---

## 4. 核心 API 参考

### 4.1 Builder — 构建 Agent

```rust
let (agent, event_rx, input_tx) = Builder::new()
    .scene(scene)            // 必选: Arc<dyn AgentScene>
    .model(model)            // 必选: Arc<dyn Model>
    .settings(settings)      // 必选: Arc<Settings>
    // 以下可选
    .tools(tools)            // Arc<InMemoryToolRegistry>（默认：空注册表）
    .permission(perm)        // Arc<dyn Permission>（默认：全部允许）
    .memory_store(mem)       // Arc<MemoryStore>（默认：基于 paths 构建）
    .compactor(c)            // Arc<dyn Compactor>（默认：DefaultCompactor）
    .hooks(h)                // Arc<HookRunner>（默认：noop）
    .session_id(id)          // 指定 session ID，BASE58 UUID 字符串（默认：自动生成）
    .instruction(path)       // CLAUDE.md / ATTA.md 路径
    .telemetry_url(url)      // 遥测端点
    .telemetry_handle(h)     // 预构建的遥测 handle
    .mcp_manager(m)          // 预构建的 MCP 管理器
    .mcp_servers(names)      // MCP 服务名列表
    .build()?;               // → (Agent, EventReceiver, InputSender)
```

### 4.2 Agent — 引擎本体

```rust
// 启动事件循环
agent.run(cancel: CancellationToken) -> ()

// 便捷方法：单次 turn
agent.run_turn(content: String, turn_id: String, cancel: CancellationToken)
    -> Result<TurnOutcome, TurnError>

// 查询接口
agent.session_info()           -> SessionSummary
agent.list_sessions()          -> Result<Vec<SessionSummary>, SessionError>  // 从 HistoryStore 查
agent.delete_session(id)       -> Result<(), SessionError>                   // 从 HistoryStore 删
agent.perf()                   -> &PerfCollector
agent.tools()                  -> &InMemoryToolRegistry
agent.settings()               -> &Settings
agent.permission()             -> &dyn Permission
agent.memory()                 -> &MemoryStore
agent.skills()                 -> &SkillManager
agent.hooks()                  -> &HookRunner
agent.mcp()                    -> &McpManager
agent.telemetry()              -> &dyn TelemetryRecorder

// 运行时修改
agent.set_model(name: String)   // 切换模型
agent.compact_now()             // 手动触发上下文压缩
agent.run_hooks(event, input)   // 执行 hooks
```

### 4.3 InputMessage — 输入通道

```rust
pub enum InputMessage {
    /// 用户文本消息（最常用）
    User {
        content: String,
        attachments: Vec<Attachment>,
        turn_id: String,       // BASE58(UUID), 22 字符
    },
    /// 工具执行结果
    ToolResult { tool_use_id: String, name: String, content: String, is_error: bool },
    /// 权限提示的响应
    PermissionResponse { prompt_id: String, decision: PermissionDecision },
    /// 系统控制消息
    System { kind: SystemKind, content: String },
}
```

**SystemKind** 变体：
| 变体 | 说明 |
|---|---|
| `SetSessionId` | 切换当前会话 ID |
| `CompactNow` | 立即触发上下文压缩 |
| `RefreshMcp` | 刷新 MCP 连接 |
| `UpdateModel` | 更新模型配置 |
| `Shutdown` | 请求引擎关闭 |

### 4.4 AgentEvent — 输出事件流

所有 turn 作用域事件都携带 `turn_id: String`（BASE58 UUID）：

```rust
#[serde(tag = "kind")]
pub enum AgentEvent {
    // 流式输出（turn 作用域）
    TextDelta    { text: String, turn_id: String },
    ToolUse      { id: String, name: String, input: Value, turn_id: String },
    ToolResult   { id: String, name: String, content: String, is_error: Option<bool>, turn_id: String },

    // 权限交互（turn 作用域）
    PermissionPrompt { prompt_id: String, tool_name: String, message: String,
                       paths: Vec<PathBuf>, turn_id: String },

    // Turn 生命周期（turn 作用域）
    TurnComplete { stop_reason: String, api_calls: u32, tool_calls: u32,
                   usage: Usage, turn_id: String },

    // 系统事件（turn 作用域）
    CompactAction { strategy: String, messages_before: usize, messages_after: usize, turn_id: String },
    Error { code: String, message: String, turn_id: String },

    // 子 Agent（turn 作用域）
    AgentSpawned { agent_id: String, parent_turn: u32, turn_id: String },
    AgentCompleted { agent_id: String, outcome: String, turn_id: String },

    // 会话级事件（无 turn_id）
    SystemInit { scene: String, tools: Vec<ToolInfo>, mcp_servers: Vec<String> },
    System { message: String },
    SessionChanged { session_id: String },
    SessionPersisted { session_id: String },
}
```

---

## 5. Session 管理（库模式）

库模式下，SessionManager **只管理磁盘历史数据**，不管理 Agent 实例。用户自己管理一个或多个 Agent 实例。

### 5.1 列举 session

```rust
// 从 HistoryStore（磁盘）读取所有 session
let sessions = agent.list_sessions().await?;
for s in &sessions {
    println!("{} — {} — {} messages — title: {:?}",
        s.session_id, s.last_modified, s.message_count, s.title);
}
```

### 5.2 删除 session

```rust
// 从磁盘删除 session 的全部数据
agent.delete_session("Ab12Cd34Ef56Gh78Ij90Kl").await?;
```

### 5.3 多 session 模式（管理多个 Agent 实例）

```rust
use std::collections::HashMap;
use core::id::Id;

struct MyApp {
    sessions: HashMap<String, (InputSender, EventReceiver, CancellationToken)>,
    // ... 共享的 model, settings 等
}

impl MyApp {
    async fn create_session(&mut self) -> anyhow::Result<String> {
        let sid = Id::new().to_string();
        let (agent, event_rx, input_tx) = Builder::new()
            .scene(self.scene.clone())
            .model(self.model.clone())
            .settings(self.settings.clone())
            .session_id(sid.clone())
            .build()?;

        let cancel = CancellationToken::new();
        tokio::spawn(async move {
            let mut agent = agent;
            let _ = agent.run(cancel.clone()).await;
        });

        self.sessions.insert(sid.clone(), (input_tx, event_rx, cancel));
        Ok(sid)
    }

    fn remove_session(&mut self, sid: &str) {
        if let Some((_, _, cancel)) = self.sessions.remove(sid) {
            cancel.cancel();
        }
    }
}
```

---

## 6. ID 体系

所有外部可见的 ID 均为 BASE58(UUID v4) 文本，22 字符（有时 21）。

```rust
use core::id::Id;

// 生成新 ID
let session_id = Id::new().to_string();   // → "Ab12Cd34Ef56Gh78Ij90Kl"
let turn_id    = Id::new().to_string();   // → "Mn12Op34Qr56St78Uv90Wx"

// 从外部输入解析验证
let parsed: Id = Id::parse("Ab12Cd34Ef56Gh78Ij90Kl")?; // 验证 16 字节 BASE58 解码
```

**唯一的生成入口**：`core::id::Id::new()`。不要在外部直接 `Uuid::new_v4()` 再手动编 BASE58。

---

## 7. Trait 实现指南

### 7.1 AgentScene — 定义 Agent 行为域

```rust
use core::interface::scene::{AgentScene, ScenePromptContext, TokenBudget};
use core::interface::prompt::PromptBlock;

struct MyCustomScene;

impl AgentScene for MyCustomScene {
    fn id(&self) -> &str { "my-scene" }
    fn name(&self) -> &str { "My Custom Scene" }
    fn description(&self) -> &str { "Custom domain-specific agent" }

    fn build_system_prompt(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock> {
        vec![PromptBlock::system(format!(
            "You are a helpful assistant. Today is {}. CWD: {}. OS: {}.",
            ctx.date, ctx.cwd, ctx.os
        ))]
    }

    fn tools(&self) -> Vec<String> { vec![] }
    fn token_budget(&self) -> TokenBudget {
        TokenBudget { compact_threshold: 150_000, compact_keep_recent: 20 }
    }

    // ── Session 自动命名（可选）──
    /// 是否在首轮后自动调用 LLM 生成 session 名称。
    fn auto_name_session(&self) -> bool { true }

    /// 返回命名 prompt。参数为首条用户消息。
    fn session_name_prompt(&self, first_message: &str) -> Option<String> {
        Some(format!("用 3-5 个词概括以下对话的主题，只输出标题：\n{first_message}"))
    }
}
```

**内置 Scene**：
| Scene | `auto_name_session` | 说明 |
|---|---|---|
| `scene::scene::coding::CodingScene` | `false` | 通用编程场景，无自动命名 |
| `scene::scene::chat::ChatScene` | `true` | 对话场景，首轮后用 Haiku 自动生成 3-5 词中文标题 |
| `scene::scene::demo::DemoScene` | `false` | 演示场景 |

### 7.2 Permission — 工具执行授权

```rust
#[async_trait]
pub trait Permission: Send + Sync {
    async fn check(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        cwd: &Path,
        session_id: &str,
    ) -> PermissionOutcome;
}
```

`PermissionOutcome` 三态：`Permit` / `Deny { reason }` / `Prompt { prompt_id, message, paths }`。

### 7.3 Model — 协议适配

内置 `AnthropicModel` 封装 Anthropic Messages API。实现 `Model` trait 即可支持其他后端。

---

## 8. Turn 处理流程

```
input_tx.send(InputMessage::User { content, turn_id })

       ▼
Agent::process_turn()
  │
  ├─ 1. 注入 CLAUDE.md（首次）
  ├─ 2. push user message 到 session
  ├─ 3. loop:
  │     a. 检查 cancel / max_api_calls
  │     b. 超过 compaction_threshold → compact
  │     c. 组装 prompt
  │     d. model.stream() → TextDelta / ToolUse 事件（均带 turn_id）
  │     e. 工具分发 → ToolResult 事件（带 turn_id）
  │     f. end_turn → break
  │     g. 错误恢复（overloaded → fallback, prompt-too-long → compaction）
  ├─ 4. emit TurnComplete（带 turn_id）
  ├─ 5. 遥测记录 turn_complete（带 turn_id）
  └─ 6. 返回 TurnOutcome

       ▼
event_rx.recv() → TextDelta / ToolUse / ToolResult → TurnComplete
```

---

## 9. 遥测与 VCR

### 9.1 事件遥测

`TelemetryEvent` 新增 `turn_id: Option<String>` 字段，所有 turn 作用域事件可携带：

```rust
// TurnComplete 遥测示例
telemetry::TelemetryEvent::turn_complete(
    session_id,
    turn_no,
    Some(turn_id),  // ← turn_id
    TurnCompletePayload {
        turn_no,
        turn_id: Some(turn_id.clone()),  // ← payload 中也带
        stop_reason: "end_turn".into(),
        api_calls: 3,
        // ...
    },
)
```

### 9.2 VCR 录制/回放

`VcrEntry` 新增 `turn_id: Option<String>`，支持按 turn 分组录制：

```rust
let mut vcr_model = VcrModel::new(inner_model, config, user_dir, local_dir);
vcr_model.set_turn_id(Some(turn_id.clone()));  // 设置当前 turn
// 之后的 stream() 调用会在 VcrEntry 中记录 turn_id
```

---

## 10. Session 持久化

启用 `session_dir` 配置后，Agent 通过 `HistoryStore`（JSONL）自动持久化对话历史。

```rust
Settings {
    session_dir: Some(local_dir.clone()), // 启用持久化
    // ...
}
```

- 每个 session 存储为 `<session_dir>/projects/<sanitized_cwd>/<session_id>.jsonl`
- `list_sessions()` 查询所有持久化 session
- `delete_session()` 删除指定 session 的 JSONL + metadata 文件

---

## 11. 与 Daemon 模式的对比

| 维度 | Daemon 模式 | 库模式 |
|---|---|---|
| **启动方式** | 独立进程，CLI 启动 | 嵌入应用，与宿主同进程 |
| **通信协议** | JSON-RPC 2.0 over Unix socket/TCP | Rust channel（mpsc） |
| **Session 管理** | SessionPool（自动管理多实例 + 驱逐） | 用户自己管理 Agent 实例 |
| **Session 创建** | `session.run_turn` 不传 session_id 自动创建 | `Builder::session_id(id).build()` |
| **Session 列表** | `session.list` RPC（活跃 + 历史） | `Agent::list_sessions()`（仅磁盘历史） |
| **Session 删除** | 暂不支持 RPC | `Agent::delete_session(id)` |
| **自动命名** | CHAT 场景首轮后自动调用 LLM | 用户自行调用 |
| **权限控制** | BypassPermissions | 自定义 `Permission` trait 实现 |
| **配置加载** | settings.json 分层加载 | 应用层自行组装 Settings |
| **并发模型** | 多连接、多 session | 应用自行管理 |
| **适合场景** | IDE 插件、多进程架构 | 桌面应用、定制 CLI、测试 |

---

## 12. 常见模式

### 12.1 CLI 风格交互式 Agent

```rust
let (mut agent, mut event_rx, input_tx) = build_agent()?;
let cancel = CancellationToken::new();
tokio::spawn(async move { agent.run(cancel.clone()).await });

let stdin = BufReader::new(tokio::io::stdin());
let mut lines = stdin.lines();
while let Ok(Some(line)) = lines.next_line().await {
    let turn_id = Id::new().to_string();
    let _ = input_tx.send(InputMessage::User {
        content: line, attachments: vec![], turn_id: turn_id.clone(),
    });
    while let Some(event) = event_rx.recv().await {
        match event {
            AgentEvent::TextDelta { text, .. } => print!("{text}"),
            AgentEvent::TurnComplete { .. } => { println!(); break; }
            _ => {}
        }
    }
}
```

### 12.2 带权限提示的 Agent

```rust
AgentEvent::PermissionPrompt { prompt_id, tool_name, message, .. } => {
    let user_approved = ask_user(&message);
    let _ = input_tx.send(InputMessage::PermissionResponse {
        prompt_id,
        decision: if user_approved {
            runtime::agent::PermissionDecision::Permit
        } else {
            runtime::agent::PermissionDecision::Deny { reason: "用户拒绝".into() }
        },
    });
}
```

### 12.3 自定义工具注册

```rust
use tools::legacy::{Tool, ToolContext, ToolResult, ToolResultContent};

struct MyTool;
#[async_trait::async_trait]
impl Tool for MyTool {
    fn name(&self) -> &str { "my_tool" }
    fn description(&self) -> &str { "我的自定义工具" }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"msg": {"type": "string"}}})
    }
    async fn call(&self, input: Value, _ctx: ToolContext, _progress: ProgressSender)
        -> Result<ToolResult, ToolError>
    {
        let msg = input["msg"].as_str().unwrap_or("");
        Ok(ToolResult {
            content: ToolResultContent::Text(format!("处理完成: {msg}")),
            is_error: false, structured_content: None, mcp_meta: None, new_messages: None,
        })
    }
}

tools.register(Arc::new(MyTool));
```

---

## 13. 参考

- [DAEMON_DEV_GUIDE.md](./DAEMON_DEV_GUIDE.md) — JSON-RPC daemon 模式开发指南
- [crates/core/src/interface/](../crates/core/src/interface/) — 全部 trait 定义
- [crates/core/src/id.rs](../crates/core/src/id.rs) — BASE58(UUID) ID 类型
- [crates/runtime/src/agent.rs](../crates/runtime/src/agent.rs) — Agent + Builder 实现
- [daemon/src/session_pool.rs](../daemon/src/session_pool.rs) — SessionPool 参考实现
- [daemon/src/main.rs](../daemon/src/main.rs) — 完整 daemon 示例（库模式最佳参考）
