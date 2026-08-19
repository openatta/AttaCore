# 插件系统设计

## 0. 概览

三句话：

1. **插件是可分发的能力包，只有两种载荷：WASM 与 MCP。** 不存在"往主进程塞脚本"的第三种。
2. **插件从三个面接入：工具面、事件面、场景面。** 三个面都接在既有机制上——工具面接 `Tool` 注册表，事件面接 `HookRunner`，场景面接 `SceneRegistry`。**主循环不新增任何调用点。**
3. **不带插件必须能构建。** 关掉 feature 时，插件相关 crate 一行不进主进程二进制。

全景：

| 接入面 | 插件贡献什么 | 接在哪个既有机制 | 适用场景 |
|---|---|---|---|
| **工具面** | 模型可调用的 tool | `InMemoryToolRegistry` | 给现有场景加能力，最常见 |
| **事件面** | 生命周期点的观察与否决 | `HookRunner` + `HookConfig` | 审计、合规、策略拦截 |
| **场景面** | 一整套系统提示 / 工具面 / 预算 | `SceneRegistry` | 深度定制，插件自成一格 |

工具面与事件面对**现有场景**开放（CodingScene 等）；场景面**另起一个场景**，不污染现有场景的行为。这条分界是整个设计的支点：**往 CodingScene 里塞自定义提示词只会让它行为古怪，不如让插件在自己的场景里自洽。**

---

## 1. 术语

统一命名，后文与代码都按这套走。

| 术语 | 含义 | 代码/配置中的形式 |
|---|---|---|
| **插件包** | 一个 `plugin.toml` 加它的载荷文件 | 缓存目录 `plugins/cache/<name>/<version>/` |
| **载荷** | 插件包里真正能执行的东西 | `[[wasm]]` / `[[mcp]]` |
| **接入面** | 插件与宿主结合的位置 | 工具面 / 事件面 / 场景面 |
| **能力声明** | 插件在清单里写明它要访问什么 | `[wasm.capabilities]` |
| **插件场景** | 由插件声明内容构成的场景 | `PluginScene`，场景 id 为 `plugin:<name>` |
| **插件宿主** | 主进程侧的插件接缝 | `runtime::plugin_host::PluginHost` |

命名约定：

- WASM 工具：`plugin__<pkg>__<tool>`
- 原生 MCP 工具：`mcp__<server>__<tool>`（既有）
- JS 插件桥接工具：`mcp__dsh-<pkg>__<tool>`
- 插件贡献的权限规则来源：`RuleSource::Plugin`
- 编译开关：`daemon` 的 `plugins` feature
- crate 划分见 §10.1

---

## 2. 隔离与信任模型

一个插件能造成什么后果，取决于它的代码跑在哪里。

| 载荷 | 执行位置 | 隔离手段 | 授权粒度 |
|---|---|---|---|
| WASM component | 主进程内 wasmtime Store | 线性内存 + capabilities 白名单 + epoch 中断 | `plugin__<pkg>` 权限规则 + capabilities |
| MCP server（原生） | 独立进程 / 远程 | OS 进程边界 | `mcp__<server>` 权限规则 |
| MCP server（JS 插件桥接） | 独立 Node 进程 | OS 进程边界 | `mcp__dsh-<pkg>` 权限规则 |

### 2.1 为什么没有第三种

**shell 脚本载荷取消。** hook 曾经是一段直接交给 shell 的字符串，装上就跑、不过权限门、等同用户权限执行任意命令。一个体系里若存在这样一条通道，其它所有隔离努力都是装饰——攻击者不会去攻 WASM 沙箱，会去写一个带 shell hook 的插件。

**hook 本身不取消，取消的是它作为"可分发载荷"的资格。** 用户自己写在 `~/.atta/hooks/` 的脚本继续有效——自己写的脚本和从网上装的包，信任级别不是一回事。插件要参与事件，走 §3.2 的沙箱后端。

**不内嵌 JS/TS 运行时。** 养第二套语言运行时的长期成本，远大于让 JS 生态从进程外接进来的那点不便。JS 生态走 §4.3 的桥接。

### 2.2 故障隔离

这是 WASM 相对同进程插件体系的结构性优势，不是额外做的容错，而是执行模型（§4.1.3 每次调用新建 Store）的自然结果：

| 故障 | 同进程插件体系 | 我们 |
|---|---|---|
| 插件抛异常 | 沿调用链上抛，该步失败 | Store 丢弃，返回一条工具错误结果，主循环继续 |
| 同步死循环 | **卡死整个事件循环**，全部会话一起挂 | epoch 中断打断，同上 |
| 内存爆 | 进程 OOM | `StoreLimits` 触发 trap，同上 |

### 2.3 隔离解决不了的那一项

工具描述和 agent 描述**必须**进入模型上下文，否则模型不知道它们存在。这是提示注入面，沙箱对它无效。

唯一的缓解是安装期审阅：`plugin install` 必须把插件贡献的**全部模型可见文案**原样打印出来（工具描述、agent 描述、场景提示块），并对长度设上限。文档里不要把这一项算进"已隔离"。

---

## 3. 三个接入面

### 3.1 工具面

插件把工具注册进会话的工具注册表，模型像调用内建工具一样调用它们。

