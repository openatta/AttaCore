# AttaCore Daemon 开发者指南 — JSON-RPC 2.0 模式

> 面向希望通过独立进程（daemon）接入 AttaCore agent 引擎的开发者。
> 适用场景：IDE 插件、CLI 工具、桌面应用通过 Unix Socket / TCP 与 agent 进程通信。

---

## 1. 逻辑架构

```
┌──────────────────────────┐
│   Client                 │  IDE 插件 / CLI / 桌面 GUI
│   (JSON-RPC 2.0 client)  │
└──────────┬───────────────┘
           │ Unix Socket (默认) 或 TCP (token 认证)
           │ newline-delimited JSON
           ▼
┌──────────────────────────┐
│   DaemonServer           │  连接管理 + 请求路由
├──────────────────────────┤
│   SessionPool            │  多 session 实例管理
│   ┌────────────────────┐ │
│   │ sessions: HashMap  │ │  每 session = 独立 Agent 实例
│   │  sid → LiveSession │ │  (独立 event channel + 后台 run loop)
│   │   ├─ input_tx      │ │
│   │   ├─ event_rx      │ │  LRU 驱逐 + 空闲超时回收
│   │   ├─ name          │ │
│   │   └─ Agent         │ │
│   └────────────────────┘ │
└──────────────────────────┘
```

- **DaemonServer**：接受连接，newline-delimited JSON 行协议，解析 `RpcRequest`，路由到对应 handler。
- **SessionPool**：管理多个 Agent 实例。每个 session 拥有独立的 Agent、event channel 和生命周期。
- **Agent**：核心引擎，持有 scene、model、tools、permission、memory_store 等组件。

---

## 2. 启动 Daemon

### 2.1 编译

```sh
cd AttaCore
cargo build -p daemon --release
```

二进制位于 `target/release/attacored`。

### 2.2 命令行参数

| 参数 | 默认值 | 说明 |
|---|---|---|
| `--socket <PATH>` | `$HOME/.atta/code/daemon.sock` | Unix socket 路径 |
| `--session-cap <N>` | `32` | 最大并发活跃 session 数 |
| `--session-idle-timeout <N>` | `3600` | Session 空闲超时秒数（超时自动回收） |
| `--model <NAME>` | `claude-sonnet-4-6` | 默认模型名 |
| `--max-tokens <N>` | `2000` | 每 turn 最大输出 token |
| `--listen <ADDR>` | 无（仅 Unix socket） | 绑定 TCP 地址，如 `127.0.0.1:7878` |
| `--token <SECRET>` | 无 | TCP 模式认证令牌（也可用环境变量） |

### 2.3 环境变量

| 变量 | 说明 |
|---|---|
| `ANTHROPIC_API_KEY` | **必需**。Anthropic API 密钥 |
| `ANTHROPIC_BASE_URL` | 可选。自定义 API 端点 |
| `ATTACORE_DAEMON_TOKEN` | TCP 模式认证令牌（`--token` 的备选） |
| `ATTA_CONFIG_HOME` | 配置根目录（默认 `$HOME/.atta/code`） |

### 2.4 启动示例

```sh
# 默认 Unix socket
export ANTHROPIC_API_KEY=sk-...
attacored

# 自定义 session 上限 + 空闲超时
attacored --session-cap 64 --session-idle-timeout 7200

# TCP 模式（远程接入）
attacored --listen 127.0.0.1:7878 --token my-secret-token
```

---

## 3. 服务发现

Daemon 启动时在配置目录写入 **discovery lock file**：`$HOME/.atta/code/daemon.lock`（权限 `0600`）。

```json
{
  "pid": 12345,
  "socket_path": "/home/user/.atta/code/daemon.sock",
  "version": "0.1.0",
  "started_at": 1718000000,
  "protocol_version": "1"
}
```

客户端通过以下流程发现 daemon：
1. 读取 `$HOME/.atta/code/daemon.lock`
2. 解析 `socket_path`
3. 连接到该 Unix socket

---

