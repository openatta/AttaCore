# AttaCore

> **AI Agent 编排引擎** — 一个 Rust workspace，为构建 AI 编程助手和智能代理运行时提供生产级基础设施。

[**English Version**](../README.md)

---

AttaCore **不是**面向终端用户的 AI 助手，而是**面向开发者的 Agent 引擎**——与驱动 Claude Code 同级别的基础设施。它提供行为对齐的工具系统、会话管理、权限控制、上下文压缩、多 Agent 协作、MCP 协议支持等能力。在此之上，你可以构建自己的 IDE 插件、桌面 GUI、CLI 工具或服务端 Agent 产品。

## 为什么选择 AttaCore

| 关注点 | 你得到什么 |
|---|---|
| **行为保真度** | 30+ 工具与 Claude Code TypeScript 参考实现行为对齐——每个函数、每个边界情况、每个压缩策略都有 `TS parity:` 源码追溯注释 |
| **上下文是硬骨头** | 多策略压缩（snip → micro-compact → collapse → LLM summarize）、反应式触发、熔断器、缓存感知编辑生成——这套系统让 200k+ token 的对话保持连贯 |
| **并发** | v2 流式工具执行器在模型仍在生成 token 时并行运行安全工具——GPU 流水线思维应用于 LLM 工具调用 |
| **安全** | 三级权限模型（允许/询问/禁止）、基于 glob 的规则引擎、Unicode 规范化路径安全、沙盒执行、LLM 辅助分类 |
| **多 Agent** | 一等公民的团队协作：Coordinator、Mailbox、共享记忆、远程 Agent 派生——像微服务一样组合 Agent |
| **可观测性** | 40+ 结构化遥测事件、OpenTelemetry 导出、VCR 录制/回放实现确定性测试、成本追踪 |
| **可嵌入** | 库模式（Rust API）或 Daemon 模式（JSON-RPC 2.0 over Unix Socket / TCP）——同一引擎，任选集成方式 |

## 架构

AttaCore 是一个 5 层、严格分层的 Rust workspace。依赖只能向上流动——每层建立在下层之上。无循环依赖，无捷径。

```
                          ┌──────────────────────────┐
                          │       你的应用             │
                          │  IDE · CLI · GUI · Server │
                          └──────────┬───────────────┘
                                     │
                          ┌──────────▼───────────────┐
                          │  L4  runtime             │
                          │  Agent 循环 · Builder    │
                          │  流式处理 · 并发调度      │
                          │  斜杠命令 (/help, …)      │
                          ├──────────────────────────┤
                          │  L3  tools · skills      │
                          │  scene · team · task     │
                          │  30+ 内置工具             │
                          │  Skill 系统 · MCP         │
                          ├──────────────────────────┤
                          │  L2  model · history     │
                          │  permissions · mcp       │
                          │  compaction · session    │
                          ├──────────────────────────┤
                          │  L1  core                │
                          │  trait 定义 · 类型 · ID  │
                          │  EngineConfig · Context  │
                          ├──────────────────────────┤
                          │  L0  auth · hooks        │
                          │  plugin · telemetry      │
                          └──────────────────────────┘
```

### 各层职责

**L0 — 横切服务**（零内部依赖）
`auth`（OAuth 2.0 PKCE 客户端）、`hooks`（生命周期回调——11 种事件类型，支持 command/prompt/HTTP/agent 钩子）、`plugin`（插件市场 + 依赖解析 + 版本缓存）、`telemetry`（40+ 结构化事件、OpenTelemetry 导出、VCR 录制/回放）。

**L1 — 基础层**（`core` / `base` crate）
全系统共享的类型和 trait：`Model`（LLM 后端抽象）、`AgentScene`（Agent 行为定义）、`Permission`（工具授权）、`Tool`（统一工具接口 v7）。以及 `Id`（BASE58 UUIDv4）、`EngineConfig`、`SessionState`、`FrozenContext`、`ToolContext`、消息/内容块类型。

