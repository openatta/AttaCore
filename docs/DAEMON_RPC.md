# daemon JSON-RPC 接口参考

> 本文档是 `attacored`（daemon 二进制）JSON-RPC 接口的完整方法清单——按命名空间列出每个方法的参数、返回值、错误情况和示例。设计过程/决策记录见 `docs/CONFIG_LAYOUT.md` §13-§14；`config.*` 相关方法的供应商配置语义另见 `docs/LLM_PROVIDERS.md`。源码：`daemon/src/server.rs`（dispatch + 各方法实现）、`daemon/src/rpc.rs`（线上类型）、`daemon/src/session_pool.rs`（`SessionPool`，真正的业务逻辑）。

---

## 协议基础

- **JSON-RPC 2.0，换行分隔**：Unix socket / TCP 连接上，每行一个完整的 JSON 对象，请求和响应都是如此（`daemon/src/rpc.rs`）。用 `nc`/`socat` 接 Unix socket 时可以直接 `tail -f` 式地看流量。
- **请求**：`{"jsonrpc":"2.0","method":"<方法名>","params":{...},"id":<number|string>}`。`id` 缺省视为通知（不期待响应，目前所有方法都是"请求-响应"式，没有真正的通知型方法）。`params` 缺省等价于 `{}`。
- **响应**：成功 `{"jsonrpc":"2.0","id":...,"result":{...}}`；失败 `{"jsonrpc":"2.0","id":...,"error":{"code":...,"message":"...","data":...}}`。
- **流式帧（`StreamFrame`）**：部分方法（`session.run_turn`、`daemon.subscribeEvents`）会在同一条连接上，最终响应之前（或之后，取决于方法）额外推送若干条 `{"jsonrpc":"2.0","method":"<事件方法名>","params":{...}}` 格式的帧，没有 `id` 字段——用 `method` 区分是流式事件帧还是最终响应（响应有 `id`，流式帧没有）。
- **连接与并发**：Unix socket（`serve_unix`）和 TCP（`serve_tcp`，需要 `--listen`+`--token`）每条连接独立处理，一条连接上的请求按到达顺序串行处理（`handle_connection` 逐行读取、逐个 dispatch）；但同一个方法的执行本身是 async 的，不同连接之间天然并发。
- **错误码**（`daemon::rpc::codes`）：

  | 常量 | 值 | 含义 |
  |---|---|---|
  | `PARSE_ERROR` | -32700 | 标准 JSON-RPC；本实现遇到解析失败的行直接跳过，不回错误帧（见下方"畸形请求"说明） |
  | `INVALID_REQUEST` | -32600 | 标准 JSON-RPC，预留 |
  | `METHOD_NOT_FOUND` | -32601 | 方法名不在下表中 |
  | `INVALID_PARAMS` | -32602 | 缺必填参数、参数类型错、或业务校验失败（比如 provider 配置格式不对） |
  | `INTERNAL_ERROR` | -32603 | 服务端内部错误（比如 session 创建失败） |
  | `SESSION_NOT_FOUND` | -32000 | daemon 扩展：引用的 session id 不存在 |
  | `SESSION_CAP_REACHED` | -32001 | daemon 扩展：预留，当前未在任何路径实际触发 |
  | `ENGINE_ERROR` | -32002 | daemon 扩展：session 运行时（Agent 内部）报错 |
  | `UNAUTHORIZED` | -32003 | daemon 扩展：TCP 连接的 `daemon.auth` 握手缺失、格式错、或 token 不匹配（仅 TCP；Unix socket 不会触发） |

  **畸形请求（整行不是合法 JSON，或缺 `method` 字段导致反序列化失败）不会收到任何响应**——`handle_connection` 直接 `continue` 跳过这一行，连接不会因此关闭，也不会收到一条 `PARSE_ERROR` 错误帧。这和很多 JSON-RPC 实现的"至少回一个 parse error"不同，调用方需要自己保证发送的每一行是合法 JSON。