`WasmToolAdapter` 与既有的 `McpToolAdapter` 同构——两者都是"外部提供、schema 运行期才知道"的工具，没有理由长得不一样：

- `is_dynamic() -> true`：schema 来自活的 component，随重载变化。
- `check_permissions()` 一律返回 `Ask`，由用户规则放行。**插件自称安全没有意义，判断权在宿主。**
- `is_read_only` / `is_concurrency_safe` 取插件声明值，但**只用于调度**（并发与只读优化），不用于放宽权限。
- `detailed_prompt()` 返回 `tool-def.doc`——插件唯一的文档通道（§3.1.1）。
- **默认 deferred**：只暴露名字与一行描述，模型要用时走 `ToolSearch{query: "select:<name>"}` 拉取完整 schema。第三方 schema 全量塞进每次请求的 `tools` 数组，是按调用次数收的固定 token 税；这套按需机制已经在跑，插件是它最合理的受益者。

#### 3.1.1 插件不贡献 skill

一个插件想教模型怎么用自己的工具——这个需求是真的，但答案不是塞一份 SKILL.md。`Tool::detailed_prompt()` 加 `ToolSearch` 已经是按需拉取的长文档通道，不占每次请求的 token。**插件的使用说明属于它的工具，不属于一份平行的 skill 文件。**

WIT 的 `tool-def.doc` 与 MCP 的 tool description 分别承担这件事，两条路都不需要新概念。

rule（全局行为指令）与 agent 人格描述的是"这个项目/这个人怎么工作"，属于用户的三层目录，插件在工具面没有话语权；要塑造这些，走场景面（§3.3）。

#### 3.1.2 场景归属

`AgentScene::tools()` 是白名单，与注册表**取交集**——插件工具即使注册成功，场景没列它就不可见。规则：

- `CodingScene::tools()` 本就是空列表（"注册什么就允许什么"），插件工具在这类场景自动可见。
- `ChatScene` / `ResearchScene` 用显式白名单，插件工具默认不可见，除非清单 `[scene] visible_in` 点名。
- 场景的 `disallowed_tools()` 永远是最终否决权。

### 3.2 事件面

**主循环一个新调用点都不加。** `crates/hooks` 已经是这层分发机制，而且已经在跑：

- `HookRunner::run(event, input) -> HookRunResult` 是统一分发入口；
- `HookEvent` 有 30 个变体，运行时与工具层已在十余处分发（PreToolUse、PostToolUse、PostToolUseFailure、PreCompact、PostCompact、SubagentStart、SubagentStop、PermissionDenied、TaskCreated、UserPromptSubmit、Stop、Elicitation、FileChanged、WorktreeCreate/Remove）；
- `HookRunResult` 已有 `blocked()` / `approved()` / `updated_input()` / `discontinued()` 四种决议；
- `has_hooks_for(event)` 是"没有订阅者就零成本早退"的现成实现；
- `HookConfig` 已有四种执行后端：`Command`（shell）、`Prompt`（小模型）、`Http`（webhook）、`Agent`（子 agent）。

**做法：第五个变体 `HookConfig::Wasm { plugin, timeout }`。** 插件是一种 hook 执行后端，走既有分发，主循环调用点数量变化为 0。执行体由注入的 `WasmHookExecutor` 提供——`hooks` crate 不能依赖 wasmtime，否则编译裁掉插件的构建里分发器本身就不存在了。

#### 3.2.1 事件白名单

插件**不能**订阅全部 30 个事件。首批开放 6 个，全部低频、语义清晰、载荷窄：

| 事件 | 用途 |
|---|---|
| `PreToolUse` | 策略拦截：否决某次工具调用 |
| `PostToolUse` | 审计：记录结果 |
| `PostToolUseFailure` | 可观测性 |
| `PermissionRequested` | 企业策略的关键点：自动放行或拒绝 |
| `SessionStart` / `SessionEnd` | 初始化与收尾 |

不开放的两类，理由不同：

- **高频点**（逐 token 的流式、每 step 带全上下文）：见 §3.2.3，与故障隔离互斥。
- **改写类决议**：`HookRunResult::updated_input()` 能改写工具入参。这个能力保留给用户自己写的 shell hook，**不给可分发插件**——插件把 `rm -rf /tmp/x` 悄悄改成别的路径，与它否决这次调用是完全不同性质的事。

**扩展点数量设硬上限（当前 6），新增必须写明理由。** 仓库里已有先例可循：`ExecutionParams` 删掉了"约束不了任何东西"的字段，理由是"一个约束不了任何东西的限制比没有更糟——它读起来像个保证"。扩展点同理：留一个没人用又不敢删的点，比不留更糟。

#### 3.2.2 卸载语义

插件在一轮中途被卸载（崩溃踢出或用户禁用），需要定义的状态只有一个：**模型调用了一个已经消失的工具**。答案是返回一条工具错误结果，不是 panic，不是静默成功。

事件订阅的卸载是干净的：`has_hooks_for` 下一次查询就返回空，回退到默认行为。

#### 3.2.3 为什么不开放高频事件

每次调用新建 Store 是故障隔离的**来源**（§2.2）。逐 token 事件意味着每 token 建一个 Store——荒谬；改成长驻实例——就把故障隔离丢了。

