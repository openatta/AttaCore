# AttaCore

AI agent 编排引擎 — 一个 Rust workspace，提供构建 AI 编程助手和智能代理运行时的全套基础设施。

## 设计目标

AttaCore 不是一个面向终端用户的 AI 助手，而是**面向开发者的 agent 引擎**。它提供了与 Claude Code 行为对齐的工具系统、会话管理、权限控制、上下文压缩等能力，让上层应用（IDE 插件、桌面 GUI、CLI 工具、服务端）可以快速构建自己的 AI agent 产品。

核心原则：

- **库优先**：所有能力通过 Rust crate 暴露，daemon 仅是参考应用
- **trait 注入**：Model、Permission、Scene 等核心行为通过 trait 由应用层控制
- **工具对齐**：内置 30+ 工具，行为与 Claude Code TS 参考实现对齐
- **安全可控**：三级权限模型（允许/询问/禁止），沙盒执行，路径安全校验

## 架构概览

```
┌──────────────────────────────────────────────────────┐
│  你的应用（IDE 插件 / CLI / 桌面 GUI / 服务端）       │
│                                                      │
│  Daemon 模式：JSON-RPC 2.0 over Unix Socket / TCP     │
│  库模式：    Rust API，直接构造 Agent 对象              │
└──────────┬───────────────────────────────────────────┘
           │
┌──────────▼───────────────────────────────────────────┐
│  L4  runtime — Agent turn loop, Builder, streaming    │
├──────────────────────────────────────────────────────┤
│  L3  tools / skills / scene / task / team             │
│      30+内置工具 · skill 加载 · 多场景 · 多agent协作   │
├──────────────────────────────────────────────────────┤
│  L2  model / history / permissions / mcp / compaction │
│      模型适配 · 会话持久化 · 权限校验 · MCP协议       │
├──────────────────────────────────────────────────────┤
│  L1  core — trait 定义 · 基础类型 · ID 体系            │
├──────────────────────────────────────────────────────┤
│  L0  auth / hooks / plugin / telemetry                │
└──────────────────────────────────────────────────────┘
```

## 功能

### 工具系统（30+ 内置工具）

| 类别 | 工具 |
|---|---|
| 文件操作 | Read, Write, Edit, Glob, Grep |
| 命令执行 | Bash（含安全校验、沙盒） |
| Web | WebFetch, WebSearch |
| 任务管理 | TaskCreate, TaskList, TaskGet, TaskUpdate, TaskStop |
| 计划模式 | EnterPlanMode, ExitPlanMode |
| 定时任务 | CronCreate, CronDelete, CronList |
| 编辑器 | LSP（go-to-def, find-refs, hover）, NotebookEdit |
| 通知 | PushNotification, Monitor, ScheduleWakeup |
| 协作 | Skill（skill 调用）, Agent（子 agent 派生） |
| MCP | 全量 MCP 协议支持（stdio / SSE / Streamable HTTP） |

### 会话管理

- 多 session 并发，独立上下文隔离
- JSONL 持久化，支持跨进程恢复
- 上下文自动压缩（compaction），避免 token 溢出
- Session 自动命名（Chat 场景）

### 权限系统

- 三态权限：Permit / Deny / AskUser
- 路径安全校验（禁止操作关键系统目录）
- Yolo 模式（自动批准）
- LLM 辅助分类

### 遥测与调试

- OpenTelemetry 集成
- VCR 录制/回放（集成测试零成本回归）
- 性能指标采集
- 成本追踪

### 团队协作

- 子 Agent 派生与管理
- Agent 间消息传递（mailbox）
- 共享团队记忆

## 使用方式

AttaCore 提供两种接入模式：

### Daemon 模式（JSON-RPC 2.0）

适合 IDE 插件、多进程架构。启动独立进程，通过 Unix Socket 或 TCP 通信。

