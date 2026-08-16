# AttaCore Daemon 接口协议

**协议版本:2**

架构设计与取舍依据见 `docs/design/2026-08-11-multi-scene-architecture.md`。
本文是规范:字段、类型、语义、错误。

协议 1 已停止支持。v2 与 v1 不兼容,主要差异:场景与项目成为一等定位参数;
`session.create` 是唯一会话创建入口;子 Agent 以侧链会话形式落盘;
新增 `scene.*` / `agent.*` / `team.*`。

---

## 1. 传输与编码

**帧格式**:每行一个 JSON 对象,`\n` 分隔(NDJSON)。不使用 Content-Length 头 ——
让 `tail -f` 和 `nc` 可以直接调试。

| 传输 | 地址 | 认证 |
|---|---|---|
| Unix socket | 见 §2 | 文件权限(`0600`) |
| TCP | `--listen <addr>` | `daemon.auth` 握手,见 §5.1 |

编码 UTF-8。单帧无长度上限,但实现建议对单行 > 16 MiB 的输入拒绝并返回 `PARSE_ERROR`。

**请求**:

```jsonc
{ "jsonrpc": "2.0", "method": "session.create", "params": { … }, "id": 1 }
```

`id` 省略 = 通知(notification),服务端不回响应。`params` 省略等价于 `{}`。

**响应**:

```jsonc
{ "jsonrpc": "2.0", "id": 1, "result": { … } }
{ "jsonrpc": "2.0", "id": 1, "error": { "code": -32004, "message": "…", "data": { … } } }
```

`result` 与 `error` 互斥,恰有其一。

---

## 2. 发现

daemon 启动时在 `~/.atta/daemon/instances.d/` 下写自己那一份实例文件,退出时删除。
**每个实例一个文件,没有共享写点** —— 多个 daemon 同时启动不会因为并发
read-modify-write 同一个索引文件而丢更新。读取方 `read_dir` 汇总。

```
~/.atta/daemon/instances.d/desktop.json
~/.atta/daemon/instances.d/ci-runner.json
```

```jsonc
{ "instance": "desktop",
  "pid": 4210,
  "pid_start_time": 1754899200,
  "socket": "/Users/x/.atta/daemon/desktop.sock",
  "scenes": ["coding", "chat", "research"],
  "protocol_version": 2,
  "started_at": "2026-08-11T10:00:00Z" }
```

单文件写入仍用原子写(临时文件 + rename),保证读取方不会读到半截 JSON。

**陈旧条目**:daemon 崩溃时文件会残留。读取方必须按
`(pid, pid_start_time)` 两项一起判定进程是否仍存活 —— **只比 pid 会在 pid 被系统
复用时误判为"存活"**。判定为陈旧的条目应忽略,并可顺手删除该文件。

**socket 路径**:

```
--socket <path> 指定        → 用它
否则 --instance <name> 指定 → ~/.atta/daemon/<name>.sock
否则                        → ~/.atta/daemon/<激活场景集合排序后连接>.sock
```

多 daemon 并存时 instance 名必须不同,冲突则启动失败。

---

## 3. 通用约定

### 3.1 定位参数

| 参数 | 类型 | 说明 |
|---|---|---|
| `scene` | string | 场景 id。会话创建时**必填** |
| `project_root` | string \| null | 绝对路径。`null` = **无项目会话**(一等形态)。省略 = 回落进程 cwd(**不推荐**) |
| `session_id` | string | 会话 id,跨重启稳定 |
| `agent_id` | string | 子 Agent 运行期 id,**仅进程内有效** |
| `team_id` | string | 团队 id,跨重启稳定 |

`project_root` 省略时响应包含 `"project_root_inferred": true`。新客户端应始终显式
传值(含显式 `null`)—— 桌面端同时打开多个项目,依赖进程 cwd 会让所有会话共享同一份
项目层配置。

**是否允许 `null` 由场景决定**(见 §3.2 场景能力):`coding` / `research` 必须有
项目,传 `null` 或省略返回 `PROJECT_REQUIRED`;`chat` / `demo` 允许无项目,此时跳过
项目层配置、transcript 落全局分区,可列举、可 resume、无 TTL。

### 3.2 场景能力

每个场景声明两个能力位,通过 `scene.list` / `scene.describe` 暴露:

| 能力 | 含义 | coding | research | chat | demo |
|---|---|---|---|---|---|
| `requires_project` | 会话必须绑定项目根 | ✅ | ✅ | ❌ | ❌ |
| `supports_team` | 注册 Team 工具、允许 `team.*` | ✅ | ✅ | ❌ | ❌ |

宿主据此决定新建会话时是否弹"选择项目"对话框,以及是否显示 Team 入口。
`supports_team == false` 的场景**根本不注册** Team 工具 —— 模型看不到它们,
不是运行时才拒绝。

### 3.3 会话类型

| `session_kind` | 谁发起 | 默认是否出现在 `session.list` |
|---|---|---|
| `primary` | 用户(`session.create`) | 是 |
| `sidechain` | 模型(`Agent` 工具 / `TeamCreate`) | 否 |

两者存储与查询机制完全一致,差异有三:

- **可见性** —— 侧链默认不进 `session.list`
- **可恢复性** —— 侧链进入终态(`completed`/`failed`/`cancelled`)后
  **不可 resume**,`SessionInfo.resumable` 为 `false`,强行 resume 返回
  `SIDECHAIN_TERMINAL`。一个跑完的一次性任务没有续跑语义;只有被中断的才可恢复。
- **寿命** —— 侧链的 transcript 锚定在父会话的**打开期**上:

  | 触发 | 侧链 |
  |---|---|
  | `session.close` / `daemon.shutdown --graceful` | **删除** |
  | `session.delete` | 删除(连同父 transcript) |
  | LRU 驱逐 / 空闲超时 | **保留**(只回收运行体,会话仍在) |
  | daemon 崩溃 | 遗留在磁盘,由下次启动的 GC 按 30 天清理 |

  实际含义:**"打开子 Agent 的会话查 history"只在父会话仍打开时可用。**
  父会话 `close` 之后,它的 transcript 里仍留有 `Agent` 工具的 `tool_use` 与
  `tool_result`(含子 Agent 最终输出),丢失的只是中间步骤。

### 3.4 时间与 id

时间一律 RFC 3339 UTC(`2026-08-11T10:00:00Z`)。
`session_id` / `team_id` 持久;`agent_id` / `turn_id` / `prompt_id` 进程内有效。

### 3.5 用量

凡返回 `usage` 的地方格式统一:

```jsonc
{ "input_tokens": 12345, "output_tokens": 678,
  "cache_read_tokens": 0, "cache_write_tokens": 0 }
```