**隔离与高频事件二选一，这里选隔离。** 需要流式观察的场景，做成宿主侧聚合后的轮末交付。

另一层成本：跨 WASM 边界不能传引用。给插件"模型将看到的消息"意味着每步序列化搬运整个上下文，100k token 就是 MB 级拷贝。**同进程插件体系对此免费，恰恰因为它没有隔离。**

### 3.3 场景面

#### 3.3.1 为什么需要它

工具面和事件面都是"在既有场景里加东西"。但有一类插件想要的是**另一套行为**：自己的系统提示、自己的工具组合、自己的压缩阈值与轮次预算。

这类需求不能塞进 CodingScene。改了 CodingScene 的上下文提示词，它的行为就会变得古怪——用户以为在用编码场景，实际行为被第三方插件改写了。**不如让插件在自己的场景里自洽。**

这也解决了一个安全上的两难：让插件改**主 agent** 的系统提示等于接管主 agent，绝不可以；但让插件定义**它自己的场景**的系统提示是完全正当的——用户显式选择进入那个场景，**选择即同意**。反对的从来是"劫持"，不是"定制"，插件场景把这两件事分开了。

#### 3.3.2 `PluginScene`

`SceneRegistry::register(&mut self, scene: Arc<dyn AgentScene>)` 是 pub 且接受任意 `Arc<dyn AgentScene>`，所以一个**数据驱动的场景**今天就能注册，不需要改场景机制。

`PluginScene` 是一个带字段的 struct，字段来自插件清单，构造后不可变——`AgentScene` 文档所说的"编译期、不可变"，真正重要的是后半句，`PluginScene` 满足。

它从清单填充这些：

| `AgentScene` 方法 | 清单来源 |
|---|---|
| `id()` | `plugin:<name>` |
| `build_system_prompt()` | `[scene] prompt` 指向的 markdown，切成 `PromptBlock` |
| `tools()` / `disallowed_tools()` / `deferred_tools()` | `[scene]` 的三个列表 |
| `extra_tools()` | 该插件自己的 WASM 工具 |
| `token_budget()` | `[scene.budget]` 的压缩阈值与保留条数 |
| `execution_params()` | `[scene.budget] max_api_calls_per_turn` |
| `build_system_reminder()` | `[scene.own] reminder` |

#### 3.3.3 场景配置层——已经存在的一半

容易被漏掉的事实：`~/.atta/scenes/<scope>/` 本来就是一个**完整的配置命名空间**，下面有独立的 settings、skills、agents、rules、hooks、plugins、mcp；`discover_plugins(global_dir, scene_dir)` 已经在按场景分层。

所以插件场景携带自己的 provider 配置、权限规则、沙箱策略、hook 集合，机制已经在跑，不用新建。

#### 3.3.4 两档野心

这是工具面与场景面的分界，也是插件作者要做的第一个选择：

| | 只提供工具的插件 | 要塑造行为的插件 |
|---|---|---|
| 清单 | 只有 `[[wasm]]` / `[[mcp]]` | 额外有 `[scene]` |
| 可见范围 | 任何场景（受 `[scene] visible_in` 约束） | 只在 `plugin:<name>` 场景内 |
| 能改系统提示吗 | 否 | 是——改的是它自己场景的 |
| 典型 | GitHub 工具集、格式化器 | 领域专家 agent、受管流程 |

#### 3.3.5 场景面的三个代价

1. **一个会话一个场景。** 两个插件想要不同场景，就得两个会话。这是限制，也是隔离——不做"场景合并"。
2. **`SCENE_MISMATCH` 的连带影响。** 会话的 `Meta.scene` 决定它属于哪个场景，插件场景的会话不能被别的场景 resume/fork（见 `docs/session_and_scene_invariants.md` §1）。这是既有不变量，但插件场景会让用户第一次真切碰到它。
3. **提示块是模型可见文案**，落在 §2.3 的注入面里，安装期必须原样展示。

---

## 4. 载荷形态

### 4.1 WASM

#### 4.1.1 选型：wasmtime + Component Model / WIT

Extism 的 ABI 是单一 bytes-in / bytes-out，上手快，但我们要的接口不止一个——`list-tools` / `call-tool` / `validate-input` / `on-event` / 场景内容，语义各不相同。在 bytes ABI 上叠出这些，等于手写一遍没有类型检查的 IDL。

Component Model 用 WIT 描述接口，`wit-bindgen` 在 guest 与 host 两侧同时生成类型化胶水；guest 可以是 Rust / Go / JS（componentize-js）/ Python（componentize-py），不用手写 FFI。Zed 的扩展系统是同一条路的实证。

代价要说清楚：`wasm32-wasip2` 工具链还年轻，Rust 官方把 Wasm Component 目标的稳定化列在 2026 年的项目目标里，WASI 0.3 的原生 async 也是 2026 年才落地。**所以版本协商是第一天就得有的东西**（§4.1.5）。

#### 4.1.2 WIT 世界