**L2 — 基础设施**
`model` — Anthropic Messages API 适配器，支持流式、token 化、VCR 包装器、备用模型路由。`history` — JSONL 持久化，路径脱敏，transcript 分片。`permissions` — 基于 glob 的规则引擎，allow/deny/ask 匹配，路径安全（Unicode NFC/NFD 规范化），YOLO 模式，LLM 分类器。`mcp` — 全量 MCP 客户端：stdio / SSE / Streamable HTTP 三种传输，工具适配，OAuth bearer token。`compaction` — 多策略上下文压缩，反应式触发 + 熔断器。`session` — 内存会话状态与自动命名。

**L3 — 领域逻辑**
`tools` — 30+ 内置工具（Bash、Read、Write、Edit、Glob、Grep、LSP、WebFetch、WebSearch、CronCreate、TaskCreate、Skill、Agent、NotebookEdit、Monitor、PushNotification …）。`skills` — 文件系统 skill 解析器 + 加载器 + 文件监视器。`scene` — 内置场景：Coding、Chat、Demo。`team` — 多 Agent 协调：Coordinator、TeamTool、Mailbox、RemoteAgent。`task` — 后台任务生命周期：running、cron、store、delete。

**L4 — 运行时**
`agent` — 核心 Agent 结构体与 Builder 模式。`turn` — turn 循环（~2200 行），全部编排逻辑。`streaming` — v2 流式工具执行器（模型生成期间并行运行安全工具）。`dispatch` — `FuturesUnordered` + `Semaphore` 控制的并发工具分发，兄弟任务中止。`commands` — 斜杠命令路由（/help、/skills、/clear、/compact、/cost，以及自定义斜杠命令）。

## 核心能力

### 工具系统（30+ 工具，Claude Code 行为对齐）

| 类别 | 工具 |
|---|---|
| **文件操作** | `Read`、`Write`、`Edit`、`Glob`、`Grep` |
| **命令执行** | `Bash`（沙盒、路径安全、超时控制） |
| **Web** | `WebFetch`、`WebSearch` |
| **任务管理** | `TaskCreate`、`TaskList`、`TaskGet`、`TaskUpdate`、`TaskStop` |
| **计划模式** | `EnterPlanMode`、`ExitPlanMode` |
| **定时任务** | `CronCreate`、`CronDelete`、`CronList` |
| **编辑器** | `LSP`（9 种操作：跳转定义、查找引用、悬停信息、文档符号、工作区符号、跳转实现、调用层次、传入/传出调用）、`NotebookEdit` |
| **通知** | `PushNotification`、`Monitor`、`ScheduleWakeup` |
| **协作** | `Skill`（skill 调用）、`Agent`（子 Agent 派生） |
| **协议** | 全量 MCP 支持（stdio / SSE / Streamable HTTP） |

每个工具都实现统一的 `Tool` trait——一致的错误处理、权限门控和遥测埋点。

### 上下文压缩

LLM Agent 领域最难的问题，生产级解决方案：

```
预算告警 (80%) → 反应式触发 → 微压缩（缓存感知）
     ↓                              ↓
  熔断器 ← 全量压缩 ← LLM 摘要压缩（成本感知）
                        ↓
                  压缩后恢复
     （重新注入文件、skill、plan 状态、任务摘要）
```

- **微压缩（Micro-compact）**：清除过时的工具结果，同时保留提示缓存
- **折叠（Collapse）**：合并连续的用户/助手消息块
- **LLM 摘要（LLM Summarize）**：委托给更便宜的模型进行激进压缩
- **反应式触发（Reactive）**：根据 token 消耗速度预测预算耗尽，提前触发
- **熔断器（Circuit breaker）**：检测压缩死循环，回退到安全默认值
- **缓存感知编辑**：生成 `cache_edits` 避免 Anthropic 提示缓存失效