- **信任边界**：没有方法级鉴权（一旦连接建立/通过握手，能调用下表**任何**方法——包括改写 LLM 供应商密钥 `config.setProvider`、读出明文密钥 `config.getProvider`+`include_secrets:true`、杀掉所有 session `daemon.shutdown`）。Unix socket 的文件权限（本地）是唯一访问控制层，连接建立即信任，无需握手。**TCP 连接必须先握手**：连接建立后的第一条消息必须是
  ```json
  {"jsonrpc":"2.0","method":"daemon.auth","params":{"token":"<--token 的值>"},"id":1}
  ```
  握手成功返回 `{"result":{"authenticated":true}}`，之后该连接的后续请求正常按下表 dispatch，不必每条都带 token（连接级信任，同 Unix socket）。握手失败（token 错、第一条消息不是 `daemon.auth`、或整行不是合法 JSON）返回 `UNAUTHORIZED` 错误帧后**服务端主动断开连接**——不会进入下面的方法分派。对待 socket/token 暴露面应该和对待 LLM 凭证本身一样谨慎。

---

## 方法总览

| 方法 | 命名空间 | 只读？ | 一句话 |
|---|---|---|---|
| [`daemon.status`](#daemonstatus) | daemon | 是 | 版本、运行时长、活跃 session 数 |
| [`daemon.doctor`](#daemondoctor) | daemon | 是 | 配置/接线状态诊断摘要 |
| [`daemon.subscribeEvents`](#daemonsubscribeevents) | daemon | 是 | 订阅 daemon 级异步通知（MCP 连接结果、导入检测……） |
| [`daemon.shutdown`](#daemonshutdown) | daemon | 否 | 关闭全部 session + 停止 daemon |
| [`session.list`](#sessionlist) | session | 是 | 列出活跃 + 历史 session |
| [`session.close`](#sessionclose) | session | 否 | 关闭单个 session |
| [`session.run_turn`](#sessionrun_turn) | session | 否 | 发一条消息、跑一轮，流式返回事件 |
| [`config.setProvider`](#configsetprovider) | config | 否 | 写入/删除一个 LLM 供应商配置 |
| [`config.getProvider`](#configgetprovider) | config | 是 | 读回当前生效的供应商配置（`api_key` 默认脱敏） |
| [`config.reload`](#configreload) | config | 否 | 不写文件，重新读一遍磁盘上当前的三层 settings.json 并生效 |
| [`mcp.status`](#mcpstatus) | mcp | 是 | 已连接 MCP server 列表 |
| [`mcp.addServer`](#mcpaddserver) | mcp | 否 | 添加并连接一个 MCP server |
| [`import.list`](#importlist) | import | 是 | 检测可导入的跨工具配置（Claude Code/Codex/Cursor） |
| [`import.run`](#importrun) | import | 否 | 执行一次导入 |
| [`commands.list`](#commandslist) | commands | 是 | 列出可用 slash command（内置 + skill + 插件贡献） |
| [`plugin.list`](#pluginlist) | plugin | 是 | 列出已安装插件（含禁用的）及其启用状态 |
| [`plugin.install`](#plugininstall) | plugin | 否 | 从显式 source（无需 marketplace）安装插件，校验 checksum |
| [`plugin.uninstall`](#pluginuninstall) | plugin | 否 | 卸载一个插件（全部或指定版本） |
| [`plugin.enable`](#pluginenable--plugindisable) | plugin | 否 | 启用一个插件 |
| [`plugin.disable`](#pluginenable--plugindisable) | plugin | 否 | 禁用一个插件 |

---

## `daemon.*`

### `daemon.status`

无参数。

```jsonc
// 请求
{"jsonrpc":"2.0","method":"daemon.status","id":1}
// 响应
{"jsonrpc":"2.0","id":1,"result":{
  "version": "0.1.0",       // CARGO_PKG_VERSION
  "uptime_secs": 42,
  "sessions": 2              // 当前活跃（非历史）session 数
}}
```

### `daemon.doctor`

无参数。只读诊断——排查"为什么我的配置没生效"时用这个，不用翻日志。详细字段说明和示例见 `docs/LLM_PROVIDERS.md`「通过 RPC 配置」一节；这里只列顶层结构：

```jsonc
{"jsonrpc":"2.0","id":1,"result":{
  "scene": "coding",
  "settings_tiers": [ {"tier":"global","path":"...","exists":true,"parses":true,"error":null}, ... ],
  "providers": {"configured":[...],"default_provider":"...","task_models":[...],"ok":true,"warnings":[],"error":null},
  "hooks": {"configured":false,"ok":true,"error":null},
  "session_persistence": {"history_store_wired":true},
  "permission_rules_count": 0,
  "model": {"model_name":"claude-sonnet-4-6","api_type":"Anthropic"}
}}
```

`settings_tiers[].exists=false` 是正常情况（不是每层都必须有文件）；`parses=false` 才说明语法有问题，看 `error` 字段。`providers.ok=false` 是硬错误（比如 `default_provider` 指向不存在的 provider）。

### `daemon.subscribeEvents`

无参数。**长连接语义**：立即返回一次确认响应，之后同一条连接会持续收到 `daemon.event` 流式帧，直到连接断开——不会再有第二次针对这次调用的"最终响应"。

```jsonc
// 请求
{"jsonrpc":"2.0","method":"daemon.subscribeEvents","id":1}
// 立即响应（确认订阅成功，不代表有事件发生）
{"jsonrpc":"2.0","id":1,"result":{"subscribed":true}}

// ……后续任意时刻，同一条连接上推送（可能多条）：
{"jsonrpc":"2.0","method":"daemon.event","params":{"kind":"mcp_connected","server":"github","transport":"stdio","tool_count":12}}
{"jsonrpc":"2.0","method":"daemon.event","params":{"kind":"mcp_connect_failed","server":"broken","error":"..."}}
{"jsonrpc":"2.0","method":"daemon.event","params":{"kind":"import_detected","sources":[{"source":"claude_code","description":"Claude Code (CLAUDE.md)"}]}}
```

当前会出现的 `params.kind`：

| `kind` | 触发时机 | 字段 |
|---|---|---|
| `mcp_connected` | daemon 启动时后台批量连接 / `mcp.addServer` 单个连接，成功 | `server`、`transport`、`tool_count` |
| `mcp_connect_failed` | 同上，失败（含配置解析失败） | `server`、`error`（一般是概述文本，具体原因看 daemon 日志） |
| `import_detected` | daemon 启动后台检测到可导入的跨工具配置（且 `.imported.json` 没有历史决定） | `sources`（数组，每项 `source`+`description`） |

**不补发历史事件**——想不错过某个事件，必须在它发生**之前**订阅（典型用法：daemon 刚起来、还没等 MCP 连接完成前先订阅）。这是一个通用机制，不只服务 MCP/import，未来新的异步后台操作复用同一套。

### `daemon.shutdown`

无参数。关闭**全部**活跃 session（不区分是否有未完成的 turn），然后触发 daemon 自身退出。没有"关闭单个 session"的等价物在这——那是 [`session.close`](#sessionclose)。

```jsonc
{"jsonrpc":"2.0","method":"daemon.shutdown","id":1}
// {"jsonrpc":"2.0","id":1,"result":{"shutting_down":true}}
```

---

## `session.*`

### `session.list`

无参数。合并"活跃 session"（内存中）与"历史 session"（`HistoryStore` 磁盘记录，仅当 daemon 启动时成功接上了 session 持久化才有），按 session_id 去重（活跃优先）。

```jsonc
{"jsonrpc":"2.0","id":1,"result":{"sessions":[
  {"session_id":"abc123...","name":"聊聊部署方案","preview":null,"message_count":0,
   "created_at":"2026-08-04T10:00:00Z","last_active":"2026-08-04T10:05:00Z","status":"active"},
  {"session_id":"def456...","name":null,"preview":null,"message_count":0,
   "created_at":"","last_active":"","status":"inactive"}
]}}
```

`preview`/`message_count` 目前恒为 `null`/`0`（占位，未实现）；`inactive` 记录的 `created_at`/`last_active`/`name` 也是空——历史 session 目前只报告"这个 id 存在过"，细节要靠 `session.run_turn` 恢复后才能看到。

### `session.close`

```jsonc
{"jsonrpc":"2.0","method":"session.close","params":{"session_id":"abc123..."},"id":1}
// {"jsonrpc":"2.0","id":1,"result":{"closed":"abc123..."}}
```

缺 `session_id` → `INVALID_PARAMS`。**引用一个不存在/已经关闭的 session_id 不是错误**——`shutdown_session` 本身是幂等的空操作，仍然返回 `{"closed": "<你传的id>"}`，不会因为对方不存在而报 `SESSION_NOT_FOUND`（和 `session.run_turn` 的行为不同，后者会尝试自动创建/恢复）。

### `session.run_turn`

发一条用户消息、跑一轮 Agent turn，流式返回过程事件，最后一条是最终响应。

```jsonc
// 请求
{"jsonrpc":"2.0","method":"session.run_turn","id":1,"params":{
  "session_id": "abc123...",     // 可选：不传则自动新建一个 session
  "message": "帮我看看这个 bug",
  "turn_id": "custom-turn-id",   // 可选：不传则自动生成（base58 UUID，21-22 字符）
  "options": {                    // 可选，仅在*新建* session 时生效，已有 session 忽略
    "vcr": {"mode": "record", "scenario": "demo", "dir": "/path/to/fixtures"},
    "telemetry": {"output": "/path/to/telemetry.jsonl"},
    // 不传 permission_mode（缺省）= 今天的行为：AllowAllPermission，工具调用
    // 全部直接放行，永远不会收到 kind:"prompt" 事件。只有显式传了才会切成
    // 真实权限检查——按 session 粒度 opt-in，不是全局开关。
    "permission_mode": "default",  // Default/Plan/AcceptEdits/BypassPermissions/DontAsk/...
    "permission_rules": [
      {"source":"session","behavior":"deny","tool_name":"Bash","rule_content":"rm -rf:*"}
    ]
  }
}}

// 流式帧（同一条连接，可能多条，method 都是 "session.event"）
{"jsonrpc":"2.0","method":"session.event","params":{"session_id":"...","turn_id":"...","event":{"kind":"text_delta","text":"让我看"}}}
{"jsonrpc":"2.0","method":"session.event","params":{"session_id":"...","turn_id":"...","event":{"kind":"tool_use","id":"...","name":"Read","input":{...}}}}
// 需要人工确认的工具调用（见下方 session.respondToPrompt）——不是终止帧，
// turn 停在这里等回应，之后正常继续 tool_use/tool_result
{"jsonrpc":"2.0","method":"session.event","params":{"session_id":"...","turn_id":"...","event":{
  "kind":"prompt","prompt_type":"permission","prompt_id":"p1",
  "tool_name":"Bash","message":"Allow Bash?","paths":[]
}}}
{"jsonrpc":"2.0","method":"session.event","params":{"session_id":"...","turn_id":"...","event":{"kind":"tool_result","id":"...","name":"Read","content":"...","is_error":false}}}
{"jsonrpc":"2.0","method":"session.event","params":{"session_id":"...","turn_id":"...","event":{"kind":"turn_complete","stop_reason":"end_turn","api_calls":3,"usage":{"input_tokens":1200,"output_tokens":340}}}}

// 最终响应（在 turn_complete 帧之后）
{"jsonrpc":"2.0","id":1,"result":{
  "session_id": "abc123...",
  "turn_id": "custom-turn-id",
  "name": null,          // 首轮且场景开启自动命名（如 chat 场景）时才会是生成的标题
  "api_calls": 3
}}
```

`session_id` 缺省时的行为：自动生成一个新 id 并创建 session。传了但对应的 session 当前不活跃时：先尝试从 `HistoryStore` 恢复（有历史记录 → 恢复原 session 继续；没有 → 当作全新 id 创建）——不会返回 `SESSION_NOT_FOUND`，这一点和 `session.close` 的"存在性检查更松"是一致的设计。

**错误/异常路径**：

- 缺 `message` → `INVALID_PARAMS`。
- Agent 运行时报错（`AgentEvent::Error`）→ `ENGINE_ERROR`，`message` 形如 `"<code>: <message>"`。
- **调用方在 turn 进行中断开连接** → daemon 检测到写失败，立即取消该 session（杀掉子进程等），响应体是 `{"session_id":...,"turn_id":...,"disconnected":true}`（此时你已经断开了，收不到这条响应——这条是写给"万一还有人在读"的收尾语义，实际效果主要是内部清理）。

### `session.respondToPrompt`

回应一个 `session.event` 里的 `kind:"prompt"` 帧（目前只有 `prompt_type:"permission"` 一种）。方法名和参数形状故意做成通用的——以后如果有别的"停下来问一句"场景，复用同一个方法、加一个新的 `prompt_type`，不需要新增 RPC 方法。

```jsonc
{"jsonrpc":"2.0","method":"session.respondToPrompt","id":2,"params":{
  "session_id": "abc123...",
  "prompt_id": "p1",
  "prompt_type": "permission",              // 可选，缺省 "permission"
  "decision": {"type":"permit"}
  // 或者拒绝： "decision": {"type":"deny","reason":"不要跑这个命令"}
}}
// {"jsonrpc":"2.0","id":2,"result":{"prompt_id":"p1"}}
```

**没有超时**——引擎侧会一直等这条回应，直到收到、或者 session 被 `session.close`/断线取消（那种情况下这次工具调用直接失败，不会自动放行）。`session_id` 不存在 → `SESSION_NOT_FOUND`（这里比 `session.close` 严格——回应一个不存在的 session 是调用方的真实错误，不能安全忽略）；缺 `session_id`/`prompt_id`/`decision`，或 `decision` 解析不出 `{"type":"permit"}`/`{"type":"deny","reason":"..."}` → `INVALID_PARAMS`；`prompt_type` 传了但不是 `"permission"` → `INVALID_PARAMS`（还没有别的类型）。`prompt_id` 已经被回应过、或者对应的等待已经因为 session 关闭而清理掉 → 这次调用仍然返回成功（`Ok`），只是引擎那边已经没人在等了，是个静默的空操作，不会报错。

---

## `config.*`

供应商/任务路由配置的写与读——完整语义（局部 patch、软降级、路由校验）见 `docs/LLM_PROVIDERS.md`「通过 RPC 配置」一节，这里只列签名。

### `config.setProvider`

```jsonc
{"jsonrpc":"2.0","method":"config.setProvider","id":1,"params":{
  "provider_id": "deepseek",
  "config": {"api_type":"openai_compatible","base_url":"...","api_key":"...","default_model":"..."},
  "default_provider": "deepseek",           // 可选
  "task_models": {"subagent":"deepseek"},   // 可选，局部合并
  "delete": false                            // 可选，默认 false；true 时忽略 config
}}
// {"jsonrpc":"2.0","id":1,"result":{
//   "written_to":"/repo/.atta/settings.json","providers":["deepseek"],
//   "default_provider":"deepseek","task_models":["subagent"],
//   "routing":{"ok":true,"warnings":[],"error":null,"router_rebuilt":false,"router_error":"provider 'deepseek': api_type 'openai_compatible' has no runtime implementation yet ..."}
// }}
```

`provider_id` 缺失 / `config` 或 `task_models` 校验不通过 → `INVALID_PARAMS`，不写文件。**已存在但内容不是合法 JSON 对象的 `settings.json`** 也会被拒绝（`INVALID_PARAMS`），不会静默清空重写——这是本仓库一次审查中修的一个真实 bug，务必别退化回去。

`routing` 里两组字段是两件独立的事，不要混着读：
- `ok`/`warnings`/`error` —— `base::provider::resolve_task_models` 纯配置校验的结果（`task_models` 引用的 provider 存不存在、模型名在不在允许列表里……），不关心 `api_type` 是否有真实实现。
- `router_rebuilt`/`router_error` —— 上面校验通过之后，是不是真的把新配置构造成了活的 `Arc<dyn Model>` 实例并换掉了正在用的路由表。`api_type: openai_compatible` 就是"`ok:true` 但 `router_rebuilt:false`"的典型例子——配置本身逻辑自洽，但引擎还没有对应协议的实现，**实际发起的请求依然走旧的路由表**，直到你换成 `api_type:"anthropic"` 的 provider。
- 无论 `routing` 结果如何，settings.json 都已经写盘——这是既有设计（校验不过不拒绝写入，只报告），本次新增的路由重建同样遵循这个哲学：重建失败不回滚文件、不影响这次 RPC 的整体成功，只是"这次没能让新配置生效"，旧路由继续服务。
- **正在运行的 session 现在也会追上新配置**——不是立即生效，而是懒惰的：每个 session 在收到`config.setProvider`/`config.reload` 之后的下一条消息时，如果当时没有别的 turn 正在处理，会先原地重建（同一个 session_id、从 `HistoryStore` 恢复历史）再处理这条消息；如果当时正忙，本次跳过重建，继续用旧配置服务完这条消息，下一次调用再检查。只对配置了 `HistoryStore` 的 daemon 生效（生产环境默认配置了）。

### `config.getProvider`

```jsonc
{"jsonrpc":"2.0","method":"config.getProvider","id":1,"params":{"include_secrets":false}}
// api_key 默认脱敏为 "***<末4位>"；include_secrets:true 才返回明文
```

### `config.reload`

```jsonc
{"jsonrpc":"2.0","method":"config.reload","id":1}
// 无参数。重新读一遍全局→scene→项目三层 settings.json（不写任何 patch，就是
// "不管谁改了文件，现在给我按磁盘上现在的内容重新生效一次"），响应形状和
// config.setProvider 的一致（少一个 written_to 字段）：
// {"jsonrpc":"2.0","id":1,"result":{
//   "providers":["deepseek"],"default_provider":"deepseek","task_models":["subagent"],
//   "routing":{"ok":true,"warnings":[],"error":null,"router_rebuilt":true,"router_error":null}
// }}
```

给"直接手改 settings.json 文件，而不是走 `config.setProvider` 一个字段一个字段 patch"这个场景用——效果和 `config.setProvider` 完全一样（同一套 `SessionPool::apply_reloaded_settings()`），只是不先写文件这一步。

---

## `mcp.*`

### `mcp.status`

无参数。当前已连接（daemon 启动时后台批量连接，或 `mcp.addServer` 陆续加入）的 MCP server 状态。

```jsonc
{"jsonrpc":"2.0","id":1,"result":{"servers":[
  {"name":"github","transport":"stdio","tool_count":12}
]}}
```

`transport` 取值：`stdio`/`streamable_http`/`sse`/`in_process`/`web_socket`（取决于 server 实际连接方式，见 `mcp::connect`）。**连接失败的 server 不会出现在这里**——只列成功连上的；失败的要靠订阅 [`daemon.subscribeEvents`](#daemonsubscribeevents) 拿 `mcp_connect_failed` 通知，或翻 daemon 日志。

### `mcp.addServer`

```jsonc
{"jsonrpc":"2.0","method":"mcp.addServer","id":1,"params":{
  "name": "github",
  "config": {"type":"stdio","command":"npx","args":["-y","@modelcontextprotocol/server-github"],"env":{}}
}}
// {"jsonrpc":"2.0","id":1,"result":{
//   "written_to":"/repo/.atta/settings.json",
//   "servers":[{"name":"github","transport":"stdio","tool_count":12}]
// }}
```

`config` 是 `mcp::config::McpServerConfig`，`type` 字段做 tag，四种取值及各自字段：

| `type` | 字段 |
|---|---|
| `stdio` | `command`（必填）、`args`（可选，默认空）、`env`（可选，默认空）、`scope`（可选，工具名前缀过滤） |
| `streamable_http` | `url`（必填）、`headers`（可选）、`oauth_provider`（可选，引用 `settings.json` 里 `oauth_providers.<name>`）、`scope`（可选） |
| `sse` | 同 `streamable_http` |
| `in_process` | `name`（必填，进程内预注册服务名）、`scope`（可选） |

写入项目层 `settings.json` 的 `mcp_servers.<name>`（局部 patch，同 `config.setProvider` 的合并语义），**并立即现场连接一次**——连接结果（成功/失败）都会走 `daemon.subscribeEvents` 的 `mcp_connected`/`mcp_connect_failed` 通知。`name`/`config` 缺失，或 `config` 不是合法 `McpServerConfig` → `INVALID_PARAMS`。**连接失败不是 RPC 层面的错误**——写入本身成功就返回 `result`（这一点和 `config.setProvider` 的"routing.ok=false 但仍写入"是同一个设计取向：patch 本身合法就接受，运行时结果单独报告）。

---

## `import.*`

跨工具配置导入（Claude Code `CLAUDE.md`/`.claude/skills/`、Codex 补 `.agents/skills/`、Cursor `.cursorrules`/`.cursor/rules/*.mdc`）——纯文件系统操作，不需要 LLM 参与，因此不必依赖 `session.run_turn`。等价的手动路径是 `/import` 斜杠命令（`ImportTool`），区别是斜杠命令每次都重新检测、忽略 `.imported.json` 历史决定；这两个 RPC 也是每次重新检测（不看历史决定，`import_already_decided` 只用在 daemon 启动时的自动通知路径，见 [`daemon.subscribeEvents`](#daemonsubscribeevents) 的 `import_detected`）。

### `import.list`

```jsonc
{"jsonrpc":"2.0","method":"import.list","id":1}
// {"jsonrpc":"2.0","id":1,"result":{"sources":[
//   {"source":"claude_code","description":"Claude Code (CLAUDE.md, .claude/skills/)"}
// ]}}
```

`sources` 为空数组表示当前项目目录没检测到任何可导入的东西（正常情况，不是错误）。

### `import.run`

```jsonc
{"jsonrpc":"2.0","method":"import.run","id":1,"params":{"source":"claude_code"}}
// {"jsonrpc":"2.0","id":1,"result":{"source":"claude_code","actions":[
//   "merged /repo/CLAUDE.md into AGENTS.md",
//   "copied 3 skill dir(s) from .claude/skills/ to .agents/skills/"
// ]}}
```

`source` 必须是 `import.list` **当前**返回的某个 `source` 值（`claude_code`/`codex`/`cursor`）——缺失、不是合法枚举值、或合法但当前项目根本没检测到这个来源，统一返回 `INVALID_PARAMS`（不区分"格式错"和"检测不到"，报错信息里会说明具体原因）。执行成功会写 `.atta/.imported.json` 决定标记（后续 daemon 启动时的自动检测不会再对这个项目发 `import_detected` 通知）。

---

## `commands.*`

### `commands.list`

无参数。返回当前生效的完整 slash command 目录：内置 local 命令（`help`/`skills`/`clear`/`compact`/`cost`）+ skill 派生的 prompt 命令 + 已启用插件（`plugin.toml` 的 `[slash_commands]`）贡献的 prompt 命令。**跟随插件安装/启用/禁用/卸载热更新**——见下方 `plugin.*`：任意 `plugin.install`/`uninstall`/`enable`/`disable` 调用成功后都会重建这份目录，对**新建**的 session 立即生效（已在运行的 session 不受影响，语义同 `config.setProvider`/`mcp.addServer`）。

```jsonc
{"jsonrpc":"2.0","method":"commands.list","id":1}
// {"jsonrpc":"2.0","id":1,"result":{"commands":[
//   {"name":"clear","description":"Clear the current session context","kind":"local","source":"builtin"},
//   {"name":"review","description":"Plugin slash command: /review","kind":"prompt","source":"plugin"},
//   {"name":"simplify","description":"...","kind":"prompt","source":"user"}
// ]}}
```

`kind`：`"local"`（立即执行、不进 LLM，`stop_reason` 会是 `"command_executed"`）或 `"prompt"`（展开 skill/插件正文、继续走一次 LLM turn）。`source`：`"builtin"`/`"user"`/`"project"`/`"plugin"`。**没有单独的执行 RPC**——`turn.rs` 的 `process_turn` 已经会拦截 `/name args` 并按 `kind` 分别处理，调用方直接把命令行文本作为 [`session.run_turn`](#sessionrun_turn) 的 `message` 发送即可，走同一条已有路径。

---

## `plugin.*`

插件的安装/启用/禁用/卸载管理面。**没有 marketplace 依赖**——`plugin.install` 直接从调用方给出的 `download_url` 安装（本地 `file://` 路径或 `https://` URL），不需要配置注册表。安装目标是两层"插件 tier"之一：`"global"`（`~/.atta/plugins/`，跨 scene 共享，默认）或 `"scene"`（`~/.atta/scenes/<scene>/plugins/`，仅当前 daemon 绑定的 scene）。

### `plugin.list`

无参数。列出**所有**已发现的插件（内置 + 两层 tier 上已安装的，不管是否禁用）及其 `enabled` 状态——管理界面需要看到并重新启用被禁用的插件，所以跟 [`commands.list`](#commandslist)（只反映激活集）不同。

```jsonc
{"jsonrpc":"2.0","method":"plugin.list","id":1}
// {"jsonrpc":"2.0","id":1,"result":{"plugins":[
//   {"name":"plugin-hello","version":"0.1.0","description":"...","enabled":true},
//   {"name":"code-review-helper","version":"1.0.0","description":"...","enabled":false}
// ]}}
```

### `plugin.install`

```jsonc
{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{
  "name": "code-review-helper",
  "version": "1.0.0",
  "download_url": "file:///abs/path/to/plugin.zip",
  "checksum": "<sha256 hex, lowercase>",
  "scope": "global"
}}
// {"jsonrpc":"2.0","id":1,"result":{"success":true,"message":"installed plugin 'code-review-helper' v1.0.0 from file:///..."}}
```

插件归档是 zip，解压时用 `enclosed_name()` 过滤路径穿越条目（zip-slip 防护，越界条目直接跳过，不写入任何位置）。**`checksum` 对 `https://`/`http://` 来源是必填的**——缺失直接拒绝，不发起网络请求；对 `file://` 本地路径是可选的（本地路径本身已经是可信来源，不存在"传输途中被篡改"这个威胁模型）。`scope` 缺省 `"global"`。`name`/`version`/`download_url` 缺失 → `INVALID_PARAMS`；checksum 不匹配 / 下载失败 / 归档格式非法 → `INVALID_PARAMS`（错误信息里说明具体原因）。成功后会重建 [`commands.list`](#commandslist) 用的共享目录（见上方"跟随插件…热更新"）。

### `plugin.uninstall`

```jsonc
{"jsonrpc":"2.0","method":"plugin.uninstall","id":1,"params":{"name":"code-review-helper","scope":"global"}}
// {"jsonrpc":"2.0","id":1,"result":{"success":true,"message":"removed plugin 'code-review-helper' (1 versions)"}}
```

`version` 可选，缺省删除该插件在这一 tier 下的全部版本。`scope` 缺省 `"global"`。

### `plugin.enable` / `plugin.disable`

```jsonc
{"jsonrpc":"2.0","method":"plugin.disable","id":1,"params":{"name":"code-review-helper","scope":"global"}}
// {"jsonrpc":"2.0","id":1,"result":{"name":"code-review-helper","enabled":false,"scope":"global"}}
```

启用状态存在 `<tier_root>/enabled.json`（`{"插件名": bool}`），从未设置过等价于启用（"已安装 == 已激活"的历史隐含行为不变）。scene 层显式设置优先于 global 层显式设置，都没设置时默认启用。`scope` 缺省 `"global"`。

---

## 已知的接口面缺口

审查这份接口时顺手记的、目前故意没做或还没排上的：

- **没有 `config.getSettings`/读取完整 `Settings`**——`config.getProvider` 只暴露 provider/task_models 部分,`daemon.doctor` 只给压缩摘要,想看 `sandbox`/`permission_rules` 等其他字段的完整内容,目前只能自己读 `settings.json` 文件。
- **没有 `mcp.removeServer`**——只能加,不能通过 RPC 删除一个已配置的 MCP server（能直接改 `settings.json` 手动删）。
- **权限模式/规则现在是按 session 创建时一次性配置的**（`session.run_turn` 的 `options.permission_mode`/`options.permission_rules`，见上），不是独立的读写 RPC——没有"运行中途查看/修改某个 session 当前生效的规则"这个能力，也没有 daemon 级别的全局默认值配置入口（全局默认永远是 `AllowAllPermission`，这是刻意的零风险默认值，不是缺口）。想要"运行中途改规则"或者"daemon 级别切换默认策略"，需要额外设计,这次没做。
- **没有 skills/agents/hooks 的列出/管理 RPC**——这些子系统本身的加载/业务逻辑是完备的（见完备性审查记录),但没有对应的只读 RPC 把"当前生效的 skill/agent 类型/hook 列表"暴露出来,想看只能翻文件系统或读 `AgentTool`/`SkillTool` 面向模型的描述文本。
