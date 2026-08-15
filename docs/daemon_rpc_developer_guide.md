# AttaCore Daemon RPC 开发者指南

本文是**任务导向**的集成指南:怎么连上 daemon、怎么跑完一轮对话、怎么正确处理流式
事件与错误。字段的精确定义、类型、错误码表以协议规范为准 ——
本文大量引用它,不重复搬运。

> 协议的权威参考是 [`docs/daemon_rpc_protocol.md`](daemon_rpc_protocol.md)
> (下文简称"协议文档")。本文假设你已经读过它的 §1~§3(传输、发现、通用约定),
> 遇到字段级细节时按章节号跳转过去查。

---

## 1. 集成前要确定的三件事

1. **传输方式**:本机进程用 Unix socket;跨主机或容器化部署用 TCP + `daemon.auth`
   握手(协议文档 §1、§5.1)。两者帧格式完全一样,区别只在建连与认证这一步。
2. **场景(scene)**:你的客户端要接入哪些场景(`coding` / `chat` / `research` /
   `demo`)决定了要不要处理"项目根路径"这个概念(协议文档 §3.2)。如果你的产品
   只做纯聊天,可以完全不管 `project_root`。
3. **单连接还是多连接**:同一个会话的实时事件只推给发起 `run_turn` 的那条连接
   (协议文档 §5.4)。桌面端多窗口场景下,**同一进程内应该只维护一条 daemon
   连接**,窗口是这条连接之上的视图,而不是各开一条连接各自发请求 —— 否则
   非发起窗口永远收不到流式事件,只能退化成轮询。

---

## 2. 找到 daemon 并建立连接

daemon 启动时把自己的实例信息写在 `~/.atta/daemon/instances.d/<name>.json`
(协议文档 §2)。集成的第一步永远是这三步:

```text
1. read_dir ~/.atta/daemon/instances.d/
2. 对每个文件,用 (pid, pid_start_time) 联合判活 —— 只查 pid 会在系统复用
   pid 号时把已崩溃的 daemon 误判为存活
3. 判活成功的条目里,取自己关心的 instance(通常按 socket 路径或 scenes 过滤),
   connect 它的 socket
```

不要自己拼 socket 路径去猜(即便协议文档 §2 给出了推导规则)—— 那是给 daemon
自己启动时用的,客户端应该总是读实例文件拿到手,这样 daemon 换了参数重启也不用
改客户端代码。

TCP 场景下,连接建立后**第一帧**必须是 `daemon.auth`,失败即被断开
(协议文档 §5.1)。Unix socket 不需要这一步。

---

## 3. 最小可用客户端

帧格式是 NDJSON:一行一个 JSON 对象。下面是一个可以直接跑的 Node.js 客户端骨架,
覆盖了"发请求、按 id 收响应、把无关的行当流式事件分发出去"这三件最容易写错的事。

```ts
import * as net from "node:net";
import * as readline from "node:readline";

type PendingResolver = { resolve: (v: any) => void; reject: (e: Error) => void };

class DaemonClient {
  private socket: net.Socket;
  private rl: readline.Interface;
  private nextId = 1;
  private pending = new Map<number, PendingResolver>();
  public onEvent: (method: string, params: any) => void = () => {};

  constructor(socketPath: string) {
    this.socket = net.createConnection(socketPath);
    this.rl = readline.createInterface({ input: this.socket });
    this.rl.on("line", (line) => this.handleLine(line));
  }

  private handleLine(line: string) {
    if (!line.trim()) return;
    const msg = JSON.parse(line);

    // 有 id 且带 result/error → 是某个 call() 的响应;
    // 否则是通知类流式帧(session.event / daemon.event),按 method 分发。
    if (msg.id !== undefined && ("result" in msg || "error" in msg)) {
      const p = this.pending.get(msg.id);
      if (!p) return; // 未知 id,丢弃而不是抛错——见下文“乱序”说明
      this.pending.delete(msg.id);
      if (msg.error) p.reject(new RpcError(msg.error));
      else p.resolve(msg.result);
    } else {
      this.onEvent(msg.method, msg.params);
    }
  }

  call(method: string, params: unknown = {}): Promise<any> {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.socket.write(JSON.stringify({ jsonrpc: "2.0", method, params, id }) + "\n");
    });
  }

  notify(method: string, params: unknown = {}) {
    this.socket.write(JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n");
  }
}

class RpcError extends Error {
  code: number;
  data?: unknown;
  constructor(err: { code: number; message: string; data?: unknown }) {
    super(err.message);
    this.code = err.code;
    this.data = err.data;
  }
}
```

三个容易踩的坑,协议文档 §5.2 有更完整的说明,这里只强调实现上的后果:

- **响应可以乱序返回**,`pending` 必须是按 `id` 索引的 map,不能假设先发先回
  当队列使用。
