# AttaCore 开发者指南

AttaCore 支持两种接入模式：**Daemon 模式**（JSON-RPC 2.0 独立进程）和**库模式**（嵌入式 Rust API）。两种模式共享同一套核心引擎（crates），仅在通信层和 session 管理方式上不同。

---

## 模式对比

| 维度 | Daemon 模式 | 库模式 |
|---|---|---|
| 启动 | 独立进程，CLI 启动 | 嵌入应用，同进程 |
| 通信 | JSON-RPC 2.0 over Unix Socket / TCP | Rust mpsc channel |
| Session 管理 | SessionPool 自动管理多实例 | 用户自行管理 Agent 实例 |
| 适合场景 | IDE 插件、多进程架构 | 桌面应用、定制 CLI、测试 |

---

## Daemon 模式

### 启动

```sh
export ANTHROPIC_API_KEY=sk-...
cargo run -p daemon --release

# 自定义参数
attacored --session-cap 64 --session-idle-timeout 7200     # 调整 pool
attacored --listen 127.0.0.1:7878 --token my-secret         # TCP 模式
```

### 服务发现

Daemon 启动时写入 `$HOME/.atta/code/daemon.lock`（权限 `0600`）：

```json
{"pid": 12345, "socket_path": "/home/user/.atta/code/daemon.sock", "version": "0.1.0", "protocol_version": "1"}
```

### RPC 方法

| 方法 | 说明 |
|---|---|
| `daemon.status` | 查询运行状态（version, uptime, session 数） |
| `daemon.shutdown` | 优雅关闭 |
| `session.list` | 列出所有 session（active + inactive） |
| `session.run_turn` | 执行一轮对话（唯一产生流式事件的方法） |

### session.run_turn

```json
// 请求 — 不传 session_id 则自动创建新 session
{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"写一个hello world"},"id":1}

// 响应（流结束后）
{"jsonrpc":"2.0","id":1,"result":{"session_id":"Ab12...","turn_id":"Cd34...","name":null,"api_calls":2}}

// 继续对话
{"jsonrpc":"2.0","method":"session.run_turn","params":{"session_id":"Ab12...","message":"加上错误处理"},"id":2}
```

### 流式事件

执行期间持续推送 `session.event` 通知：

| kind | 说明 |
|---|---|
| `text_delta` | 模型文本增量 |
| `tool_use` | 模型请求工具调用 |
| `tool_result` | 工具执行完成 |
| `turn_complete` | Turn 结束（流终止标志） |

### 快速测试

```sh
# 列出 session
echo '{"jsonrpc":"2.0","method":"session.list","id":1}' | socat - UNIX-CONNECT:$HOME/.atta/code/daemon.sock

# 对话
echo '{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"介绍你自己"},"id":2}' | socat - UNIX-CONNECT:$HOME/.atta/code/daemon.sock
```

---

## 库模式

### 最小可运行 Agent

```rust
use core::interface::settings::Settings;
use model::adapter::AnthropicModel;
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};
use runtime::agent::{Builder, InputMessage, AgentEvent};
use scene::scene::coding::CodingScene;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Settings
    let settings = Arc::new(Settings::default());
    // 2. Model
    let api_key = std::env::var("ANTHROPIC_API_KEY")?;
    let client: Arc<dyn AnthropicClient> = Arc::new(HttpAnthropicClient::new(AuthMode::ApiKey(api_key))?);
    let model = Arc::new(AnthropicModel::new(client));
    // 3. Scene
    let scene: Arc<dyn core::interface::scene::AgentScene> = Arc::new(CodingScene);
    // 4. Build
    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(scene).model(model).settings(settings)
        .build()?;

    // 5. Run
    let cancel = tokio_util::sync::CancellationToken::new();
    tokio::spawn(async move { agent.run(cancel.clone()).await });

    // 6. Send message
    let turn_id = core::id::Id::new().to_string();
    input_tx.send(InputMessage::User { content: "写一个 hello world".into(), attachments: vec![], turn_id })?;

    // 7. Receive events
    while let Some(event) = event_rx.recv().await {
        match event {
            AgentEvent::TextDelta { text, .. } => print!("{text}"),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    Ok(())
}
```

### Builder 选项

```rust
Builder::new()
    .scene(scene)            // 必选: Arc<dyn AgentScene>
    .model(model)            // 必选: Arc<dyn Model>
    .settings(settings)      // 必选: Arc<Settings>
    .permission(perm)        // 可选: Arc<dyn Permission>（默认全部允许）
    .memory_store(mem)       // 可选: Arc<MemoryStore>
    .tools(tools)            // 可选: Arc<InMemoryToolRegistry>
    .session_id(id)          // 可选: 指定 session ID，不传则自动生成
    .mcp_servers(names)      // 可选: MCP 服务名列表
    .build()?;               // → (Agent, EventReceiver, InputSender)
```