## 4. JSON-RPC 2.0 协议

### 4.1 传输格式

- 每行一个完整的 JSON 对象（newline-delimited JSON）。
- 请求：`RpcRequest`，一行。
- 响应：`RpcResponse`，一行。
- 流事件：`StreamFrame`，每事件一行（仅在 `session.run_turn` 期间发送）。

### 4.2 RpcRequest

```json
{
  "jsonrpc": "2.0",
  "method": "<方法名>",
  "params": { ... },
  "id": <任意数字或字符串>
}
```

- `id` 为 `null` 或不存在的请求视为 **通知（notification）**，不返回响应。

### 4.3 RpcResponse（正常）

```json
{
  "jsonrpc": "2.0",
  "id": "<请求的 id>",
  "result": { ... }
}
```

### 4.4 RpcResponse（错误）

```json
{
  "jsonrpc": "2.0",
  "id": "<请求的 id>",
  "error": { "code": -32000, "message": "session not found: ..." }
}
```

---

## 5. RPC 方法参考

### 5.1 `daemon.status`

查询 daemon 运行状态。

**Params**：无。

**Result**：
```json
{
  "version": "0.1.0",
  "uptime_secs": 3600,
  "sessions": 7
}
```

- `sessions`：当前活跃 session 数量（SessionPool 中存活的实例数）。

### 5.2 `daemon.shutdown`

优雅关闭 daemon。回收所有 session，清理 lock file，退出进程。

**Params**：无。

**Result**：
```json
{ "shutting_down": true }
```

### 5.3 `session.list`

列出所有 session（活跃 + 仅历史记录），合并去重。每个 session 带有 `status` 区分是否有活着的 Agent 实例。

**Params**：无。

**Result**：
```json
{
  "sessions": [
    {
      "session_id": "Ab12Cd34Ef56Gh78Ij90Kl",
      "name": "讨论 Rust 并发模型",
      "preview": null,
      "message_count": 0,
      "created_at": "2026-06-13T10:30:00Z",
      "last_active": "2026-06-13T10:35:00Z",
      "status": "active"
    },
    {
      "session_id": "Mn12Op34Qr56St78Uv90Wx",
      "name": null,
      "preview": null,
      "message_count": 0,
      "created_at": "",
      "last_active": "",
      "status": "inactive"
    }
  ]
}
```

| `status` | 含义 |
|---|---|
| `"active"` | 有活着的 Agent 实例在内存中运行 |
| `"inactive"` | 仅有磁盘历史数据，无活跃实例 |

> **不变量：同一个 `session_id` 的活跃实例最多存在一个。**

### 5.4 `session.run_turn`

在指定 session 中执行一次 turn。**这是唯一会产生流式事件的方法。**

**Params**：
```json
{
  "session_id": "Ab12...",   // 可选！不传 = 自动创建新 session
  "turn_id": "...",          // 可选！BASE58(UUID)，不传 = 自动生成
  "message": "帮我..."
}
```

| 参数 | 必需 | 说明 |
|---|---|---|
| `message` | ✅ | 用户消息文本 |
| `session_id` | ❌ | 不传则自动新建 session（默认 CODING 场景）。传入已存在的 session_id 可恢复对话 |
| `turn_id` | ❌ | BASE58(UUID) 格式 22 字符。不传则 daemon 自动生成 `Id::new()` |

**响应**（流结束后）：
```json
{
  "session_id": "Ab12...",
  "turn_id": "Cd34...",
  "name": "讨论 Rust 并发模型",
  "api_calls": 3
}
```

| 字段 | 说明 |
|---|---|
| `session_id` | 本 turn 所属 session 的 BASE58(UUID) |
| `turn_id` | 本 turn 的 BASE58(UUID) |
| `name` | Session 名称。新 session 首轮完成后，CHAT 场景会额外调用 LLM 自动生成；CODING 场景为 `null` |
| `api_calls` | 本 turn 中调用了多少次 LLM API |