```wit
package atta:plugin@0.1.0;

interface types {
  record tool-def {
    name: string,
    description: string,          // 一行，进 tools 数组
    doc: option<string>,          // 长文档，按需经 ToolSearch 拉取
    input-schema: string,         // JSON Schema
    read-only: bool,
    concurrency-safe: bool,
  }
  /// 与 base::tool::ToolResult 一一对应：模型可见内容 + 结构化值 + 错误位。
  record tool-output {
    content: string,                    // 模型可见文本
    structured: option<string>,         // JSON，供程序消费
    is-error: bool,
  }
  variant hook-decision {
    proceed,
    block(string),                      // 理由
    add-context(string),                // 追加一段可归因的上下文
  }
}

interface host {
  log: func(level: string, msg: string);
  progress: func(call-id: string, text: string);
  now-ms: func() -> u64;
  http-request: func(method: string, url: string, headers: list<tuple<string, string>>,
                     body: option<list<u8>>) -> result<list<u8>, string>;
  secret: func(key: string) -> option<string>;
  kv-get: func(key: string) -> option<list<u8>>;
  kv-set: func(key: string, value: list<u8>);
}

interface tools {
  use types.{tool-def, tool-output};
  list-tools: func() -> list<tool-def>;
  call-tool: func(name: string, input-json: string, call-id: string) -> tool-output;
  validate-input: func(name: string, input-json: string) -> result<_, string>;
}

/// 可选导出。未导出即不参与事件面。
interface events {
  use types.{hook-decision};
  on-event: func(event: string, payload-json: string) -> hook-decision;
}

world plugin {
  import host;
  export tools;
  export events;
  export init: func(config-json: string) -> result<_, string>;
}
```

三处刻意的设计：

**`tool-output` 是 record 不是 variant。** 早期草案写成 `variant { text / json / error }`，那会让插件工具无法同时返回结构化值和模型文本——而宿主的 `ToolResult` 本来就有 `content` + `structured_content` + `is_error`。让插件工具比内建工具低一等没有道理。

**文件读写不在 `host` 里。** 那是 WASI `preopen` 管的事，`capabilities.fs_read` 直接映射成 preopened dir。做成 host function 等于自己重写一遍路径校验，而这正是最容易写漏的地方。

**没有 `check-permissions` 导出。** 判断权在宿主，见 §3.1。

#### 4.1.3 执行模型：Engine 常驻，Store 每次调用新建

`wasmtime::Engine` 与编译后的 `Component` 昂贵，进程级单例，跟着 daemon 活；`InstancePre` 预解析链接。但**每次 `call-tool` / `on-event` 新建 Store 和 instance**：

- 可取消：`ToolContext.cancel` 触发时用 epoch 中断打断，Store 整个丢掉，不留半个状态。
- 无泄漏：一次调用崩了或跑飞了，影响面就是那个 Store。
- 并发声明可信：没有跨调用的实例状态，`concurrency_safe` 才不是自我担保。

代价是插件不能把状态放在实例内存里，所以给了 `kv-get` / `kv-set`，状态显式落在宿主侧、按插件名隔离命名空间。这是特意的：**插件的持久状态应该是宿主看得见、清得掉、能随插件卸载一起删掉的东西。**

资源约束三件套：

- `StoreLimits`——内存上限（`capabilities.max_memory_mb`）、实例数、表大小。
- **epoch 中断**——超时与取消。选它而不是 fuel：fuel 按指令计数，语义是"算了多久"；我们要的是墙钟超时和用户按下 Ctrl-C。
- host function 白名单——`http-request` 查 `capabilities.net`，`secret` 查 `capabilities.env`。

冷启动：`Component::serialize` 的 AOT 产物按 `(wasmtime 版本, component sha256)` 为键缓存在 `plugins/cache/<name>/<version>/.aot/`；wasmtime 升级自动失效（反序列化会拒绝版本不匹配的产物）。

异步：`Config::async_support(true)`，`call-tool` 在 tokio 里 await，host function 可以是 async。WASI 0.3 的 guest 侧原生 async 落地之前，guest 看到同步调用、宿主侧异步实现——对"一进一出"的形态足够。

#### 4.1.4 能力声明

`[wasm.capabilities]` 是清单里唯一一段**安装期被人读、运行期被宿主强制**的内容。缺省全空：不声明任何 capability 的 component 只能做纯计算——没有文件、没有网络、没有环境变量。**插件作者要什么就得写出来，写出来就会在安装时被看见。**

#### 4.1.5 API 版本协商

`plugin.api_version` 与宿主支持的版本集合比对，不匹配则拒绝加载并说明原因，**不做静默降级**。WIT world 用 `atta:plugin@0.1.0` 打包版本，component 里的 WIT 元数据是第二道校验——清单撒谎时以元数据为准。

`0.x` 期间**不承诺跨版本兼容**：宿主升小版本可以要求插件重编。这条要写进插件开发手册第一段。

### 4.2 MCP（原生）

`kind = "native"` 时清单里只有一个接入点：一份 MCP server 配置。连接、工具发现、调用、输出缓存全部由既有的 `crates/mcp` 承担，插件系统在这条路上**只做分发与启停**，不参与运行时。

这也解释了 MCP 形态为什么天然满足 §8 的"不带插件也能用"：把插件关掉，用户仍可手写同一份配置塞进 settings。**关掉插件系统失去的是打包分发，不是能力本身。**