daemon 按**会话**如实上报,不做跨会话/跨场景聚合 —— 聚合口径由调用方决定。

### 3.6 分页

`session.list` / `session.history` / `team.events` 支持游标分页:

```jsonc
// →  { …, "limit": 100, "cursor": "…" }
// ←  { …, "next_cursor": "…" }        // null = 没有更多
```

`limit` 默认 100,上限 1000。桌面端长期使用后磁盘会话可达数千条,不建议不带 `limit`。

**`cursor` 的契约**:

| 属性 | 约定 |
|---|---|
| 格式 | **不透明字符串**,客户端不得解析、构造或修改 |
| 有效期 | 单个 daemon 进程内有效;daemon 重启后失效 |
| 失效表现 | 返回 `INVALID_PARAMS`,`data.reason = "stale_cursor"`;客户端应从头重新分页 |
| 数据变化 | 分页期间新增的条目**可能不出现**在后续页(游标锚在某个已排序位置);删除的条目不会导致错误 |
| 一致性 | **不保证快照隔离**。需要一致视图的客户端应一次性拉完(调大 `limit`)而不是依赖跨页一致 |

---

## 4. 数据类型

本章定义协议里所有非平凡的 JSON 结构。客户端只靠本章就能完整解析与构造消息,
不需要读服务端源码。所有带 `type` 字段的联合体都是**标签联合**,`type` 是判别式。

### 4.1 `ContentBlock` — 消息体

出现在 `session.history` 的 `messages[].content`,以及 `tool_result.content` 的数组
形态里。字段名与取值刻意与 Anthropic Messages API 一致,便于直传。

```jsonc
// type: "text"
{ "type": "text", "text": "……" }

// type: "image"
{ "type": "image", "source": <ImageSource> }

// type: "tool_use"
{ "type": "tool_use", "id": "toolu_…", "name": "Bash", "input": { /* 任意 JSON */ } }

// type: "tool_result"
{ "type": "tool_result",
  "tool_use_id": "toolu_…",
  "content": <ToolResultContent>,
  "is_error": false }                  // 省略即 false

// type: "thinking" —— 扩展思考块
{ "type": "thinking", "thinking": "……", "signature": "…" }

// type: "redacted_thinking" —— 服务端加密的思考,对客户端不透明
{ "type": "redacted_thinking", "data": "…" }
```

> **`thinking` / `redacted_thinking` 必须原样保留。** 它们的 `signature` / `data` 是
> 载荷,不是装饰:开启思考且该轮有工具调用时,下一次请求必须把上一轮的思考块连同
> 签名回传。客户端如果做转发或改写,丢掉它们会让 `thinking + tool_use` 失败。
> 纯展示的客户端可以不渲染,但不要丢弃。

还有一个仅出现在**请求方向**、客户端不会收到的块(列出以免解析器遇到时报错):

```jsonc
{ "type": "cache_edits", "cache_edits": [ { "type": "delete_tool_result", "tool_use_id": "…" } ] }
```

### 4.2 `ImageSource`

```jsonc
{ "type": "base64", "media_type": "image/png", "data": "<base64,无 data-URI 前缀>" }
{ "type": "url",    "url": "https://…" }
```

### 4.3 `ToolResultContent`

**无标签联合** —— 按 JSON 类型判别,不是靠 `type` 字段:

```jsonc
"纯文本结果"                          // string 形态
[ <ContentBlock>, <ContentBlock> ]    // 数组形态(工具返回了图片或多段内容)
```

客户端必须同时处理两种:`typeof x === "string" ? … : …`。

### 4.4 `Attachment` — `session.run_turn` 的附件

```jsonc
{ "kind": "file",  "path": "/abs/path.png" }
// daemon 读盘。按扩展名判定:图片 → image 块;其余 → 内联为文本

{ "kind": "text",  "path": "selection", "content": "……" }
// 宿主已读好的文本。path 只作展示标签,可以是合成名(如编辑器选区)

{ "kind": "image", "media_type": "image/png", "data": "<base64>" }
// 宿主已解码的图片,磁盘上没有文件(剪贴板粘贴)
```

**大小限制**:单帧上限 16 MiB(§1)。base64 会把二进制放大约 4/3,所以单张内联图片
的实际上限约 12 MiB。超限时 daemon 返回 `INVALID_PARAMS`,
`data.reason = "attachment_too_large"`,`data.limit_bytes` 给出上限。
超大文件应改用 `kind: "file"` 让 daemon 自己读盘,不占帧。

### 4.5 `PermissionMode`

`session.create` 的 `options.permission_mode`、settings.json 的 `permission_mode`、
`TeamCreate` 的 `permission_mode` 共用这一组取值:

| 值 | 含义 | 客户端可设 |
|---|---|---|
| `default` | 未显式放行的工具调用都发 `prompt` 询问 | ✅ |
| `acceptEdits` | 自动放行文件写入(Write/Edit),其余询问 | ✅ |
| `plan` | 只允许只读工具 | ✅ |
| `dontAsk` | 不询问,未显式放行的一律拒绝 | ✅ |
| `bypassPermissions` | 跳过全部权限检查 | ✅ |
| `yolo` | 激进自动放行 | ✅ |
| `auto` | 依赖会话分类器 | ❌ **程序专用**,客户端传入被拒 |
| `bubble` | 把询问上抛给父 Agent | ❌ **程序专用** |

传 `auto` / `bubble` 返回 `INVALID_PARAMS`。

### 4.6 `PermissionRule`

`session.create` 的 `options.permission_rules[]`:

```jsonc
{ "source": "session",              // 见下表
  "behavior": "allow",              // allow | deny | ask
  "tool_name": "Bash",
  "rule_content": "git push:*" }    // 可选;省略 = 匹配该工具的全部调用
```

`rule_content` 的写法按工具而定:`Bash` 用命令前缀模式(`"git push:*"`),
文件类工具用 glob(`"/etc/**"`)。

`source` 决定优先级,数值越大越优先:

| `source` | 优先级 | 来源 |
|---|---|---|
| `cliArg` | 60 | 命令行参数 |
| `session` | 50 | **客户端经 RPC 传入的规则用这个** |
| `command` | 45 | 斜杠命令展开时临时注入 |
| `localSettings` | 40 | `<project>/.atta/settings.local.json` |
| `projectSettings` | 30 | `<project>/.atta/settings.json` |
| `userSettings` | 20 | `~/.atta/settings.json` |
| `policySettings` | 10 | 受管策略 |

注意 `source` 是 **camelCase**,而 `behavior` 是小写 —— 这是既有序列化格式,
不是笔误。

### 4.7 `Usage`

```jsonc
{ "input_tokens": 12345, "output_tokens": 678,
  "cache_read_tokens": 0, "cache_write_tokens": 0 }
```