```sh
# 启动 daemon
export ANTHROPIC_API_KEY=sk-...
cargo run -p daemon --release

# 通过 socat 交互
echo '{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"写一个hello world"},"id":1}' \
  | socat - UNIX-CONNECT:$HOME/.atta/code/daemon.sock
```

### 库模式（嵌入式 Rust API）

适合桌面应用、定制 CLI、服务端。直接操作 Agent 对象，完全控制。

```rust
use runtime::agent::Builder;
use scene::scene::coding::CodingScene;
use model::adapter::AnthropicModel;

// 构建 Agent，一个实例 = 一个 session
let (mut agent, event_rx, input_tx) = Builder::new()
    .scene(Arc::new(CodingScene))
    .model(model)
    .settings(settings)
    .session_id(session_id)
    .build()?;

// 后台运行事件循环
tokio::spawn(async move { agent.run(cancel).await });

// 发送消息，接收流式事件
input_tx.send(InputMessage::User { content: "...", attachments: vec![], turn_id })?;
while let Some(event) = event_rx.recv().await {
    match event {
        AgentEvent::TextDelta { text, .. } => print!("{text}"),
        AgentEvent::TurnComplete { .. } => break,
        _ => {}
    }
}
```

详细 API 参考见 [DEV_GUIDE.md](docs/DEV_GUIDE.md)。

## Crate 地图

| 层级 | Crate | 用途 |
|---|---|---|
| L0 | auth, hooks, plugin, telemetry | 零依赖叶节点 |
| L1 | core | 基础 trait、类型、ID 体系 |
| L2 | model, history, permissions, mcp, compaction, session | 单依赖 core |
| L3 | tools, skills, scene, task, team | 多依赖组合 |
| L4 | runtime | Agent + turn loop，串联全部组件 |

## 快速开始

### 前置

- Rust 1.80+
- Anthropic API Key（或兼容的 API 端点）

### 构建 & 测试

```sh
# 全量构建
cargo build --workspace

# 运行全部测试
cargo test --workspace

# 单 crate 测试
cargo test -p tools

# Daemon 测试
cargo test -p daemon
```

### 运行 Daemon

```sh
export ANTHROPIC_API_KEY=sk-...
cargo run -p daemon
# 监听 $HOME/.atta/code/daemon.sock
# 写入 discovery lock file → 客户端自动发现
```

### 运行集成测试

```sh
# 前置：在仓库根目录配置 .deepseek（API key）
# API 模式（直接构造 Agent）
./tests/run_api.sh 000.c_project

# CLI 模式（启动 daemon → JSON-RPC）
./tests/run_cli.sh 000.c_project
```

## 配置

### settings.json

Daemon 模式支持分层配置（优先级从低到高）：

1. 内置默认值
2. `$HOME/.atta/code/settings.json`
3. `<project>/.atta/code/settings.json`
4. CLI 参数

```json
{
  "model": "claude-sonnet-4-6",
  "max_tokens": 4096,
  "mcp_servers": {}
}
```

### 环境变量

| 变量 | 说明 |
|---|---|
| `ANTHROPIC_API_KEY` | 必需。API 密钥 |
| `ANTHROPIC_BASE_URL` | 可选。自定义 API 端点 |
| `ATTACORE_DAEMON_TOKEN` | TCP 模式认证令牌 |
| `ATTA_CONFIG_HOME` | 配置根目录（默认 `$HOME/.atta/code`） |

## ID 体系

所有外部可见的 ID 均为 **BASE58(UUID v4)**，22 字符，URL-safe：

```
Ab12Cd34Ef56Gh78Ij90Kl  ← session_id / turn_id / agent_id
```

唯一生成入口：`core::id::Id::new()`。不允许在外部直接生成 UUID 并自行编码。

## 文档

- [DEV_GUIDE.md](docs/DEV_GUIDE.md) — Daemon 与库模式完整 API 参考
- [Cargo.toml](Cargo.toml) — Workspace 依赖与 crate 关系

## 许可

Apache-2.0