### 权限与安全

```
RuleSet { allow: [Glob], ask: [Glob], deny: [Glob] }
        ↓
路径安全（Unicode NFC/NFD 规范化、系统目录阻止列表）
        ↓
LLM 分类器（可选：将模糊情况委托给快速模型）
        ↓
YOLO 模式（CI/自动化场景自动批准）
```

三级决策：**Permit（允许）** / **AskUser（询问）** / **Deny（拒绝）**。规则按 glob 模式匹配，具备目录感知语义。路径安全对 Unicode 进行规范化以防止同形异义攻击，并阻止写入系统目录。

### 多 Agent 团队

像调用函数一样自然派生子 Agent：

```
Coordinator → [Agent A] [Agent B] [Agent C]
     ↕            ↕         ↕         ↕
  Mailbox  ←  消息传递  →  Mailbox  ←→  Mailbox
     ↕
  共享记忆（基于文件、wikilink 交叉引用）
```

- **Agent 派生**：`Agent` 工具支持类型选择、worktree 隔离、后台执行
- **Mailbox**：Agent 间类型化消息传递
- **共享记忆**：基于文件的持久化知识，YAML frontmatter、`[[wikilink]]` 交叉引用、陈旧度评分、基于 LLM 的提取和相关性选择
- **Coordinator**：任务分解与结果合成

### MCP 集成

全量 Model Context Protocol 支持，覆盖全部三种传输：

| 传输方式 | 状态 |
|---|---|
| **stdio** | 子进程生命周期管理，自动重启 |
| **SSE** | 长连接 HTTP 流 |
| **Streamable HTTP** | 无状态请求/响应 |

MCP 工具被适配为原生 `Tool` trait 并注入到系统提示词中。MCP 服务器也可以注册为 skill 供用户调用。支持 OAuth 2.0 bearer token 交换。

### 遥测与 VCR

40+ 结构化事件类型覆盖 Agent 生命周期的每个阶段：turn 开始/完成、工具执行、API 错误、权限决策、压缩操作、memory 快照、MCP 连接/断开、会话生命周期、启动时序、模型路由、钩子执行、斜杠命令使用。

**VCR 模式**：用 `VcrModel` 包装任意 `Model` 实现，将 LLM 交互录制到 JSONL 文件，然后确定性回放——集成测试零 API 成本，完美可复现。

## Crate 地图

| 层级 | Crate | 职责 | 关键导出 |
|---|---|---|---|
| L0 | `auth` | OAuth 2.0 PKCE 客户端 | `OAuth2Client`、`TokenStore`、`PkceVerifier` |
| L0 | `hooks` | 生命周期钩子运行器 | `HookRunner`、`HookConfig`、`HookEvent`（11 种） |
| L0 | `plugin` | 插件市场 + 解析 | `Plugin`、`PluginManifest`、`DependencyResolver` |
| L0 | `telemetry` | 遥测 + VCR | `TelemetryHandle`、`TelemetryEvent`（40+）、`VcrModel`、`FileRecorder` |
| L1 | `core` (base) | 共享类型、trait、ID | `Model`、`AgentScene`、`Permission`、`Tool`、`Id`、`EngineConfig`、`FrozenContext` |
| L2 | `model` | Anthropic API 适配器 | `AnthropicModel`、`AnthropicClient`、`ModelEvent`、`Usage` |
| L2 | `history` | JSONL 会话持久化 | `HistoryStore`、`TranscriptEntry` |
| L2 | `permissions` | 权限引擎 | `RuleSet`、`Gate`、`LLMClassifier`、`PathSafety` |
| L2 | `mcp` | MCP 协议客户端 | `McpManager`、`McpClient`、`ToolAdapter`、`OutputCache` |
| L2 | `compaction` | 上下文压缩 | `Compactor`、`DefaultCompactor`、反应式/缓存/时间驱动策略 |
| L2 | `session` | 内存会话状态 | `SessionManager`、`SessionSummary` |
| L3 | `tools` | 30+ 内置工具 | `BashTool`、`FileReadTool`、`FileWriteTool`、`LspTool`、`WebFetchTool` … |
| L3 | `skills` | Skill 加载器/管理器 | `SkillManager`、`SkillWatcher`、`McpBuilder` |
| L3 | `scene` | 内置 Agent 场景 | `CodingScene`、`ChatScene`、`DemoScene` |
| L3 | `team` | 多 Agent 协调 | `Coordinator`、`TeamTool`、`Mailbox`、`RemoteAgent` |
| L3 | `task` | 后台任务生命周期 | `TaskManager`、`TaskStore`、`RunningTask`、`CronTask` |
| L4 | `runtime` | Agent 运行时 + turn 循环 | `Agent`、`Builder`、`TurnOutcome`、`StreamResult`、`CommandRegistry` |
| — | `daemon` | JSON-RPC 2.0 服务器 | `DaemonServer`、`SessionPool`（LRU + 空闲驱逐） |
| — | `test-runner` | .test 场景运行器 | API runner、CLI runner、LLM comparator、reporter |