后两项在服务端未报告缓存信息时为 `0`,不会缺省;客户端可以无条件读取。

### 4.8 `stop_reason`

`turn_complete` 事件与 `run_turn` 的最终响应共用:

| 值 | 含义 |
|---|---|
| `end_turn` | 模型正常结束 |
| `max_tokens` | 触到单次输出上限 |
| `tool_use` | 模型请求工具(轮内中间态,一般不会作为最终值出现) |
| `stop_sequence` | 命中停止序列 |
| `max_turns` | 触到本轮 API 调用次数上限 |
| `budget_exceeded` | 触到 `max_budget_usd` |
| `max_structured_output_retries` | 结构化输出重试超限 |
| `stopped_by_hook` | 被 `UserPromptSubmit` / `PostToolUse` / `Stop` 钩子中止 |
| `cancelled` | 被 `session.interrupt` 或会话关闭取消 |
| `command_executed` | 本地斜杠命令,未调用模型(`api_calls: 0`) |

---

## 5. 连接、认证、并发与顺序

本章是写客户端时最容易踩坑的部分。

### 5.1 认证

仅 TCP 需要。连接建立后的**第一帧**必须是:

```jsonc
{ "jsonrpc":"2.0", "method":"daemon.auth", "params":{"token":"…"}, "id":0 }
```

失败或缺失 → `UNAUTHORIZED (-32003)`,连接关闭。

认证粒度是 **daemon 级**,不按场景或会话签发 —— 一条通过认证的连接可以访问该 daemon
的全部场景与会话。哪些暴露给哪个客户端由上层应用决定。Unix socket 依赖文件系统权限
(`0600`),无需握手。

### 5.2 一条连接上的并发

**允许多个请求同时在途(in-flight)。** 服务端不要求串行,响应可以乱序返回 ——
客户端必须按 `id` 匹配响应,不能假设先发先回。

`id` 由客户端生成,在**同一连接内**必须唯一。数字或字符串均可。

**通知(无 `id`)只对无返回值的方法有意义**:`daemon.ping` 之类。对
`session.run_turn`、`session.create` 这类需要返回值的方法发通知是**未定义行为** ——
服务端会执行但不回响应,客户端拿不到 `session_id` 或结果。不要这么用。

### 5.3 同一会话的并发 `run_turn`

**一个会话同一时刻只能有一个 turn。**

第二个 `run_turn` 到达时立刻失败,返回 `SESSION_BUSY (-32015)`,`data` 带当前正在跑
的 `turn_id`。**不排队、不抢占**。

```jsonc
{ "error": { "code": -32015, "message": "session is busy",
             "data": { "session_id": "S1", "current_turn_id": "t1" } } }
```

客户端要发新消息必须先 `session.interrupt` 再发。桌面端的"发送"按钮应在
`turn_state != idle` 时置灰。

### 5.4 事件帧推给谁

`session.event` 帧**只推给发起该 `run_turn` 的那条连接**,不广播。

**多窗口场景**(同一会话在两个 UI 窗口打开)由此有一个明确后果:只有发起方能看到
实时流。另一个窗口要跟进有两条路:

1. 共享同一条 daemon 连接(推荐 —— 桌面端应用内单例连接,窗口是同一进程的视图);
2. 轮询 `session.get` 看 `turn_state`,turn 结束后拉 `session.history`。

`daemon.event`(§8)与之不同,它推给**所有**订阅者。

### 5.5 事件顺序保证

客户端渲染的正确性依赖这些保证,它们是服务端实现的既定行为:

| 保证 | 说明 |
|---|---|
| **同一 turn 内 `text_delta` 严格有序** | 按发出顺序拼接即得完整文本,不需要序号 |
| **`tool_use` 按模型生成顺序发出** | 与模型流中的出现顺序一致 |
| **`tool_result` 按*完成*顺序发出** | ⚠️ **不是提交顺序** |
| **每个 `tool_result` 必有先行的同 `id` `tool_use`** | 反之不然:被取消的工具可能没有 result |
| **`turn_complete` 是本 turn 最后一个事件** | 其后只有 `run_turn` 的最终响应 |

> **⚠️ 必须按 `id` 配对,不能按位置配对。** 并发安全的工具是并行执行的,
> 谁先跑完谁先发 `tool_result`,所以事件流里 `tool_result` 的顺序与 `tool_use` 的
> 顺序**可以不同**。而 `session.history` 里的块又被还原成了**提交顺序**。
> 两者故意不一致:事件流优化实时性,transcript 优化可重放性。按 `tool_use_id`
> 配对的客户端在两边都对;按数组下标配对的客户端在事件流里会错。

**批次语义**(影响进度条与错误呈现):连续的并发安全工具组成一个批次并行执行;
遇到非并发安全的工具会形成屏障,先排空当前批次。批次内任一工具出错会取消同批的
兄弟工具 —— 客户端可能看到若干 `tool_result` 带 `is_error: true` 且内容是取消提示。

**子 Agent 事件**以 `subagent_progress` 包裹,其 `event` 字段是嵌套的同构事件。
父与子的事件在同一条流上**交错**出现,靠 `agent_id` / `agent_label` 区分归属。
子 Agent 的内部事件之间保持上述同样的顺序保证。

### 5.6 断线与重连

**turn 进行中断线** → daemon 立即取消该会话的 turn 并杀掉派生的子进程。这是刻意的:
没有接收方的 turn 继续烧钱没有意义。

重连后要判断"我刚才那个 turn 怎么样了":

```
session.get {session_id}
  ├─ turn_state == "idle"        → 上一个 turn 已结束
  └─ 读 session.history 尾部     → 看到哪里,以及最后一条是否完整
```

**没有"续接流"的机制** —— 不能重连后接着收上一个 turn 剩下的事件帧。已产生的内容都
已进入 transcript,用 `session.history` 取。

**心跳**:daemon 不主动发心跳。长时间空闲的连接由 TCP keepalive 或客户端自己的
`daemon.ping` 维持。

### 5.7 能力探测

`daemon.status` 返回 `protocol_version` 与 `features[]`:

```jsonc
{ "protocol_version": 2,
  "features": ["scene.hot-activate", "agent.send", "team.events", "session.delete.dry-run"],
  … }
```

**版本决定大结构,`features` 决定可选能力。** 客户端应:先检查
`protocol_version == 2`(不匹配则拒绝连接,v2 与 v1 不兼容);再对可选功能查
`features`,而不是靠调用后收 `METHOD_NOT_FOUND` 来试探。

新增方法会同时加一个 feature 标记;移除方法会升 `protocol_version`。

---

## 6. 方法