现状与缺口：MCP 协议是双向 JSON-RPC，server 可以主动发 `notifications/tools/list_changed`、`sampling/createMessage`、`elicitation/create`。我们**一个都没接**——`McpNotification` / `McpNotificationHandler` / `dispatch_notification` 都在 `crates/mcp/src/manager.rs`，但没有任何代码从 wire 上构造过 `McpNotification`，`connect.rs` 的读循环不解析 notification。

决定：**补 `tools/list_changed` → `refresh_tools()` 这一条**，其余（sampling / roots）暂不做。理由是这一条直接支撑插件热更新——server 改了工具列表不用重启 daemon；而 sampling 是让 MCP server 反向消费我们的模型额度，需要独立的预算与权限设计，不在本轮范围。

### 4.3 MCP（JS 插件桥接）

外部 JS 生态里有一类插件契约：模块导出 `apply(ctx)`，工具用 `defineTool` 定义。bridge 适配的就是这个形状——

```typescript
export const inject = ['tools']
export function apply(ctx: Context) {
  ctx.tools.register(defineTool({
    name: 'greet',
    description: 'Greet someone by name.',
    parameters: { name: { type: 'string', required: true, description: 'The name to greet' } },
    output: {
      schema: { type: 'string' },
      render: (_args, value) => [{ type: 'text', text: value }],
    },
    async execute(args) { return `Hello, ${args.name}!` },
  }))
}
```

这个契约与 MCP 的距离比看上去近：`parameters` 是 JSON Schema 的扁平写法，`render` 的产物 `[{type:'text', text}]` 与 MCP content block 形状几乎一致，`execute` 就是 `tools/call`。

**`atta-dsh-bridge`：一个独立的 Node 进程，对内加载这类插件，对外讲 MCP stdio。**

1. 构造最小 `Context`：`ctx.tools`（`register()` 返回 disposer）、`ctx.effect()`、`ctx.on()`、`ctx.logger`；解析 `inject`。
2. `import()` 插件模块，认三种形态：`export function apply`、`export default { apply }`、`export default class extends Service`。
3. `defineTool` → MCP `tools/list`：`parameters` 的 `{name: {type, required, description}}` 翻成 `{type:"object", properties, required:[...]}`。
4. `tools/call` → `execute(args)` → `output.render(args, value)` → MCP content block。
5. AttaCore 侧**零改动**：它看到的就是一个普通 MCP server，`McpToolAdapter` 原样复用。

**支持边界：我们适配的是 tool 契约，不是那套插件框架的内核。**

`inject` 里出现 `tools` 以外的服务（`llm`、`sessions`、`sandbox`、`agentLoop` 等），bridge **在加载阶段就拒绝并报出缺哪个服务**，不是加载完在运行时崩。理由是取舍而非能力：那张服务图是上游的内部架构，仍在演进且明说契约会变，跟着走等于把我们的生命周期绑在别人的内核上。tool 契约不同——它稳定、可枚举、与 MCP 语义一一对应。

已知落差，写进 bridge 文档：

| 上游契约的空缺 | 我们的处理 |
|---|---|
| 无权限/确认模型 | 走 `mcp__dsh-<pkg>` 权限规则 |
| 无 streaming / progress | 暂不映射，`execute` 返回即结束 |
| 无 abort signal | 取消 = MCP cancellation + 必要时杀进程 |
| `ctx.effect` 清理 | bridge 退出时统一 dispose |

bridge 需要外部有 `node`，这**正是走 MCP 路线的意义**：JS 运行时留在进程外，Rust 侧不因为要支持 TS 插件而多一行依赖。bridge 本身是可选组件。

---

## 5. 清单格式

```toml
[plugin]
name = "github-tools"
version = "1.2.0"
description = "GitHub PR/issue 工具集"
api_version = "0.1"

# 宿主在 init 之前用它校验用户配置；非法配置在加载阶段就报错
[plugin.config]
schema = "config.schema.json"

# ── 载荷 ──
[[wasm]]
component = "github_tools.wasm"
tools = ["diff", "parse-patch"]          # 安装期可见性；运行期以 list-tools() 为准
events = ["PreToolUse", "PostToolUse"]   # 订阅的事件，必须在白名单内

[wasm.capabilities]
fs_read   = ["${workspace}"]
fs_write  = []
net       = ["api.github.com"]
env       = ["GITHUB_TOKEN"]
max_memory_mb = 64
timeout_ms    = 30000

[[mcp]]
name = "github"
kind = "native"
config = "mcp/github.json"

[[mcp]]
name = "pr-helper"
kind = "dsh"
entry = "dist/index.js"
env = ["GITHUB_TOKEN"]

# ── 工具面：在别人的场景里怎么出现 ──
[scene]
visible_in = ["coding"]                  # 缺省 = 仅空白名单场景

# ── 场景面：可选。出现则生成 plugin:github-tools 场景 ──
[scene.own]
name = "GitHub 工作流"
description = "PR 审阅与 issue 分诊"
prompt = "scene/prompt.md"
reminder = "scene/reminder.md"
tools = ["Read", "Grep", "plugin__github-tools__diff"]
disallowed_tools = ["Bash"]
deferred_tools = ["plugin__github-tools__parse-patch"]

[scene.own.budget]
compact_threshold = 120000
compact_keep_recent = 20
max_api_calls_per_turn = 40

# ── 插件声明的 agent 类型 ──
[[agent]]
name = "pr-reviewer"
description = "审 PR 的子 agent"
prompt = "agents/reviewer.md"
allowed_tools = ["Read", "Grep", "plugin__github-tools__diff"]
disallowed_tools = []
model = "..."                            # 见 §7.2
effort = "high"
max_turns = 30
scene = "plugin:github-tools"            # 在插件场景里跑，见 §6.2
```