- **`id` 只需要在同一连接内唯一**,单调递增计数器就够,不需要 UUID。
- **通知(无 `id`)没有响应**——`notify()` 只应该用在 `daemon.ping` 这类无返回值
  的方法上,对 `session.run_turn` 这样需要拿到结果的方法发通知会让 `call()`
  的 Promise 永远不 resolve。

---

## 4. 典型集成流程

一次完整的对话集成,按顺序是:

```text
scene.list                              → 决定要不要弹"选择项目"
session.create  {scene, project_root}   → 拿到 session_id
daemon.subscribeEvents                  → (可选,另一条连接) 订阅 daemon 级通知
session.run_turn {session_id, message}  → 流式收 session.event,最后收最终响应
  ...根据需要重复 run_turn...
session.close   {session_id}            → 关闭时机见下
```

### 4.1 什么时候 `create`,什么时候 `resume`

`session.create` 只在**第一次**新建对话时调用一次。用户下次打开应用、或者切回
一个之前的对话时,走 `session.resume`(不是重新 `create`)。已经在内存里跑着的
会话直接用 `session_id` 调 `run_turn` 即可,daemon 会自动识别"磁盘上有历史但不
在内存中"的情况并自动 resume(协议文档 §6.3 `session.run_turn`)——但**跨场景
恢复**(比如把一个 `chat` 场景的会话当 `coding` 打开)不会自动纠正,会直接返回
`SCENE_MISMATCH`,客户端要用错误里的 `data.recorded_scene` 重试。

### 4.2 `run_turn` 期间的事件处理

`run_turn` 是流式方法:响应会在 0..N 个 `session.event` 之后才到达。用上面
`onEvent` 回调按 `params.session_id` / `params.turn_id` 过滤出属于当前 turn 的
事件,再按 `event.kind` 分发:

```ts
client.onEvent = (method, params) => {
  if (method !== "session.event") return;
  const { event } = params;
  switch (event.kind) {
    case "text_delta":
      appendToTranscript(event.text); // 严格有序,直接拼接
      break;
    case "tool_use":
      renderToolCall(event.id, event.name, event.input);
      break;
    case "tool_result":
      // 按 tool_use_id 配对,不能按数组下标——完成顺序可能和发起顺序不同
      updateToolCall(event.id, event.content, event.is_error);
      break;
    case "prompt":
      showPermissionDialog(event); // 见 §5
      break;
    case "turn_complete":
      markTurnDone(event.stop_reason, event.usage);
      break;
    // agent_state / subagent_progress / team_progress / compact 同理
  }
};

const result = await client.call("session.run_turn", {
  session_id, turn_id: crypto.randomUUID(), message: userInput,
});
// result.stop_reason 到这里才是权威的最终值;turn_complete 事件是提前量,
// 用来更早更新 UI,但两者理论上应该一致。
```

`tool_result` 按*完成*顺序发出而不是提交顺序,这一条是协议文档 §5.5 里最容易
被忽略也最容易导致 UI 渲染错乱的一条约定 —— 按 `id` 配对的实现在事件流和
`session.history`(还原成提交顺序)两边都对,按位置配对的实现只在其中一边对。

---

## 5. 处理权限询问

工具调用需要授权时,daemon 推一个 `kind: "prompt"` 事件并**阻塞那次工具调用**,
直到收到 `session.respondToPrompt` 或超过 300 秒超时(超时按拒绝处理,不会自动
放行,见协议文档 §10)。集成时这是一个独立的 UI 分支,不是错误路径:

```ts
case "prompt": {
  const decision = await showPermissionDialog({
    tool: event.tool_name, message: event.message, paths: event.paths,
  });
  await client.call("session.respondToPrompt", {
    session_id, prompt_id: event.prompt_id, decision,
  });
  break;
}
```

`decision` 有四种形态(协议文档 §6.3),桌面端通常至少要提供"允许一次"和"拒绝"
两个按钮;"总是允许(本次会话/写入 settings.local.json)"是加分项而不是必需项。

**发送按钮的置灰时机**:同一会话同一时刻只能有一个 turn(`SESSION_BUSY`,
协议文档 §5.3、§9),`prompt` 事件到达后 turn 仍然算"进行中"(等待用户决策),
所以判断"能不能发下一条消息"应该看 `turn_state`,而不是看有没有 `prompt` 挂起。

---

## 6. 错误处理

### 6.1 先判断是 RPC 错误还是 turn 内错误

两类失败长得不一样,处理方式也不同:

| 失败类型 | 长什么样 | 典型原因 | 处理方式 |
|---|---|---|---|
| RPC 错误 | `call()` 的 Promise reject,带 `code`/`message`/`data` | 参数错、会话不存在、场景不匹配 | 按 `code` 分支处理,通常是客户端逻辑要改 |
| turn 内错误 | `call()` 正常 resolve,但期间某个 `tool_result` 带 `is_error:true`,或 `stop_reason` 不是 `end_turn` | 工具执行失败、权限超时拒绝、触发预算上限 | 呈现给用户,不代表集成本身有问题 |