### 6.1 `daemon.*`

#### `daemon.ping`
无参 → `{ "pong": true, "protocol_version": 2 }`

#### `daemon.status`

```jsonc
{ "protocol_version": 2, "instance": "desktop", "pid": 4210, "uptime_secs": 3600,
  "features": ["scene.hot-activate", "agent.send", "team.events",
               "session.delete.dry-run", "session.interrupt"],
  "scenes": [ { "scene":"coding", "active":true, "sessions":3, "generation":7 } ],
  "projects": [ { "project_root":"/Users/x/repo-a", "sessions":3, "generation":1 },
                { "project_root": null, "sessions":8, "generation":0 } ],
  "sessions": { "primary": 4, "sidechain": 11, "cap": 32 },
  "agents": { "running": 2 } }
```

#### `daemon.doctor`
返回按场景/项目分组的自检报告。

```jsonc
{ "ok": false,
  "checks": [
    { "scope":"global", "name":"settings_readable", "ok":true },
    { "scope":"scene:research", "name":"provider_reachable", "ok":false,
      "detail":"connect timeout" },
    { "scope":"project:/Users/x/repo-a", "name":"teams_dir_writable", "ok":true } ],
  "orphan_sidechains": ["…"] }
```

`orphan_sidechains` 列出磁盘上遗留的侧链(正常关闭会清掉侧链,所以启动后仍存在的
按定义就是上次非正常退出留下的)。供人工检查;自动清理由启动 GC 按
`settings.execution.sidechain_orphan_retention_days`(默认 30)执行。

#### `daemon.subscribeEvents`
把当前连接标记为异步通知订阅者,此后推送 §7 的 `daemon.event` 帧。
立即返回 `{ "subscribed": true }`。

#### `daemon.shutdown`
可选 `{ "graceful": true }`。优雅关闭时等待进行中的 turn 结束(上限 30 秒),
然后逐个关闭会话 —— 因此**优雅关闭会清掉所有侧链**(§3.3)。非优雅关闭或崩溃时
侧链遗留在磁盘上,由下次启动的 GC 按 30 天清理。

---

### 6.2 `scene.*`

#### `scene.list`

```jsonc
{ "scenes": [
    { "scene":"coding", "name":"Coding", "description":"…",
      "active":true, "sessions":3, "generation":7,
      "capabilities": { "requires_project":true, "supports_team":true } },
    { "scene":"chat", "name":"Chat", "description":"…",
      "active":true, "sessions":8, "generation":2,
      "capabilities": { "requires_project":false, "supports_team":false } } ] }
```

未激活的已注册场景也会列出,`active: false`。`capabilities` 见 §3.2。

#### `scene.activate`
`{ "scene":"research" }` → `{ "scene":"research", "active":true, "generation":0 }`
幂等。场景 id 未注册 → `SCENE_NOT_FOUND`。

#### `scene.deactivate`
`{ "scene":"research" }`

该场景下存在**活跃**会话时失败:

```jsonc
{ "error": { "code": -32009, "message": "scene has active sessions",
             "data": { "scene":"research", "sessions":["s1","s2"] } } }
```

调用方先 `session.close` 再重试。不提供 `force`。停用只销毁运行时资源
(watcher / MCP 连接 / catalog),**不删除磁盘任何内容**;历史会话在重新激活后仍可
resume。

#### `scene.describe`
`{ "scene":"coding", "project_root":"…", "include_secrets":false }`

返回该 `(场景, 项目)` 组合下的**完整有效配置**,并标注每个字段来自哪一层:

```jsonc
{ "scene":"coding", "project_root":"/Users/x/repo-a",
  "capabilities": { "requires_project":true, "supports_team":true },
  "generation": { "global":3, "scene":7, "project":1 },
  "settings": { /* 完整合并结果 */ },
  "sources": { "model.model_name":"scene",
               "execution.max_api_calls_per_turn":"project",
               "permission_mode":"global" },
  "tools": { "allowed":[…], "disallowed":[…], "deferred":[…] },
  "skills": [ { "name":"…", "source":"project" } ],
  "agent_types": [ { "name":"Explore", "source":"builtin" } ],
  "mcp_servers": [ { "name":"…", "connected":true, "tools":12 } ] }
```

`include_secrets` 默认 `false`:`auth_token` / `api_key` 一类字段脱敏为
`"sk-…abcd"`(保留末 4 位)。设为 `true` 才返回原值 —— 与 `config.getProvider` 同一门控。

---

### 6.3 `session.*`

#### `session.create`

```jsonc
// →
{ "scene": "coding",
  "project_root": "/Users/x/repo-a",     // requires_project 的场景必填非 null
  "name": "重构权限层",                   // 可选
  "options": {
    "permission_mode": "default",        // default|acceptEdits|plan|bypassPermissions
    "permission_rules": [ … ],
    "vcr": { "mode":"replay", "scenario":"…", "dir":"…", "strict":false },
    "telemetry": { "output":"/abs/path.jsonl" } } }
// ←
{ "session_id":"…", "scene":"coding", "session_kind":"primary",
  "project_root":"/Users/x/repo-a", "created_at":"…" }
```

`scene` 必填且必须已激活。场景的 `requires_project` 为真时 `project_root` 必须是
非 `null` 的存在目录,否则返回 `PROJECT_REQUIRED`。`options` 在创建时生效,
之后不可改(改配置走 `config.*` + 会话重建)。

#### `session.run_turn`

**流式方法。** 返回最终响应前先在同一连接推送 0..N 个 `session.event` 帧(§7)。并发约束见 §5.3,顺序保证见 §5.5。

```jsonc
// →
{ "session_id":"…", "turn_id":"t-1",
  "message":"把权限层重构一下",
  "attachments": [
    { "kind":"file",  "path":"/abs/x.png" },
    { "kind":"text",  "path":"selection", "content":"…" },
    { "kind":"image", "media_type":"image/png", "data":"<base64>" } ] }
// ← (全部事件帧推送完毕后)
{ "session_id":"…", "turn_id":"t-1", "stop_reason":"end_turn",
  "api_calls":3, "tool_calls":5, "usage": { … }, "name":"重构权限层" }
```

`session_id` 必填 —— v2 不再隐式创建会话。会话不存在 → `SESSION_NOT_FOUND`;
磁盘上有历史但不在内存中时自动 resume(校验场景)。

客户端在 turn 中途断开:daemon 立即取消该会话的 turn 并杀掉派生的子进程。

#### `session.interrupt`
`{ "session_id":"…" }` → `{ "session_id":"…", "interrupted":true }`

取消当前 turn,**保留会话**。无进行中的 turn 时返回 `{"interrupted":false}`,不是错误。

#### `session.close`
`{ "session_id":"…" }`