### Agent 方法

```rust
agent.run(cancel)             // 启动事件循环（后台）
agent.session_info()          // → SessionSummary
agent.list_sessions()         // → Vec<SessionSummary>
agent.delete_session(id)      // → Result<()>
agent.settings()              // → &Settings
agent.tools()                 // → &InMemoryToolRegistry
agent.permission()            // → &dyn Permission
agent.memory()               // → &MemoryStore
```

### Event 类型

```rust
pub enum AgentEvent {
    TextDelta    { text, turn_id },           // 流式输出
    ToolUse      { id, name, input, turn_id }, // 工具调用
    ToolResult   { id, name, content, turn_id }, // 工具结果
    TurnComplete { stop_reason, api_calls, .. }, // Turn 结束
    Error        { code, message, .. },         // 错误
    // 更多：PermissionPrompt, AgentSpawned, CompactAction...
}
```

### 多 Session 管理

```rust
struct MyApp {
    sessions: HashMap<String, (InputSender, EventReceiver, CancellationToken)>,
}

impl MyApp {
    async fn create_session(&mut self) -> anyhow::Result<String> {
        let sid = Id::new().to_string();
        let (agent, event_rx, input_tx) = Builder::new()
            .scene(self.scene.clone()).model(self.model.clone())
            .settings(self.settings.clone()).session_id(sid.clone())
            .build()?;
        let cancel = CancellationToken::new();
        tokio::spawn(async move { agent.run(cancel.clone()).await });
        self.sessions.insert(sid.clone(), (input_tx, event_rx, cancel));
        Ok(sid)
    }
}
```

### 自定义 Scene

```rust
use core::interface::scene::{AgentScene, ScenePromptContext};
use core::interface::prompt::PromptBlock;

struct MyCustomScene;
impl AgentScene for MyCustomScene {
    fn id(&self) -> &str { "my-scene" }
    fn name(&self) -> &str { "My Scene" }
    fn build_system_prompt(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock> {
        vec![PromptBlock::system(format!("You are a helpful assistant. OS: {}.", ctx.os))]
    }
    fn tools(&self) -> Vec<String> { vec!["Bash".into(), "Read".into()] }
    fn auto_name_session(&self) -> bool { true }
}
```

内置 Scene：`CodingScene`（编程）、`ChatScene`（对话，自动命名）、`DemoScene`（演示）。

### 自定义工具

```rust
#[async_trait::async_trait]
impl Tool for MyTool {
    fn name(&self) -> &str { "my_tool" }
    fn description(&self) -> &str { "我的工具" }
    fn input_schema(&self) -> Value { json!({"type":"object","properties":{"msg":{"type":"string"}}}) }
    async fn call(&self, input: Value, ctx: ToolContext, ..) -> Result<ToolResult, ToolError> {
        Ok(ToolResult { content: ToolResultContent::Text("done".into()), is_error: false, .. })
    }
}
```

---

## ID 体系

所有外部 ID 为 **BASE58(UUID v4)**，22 字符：

```rust
use core::id::Id;
let id = Id::new().to_string();       // "Ab12Cd34Ef56Gh78Ij90Kl"
let parsed = Id::parse("Ab12...")?;   // 验证 16 字节
```

## Session 生命周期

```
session.run_turn {message} (无 session_id)
  → Id::new() 生成 session_id → 创建 Agent → 执行首轮 → 返回
session.run_turn {session_id, message} × N  (上下文累积)
  → 空闲 > idle_timeout → 回收（内存释放，磁盘保留）
session.run_turn {session_id, message}  (后续使用)
  → 不在内存 → 从 HistoryStore 恢复 → 重建 Agent
```

## 配置

分层配置（优先级从低到高）：内置默认 → 用户级 settings.json → 项目级 settings.json → CLI 参数。

```json
{
  "model": "claude-sonnet-4-6",
  "max_tokens": 4096,
  "mcp_servers": {}
}
```

## 环境变量

| 变量 | 说明 |
|---|---|
| `ANTHROPIC_API_KEY` | Anthropic API 密钥 |
| `ANTHROPIC_BASE_URL` | 自定义 API 端点 |
| `ATTACORE_DAEMON_TOKEN` | TCP 模式认证令牌 |

## 参考

- [README.md](../README.md) — 项目概览
- [Cargo.toml](../Cargo.toml) — 完整 workspace 依赖与 crate 关系
- [crates/core/src/interface/](../crates/core/src/interface/) — 全部 trait 定义
- [crates/core/src/id.rs](../crates/core/src/id.rs) — BASE58 UUID ID 类型
- [crates/runtime/src/agent.rs](../crates/runtime/src/agent.rs) — Agent + Builder 实现