### 6.2 必须处理的错误码

协议文档 §9 列了完整错误码表,但集成时**必须**主动处理的只有这几个
(其余大多是编码期就能避免的参数错误):

- **`SESSION_BUSY (-32015)`** —— 见 §5 的"发送按钮置灰"。真发生时正确恢复路径是
  先 `session.interrupt` 再重发,不是重试原请求。
- **`SCENE_MISMATCH (-32006)`** —— resume 到了错的场景。用 `error.data.recorded_scene`
  重试,不要当普通失败吞掉,这通常意味着 UI 里这条会话的场景标记本身就是错的。
- **`PROJECT_REQUIRED (-32012)`** —— 在 `requires_project` 的场景创建会话没传
  `project_root`。提前用 `scene.list` 的 `capabilities.requires_project` 判断要不要
  弹项目选择框,能完全避免这个错误码出现在运行时。
- **`INVALID_PARAMS` 里 `reason: "stale_cursor"`** —— 分页游标跨 daemon 重启失效,
  丢弃游标从头重新拉,不要重试同一个 cursor。

### 6.3 断线与重连

**turn 进行中断线,daemon 会立即取消该 turn**(协议文档 §5.6)——这不是 bug,
是刻意设计:没有接收方的 turn 继续跑没有意义。重连后不存在"接着收剩下的事件帧"
这回事,恢复逻辑固定是:

```text
session.get {session_id}
  turn_state == "idle" → 上一个 turn 已经结束(或被断线取消),读 session.history 尾部
                          补全 UI 未渲染完的部分
  turn_state != "idle" → 不应该发生(断线应该已经把它取消了);
                          如果发生,当成 idle 处理并记录异常
```

daemon 不主动发心跳;长连接空闲期间靠 TCP keepalive 或客户端自己定期发
`daemon.ping` 维持。

---

## 7. 子 Agent 与 Team(如果你的场景 `supports_team`)

对纯聊天类集成可以跳过本节 —— `chat` / `demo` 场景不注册 Team 工具,模型侧看不到
它们,调 `team.*` 会直接拿到 `SCENE_CAPABILITY_MISSING`。

对 `coding` / `research` 场景,子 Agent 的执行会在事件流里表现为 `agent_state` 与
`subagent_progress`(嵌套事件,結构与主流一致,见协议文档 §5.5 末尾、§7)。渲染上
通常做法是:收到 `tool_use {name:"Agent"}` 时开一个折叠的子面板,后续同一
`agent_id` 的 `agent_state` / `subagent_progress` 事件都渲染进这个面板,而不是
混进主对话流。

**父会话关闭后子 Agent 的记录还在不在**是本节唯一容易踩的坑:子 Agent 是"侧链
会话",生命周期锚定在父会话的打开期上 —— `session.close`(不是 `session.delete`)
就会把它删掉(协议文档 §3.3)。如果产品需要"关闭对话后还能翻看它当时调用过哪些
子 Agent",不要指望事后能查到侧链的完整 transcript,只能依赖父会话 transcript
里保留的 `tool_use` / `tool_result` 摘要。要保留完整可查历史,就不要在用户离开
页面时调 `session.close`。

---

## 8. 集成自检清单

上线前过一遍,每一条对应前面某一节:

- [ ] 发现逻辑用 `(pid, pid_start_time)` 联合判活,不是只比 `pid`(§2)
- [ ] 同一桌面进程内的多窗口共享一条 daemon 连接,而不是各自连接(§1)
- [ ] `call()` 的响应按 `id` 匹配,不假设先发先回(§3)
- [ ] `tool_result` 按 `tool_use_id` 配对渲染,不按数组下标(§4.2)
- [ ] `thinking` / `redacted_thinking` 块转发时原样保留,不因为不渲染就丢弃
      (协议文档 §4.1 的警告 —— 下一轮请求需要把它们连同签名回传)
- [ ] 发送按钮的可用性看 `turn_state`,而不是靠等 `SESSION_BUSY` 报错来判断
- [ ] `session.create` 只在真正新建时调用,恢复历史对话走 `session.resume`(§4.1)
- [ ] 权限询问超时后 UI 能正确显示"已拒绝",不会一直转圈(§5)
- [ ] 断线重连后走 `session.get` + `session.history` 补全,不假设能续接事件流(§6.3)
- [ ] 关闭对话前想清楚要不要保留子 Agent 侧链记录(§7)

---

## 9. 延伸阅读

- 字段级精确定义、错误码 `data` 形状、完整请求/响应示例:
  [`docs/daemon_rpc_protocol.md`](daemon_rpc_protocol.md)
- 场景/项目/会话这套架构为什么这么设计:
  `docs/design/2026-08-11-multi-scene-architecture.md`