```jsonc
{ "closed": true, "sidechains_deleted": 2 }
```

销毁运行实体并释放资源。**父会话的 transcript 保留**,之后仍可 resume ——
但**其全部侧链会被删除**(§3.3)。仍在运行的子 Agent 先取消再删;取消超时(5 秒)
则跳过删除,留给启动 GC 兜底。侧链删除失败不会让 `close` 失败。

#### `session.delete`
`{ "session_id":"…", "dry_run":false }`

**真正删除磁盘 transcript,并级联删除其全部侧链子会话。** 不可逆。

```jsonc
{ "session_id":"…", "deleted":true,
  "sidechains_deleted": 3, "sidechain_ids": ["…","…","…"] }
```

`dry_run: true` 只返回将要删除的清单,不实际删除 —— 供宿主做二次确认。

#### `session.get`
`{ "session_id":"…" }`

```jsonc
{ "session_id":"…", "scene":"coding", "scene_active":true,
  "session_kind":"primary", "parent_session_id":null, "resumable":true,
  "project_root":"…", "name":"…",
  "status":"active", "turn_state":"idle",
  "message_count":24, "created_at":"…", "last_active":"…",
  "usage": { … },
  "config_generation": { "global":3, "scene":7, "project":1 },
  "agents": [ /* AgentRef 简表,见 5.4 */ ] }
```

#### `session.list`

```jsonc
// →
{ "scene":"coding",                 // 可选,省略 = 全部场景
  "project_root":"…",               // 可选;显式 null = 只要无项目会话
  "status":"active|inactive|all",   // 默认 all
  "include_children": false,        // 默认 false = 只返回 primary
  "parent_session_id": null,        // 给定则只返回该会话的侧链子会话
  "limit":100, "cursor":"…" }
// ←
{ "sessions": [
    { "session_id":"…", "scene":"coding", "scene_active":true,
      "session_kind":"primary", "parent_session_id":null, "resumable":true,
      "project_root":"…", "name":"…",
      "message_count":24, "status":"active",
      "created_at":"…", "last_active":"…",
      "live_agents":2, "usage": { … } } ],
  "next_cursor": null }
```

不指定 `scene` 返回进程内**全部**会话,每条带宿主场景。已停用场景的历史会话仍会
返回,标记 `"scene_active": false`;对它们 resume 前必须先 `scene.activate`。

`include_children: true` 时侧链会话一并返回,带 `parent_session_id`,宿主可折叠在
父节点下渲染。

**`parent_session_id` 是事后查子 Agent 的持久路径。** 与 `agent.list` 的分工:

| | 方法 | 数据源 | 进程重启后 |
|---|---|---|---|
| 运行期 | `agent.list {session_id}` | `AgentRegistry`(内存) | 失效 |
| 事后 | `session.list {parent_session_id}` | 扫描 transcript `Meta`(磁盘) | **仍可用** |

拿到子会话 id 后用普通 `session.history` 读内容 —— 侧链与主线共用同一套查询 API。
**不提供**父子合并视图:子 Agent 执行的是一次性固定任务、不与用户交互,逐个查足够。

#### `session.history`
`{ "session_id":"…", "limit":200, "cursor":"…" }`

返回投影后的消息视图(不是原始 JSONL 行):

```jsonc
{ "session_id":"…", "scene":"coding", "session_kind":"primary",
  "messages": [ { "role":"user", "content":[ … ], "ts":"…" } ],
  "next_cursor": null }
```

侧链会话有自己完整的 transcript,用它自己的 `session_id` 查询即可。本方法**不做**
父子合并视图。

#### `session.resume`
`{ "session_id":"…", "scene":"coding", "project_root":"…" }`

把磁盘 transcript 投影回内存并建立运行实体。

```
Meta.scene
  ├─ 缺失(v1 老文件)→ 用请求的 scene,响应带 "scene_inferred": true
  ├─ 与请求不符      → SCENE_MISMATCH,拒绝
  └─ 相符           → 恢复
Meta.project_root
  └─ 与请求不符      → 允许(项目可能被移动),响应带 "project_root_changed": true
Meta.session_kind == "sidechain"
  ├─ 上次是终态收尾  → SIDECHAIN_TERMINAL,拒绝(跑完的一次性任务没有续跑语义)
  └─ 上次是被中断    → 恢复
```

```jsonc
{ "session_id":"…", "scene":"coding", "session_kind":"primary",
  "message_count":24, "scene_inferred":false, "project_root_changed":false }
```

#### `session.fork`
`{ "session_id":"…", "at_message":12 }`

从指定位置分叉出一个**新的 primary 会话**,继承源会话的场景与项目,
`parent_session_id` 指向源会话。

```jsonc
{ "session_id":"<新 id>", "parent_session_id":"<源 id>",
  "session_kind":"primary", "scene":"coding", "message_count":12 }
```

#### `session.respondToPrompt`

```jsonc
{ "session_id":"…", "prompt_id":"…", "decision": { "type":"permit" } }
// decision 四种形态:
//   { "type":"permit" }
//   { "type":"deny", "reason":"…" }
//   { "type":"permit_always", "scope":"session" }
//   { "type":"permit_always", "scope":"local" }   // 写入 settings.local.json
```

`prompt_id` 未知(已超时或重复回答)时静默成功 —— 不是错误。

未在 `permission_prompt_timeout`(默认 300 秒)内回答的询问**失败关闭**:
按拒绝处理,turn 带着一条 error `tool_result` 继续,不会挂死。

---

### 6.4 `agent.*`

子 Agent 有两个把手:`agent_id`(运行期,进程内有效)与 `session_id`(其侧链会话,
持久)。进程重启后 `AgentRef` 消失,但侧链会话还在 —— 用 `session.history` 读内容。

#### `agent.list`
`{ "session_id":"…", "state":"running|all" }`

```jsonc
{ "agents": [
    { "agent_id":"ag_7f3c…",
      "session_id":"…",                    // 侧链会话 id,持久
      "parent_session_id":"…", "scene":"coding",
      "kind":"subagent",                   // subagent | background | team_member
      "label":"Explore#7f3c1a2b", "agent_type":"Explore",
      "state":"running",                   // spawned|running|blocked|completed|failed|cancelled
      "started_at":"…", "ended_at":null,
      "turn_ref": { "turn_id":"t-1", "tool_use_id":"toolu_…" },
      "summary": null } ] }
```

#### `agent.get`
`{ "agent_id":"…" }` → 单条上述结构。不存在或已清除 → `AGENT_NOT_FOUND`
(此时若知道 `session_id`,内容仍可从 `session.history` 读到)。

#### `agent.output`
`{ "agent_id":"…" }` → `{ "agent_id":"…", "state":"completed", "output":"…" }`