---

## 6. 激活

三条路径，**全部复用既有机制，不新增激活工具**。

### 6.1 用户显式进入插件场景

```jsonc
// 创建会话时指定
{"method": "session.create", "params": {"scene": "plugin:github-tools"}}
```

需要的改动只有一处：daemon 的 `resolve_scene()` 与 `runtime::agent_tool::scene_by_id()` 目前都是**硬编码闭集**（`coding` / `chat` / `demo` / `research`），要改成查 `SceneRegistry`。两处的注释已经明说它们是刻意保持同一个闭集，所以要一起改。

会话内切换沿用既有的 `scene.activate` / `scene.deactivate` RPC。

### 6.2 主 agent 委派进插件场景

**这条不需要任何新机制——`AgentTypeDefinition` 早就有 `scene: Option<String>` 字段**（`crates/runtime/src/agent_tool.rs`），文档写着"把子 agent 投进哪个场景，覆盖从父级继承的场景；`None` 继承；未知 id 忽略并 warn"，agent 定义的 frontmatter 也已经解析 `scene:` 键。

所以：插件在 `[[agent]]` 里声明 `scene = "plugin:<name>"`，主 agent 用既有的 `Agent` 工具 `subagent_type` 指定它，那个子 agent 就跑在插件场景里，父会话不受影响。

**唯一挡路的还是 §6.1 那个硬编码闭集**——`plugin:` 前缀解析不到就走"未知 id 忽略"分支，静默退回继承。修掉它，这条路径自动通。

### 6.3 只用工具的插件：不需要激活

工具面的插件在 `[scene] visible_in` 允许的场景里自动可见，没有"激活"这个动作。这与 §3.3.4 的两档野心对应：**要激活的是场景，不是插件。**

### 6.4 生命周期

| 阶段 | 动作 |
|---|---|
| 安装 | 下载 → sha256 校验 → 解压进版本化缓存 → **展示全部模型可见文案与 capabilities** → 写 `enabled.json` |
| 加载 | 校验 `api_version` → 校验用户配置（`[plugin.config]`）→ `init(config-json)` → `list-tools()` → 注册工具 / 事件订阅 / 场景 |
| 卸载 | 注销工具（在途调用返回工具错误）→ 取消事件订阅 → 注销场景（已有会话不受影响，直到结束）→ `remove_rules_by_source(Plugin)` → 清 kv 命名空间 |
| 崩溃 | Store 丢弃 → 计数；连续失败超阈值自动禁用并告知用户 |

`PermissionGate` 已有 `add_rules()` 与 `remove_rules_by_source()`，`RuleSource` 是带来源标记的枚举——加一个 `Plugin` 变体，"插件卸载则它的规则全部撤销"就是一行。

---

## 7. 权限与预算

### 7.1 铁律

**能力 = 清单声明 ∩ 用户规则。永远取交集，绝不取并集。**

这条对三个面同时成立：

- 工具面：插件工具的调用受 `plugin__<pkg>` / `mcp__<server>` 规则约束，默认 `Ask`。
- 事件面：插件的 `block` 决议只能**收紧**，`proceed` 不能推翻用户的 deny 规则。
- 场景面：插件场景的工具白名单与用户的权限规则取交集。

### 7.2 插件声明的 agent 不得放宽权限（必须先修）

`crates/runtime/src/agent_tool.rs::apply_agent_type_overrides` 目前是**无条件赋值**：

```rust
if let Some(mode) = def.permission_mode { settings.permission_mode = mode; }
if let Some(max_turns) = def.max_turns { settings.execution.max_api_calls_per_turn = max_turns; }
```

今天这不算问题——agent 类型来自用户自己写的本地文件，用户给自己的 agent 声明 `bypassPermissions` 是他自己的选择。**但插件化之后，agent 类型可以来自网络。** 那时这个函数就是提权路径：声明 `permission_mode = "bypassPermissions"` 的插件 agent，里面所有工具调用直接绕过权限门；声明 `max_turns = 10000` 就是烧 token。

仓库里已有正确做法可对照：`ExecutionParams::max_api_calls_per_turn` 明写场景与设置取 `min`，并解释了为什么不能让一方静默放宽。**agent 类型这条路没遵守同一条规则，插件化之后必须遵守。**

修法是来源感知的钳制：

- `permission_mode`：插件来源**只能收紧不能放宽**（取会话与声明的较严者）
- `max_turns` / `max_api_calls_per_turn`：取 `min`，与场景一致
- 本地目录来源保持现状（用户对自己的文件有完全权力）

同一条钳制适用于插件场景的 `[scene.own.budget]`。

### 7.3 模型选择

插件 agent 与插件场景的 `model` **只能引用已配置的 provider**（`settings.providers` 的键），不能内联 endpoint 与凭据。