## 快速开始

### 前置条件

- **Rust** 1.80+
- **Anthropic API Key**（或兼容的 API 端点）

### 构建与测试

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
# 前置：在仓库根目录放置 .deepseek 文件（包含 API key）
# API 模式（直接构造 Agent）
./tests/run_api.sh 000.c_project

# CLI 模式（启动 daemon → JSON-RPC）
./tests/run_cli.sh 000.c_project
```

## 使用方式

### Daemon 模式（JSON-RPC 2.0）

适合 IDE 插件、多进程架构、远程客户端。引擎作为独立进程运行，通过 Unix Domain Socket 或 TCP 通信。

```sh
# 启动 daemon
export ANTHROPIC_API_KEY=sk-...
cargo run -p daemon --release

# 通过 socat 发送 turn
echo '{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"用 Rust 写一个 TCP echo 服务"},"id":1}' \
  | socat - UNIX-CONNECT:$HOME/.atta/code/daemon.sock
```

Daemon 特性：
- **会话池**：可配置容量上限、LRU 驱逐、空闲超时
- **服务发现**：通过 PID lock file + Unix socket，客户端自动发现
- **优雅关闭**：等待进行中的 turn 完成后退出
- **TCP 模式**：基于 token 的认证，支持远程访问

### 库模式（嵌入式 Rust API）

适合桌面应用、自定义 CLI、服务端 Agent。直接控制引擎的每个方面。

```rust
use runtime::agent::Builder;
use scene::scene::coding::CodingScene;
use model::adapter::AnthropicModel;

// 一个 Agent = 一个 session
let (mut agent, event_rx, input_tx) = Builder::new()
    .scene(Arc::new(CodingScene))
    .model(model)
    .settings(settings)
    .session_id(session_id)
    .build()?;

// 后台运行事件循环
tokio::spawn(async move { agent.run(cancel).await });

// 发送消息，接收流式事件
input_tx.send(InputMessage::User {
    content: "写一个 TCP echo 服务".into(),
    attachments: vec![],
    turn_id,
})?;

while let Some(event) = event_rx.recv().await {
    match event {
        AgentEvent::TextDelta { text, .. } => print!("{text}"),
        AgentEvent::TurnComplete { .. } => break,
        _ => {}
    }
}
```

`Builder` 在编译期强制要求必需字段（`scene`、`model`、`settings`），其余使用合理默认值——`AllowAll` 权限、内存工具注册表、默认压缩器、空钩子。

### 自定义行为

通过 trait 注入你自己的实现：

```rust
Builder::new()
    .scene(my_scene)              // impl AgentScene — 控制系统提示词和行为
    .model(my_model)              // impl Model — 任意 LLM 后端
    .permission(my_permission)    // impl Permission — 你的授权逻辑
    .tool_registry(my_tools)      // impl ToolRegistry — 自定义工具集
    .hook_runner(my_hooks)        // impl HookRunner — 生命周期回调
    .compactor(my_compactor)      // impl Compactor — 自定义压缩策略
    .build()?;