**路由逻辑**：
```
session_id 存在?
  ├─ 是 → 在 SessionPool 中查找
  │        ├─ active   → 直接用已有 Agent 实例
  │        └─ 不在内存 → 尝试从 HistoryStore 恢复 → 创建新 Agent
  │                      （无法恢复时创建全新 session）
  └─ 否 → 创建新 session
           1. Id::new() 生成 session_id
           2. Builder 创建 Agent 实例
           3. 注入 SessionPool
           4. 执行首轮
           5. Scene.auto_name_session()?
              ├─ false (CODING) → name = null
              └─ true  (CHAT)   → 额外 LLM 调用 → name = 3-5 词中文标题
           6. 返回 {session_id, turn_id, name, api_calls}
```

---

## 6. 流式事件（StreamFrame）

在 `session.run_turn` 执行期间，daemon 会持续推送 `StreamFrame` 事件。每个事件都携带 `session_id` 和 `turn_id`：

```json
{
  "jsonrpc": "2.0",
  "method": "session.event",
  "params": {
    "session_id": "Ab12...",
    "turn_id": "Cd34...",
    "event": { "kind": "...", ... }
  }
}
```

### 6.1 事件类型

#### `text_delta` — 模型文本增量

```json
{
  "kind": "text_delta",
  "text": "这是流式输出的片段"
}
```

#### `tool_use` — 模型请求工具调用

```json
{
  "kind": "tool_use",
  "id": "toolu_xxx",
  "name": "Bash",
  "input": { "command": "ls -la" }
}
```

#### `tool_result` — 工具执行完成

```json
{
  "kind": "tool_result",
  "id": "toolu_xxx",
  "name": "Bash",
  "content": "total 24\ndrwxr-xr-x ...",
  "is_error": false
}
```

#### `turn_complete` — Turn 结束（流终止标志）

```json
{
  "kind": "turn_complete",
  "stop_reason": "end_turn",
  "api_calls": 3,
  "usage": {
    "input_tokens": 15000,
    "output_tokens": 800
  }
}
```

> 收到 `turn_complete` 后，客户端应停止读取流并等待最终的 `RpcResponse`。

---

## 7. 错误码

| 错误码 | 名称 | 说明 |
|---|---|---|
| `-32700` | `PARSE_ERROR` | JSON 解析失败 |
| `-32600` | `INVALID_REQUEST` | 请求格式无效 |
| `-32601` | `METHOD_NOT_FOUND` | 未知方法名 |
| `-32602` | `INVALID_PARAMS` | 参数缺失或类型错误 |
| `-32603` | `INTERNAL_ERROR` | 内部错误 |
| `-32000` | `SESSION_NOT_FOUND` | `session_id` 指定的 session 不存在且无法恢复 |
| `-32002` | `ENGINE_ERROR` | Agent 引擎执行错误 |

---

## 8. 完整交互示例

### 8.1 用 `socat` 快速测试

```sh
# Terminal 1: 启动 daemon
export ANTHROPIC_API_KEY=sk-...
cargo run -p daemon

# Terminal 2: 列出 session
echo '{"jsonrpc":"2.0","method":"session.list","id":1}' \
  | socat - UNIX-CONNECT:$HOME/.atta/code/daemon.sock

# Terminal 2: 不指定 session_id → 自动创建新 session
echo '{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"用Rust写一个hello world"},"id":2}' \
  | socat - UNIX-CONNECT:$HOME/.atta/code/daemon.sock

# Terminal 2: 指定 session_id 继续对话
echo '{"jsonrpc":"2.0","method":"session.run_turn","params":{"session_id":"Ab12Cd34...","message":"加上错误处理"},"id":3}' \
  | socat - UNIX-CONNECT:$HOME/.atta/code/daemon.sock
```

### 8.2 用 Rust 客户端接入