宿主侧取回输出,与模型侧的 `TaskOutput` 工具等价。读的是 `AgentRef.summary`
(内存),**不依赖侧链 transcript** —— 所以它的可用期与 `AgentRef` 一致,
不受侧链清理影响。

#### `agent.stop`
`{ "agent_id":"…" }` → `{ "stopped":true }`

#### `agent.send`
`{ "agent_id":"…", "message":"…" }`

向 `background` / `team_member` 形态发消息。对一次性 `subagent` 返回
`INVALID_PARAMS`(它只接受创建时的那一个 prompt)。

---

### 6.5 `team.*`

Team 只在 `supports_team == true` 的场景可用(`coding` / `research`)。这些场景同时
也是 `requires_project == true` 的,所以团队一定有项目锚点 —— 不需要额外的运行时
检查。`supports_team == false` 的场景根本不注册 Team 工具,模型看不到它们;
宿主侧对这些场景调 `team.*` 返回 `SCENE_CAPABILITY_MISSING`。

#### `team.list`
`{ "project_root":"…", "scene":"coding", "session_id":"…" }` — 三个过滤器都可选。

```jsonc
{ "teams": [
    { "team_id":"team-refactor-1754899200", "name":"refactor",
      "scene":"coding", "project_root":"…",
      "owner_session_id":"…", "mode":"batch",
      "status":"completed", "current_stage":2,
      "created_at":"…", "updated_at":"…",
      "members": [ { "label":"a", "lifecycle":"completed",
                     "agent_id":null, "session_id":"…" } ] } ] }
```

`status` 取 `running | completed | failed | cancelled | interrupted`。
`interrupted` = daemon 重启时该团队仍标记 running 但已无活跃成员。

#### `team.get`
`{ "team_id":"…" }` → `team.json` 与 `state.json` 的合并视图,含完整 stage 声明。

#### `team.events`
`{ "team_id":"…", "since":0, "limit":500 }` → `events.jsonl` 分页读取。

#### `team.scratchpad`
`{ "team_id":"…", "stage":1 }` — `stage` 省略返回 `SCRATCHPAD.md` 索引,给定则返回
`stages/NN-<name>.md`。

#### `team.delete`
`{ "team_id":"…" }`

停止仍存活的持久成员 → 删除 `<project>/.atta/teams/<team_id>/` → 移出索引。
团队不存在 → `TEAM_NOT_FOUND`(不静默成功)。

成员的侧链会话**不随团队删除** —— 它们归属发起会话,随该会话的 `session.delete`
级联删除。

---

### 6.6 `config.*`

#### `config.get`
`{ "scene":"coding", "project_root":"…", "tier":"effective" }`

`tier` 取 `global | scene | project | effective`。`effective` 返回合并结果并附
`sources` 溯源表;其余返回该层原始内容。

#### `config.reload`

```jsonc
{ "tier":"scene",             // global | scene | project | all
  "scene":"coding",           // tier=scene 时必填
  "project_root":"…" }        // tier=project 时必填
```

按层传播失效:

```
global  → 全部 (场景, 项目) 组合失效
scene   → (该场景, *) 失效
project → (*, 该项目) 失效
all     → 全部失效
```

```jsonc
{ "ok":true,
  "affected": [ { "scene":"coding", "project_root":"…", "generation":8 } ],
  "warnings": [], "errors": [],
  "mcp_connected": ["fs"], "mcp_failed": [] }
```

失效**惰性生效**:已有会话在下一次 `run_turn` 分派前若空闲则原地重建;忙碌会话跳过
本轮,下次再检查。**正在进行的 turn 不会被中断。**

#### `config.getProvider` / `config.setProvider`

```jsonc
// get → { "scene":"coding", "include_secrets":false }
// set → { "tier":"scene", "scene":"coding",
//         "provider": { "id":"anthropic", "api_type":"anthropic",
//                       "base_url":"…", "auth_token":"…", "models":[…] },
//         "default": true }
```

`set` 写入指定层的 settings.json 并触发该层的失效传播。

---

### 6.7 其余命名空间

以下方法全部接受可选 `scene` / `project_root` 定位参数:

| 方法 | 说明 |
|---|---|
| `mcp.status` | 该 (场景,项目) 组合下 MCP 服务器的连接状态与工具数 |
| `mcp.addServer` | 写入指定层 settings 并连接 |
| `plugin.list` / `install` / `uninstall` / `enable` / `disable` | 插件管理 |
| `commands.list` | 可用斜杠命令(skill 派生 + 内置 + 插件 + MCP prompt) |
| `import.list` / `import.run` | 从 Claude Code / Codex / Cursor 导入配置 |

---

## 7. 流式帧:`session.event`

`session.run_turn` 期间在同一连接推送。**非终止** —— turn 的终止标志是 `run_turn`
的最终响应。

```jsonc
{ "jsonrpc":"2.0", "method":"session.event",
  "params": { "session_id":"…", "turn_id":"…", "event": { "kind":"…", … } } }
```

| `kind` | 载荷 | 说明 |
|---|---|---|
| `text_delta` | `{text}` | 模型输出增量 |
| `tool_use` | `{id,name,input}` | 模型发起工具调用 |
| `tool_result` | `{id,name,content,is_error}` | 工具返回 |
| `prompt` | `{prompt_type,prompt_id,tool_name,message,paths[]}` | 需宿主决策,用 `session.respondToPrompt` 回答 |
| `turn_state` | `{state,prev}` | `running｜blocked｜interrupted｜complete` |
| `agent_state` | `{agent_id,session_id,label,kind_of,state,prev}` | 子 Agent 状态变更 |
| `subagent_progress` | `{agent_label,agent_id,agent_type,parent_turn,event}` | 子 Agent 事件镜像,`event` 是嵌套的本表结构 |
| `team_progress` | `{team,team_id,stage,stage_index,stage_count,status,members[],failed[]}` | 团队阶段生命周期 |
| `compact` | `{strategy,messages_before,messages_after,tokens_saved}` | 发生了上下文压缩 |
| `turn_complete` | `{stop_reason,api_calls,tool_calls,usage}` | 模型侧完成;最终响应随后到达 |

`prompt_type` 目前只有 `"permission"`,但字段刻意保持通用 —— 未来的"停下来问一句"
场景可复用同一帧与同一回答通道,不需新增 RPC。

---

## 8. 异步通知:`daemon.event`

推送给所有调用过 `daemon.subscribeEvents` 的连接,不属于任何会话或 turn。

```jsonc
{ "jsonrpc":"2.0", "method":"daemon.event", "params": { "kind":"…", … } }
```