```

## 配置

### Settings 层级（优先级从低到高）

1. 内置默认值
2. `$HOME/.atta/code/settings.json`（或 `.toml`）
3. `<project>/.atta/code/settings.json`（或 `.toml`）
4. CLI 参数

```json
{
  "model": "claude-sonnet-4-6",
  "max_tokens": 4096,
  "permission": {
    "mode": "default",
    "default_mode": "require_user_permission",
    "yolo": false
  },
  "mcp_servers": {}
}
```

### 环境变量

| 变量 | 用途 |
|---|---|
| `ANTHROPIC_API_KEY` | **必需。**模型提供商的 API 密钥 |
| `ANTHROPIC_BASE_URL` | 自定义 API 端点（代理、兼容提供商） |
| `ATTACORE_DAEMON_TOKEN` | TCP 模式认证令牌 |
| `ATTA_CONFIG_HOME` | 配置根目录（默认：`$HOME/.atta/code`） |
| `ATTA_VCR_RECORD` | 录制模式：`ATTA_VCR_RECORD=<场景名>` |
| `ATTA_VCR_REPLAY` | 回放模式：`ATTA_VCR_REPLAY=<场景名>` |

## ID 体系

所有外部可见的标识符均为 **BASE58(UUID v4)**——22 字符，URL-safe：

```
Ab12Cd34Ef56Gh78Ij90Kl   ← session_id / turn_id / agent_id / tool_call_id
```

唯一生成入口：`core::id::Id::new()`。禁止在此外部直接生成 UUID 并自行 BASE58 编码。`Id` 类型是 `[u8; 16]` 上的 `#[sqlx(transparent)]` newtype，在 Postgres 和 SQLite 中均映射为 `TEXT`。

```rust
use base::id::Id;

let id = Id::new();            // 随机分配——唯一的生成路径
let id = Id::parse(s)?;        // 从外部输入验证解码（检查 16 字节长度）
```

## 设计原则

1. **库优先。**每个能力通过 Rust crate 暴露。Daemon 是参考应用，不是产品本身。
2. **Trait 注入。**`Model`、`Permission`、`AgentScene`——核心行为是你来实现的 trait。引擎不拥有任何策略。
3. **工具对齐。**30+ 工具，行为与 Claude Code TypeScript 实现逐一验证。系统中保留 `TS parity:` 注释。
4. **默认安全。**三级权限模型、Unicode 规范化路径安全、沙盒执行——你选择降低安全，而非提升安全。
5. **处处可观测。**40+ 结构化遥测事件。VCR 确定性回放。成本追踪。OpenTelemetry 导出。

## 项目结构

```
AttaCore/
├── crates/           # 18 个 Rust crate（引擎本体）
├── daemon/           # JSON-RPC 2.0 daemon（参考应用）
├── tests/            # 集成测试 + 测试运行器 + fixtures
├── docs/             # 文档
├── 3rds/             # 第三方依赖 / vendored 代码
├── Cargo.toml        # Workspace 根（22 个成员）
└── README.md         # 英文版本
```

## 文档

| 文档 | 受众 |
|---|---|
| [README.md](../README.md) | 英文版本 — 项目概览、架构、快速开始 |
| [README.zh.md](README.zh.md) | **你在这里** — 中文版，面向中文开发者 |
| [DEV_GUIDE.md](DEV_GUIDE.md) | Daemon 与库模式的完整 API 参考 |
| [CLAUDE.md](../CLAUDE.md) | Agent 指令 — 代码规范、设计规则 |

## 许可

Apache-2.0