```rust
use tokio::net::UnixStream;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use serde_json::{json, Value};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let socket_path = dirs::home_dir()
        .unwrap()
        .join(".atta/code/daemon.sock");
    let stream = UnixStream::connect(&socket_path).await?;
    let (r, mut w) = stream.into_split();

    // 发送消息（不指定 session_id，自动创建）
    let req = json!({"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"hello"},"id":1});
    w.write_all((serde_json::to_string(&req)? + "\n").as_bytes()).await?;

    let mut sid = String::new();
    let mut lines = BufReader::new(r).lines();

    // 读取流事件 + 最终响应
    while let Some(line) = lines.next_line().await? {
        let frame: Value = serde_json::from_str(&line)?;
        if frame["method"] == "session.event" {
            let event = &frame["params"]["event"];
            let tid = frame["params"]["turn_id"].as_str().unwrap_or("");
            match event["kind"].as_str() {
                Some("text_delta") => print!("{}", event["text"].as_str().unwrap_or("")),
                Some("turn_complete") => { println!("\n[turn:{:.8}]", tid); break; }
                _ => {}
            }
        } else {
            let r = &frame["result"];
            sid = r["session_id"].as_str().unwrap_or("").to_string();
            println!("session: {}  name: {}", sid,
                r["name"].as_str().unwrap_or("(none)"));
            break;
        }
    }

    // 继续对话（使用相同 session_id）
    let req = json!({"jsonrpc":"2.0","method":"session.run_turn","params":{"session_id":sid,"message":"继续"},"id":2});
    // ... 同上处理 ...
    Ok(())
}
```

---

## 9. Session 生命周期

```
session.run_turn {message} (无 session_id)
  │
  ├─ Id::new() 生成 session_id
  ├─ 创建 Agent 实例 → 注入 SessionPool
  ├─ 执行首轮
  ├─ 场景支持自动命名 → 额外 LLM 调用生成 name
  └─ 返回 {session_id, turn_id, name, api_calls}

session.run_turn {session_id, message} × N
  │  (同一 session 内多轮对话，上下文累积)
  │
  ▼
session 空闲 > idle_timeout → SessionPool 自动回收
  │  (Agent 销毁，内存释放；历史数据如启用持久化则保留在磁盘)
  │
  ▼
session.run_turn {session_id, message}  (再次使用)
  │  SessionPool 中不存在 → 从 HistoryStore 恢复 → 新建 Agent
```

---

## 10. SessionPool 驱逐策略

| 触发条件 | 行为 |
|---|---|
| 新建 session 时 pool 已满（`session_cap`） | 驱逐最久未活跃的 idle session（LRU） |
| 后台定时器（每 5 分钟） | 驱逐 `last_active > idle_timeout` 的 session |

被驱逐的 session：
- Agent 实例被 drop，内存释放
- 如果启用了 HistoryStore 持久化，对话历史保留在磁盘
- 在 `session.list` 中变为 `status: "inactive"`
- 下次 `session.run_turn` 传入该 session_id 时会重新创建 Agent 实例

---

## 11. ID 体系

所有外部可见的 ID 均为 **BASE58(UUID v4)** 文本形式，22 字符（有时 21）：

| ID 类型 | 生成方式 | 示例 |
|---|---|---|
| `session_id` | `Id::new().to_string()` | `Ab12Cd34Ef56Gh78Ij90Kl` |
| `turn_id` | `Id::new().to_string()` | `Mn12Op34Qr56St78Uv90Wx` |

唯一生成入口：`core::id::Id::new()`。

---

## 12. 配置系统

Daemon 使用分层配置（优先级从低到高）：

1. **内置默认值**
2. **用户级 `settings.json`**：`$HOME/.atta/code/settings.json`
3. **项目级 `settings.json`**：`<project>/.atta/code/settings.json`
4. **CLI 参数**

`settings.json` 支持的字段：

```json
{
  "model": "claude-opus-4-8",
  "max_tokens": 4096,
  "mcp_servers": { ... }
}
```

---

## 13. TCP 模式（远程接入）

当 daemon 以 `--listen` 启动时，同时监听 TCP 端口。认证方式与之前相同：连接后首行发送认证令牌。

```sh
attacored --listen 127.0.0.1:7878 --token my-secret
```