但插件**可以贡献 provider 配置**——`settings.providers` 是 `HashMap<String, ProviderConfig>`，插件加一个模型端点就是一次 map 插入，推理路径上没有任何插件代码。两条合起来的效果是：插件可以自带模型，但那个模型必须以一条用户可见、可审计、可删除的配置形式存在，而不是藏在插件代码里。

---

## 8. 不带插件也能构建

硬要求：有些部署必须能证明"这个二进制里不存在插件执行路径"。编译期开关为主，运行期开关为辅。

### 8.1 编译期

- `crates/plugin`（分发与清单）与 `crates/wasm-host`（执行）都是可选依赖。
- `daemon` 的 `default = ["plugins"]`；锁定构建用 `cargo build -p daemon --no-default-features`，插件相关 crate 及其依赖（wasmtime 编译时间以分钟计）完全不参与编译。
- **注意 cargo 的 feature 统一**：`cargo build --workspace` 时只要有任一成员启用该 feature 就会打开。锁定产物必须用 `-p daemon --no-default-features` 单独构建，这条写进发布脚本，并在 CI 加一个"锁定构建"作业防回归。
- `daemon.doctor` 报告 `plugins: compiled-out` / `enabled` / `disabled-by-policy`，让"到底有没有"可验证，而不是靠读发布说明。

### 8.2 接缝必须少到能数出来

插件对主进程的全部贡献收敛成 `base` 里的一个 trait：

```rust
pub trait PluginHost: Send + Sync {
    /// WASM 插件提供的工具（已包装成 WasmToolAdapter）
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
    /// 插件声明的 MCP server 接入点，交给既有的 McpManager
    fn mcp_servers(&self) -> Vec<(String, serde_json::Value)>;
    /// 插件订阅的事件 → HookConfig::Wasm 条目
    fn hook_configs(&self) -> Vec<(HookEvent, HookConfig)>;
    /// 插件贡献的场景
    fn scenes(&self) -> Vec<Arc<dyn AgentScene>>;
    /// 插件声明的 agent 类型
    fn agent_types(&self) -> Vec<AgentTypeDefinition>;
}
```

`mcp_servers` 返回未解析的 JSON 而不是 `McpServerConfig`，是为了不让 `base` 依赖 `mcp`——daemon 那边本来就在做这次解析。

daemon 持有 `Option<Arc<dyn PluginHost>>`，feature 关闭时恒为 `None`。**主进程侧的调用点一共六处**：工具注册、MCP 合并、hook 注册、场景注册、agent 类型合并、`plugin.*` RPC（无宿主时返回 `PLUGINS_DISABLED`）。六处都是 `if let Some(host)`，没有散落的条件编译。

**这条约束反过来也约束以后的设计：任何要求增加第七个接缝的插件能力，先证明它值得破坏这个性质。**

### 8.3 运行期

- `settings.plugins.enabled = false`——整体关闭。
- `settings.plugins.allow = [...]`——白名单，只有列出的包能加载。
- capabilities 默认全空——即便加载了，不声明就什么也做不了。

三层递进：编译期决定"能不能有"，运行期策略决定"允不允许有"，capabilities 决定"有了能干什么"。

---

## 9. 扩展接缝的覆盖盘点

把"插件能改什么"逐条列出来，按**配置式 / 决策式 / 实现式**三档归位。这张表的用处是
让"我们放弃了可替换性"这类粗略说法失效——绝大多数扩展诉求都有降级方案，真正的空白
只有一个。

| 扩展诉求 | 我们的方案 | 档位 |
|---|---|---|
| 注册工具 | 工具面（§3.1） | — |
| 定义 agent | `[[agent]]` → `AgentTypeDefinition` | 配置式 |
| 定制系统提示 | **插件场景**（§3.3） | 配置式 |
| 接新模型端点 | 插件贡献 `ProviderConfig` | 配置式，推理路径零插件代码 |
| 文件访问策略 | 插件贡献 `PermissionRule` + `PreToolUse` 决议 | 配置式 + 决策式 |
| 换命令执行后端 | MCP server 提供执行工具 + 场景 `disallowed_tools` 去掉 `Bash` | 实现式，但在进程外 |
| 换关押机制 | 同上——容器执行的 MCP server 就是一个沙箱后端 | 实现式，但在进程外 |
| 会话存别处 | 事件面广播，插件自己存 | 观察式 |
| 后台任务 | `crates/task` + 后台 agent | 可用不可换 |
| 人向命令入口 | 折进激活（§6）——进入插件场景即入口 | — |
| 常驻终端 | 无对应概念 | — |
| 每 agent 隔离注册域 | `allowed_tools` / `disallowed_tools` | 粒度更粗 |
| 拦截管线 | 事件面（§3.2），限低频、限观察+否决 | 决策式 |
| **不同的循环形状** | **无** | **唯一空白** |

### 9.1 关于沙箱可替换性的澄清

"沙箱"在这里指**agent 要执行的命令关在哪里**，跟插件隔离不是一回事。我们的实现是
`crates/tools/src/bash/sandbox.rs`：macOS `sandbox-exec` + TinyScheme profile，Linux
`bwrap --ro-bind`，Windows 不做。