| `kind` | 载荷 |
|---|---|
| `config_reloaded` | `{tier, scene?, project_root?, generation}` |
| `mcp_connect_failed` / `mcp_connected` | `{scene, server, error?｜tools?}` |
| `scene_activated` / `scene_deactivated` | `{scene}` |
| `scene_degraded` | `{scene, reason}` — 仍激活但部分能力不可用 |
| `import_detected` | `{sources:[{source,description}]}` |
| `session_evicted` | `{session_id, reason}` — `idle_timeout｜lru｜cap` |
| `team_interrupted` | `{team_id, reason}` — 重启后发现的孤儿团队 |

---

## 9. 错误码

| 码 | 名称 | 含义 |
|---|---|---|
| -32700 | PARSE_ERROR | 非法 JSON |
| -32600 | INVALID_REQUEST | 缺 `method`,或 `jsonrpc` 非 `"2.0"` |
| -32601 | METHOD_NOT_FOUND | 未知方法 |
| -32602 | INVALID_PARAMS | 参数缺失/类型错/取值非法 |
| -32603 | INTERNAL_ERROR | 服务端内部错误 |
| -32000 | SESSION_NOT_FOUND | 会话不存在(内存与磁盘都没有) |
| -32001 | SESSION_CAP_REACHED | 达到 `--session-cap` 且无可驱逐会话 |
| -32002 | ENGINE_ERROR | turn 执行期错误,`data` 带引擎错误码 |
| -32003 | UNAUTHORIZED | TCP 握手缺失/失败 |
| -32004 | SCENE_NOT_FOUND | 场景未注册,或未激活 |
| -32005 | SCENE_REQUIRED | 需要 `scene` 但未提供且无法推断 |
| -32006 | SCENE_MISMATCH | resume/fork 的场景与 transcript 记录不符 |
| -32007 | AGENT_NOT_FOUND | `agent_id` 不存在或已清除 |
| -32008 | TEAM_NOT_FOUND | `team_id` 不存在 |
| -32009 | SCENE_HAS_ACTIVE_SESSIONS | `scene.deactivate` 被活跃会话阻止 |
| -32010 | PROJECT_NOT_FOUND | `project_root` 不存在或不可读 |
| -32011 | TEAM_DIR_LOCKED | 团队目录被另一 daemon 实例持有 |
| -32012 | PROJECT_REQUIRED | 场景 `requires_project` 为真,但未给 `project_root` |
| -32013 | SCENE_CAPABILITY_MISSING | 场景不具备该能力(如对 `chat` 调 `team.*`) |
| -32014 | SIDECHAIN_TERMINAL | 侧链会话已终态收尾,不可 resume |
| -32015 | SESSION_BUSY | 该会话已有 turn 在跑(§5.3) |
| -32016 | PLUGINS_DISABLED | 插件子系统不可用——被编译裁掉或被策略关闭(§9.2) |

### 9.1 `error.data` 的形状

以下错误码的 `data` 是**契约的一部分**,客户端可以依赖:

```jsonc
// SESSION_BUSY
{ "session_id": "S1", "current_turn_id": "t1" }

// SCENE_HAS_ACTIVE_SESSIONS
{ "scene": "research", "sessions": ["s1", "s2"] }

// SCENE_MISMATCH
{ "session_id": "S1", "recorded_scene": "chat", "requested_scene": "coding" }

// SCENE_NOT_FOUND
{ "scene": "foo", "registered": ["coding","chat","research","demo"], "active": ["coding","chat"] }

// PROJECT_REQUIRED
{ "scene": "coding" }

// SCENE_CAPABILITY_MISSING
{ "scene": "chat", "capability": "supports_team" }

// PROJECT_NOT_FOUND
{ "project_root": "/no/such/dir", "reason": "not_a_directory" }   // not_found | not_a_directory | unreadable

// TEAM_DIR_LOCKED
{ "project_root": "…", "holder": { "pid": 4210, "instance": "other-daemon",
                                   "acquired_at": "…" } }

// SIDECHAIN_TERMINAL
{ "session_id": "S1c1", "parent_session_id": "S1", "final_state": "completed" }

// AGENT_NOT_FOUND —— 若该子 Agent 的侧链会话仍在,给出 session_id 让客户端改查 history
{ "agent_id": "ag_7f3c", "session_id": "S1c1" }

// SESSION_CAP_REACHED
{ "cap": 32, "active": 32 }

// INVALID_PARAMS —— 带 reason 判别子类
{ "reason": "stale_cursor" }
{ "reason": "attachment_too_large", "limit_bytes": 16777216 }
{ "reason": "program_only_permission_mode", "value": "bubble" }

// ENGINE_ERROR
{ "engine_code": "turn_error", "turn_id": "t1" }
```

其余错误码的 `data` 可能缺省;客户端不应对未列出的形状做假设。

---

## 10. 权限默认值

daemon 会话的默认权限模式是 **`default`(ask)**,不是放行一切。

工具调用触发询问时,daemon 推送 `session.event {kind:"prompt"}` 并**阻塞该次工具
调用**,直到:收到 `session.respondToPrompt`;或超过 `--permission-prompt-timeout`
(默认 300 秒)→ **按拒绝处理**;或会话被关闭/客户端断开 → 按拒绝处理。

超时**永远失败关闭**,不会自动放行 —— 无人应答不等于同意。

宿主自己已做沙箱、不需要第二层权限时显式声明放行:任意层 settings.json 的
`"permission_mode": "bypassPermissions"`(daemon 范围),或 `session.create` 的
`options.permission_mode: "bypassPermissions"`(单会话范围)。

会话级只能**收紧**、不能放宽,除非 settings 里开了 `allow_client_permission_override`。

---

## 11. 完整示例

### 11.1 正常流程