替换它的意思是换成 docker / gVisor / 远程 VM 去跑 agent 的命令。**WASM 做不了**（插件
没有 OS 权限去建 namespace 或写 seatbelt profile），**MCP 可以**——一个"在容器里执行
命令"的 MCP server 就是一个沙箱后端。缺的开关是"把内建 Bash 换掉"，而场景面正好有：
`disallowed_tools` 去掉 `Bash`，`tools` 放行 `mcp__docker__exec`。

相关的既有缺口见 §10.3。

### 9.2 唯一的空白，以及为什么它是边界而非待办

自定义**循环形状**是唯一既没有降级方案、代价又过高的接缝。没有配置式降级，因为循环
的形状不是参数能表达的；实现式又落在 §3.2.3 的禁区（每步搬运全上下文）。

插件场景给的是循环的**包络**——压缩阈值、保留条数、轮次上限、工具面、系统提示、每轮
提醒、记忆提取。大部分人说"我要自定义循环"，想要的其实是这些。真正要不同**形状**的
（树搜索、辩论式、回溯投票），插件场景也给不了。

**这一条写成设计边界，不是待办事项：要不同循环形状的人，应该 fork 场景或写自己的
harness，不该走插件。**

---


## 10. 实现现状

### 10.1 crate 划分

| crate / 目录 | 职责 |
|---|---|
| `crates/plugin` | 清单解析、版本化缓存、市场、启停状态、安装期披露。依赖极轻，daemon 可编译裁掉 |
| `crates/plugin-host` | 唯一同时认识清单与引擎类型的地方：适配工具、构造场景、翻译 agent 类型、校验配置、分发事件 |
| `crates/wasm-host` | wasmtime 引擎、WIT 绑定、capability 强制、每次调用一个 Store、AOT 缓存、健康记录 |
| `bridges/atta-dsh-bridge` | 独立 Node 进程，把外部 JS 插件当成 MCP server 提供出去 |

`crates/runtime` **不依赖** `crates/plugin`——这是插件子系统能整体编译裁掉的前提。

### 10.2 接缝

插件对主进程的全部贡献经由 `runtime::plugin_host::PluginHost`：五个贡献点
（tools / mcp_servers / hook_configs / scenes / agent_types）加两个服务于它们的
方法（hook_executor / permission_rules）。daemon 持 `Option<Arc<dyn PluginHost>>`，
feature 关闭时恒为 `None`。

新增第六个贡献点需要论证，因为可裁剪性依赖于接缝数量少到能数得清。

### 10.3 三层开关

| 层 | 机制 | 决定 |
|---|---|---|
| 编译期 | `daemon` 的 `plugins` feature | 能不能有 |
| 运行期 | `settings.plugins.{enabled,allow}` | 允不允许有 |
| 每插件 | `[wasm.capabilities]` | 有了能干什么 |

锁定产物用 `cargo build -p daemon --no-default-features` 构建，**不能用
`--workspace`**：cargo 会跨图统一 feature。`tests/scripts/locked_build.sh`
断言依赖图里没有插件 crate，而不是相信 flag。

### 10.4 加载与执行的边界值

| 约束 | 值 | 在哪 |
|---|---|---|
| 并发加载数 | 4 | `plugin_host::MAX_CONCURRENT_LOADS` |
| 加载阶段总时长 | 30s | `plugin_host::LOAD_BUDGET` |
| 连续 fault 后禁用 | 3 | `wasm_host::health::FAULT_LIMIT` |
| epoch 心跳 | 10ms | `wasm_host::EPOCH_TICK` |
| 单次调用超时 / 内存 | 清单声明 | `[wasm.capabilities]` |
| bridge 在途请求 | 8 | `ATTA_DSH_MAX_IN_FLIGHT` |
| bridge 单工具超时 | 120s | `ATTA_DSH_TOOL_TIMEOUT_MS` |

### 10.5 已知边界

- **自定义循环形状**不提供，见 §9.2。
- **`Tool::validate_input` 在引擎里没有生产调用点**，所以 WIT 不导出
  `validate-input`——否则那是一份给插件作者的、永远不会被调用的契约。
- **运行期仍链接 Cranelift**。AOT 缓存让加载不再编译（`Component::serialize`
  的产物按 `(precompile_compatibility_hash, 内容哈希)` 存在插件目录的 `.aot/`
  下），但编译器仍在二进制里，缓存未命中仍会编译。把编译器整个移出运行期
  需要拆一个独立的编译步骤——wasmtime 的 `runtime` 与 `cranelift` 是两个
  feature，只留前者时 `Component::new` 直接不存在，所以"不可能编译"可以是
  编译期保证而不是政策。

## 参考

- [Life of a Zed Extension: Rust, WIT, Wasm](https://zed.dev/blog/zed-decoded-extensions) —— Rust + WIT + wasmtime 扩展系统的实证
- [Wasm Components — Rust Project Goals 2026](https://rust-lang.github.io/rust-project-goals/2026/wasm-components.html) —— 工具链成熟度现状
- [wasmtime::component API](https://docs.wasmtime.dev/api/wasmtime/component/index.html)
- [Extism FAQ](https://extism.org/docs/questions/) —— bytes ABI 路线的对照