```jsonc
// 1. 发现:读 ~/.atta/daemon/instances.d/*.json,校验进程存活后连上 socket

// 2. 看有哪些场景
→ {"jsonrpc":"2.0","method":"scene.list","id":1}
← {"jsonrpc":"2.0","id":1,"result":{"scenes":[
     {"scene":"coding","active":true,"sessions":0,"generation":0}, … ]}}

// 3. 订阅异步通知(可以是另一条连接)
→ {"jsonrpc":"2.0","method":"daemon.subscribeEvents","id":2}
← {"jsonrpc":"2.0","id":2,"result":{"subscribed":true}}

// 4. 建会话
→ {"jsonrpc":"2.0","method":"session.create","id":3,
   "params":{"scene":"coding","project_root":"/Users/x/repo-a"}}
← {"jsonrpc":"2.0","id":3,"result":{"session_id":"S1","scene":"coding",
   "session_kind":"primary","project_root":"/Users/x/repo-a",
   "created_at":"2026-08-11T10:00:00Z"}}

// 5. 跑一轮
→ {"jsonrpc":"2.0","method":"session.run_turn","id":4,
   "params":{"session_id":"S1","turn_id":"t1","message":"看看 permissions crate"}}

← {"jsonrpc":"2.0","method":"session.event","params":{"session_id":"S1","turn_id":"t1",
   "event":{"kind":"turn_state","state":"running","prev":"idle"}}}
← {"…":"…","event":{"kind":"text_delta","text":"我先看一下"}}
← {"…":"…","event":{"kind":"tool_use","id":"tu1","name":"Agent",
   "input":{"subagent_type":"Explore","prompt":"…"}}}
← {"…":"…","event":{"kind":"agent_state","agent_id":"ag_7f3c","session_id":"S1c1",
   "label":"Explore#7f3c1a2b","kind_of":"subagent","state":"running","prev":"spawned"}}
← {"…":"…","event":{"kind":"subagent_progress","agent_label":"Explore#7f3c1a2b",
   "agent_id":"ag_7f3c","event":{"kind":"tool_use","id":"tu2","name":"Grep","input":{…}}}}
← {"…":"…","event":{"kind":"agent_state","agent_id":"ag_7f3c","state":"completed","prev":"running"}}
← {"…":"…","event":{"kind":"tool_result","id":"tu1","name":"Agent","content":"…","is_error":false}}

// 6. 需要授权
← {"…":"…","event":{"kind":"prompt","prompt_type":"permission","prompt_id":"p1",
   "tool_name":"Bash","message":"运行 cargo test?","paths":[]}}
→ {"jsonrpc":"2.0","method":"session.respondToPrompt","id":5,
   "params":{"session_id":"S1","prompt_id":"p1","decision":{"type":"permit"}}}
← {"jsonrpc":"2.0","id":5,"result":{"ok":true}}

// 7. 结束
← {"…":"…","event":{"kind":"turn_complete","stop_reason":"end_turn",
   "api_calls":3,"tool_calls":5,"usage":{…}}}
← {"jsonrpc":"2.0","id":4,"result":{"session_id":"S1","turn_id":"t1",
   "stop_reason":"end_turn","api_calls":3,"tool_calls":5,"usage":{…},"name":null}}

// 8. 会话仍打开时:查 S1 有哪些子 Agent —— 走磁盘,不依赖 AgentRegistry
//    (S1 一旦 close,侧链即被清除,此步不再返回结果)
→ {"jsonrpc":"2.0","method":"session.list","id":6,
   "params":{"parent_session_id":"S1"}}
← {"jsonrpc":"2.0","id":6,"result":{"sessions":[
   {"session_id":"S1c1","session_kind":"sidechain","parent_session_id":"S1",
    "resumable":false,"name":"Explore#7f3c1a2b","message_count":6}],
   "next_cursor":null}}

// 9. 打开那个子 Agent 的会话查 history —— 与主线会话同一个 API
→ {"jsonrpc":"2.0","method":"session.history","id":7,"params":{"session_id":"S1c1"}}
← {"jsonrpc":"2.0","id":7,"result":{"session_id":"S1c1","scene":"coding",
   "session_kind":"sidechain","messages":[…],"next_cursor":null}}

// 10. 删除对话(级联删侧链),先预演
→ {"jsonrpc":"2.0","method":"session.delete","id":8,
   "params":{"session_id":"S1","dry_run":true}}
← {"jsonrpc":"2.0","id":8,"result":{"session_id":"S1","deleted":false,
   "sidechains_deleted":1,"sidechain_ids":["S1c1"]}}
```

### 11.2 错误路径

写客户端时真正需要处理的六种情况。

```jsonc
// A. 会话忙 —— 用户在生成过程中又点了发送
→ {"jsonrpc":"2.0","method":"session.run_turn","id":10,
   "params":{"session_id":"S1","turn_id":"t2","message":"再改一下"}}
← {"jsonrpc":"2.0","id":10,"error":{"code":-32015,"message":"session is busy",
   "data":{"session_id":"S1","current_turn_id":"t1"}}}
// 正确处理:先 session.interrupt,收到 turn_state:"interrupted" 后再发。
// UI 应在 turn_state != "idle" 时把发送按钮置灰,让这个错误根本不会发生。

// B. 场景需要项目但没给
→ {"jsonrpc":"2.0","method":"session.create","id":11,
   "params":{"scene":"coding","project_root":null}}
← {"jsonrpc":"2.0","id":11,"error":{"code":-32012,"message":"scene requires a project",
   "data":{"scene":"coding"}}}
// 正确处理:先用 scene.list 读 capabilities.requires_project,弹选择项目对话框。

// C. 恢复到了错的场景 —— 最该被拦住的一类
→ {"jsonrpc":"2.0","method":"session.resume","id":12,
   "params":{"session_id":"S9","scene":"coding"}}
← {"jsonrpc":"2.0","id":12,"error":{"code":-32006,"message":"scene mismatch",
   "data":{"session_id":"S9","recorded_scene":"chat","requested_scene":"coding"}}}
// 正确处理:用 data.recorded_scene 重试(必要时先 scene.activate)。
// 不要把它当普通失败吞掉 —— 它意味着 UI 里那条会话挂在了错误的场景下。

// D. 停用场景被活跃会话挡住
→ {"jsonrpc":"2.0","method":"scene.deactivate","id":13,"params":{"scene":"research"}}
← {"jsonrpc":"2.0","id":13,"error":{"code":-32009,"message":"scene has active sessions",
   "data":{"scene":"research","sessions":["s1","s2"]}}}
// 正确处理:遍历 data.sessions 逐个 session.close 后重试。

// E. 权限询问超时 —— 注意这不是 RPC 错误
← {"…":"…","event":{"kind":"prompt","prompt_type":"permission","prompt_id":"p9",
   "tool_name":"Bash","message":"运行 rm -rf build?","paths":[]}}
// (客户端 300 秒内没有调 session.respondToPrompt)
← {"…":"…","event":{"kind":"tool_result","id":"tu9","name":"Bash",
   "content":"Denied by permission: no answer to the permission prompt within 300s",
   "is_error":true}}
← {"…":"…","event":{"kind":"turn_complete","stop_reason":"end_turn",…}}
// turn 正常走完,只是那次工具调用被拒。失败关闭,永不自动放行。

// F. 游标失效(daemon 重启过)
→ {"jsonrpc":"2.0","method":"session.list","id":14,"params":{"cursor":"eyJ…"}}
← {"jsonrpc":"2.0","id":14,"error":{"code":-32602,"message":"invalid cursor",
   "data":{"reason":"stale_cursor"}}}
// 正确处理:丢弃游标,从第一页重新拉。
```
