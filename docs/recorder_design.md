# Recorder —— LLM 交互录制与回放

实现位于 `crates/telemetry/src/recorder/`。

---

## 1. 定位

**录像机。** 在 `Model::stream()` 这个接缝上，把每一次 LLM 调用的完整输入与完整输出
按时序落盘；回放时把录像喂回去，得到确定性的重跑。

用途是**定位问题与优化逻辑**——事后回答"这次请求到底带了什么给模型""模型到底吐了
什么""为什么这一轮和上一轮行为不同"。

### 不是什么

- **不是会话状态。** 不参与 resume、fork、上下文重建、压缩。删掉整个录像目录，会话
  照常工作。
- **不是 history 的替代或补充。** `crates/history` 存"对话"，服务业务；recorder 存
  "调用"，服务诊断。两者独立演进，互不引用。
- **不记录工具执行过程。** 工具结果会出现在**下一次调用的 messages 里**，因而被隐式
  完整捕获。单独再记一份是冗余。

### 默认关

无配置时 `RecorderModel` 是纯 pass-through，零开销。

清理由上层负责：一份录像就是一个自包含目录，`rm -rf` 即可。recorder 自身不做保留
策略、不做定时清理。

---

## 2. 为什么落在 Model 接缝上

`Model::stream()` 是 Core 里**唯一同时看得见完整请求和完整响应的地方**：

| | 请求 | 响应流 |
|---|---|---|
| `turn.rs` 装配处 | ✅ | ❌ 把 stream 交给 `execute_stream` 后不再持有 |
| `streaming.rs::execute_stream` | ❌ 只拿到 stream | ✅ |
| `Model::stream()` | ✅ 参数 | ✅ 返回值 |

这不是巧合，是接口边界天然如此。任何别的落点都得把两侧的信息人为拼起来，而拼接点
就是漂移点。

**代价上的收益**：不碰 `LogEntry`，就不碰投影语义。`session.history` 的 `total`、
`session.fork` 的切点、`SessionManager::resume` 的消息数——这些坐标不变量一个都不需
要动，相关的回归风险归零。

---

## 3. 存储布局

一次录像 = 一个自包含目录。

```
<root>/<name>/
    calls.jsonl          调用时间线（append-only）
    blobs/<sha16>        内容块，录像内去重
```

- `<name>`：录像名。生产录制默认取 `session_id`；测试显式指定场景名。**同一套机制**，
  只是名字来源不同。
- 去重范围是**录像内**，不跨录像。多占一点磁盘，换来"删一个目录就干净了"——这正是
  上层清理需要的性质。
- 子 Agent（侧链会话）各自一个目录，靠 header 里的 `parent` 关联。

`root` 由配置给出。生产默认 `~/.atta/<scope>/recordings/`；测试给仓库内的 fixture
路径。

---

## 4. `calls.jsonl` 格式

第一行是 header，其余每行一条记录。所有记录用 `type` 区分。

### 4.1 Header

```jsonc
{"type":"recording","version":1,
 "name":"…","session_id":"…","parent":"…","agent_type":"code-reviewer",
 "created_at":1755500000123,
 "engine_version":"0.2.0"}
```

`version` 用于将来真正的破坏性变更。**新增记录类型与新增字段都不动它**——见 §4.5 的兼容约定。

`parent` 与 `agent_type` 由 `CallOrigin` 带进来，在**第一次调用时**定死（header 随 writer 一次写成）。因此子 Agent 必须在 spawn 时就知道自己的血缘，而不是跑起来再补——`Builder::lineage()` 就是为此存在。

配合每条记录的毫秒时间戳，这两个字段把一个目录下的多份录像变成**一棵树 + 一条时间轴**，而不是一堆互不相干的文件。注意 `ts` 是墙钟：同进程内可比，跨机器不可比（`dt` 用 `i64` 正是因为墙钟会倒退）。

### 4.2 `call` —— 请求

在**发起流之前**写，因为此时请求已经完全确定。这样即使进程崩在流中间，录像里也留着
"崩之前发出去的是什么"——恰恰是诊断崩溃最需要的一条。

```jsonc
{"type":"call","seq":12,"ts":1755500000123,
 "session_id":"S1","turn":3,"step":2,
 "provider":"deepseek","api_type":"openai_compatible",
 "params":{"model":"claude-opus-5","max_tokens":8192,
           "thinking_mode":"off","fallback_model":"claude-sonnet-5",
           "cache_edits":[]},
 "system":["a1b2c3d4e5f6a7b8","c3d4e5f6a7b8c9d0"],
 "tools":"e5f6a7b8c9d0e1f2",
 "messages":["1122334455667788","99aabbccddeeff00"],
 "input_map":{"user_message":7,
              "spans":[{"source":"user_prompt","block":0,"range":[0,512]},
                       {"source":"mcp_resource","block":0,"range":[512,930]},
                       {"source":"attachment","block":1}]}}
```

`session_id` 是每条 call 都带的，不是只在 header 里说一次：显式命名的录像会把父会话
和它的子 Agent 装进同一个文件，回放要按会话切片（§6），文件就得逐条说清楚。

`provider` 是 settings.json 里的 provider **名字**，路由配置过时如此；没有路由时退回
api_type 的字符串化。只记 api_type 分不出两个不同的 openai-compatible 端点。

`purpose` 只在**不属于任何 turn 的调用**上出现（§7），此时 `turn`/`step` 为 0。

### 输入的组成如何被标注

块的边界一直都在，但**边界不等于身份**：读的人只能数第几块，而块数随配置浮动（rules、
MCP、prompt_append 都是可选的，scene 的 dynamic 段还会合并）。所以每一块都自己说明来
源：

| 位置 | 怎么标 |
|---|---|
| `system[]` 的每个 blob | `PromptBlock.source` —— `scene` 各 section 名 / `skills` / `memory` / `rules` / `mcp` / `prompt_append` / `prompt_override` |
| `tools` blob 里每个工具 | `ToolDef.source` —— `builtin` / `mcp:<server>` / `plugin:<id>` |
| 用户消息 | `input_map` 的字节区间 —— `user_prompt` / `mcp_resource` / `attachment` |

**标注一律是旁路字段或坐标，永不进内容。** 这条是硬约束，不是风格偏好：两个 adapter
把 `PromptBlock` / `ToolDef` 翻译成 wire 格式时都是显式逐字段映射，多出来的字段根本不
会被复制，所以带标注的请求与不带标注的请求**逐字节相同**，缓存前缀照常命中。
（`crates/model/src/openai/request.rs` 里有一条断言把这件事钉住。）

用户消息用坐标而不是拆 block，也是同一个约束：把 MCP resource 拆成独立的
`ModelContentBlock` 会改变实际发出去的结构。`input_map.user_message` 在**请求装配时**
解析（按内容指纹回找），不是在消息入队时记下标——压缩会在这中间重写消息列表，早记的
下标会指向别的消息。压缩把这条消息折没了就没有 `input_map`，这是诚实的结果。

`system` 是**按块的引用数组，不是 join 后的整串**。把 `PromptBlock` 拼成一个整串会让
块的边界与来源消失——系统提示词、skills 清单、MCP 指令混成一坨。

`tools` 是整张工具表一个 blob。工具表是整体变化的（scene 切换、MCP 连上、plugin
reload），拆到每工具一个 blob 只会让每条 call 多出几十个引用而换不到多少去重。

### 4.3 `chunk` / `chunks` —— 响应

流到达的过程中**增量追加**。先把整条流收齐再落盘会让录制改变交互的延迟特征，也会
在进程中途死亡时丢掉全部响应——必须边转发边写。

单条：

```jsonc
{"type":"chunk","seq":41,"ts":1755500000200,"call":12,
 "chunk":{"kind":"thinking_signature","signature":"…"}}
```

打包的连续同类增量，按类型分成三种行 —— `text_chunks` / `thinking_chunks` /
`tool_args_chunks`：

```jsonc
{"type":"text_chunks","seq0":42,"ts0":1755500000212,"call":12,
 "texts":["Hel","lo"," wor","ld"],
 "dt":[12,8,15]}
```

`tool_args_chunks` 多一个 `id`（run 内共享的 tool_use id），成员字段叫 `args`。

`seq0 + k` 还原第 k 个成员的 `seq`，`ts0` 加前 k 个 `dt` 还原时间戳。

**`texts` 永远不 join。** token 边界是数据不是噪声——它是判断"模型在哪里犹豫了""哪
一段是缓存命中"的依据。为了可读性合并连续 `TextDelta` 在测试固件里是合理取舍，在录
像里是丢证据。

打包阈值：连续 ≥3 条才打包，不足则原样逐条写。低于 3 条时打包行的信封开销已经和它
替代的行相当。**这是格式常量不是可调参数**——两种布局解码结果完全相同，改动它不会
让已有录像失效。

### 4.4 `end` —— 调用结束

```jsonc
{"type":"end","seq":58,"ts":1755500001456,"call":12,
 "outcome":{"status":"ok"},
 "stop_reason":"tool_use",
 "usage":{"input_tokens":12043,"output_tokens":387},
 "duration_ms":1333}
```

`outcome` 是带判别式的对象：`{"status":"ok"}`、`{"status":"cancelled"}`，或
`{"status":"error","error":"overloaded",…}`——错误按 `ModelError` 的变体展开，回放时
能原样重建。

流被丢弃而没读完（turn 被打断、调用方停止读取）记 `cancelled`，这样每条 `call` 都有
配对的 `end`。

**失败的调用也要记。** overloaded 触发 fallback 模型重试时，失败的那次是一条完整的
`call` + `end{outcome:"error"}`，重试是另一条 `call`。录像里因此看得见重试的全过程。

只记成功调用的录像回放不出失败路径——而失败路径恰恰是最需要复现的那条。显式记
`outcome` 让"这次 overload 了、于是换了模型"能原样重放。

### 4.5 未知记录类型

读端遇到不认识的 `type`：**跳过并 warn，不中断**。

这与 `history` 的取舍相反，而且是有意的。history 的日志是 resume 的依据，静默跳过
一条必需记录会重建出一个错的会话，宁可拒绝加载；录像是旁路诊断数据，读不懂的部分
跳过仍然能看懂其余部分，整份读不出来才是更差的结果。

代价是真正损坏的行也会被静默跳过，所以 warn 里必须带行号。

---

## 5. 内容寻址

`blobs/<sha16>` —— SHA-256 前 16 位十六进制，内容是该项的规范 JSON。已存在则不重写。
机制与 `history::entry::PasteStore` 相同（`entry.rs:245`），但 recorder 自带一份：
`crates/telemetry` 目前只依赖 `base`，不该为了 50 行去引入对 `history` 的依赖。

三类 blob：

| 引用位置 | 内容 |
|---|---|
| `system[]` | 一个完整 `PromptBlock`（含 `role` 与 `cache_strategy`） |
| `tools` | 完整 `Vec<ToolDef>` |
| `messages[]` | 一条完整 `ModelMessage` |

**内容原样存，不做归一化替换。** 把 CWD/HOME/日期/git 状态换成占位符是哈希匹配方案
为了跨机器稳定才需要的；本设计的回放不靠哈希查表（§6），这层替换在存储路径上没有存
在理由——而录像的价值恰恰在于"实际发出去的是什么"。

### 为什么是内容寻址而不是"变化时快照"

另一种常见做法是只在信封变化时写一条快照，读时 fold 回来。等价的效果，本设计用内容
寻址达成，理由有三：

1. **没有变化判定逻辑可以写错。** 快照方案要维护一套信封指纹与相等性判断；Core 还有
   **三个** `model.stream()` 调用点（主路径、overloaded 回退、prompt-too-long 回退），
   分散的判定必然在边界条件上分叉。内容没变就自然指向同一个 blob，"一个会话只有
   1–3 份信封物理拷贝"是结果而不是需要维护的不变量。
2. **正确处理"变回来了"。** scene 切走再切回，快照方案会写出第三条冗余快照；内容寻
   址直接指回第一个 blob。
3. **顺带解决 messages 的 O(n²)。** 每次调用都带完整 messages，第 50 次调用里那 49
   条历史消息全是已存在 blob 的引用。50 次调用的会话从约 2.5 MB 的重复文本降到约
   100 KB。

---

## 6. 回放：序列匹配

**按顺序回放：第 k 次实时调用取录像里第 k 条 `call` 的响应。** 不做哈希查表。

### 序列是每会话一条，不是整份录像一条

一份录像可能装着多个会话（显式指定 `name` 时，父会话与子 Agent 都写进同一个文件）。
它们在文件里的交错顺序是**调度运气，不会重现**，所以每个实时会话只走属于它的那个子序
列，交错顺序允许不同。

实时会话与录像里的会话如何配对：**id 相同的直接对上**（用同一批 session id 驱动回放时
就是精确匹配）；对不上的按**首次调用的先后**依次绑定——这是实时运行唯一能保证的顺序
（父会话必然先于它派生的子 Agent 发出调用）。并发的兄弟子 Agent 会让这个绑定变得不确
定，这是已知边界。

### 手改录像：`replay.override.json`

调测时常要问"如果模型这里说的是别的，下游会怎样"。直接改 `calls.jsonl` 能回答，但把录
像毁了；改 blob 更糟——blob 是内容寻址且去重的，改一个会**同时改掉所有引用它的调用**。

所以覆盖写在**另一个文件**里，录像原样不动，编辑本身成为一份可评审、可丢弃的产物：

```jsonc
// 整体替换
[{"response":[{"kind":"text_delta","text":"…"}]}]

// 或按位置打补丁
{"patches":[{"at":3,"response":[{"kind":"text_delta","text":"…"}]}]}
```

`at` 是**录像自身顺序**里的 0-based 下标（`calls.jsonl` 里数第几条 call），**不是**回
放按会话切出来的子序列下标——覆盖是对一个文件的编辑，不该因为回放换了种分组方式就改变
含义。

下标越界或重复直接失败：覆盖是人有意写的，静默忽略只会让人对着回放输出找一处从未生效
的编辑。重录会重新编号，所以旧覆盖文件配新录像必然报错，这正是想要的。

**覆盖改的是响应，不是请求。** "改掉某个 system 块、看模型输出怎么变"是另一条路径 ——
见 §6.1。

---

## 6.1 Rerun：拿录像的请求去打真 API

回放证明"这次跑能重现"。它回答不了录像真正引出的另一个问题：**如果提示词不一样，模型
会说什么？**

原因是结构性的：**回放从不读请求的内容**——它比的是 blob **id**，返回的是录好的响应。
所以改 blob 内容对回放毫无影响（id 是文件名，没变），而它返回的响应本来也不是请求的函
数。

`Rerun` 模式把请求从 blob 里读回来（手改在这一步生效），真发出去，再报告变了什么：

```
录制 → 编辑 blobs/<id> → rerun → 读 diff
```

```rust
let request = rerun::load_request(&dir, k)?;   // 读回第 k 条，含手改
let (diff, live) = rerun::rerun_one(&model, request, cancel).await?;
println!("{}", diff.report());                 // 空串 = 没变
```

要点：

- **录像是输入，不是输出。** rerun 不往录像里写任何东西——上例跑完，`calls.jsonl` 与
  `blobs/` 一字节未变。想留新结果就另录一份。
- `model` 必须是**真 provider**，不能是回放模式的 `RecorderModel`——那样每个 diff 都是
  空的。
- rerun 发出的调用带 `purpose: "rerun"`，下游若在录制不会被误当成 turn 工作。

### 6.2 判定：两半，反着来

这是整个比对策略的核心，也是最容易做错的地方。

| 层 | 比什么 | 怎么判 |
|---|---|---|
| 工具名序列 | 精确 | 硬失败 |
| **工具参数（全部 key）** | 规范化 JSON 后精确 | **硬失败，不交判官** |
| `stop_reason` | 精确 | 硬失败 |
| 文本回复 | 语义 | 交判官 |

**为什么工具调用不能交给语义判官。** 一次工具调用是一个**动作**，不是一段话。
`Write(path="src/main.c")` 和 `Write(path="src/Main.c")` 不是"差不多的意思"——一个能
编译，另一个不能。判官被问到这两者时会说"都是创建主程序"，而这恰恰是最该抓的那类回归。

参数比对做了两件规范化：key 排序（顺序是序列化细节，模型不控制）、值取规范 JSON
（`1.0` 和 `1` 是同一个参数）。除此之外一律精确。

**已知代价，是有意接受的。** 生成的文件内容与自由文本的 description 每次都不同：

```
Write    content=93~313字符, file_path=~100字符
Bash     command=10~86字符,  description=9~33字符
```

`content` 和 `description` 会稳定判失败。所以报告**逐 key 展开**：

```
Write: arguments differ
    ✓ file_path    recorded src/main.c, live src/main.c
    ✗ content      recorded 313 chars, live 297 chars
```

看一眼就知道是良性漂移（`content`）还是真回归（`file_path`）。判定不放水，信息量补上。

### 6.3 判官只在需要时出场

判官自己也是 LLM，也会翻。三道闸：

1. **字面相同先过滤**——文本逐字节一致就不叫判官。零成本零抖动，吃掉相当比例的调用。
2. **判官只看文本层**——最容易翻车的工具层已交给确定性规则。
3. **结构化输出** `{"equivalent": bool, "reason": "…"}`。解析不出来**判不一致**——
   "通过"是没人会去查的那个结果，不能当兜底。

### 6.4 每条调用独立重跑

**每一条都跑，包括第一处分歧之后的。**

理由是隔离 rerun 的语义：第 k 条发出去的是**录像里存的输入**，其中的工具结果来自**录像
里**第 k-1 条，不含本次跑出来的任何东西。所以"给定这个确切输入，答案还一样吗"对每个 k
都是独立成立的问题，前面有没有分歧都不影响它。

这条曾经写反过。先前的实现在第一处分歧后停下、把余下的标 `downstream` 不跑，理由写的是
"前提不会发生"——那是**级联**语义下的道理（真接着往下跑、真执行工具），而这里是**隔离**
语义，两者被混了。代价很实：一份 12 条的录像只有 1 条被真正检查，而那 1 条恰恰是最不稳
的开场调用。

分歧之后的调用仍然标记（`↓`），因为"本次这段对话不会这样展开"是读报告时该知道的上下文
——但标记不再吞掉判定。

### 6.4.1 开场调用单列

跨全部录像观察到一个稳定现象：**分歧几乎全部落在会话的第一条调用，且都是工具选择不同**
——`Bash` vs `Write`（要不要先 `ls`）、`TodoWrite` vs 直接作答、`ToolSearch` vs 直接派
子 Agent。

这不是回归，是模型只凭提示词选第一步时的自由度。往后的调用输入里带着具体的工具结果，
模型的活动空间小得多。所以报告把开场调用（标 `开`）单列，并额外给一个"开场之外的分歧
数"——**真正值得先看的是那个数**。把两者平均起来，只会让真回归淹没在开场的噪声里。

顺带一个提醒：开场分歧里有本次比录像**更好**的情况（`000` 的用例文件明写"不需要在创建
文件前先用 ls"，而录像里的模型偏偏 `ls` 了）。这正是 §6.5 那条边界的活例子——rerun 只
说"变了"，不说"变对还是变错"。

### 6.5 rerun 验证的是"录像还成立吗"，不是"模型答得对吗"

录像就是基准。录制时模型如果答错了，rerun 照样说一致。**正确性**由 `.test` 里人写的
预期块和 `comparator.rs` 负责。

配合 replay，两者是一套东西的两半：

| | 比什么 | 抓什么 |
|---|---|---|
| replay（strict） | 引擎组装的请求 vs 录像里的请求（blob id） | **引擎变了** |
| rerun + 判官 | 同一输入下的新输出 vs 录像里的输出 | **录像坏了 / 模型漂了** |

rerun 发的是录像里存的输入，所以它对引擎变化是**盲的**；replay 只比输入、不看输出。
合起来才是完整的：`录像正确 = 输入侧对得上 + 输出侧对得上`。

### 手改 blob 的两个约束

1. **blob 是去重的**：同一块 system prompt 在 50 次调用里是**同一个文件**。原地改会同
   时改掉所有引用它的调用。只想改第 k 条，就新建一个 blob 文件、再改 `calls.jsonl` 里
   那一条的引用。
2. **改过的目录不要再用来录制**：读 blob 不校验哈希（这正是手改可行的原因），但再次录
   制时 `put_raw` 会发现文件内容与文件名对不上，**warn 一声并写回正确内容**——你的编辑
   就没了。要留就先拷走。

### 收尾断言

实时跑的调用数**少于**录像的情况，逐条检查全都通不出来：没有分歧，也没有用尽。
`Recorder::unconsumed()` 在跑完之后报告哪个会话只走了 n/m 条——这个方向的错只能在结尾
问。

### 为什么放弃哈希匹配

前一版实现把请求哈希当主键，代价留在了仓库里：录制数据的轮次目录名是一部与哈希不
稳定搏斗的编年史（`2026-08-10-fixed`、`2026-08-11-dehydrate-fix`、
`2026-08-11-postfix`）；换个模型名就必定全 miss，因为模型名编进了哈希；而哈希本身无
法反推出"哪个字段变了"，miss 了只知道 miss。

维持哈希稳定需要一整套归一化正则（CWD、HOME、日期、OS 版本、git 分支、git status
块、知识截止时间……）。每加一处动态内容就要补一条正则，漏一条就是一轮全 miss。

序列匹配不需要这些，所以那套正则管线整个不存在。

### 分歧处理

录像里存着**完整请求**，所以实时请求与录像对不上时，能指出**差在哪一项**，而不是只
说"不匹配"。比较在 blob id 这一层进行——id 相同即内容相同，不必读回内容：

```
recorder: call #12 diverges from recording "run":
  system[2]: recorded c3d4e5f6a7b8c9d0, live 9f8e7d6c5b4a3928
  messages: recorded 8 messages, live 9
```

要看具体差在哪几个字节，拿报告里的两个 blob id 去 `blobs/` 里对比即可。

策略：

| `on_divergence` | 行为 |
|---|---|
| `strict` | 硬失败，打印字段级 diff。CI 与 `cargo test` 下的默认 |
| `warn` | 打印 diff，继续用录像的响应。开发者手工回放时的默认 |

### 录像用尽

实时跑的调用数超过录像条数时报错，并说明录像有几条、实时要第几条。

---

## 7. 会话与子 Agent

序列是**每会话独立**的，不是全局的。子 Agent 的调用会与父会话交错，全局序列会错位。

因此 `Model::stream()` 必须知道调用属于哪个会话。`StreamParams` 带一个坐标：

```rust
pub struct StreamParams {
    // 既有字段不变
    pub origin: Option<CallOrigin>,
    pub input_map: Option<InputMap>,
}

pub struct CallOrigin {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub agent_type: Option<String>,
    pub kind: CallKind,
}

pub enum CallKind {
    Turn { turn: u32, step: u32 },
    Auxiliary { purpose: String },
}
```

比在装饰器上开 setter 干净：每个 stream 调用点都必须显式传，漏不掉。`step` 就是 turn
循环里已有的 `api_calls` 计数。

### 辅助调用：分类，不是丢弃

压缩摘要、记忆抽取、权限分类、会话命名、hook —— 这些不属于任何 turn，但**照样是真实
的 LLM 调用，照样录**。给它们编 turn/step 会让坐标说谎，所以用 `kind` 判别：
`Auxiliary { purpose }` 保留会话身份，同时明说这不是 turn 工作。读端按 `purpose` 过滤
掉自己不关心的部分。

`purpose` 是**自由字符串**，不是枚举：这个集合随引擎生长，做成枚举意味着每加一种就要
改 `crates/core`。约定值列在这里，新增只改文档：

| purpose | 来源 |
|---|---|
| `compact` | 压缩摘要（`compaction::llm_summary`） |
| `memory` | 记忆召回/抽取（`base::interface::memory`） |
| `classify` | 权限分类（`permissions::llm_classifier`） |
| `title` | 会话命名 |
| `hook` | `"type": "prompt"` hook（`runtime::hook_executors`） |
| `team_judge` | team 聚合的 pick/merge（`team::coordinator`） |
| `rerun` | §6.1 从录像重发的请求 |

带 session 身份的做法统一是**在构造时把 session 交给持有者**（`LlmSummarizer::for_session`、
`LlmClassifier::for_session`、`ModelPromptHookExecutor::new`），而不是给调用方法加参数：
这些组件本来就是每会话一个，而 `AutoClassifier::classify` 这种窄接口没有理由长出一个
session 参数。

**`purpose` 不参与回放的分歧比对**（`diverges()` 只比 params / system / tools /
messages）。它是调用的身份标注，不是请求内容；给一个分类改名不该让通过的回放变红。

### 没有会话身份的调用

`origin: None` 现在只有一个含义：**这个调用说不出自己属于哪个会话**（权限分类器与
team judge 的调用路径目前如此——它们的 trait 签名不带 session）。这种调用**不录**，直接
透传。

以前它们和所有同类一起落进一个共享的 `unattributed` 目录。那个目录有两个问题：把互不
相干的调用归到一起；而 writer 打开文件时是 truncate 的，两个并发会话用它就会互相清空。
说不出归属就没有录像可归属，直说比编一个好。

录制时每个会话名对应一个 writer，子 Agent 因此天然落进自己的目录，而不是和父会话交
错进同一份录像。

---

## 8. 配置

```rust
pub struct RecorderConfig {
    pub mode: RecorderMode,
    pub name: Option<String>,
    pub root: PathBuf,
    pub on_divergence: Divergence,
}

pub enum RecorderMode { Record, Replay, Rerun }
pub enum Divergence { Strict, Warn }
```

`name: None` 表示用 session id。

`root` 的解析顺序（`daemon::session_pool::recordings_root`）：

1. 调用方在 `session.*` 的 `options.recorder.dir` 里显式指定（测试固件走这条）；
2. `settings.recorder.root`；
3. `ConfigPaths::global_recordings_dir()`，即 `~/.atta/recordings/`。

`dir` 因此是**可选**的：想开录像的宿主不必再自己编一个路径。**但录像默认仍然是关的**
——配置只决定"开了往哪写"，不决定开不开（理由见 §12）。

环境变量：

| 变量 | 含义 |
|---|---|
| `ATTA_RECORDINGS_DIR` | 录像根目录。**不设则整个 recorder 不启用** |
| `ATTA_RECORD=<name>` | 录制 |
| `ATTA_REPLAY=<name>` | 回放 |
| `ATTA_REPLAY_STRICT=1` | 强制分歧即失败 |
| `ATTA_REPLAY_WARN=1` | 强制分歧只告警 |
| `ATTA_RECORDER_AUTO_DETECT` | 按测试环境处理（默认严格） |

默认策略由 `RecorderModel::default_divergence()` 独家决定，调用方**问它而不是自己重
新推导**——重新推导正是同类开关曾经变成静默空操作的原因。规则：**凡是分歧意味着测试
坏了的地方（cargo test 下、CI 下）默认严格**；开发者手工回放时默认宽松，
这样一个全新用例第一次本地跑不需要先有录像。

---

## 9. 代码分布

| 位置 | 职责 |
|---|---|
| `crates/telemetry/src/recorder/mod.rs` | `Recorder` 共享状态 + `RecorderModel` 装饰器：录制、按会话回放、分歧 diff |
| `crates/telemetry/src/recorder/blob.rs` | 内容寻址存储 |
| `crates/telemetry/src/recorder/format.rs` | `calls.jsonl` 的记录类型 |
| `crates/telemetry/src/recorder/override_doc.rs` | `replay.override.json` 的解析与套用 |
| `crates/telemetry/src/recorder/rerun.rs` | 从 blob 重建请求、重发、响应 diff |
| `crates/telemetry/src/recorder/pack.rs` | 增量打包编解码、行号分配 |
| `crates/telemetry/src/recorder/writer.rs` | 追加写入与 flush 纪律 |
| `crates/telemetry/src/recorder/reader.rs` | 读回，跳过读不懂的行并计数 |
| `crates/core/src/interface/settings.rs` | `RecorderConfig` / `RecorderMode` / `Divergence` |
| `crates/core/src/interface/model.rs` | `CallOrigin` / `CallKind` / `InputMap` / `ToolDef::source` |
| `crates/core/src/interface/prompt.rs` | `PromptBlock::source` 与 `source::*` 常量 |
| `crates/core/src/provider.rs` | `TaskRouter::map_models` —— 让装饰器覆盖所有被路由的 model |
| `crates/core/src/tool.rs` | `Tool::source()` |
| `crates/scene/src/scene/coding.rs` | section 名带进 `PromptBlock::source` |
| `crates/model/src/adapter.rs` | 旁路转发工具参数原始片段 |
| `crates/runtime/src/turn.rs` | 三个 stream 调用点填 `origin`；采集 `input_map` |
| `crates/runtime/src/agent_tool.rs` | spawn 时把血缘交给子 Agent |
| `daemon/src/rpc.rs` / `session_pool.rs` | `RecorderOptions`、root 解析、包住 router 的每个 provider |

### 为什么每个 provider 都要包

`session_pool` 曾经只把对话用的 model 包进 recorder，却把**未包装的** `TaskRouter` 一
并交给 `Builder`。于是配了 `providers` / `task_models` 之后，子 Agent、压缩、记忆抽取
走的是另一个实例，一条都不录，还不报错。

现在两边都包，并共享同一个 `Arc<Recorder>` —— 共享是必须的：writer 打开文件时 truncate，
各包各的会让第二个把第一个的录像清空。

### 关于行号

打包行承诺 `seq0 + k` 是第 k 个成员的行号，所以一行的号必须**整块一次分配**
（`next_seq_block`），并且在**写出时**分配而不是在 chunk 到达时。

早先的实现在 chunk 到达时逐个取号，并要求同一个 run 的成员号连续。父会话与子 Agent 写
进同一份录像时号会互相穿插，于是每个 run 长度都退化成 1 —— 打包在最需要它的场景下静默
失效。现在连续性由"同一个 packer 的到达顺序"定义（一个 packer 只见一个 call 的流），号
在落盘那一刻整块取。

**与之无关**：`crates/history` 全部、`crates/session` 全部、投影逻辑、`session.history` /
`session.fork` / `resume` / 压缩——录像目录删掉，这些行为一个字都不变。

### 关于块序号

打包通常需要按内容块序号分组，避免把跨块的增量合进一个 run。**这里不需要。**

因为 `texts` 数组永远不 join，即使一个 run 跨了块边界，展开后仍然逐条还原出完全相同
的事件序列——`ModelEvent::TextDelta` 本身没有 `index` 字段，没有需要还原的东西。而块
边界上的 `ContentBlockStart`/`Stop` 是不可打包事件，本来就会打断 run。

于是整套块序号一致性检查都不必存在。

---

## 10. 迁移

### 加标注这一轮

`PromptBlock` / `ToolDef` 多了 `source` 字段，blob 存的是序列化后的结构，所以**所有既
有录像的 system / tools blob id 都变了**。回放会在第一条 call 上报 `system[i]` 分歧。

修法只有一个：**重录**。规模与下面那次一样，约 15 份。

注意这只影响**存储**：发出去的字节没变（§4.2），所以重录出来的请求与重录前逐字节相同，
线上缓存不受影响。

### 更早那次（哈希 → 时间线）

既有的录制数据是"哈希 → 罐头响应"的查找表，与时间线格式不兼容，**必须重录**。

规模可控：6 个场景（`000`–`004`、`skills`），每场景 2–3 个模式，只有各自**最新轮次**
需要重录，约 15 份。旧轮次目录原样留在磁盘上，不删——轮次机制本来就是"不同轮次物理
隔离、互不覆盖"，让它继续这样。

重录后 `tests/runner` 的用例语义完全不变：仍然是"跑这个场景、比对输出"。变的只是命
中方式，而序列匹配比哈希匹配更稳——换模型名不再全 miss，只是报一条 `params` 分歧。

---

## 11. 验收标准

- [x] 无配置时 `RecorderModel` 是 pass-through，`calls.jsonl` 不产生，无额外开销。
- [x] 录制一个含工具调用、压缩、子 Agent 的会话；`calls.jsonl` 里每次模型调用（含
      overloaded 失败重试）各有一条 `call` + 一条 `end`。
- [x] 打包与不打包两种布局，解码结果逐字段相同；读端不依赖任何写侧开关。
- [x] 展开后的 chunk 序列与录制时经过 `RecorderModel` 的事件序列**逐条相同**，含
      token 边界与时间戳。
- [x] 一个会话录像内，未变化的 system 块与工具表各只有一份 blob 文件。
- [x] 进程在流中间被 kill：录像可读，且含崩溃前那条 `call` 的完整请求。
- [x] 含未知 `type` 行的录像能读出其余部分，并 warn 出行号。
- [x] 回放同一录像两次，产生的 turn 结果一致。
- [x] 实时请求与录像分歧时，`strict` 报出**差异所在的字段**，不只是"不匹配"。
- [x] 删掉整个录像目录，会话的 resume / fork / history / 压缩行为不受任何影响。
      （由 crate 分层保证而非测试：`crates/telemetry` 不依赖 `history` / `session` /
      `runtime`，所以录像根本没有可以影响它们的途径。）
- [x] 带标注与不带标注的请求序列化后**逐字节相同**——标注不进 wire。
- [x] 每个 system 块都能说出自己是 scene 的哪个 section / skills / memory / rules / MCP。
- [x] 工具表里的每个工具都能说出自己是 builtin / 哪个 MCP server / 哪个 plugin。
- [x] 子 Agent 录像的 header 带 `parent` 与 `agent_type`。
- [x] 辅助调用落在**发起它的会话**的录像里，带 `purpose`，且不产生 `unattributed` 目录。
- [x] 说不出会话身份的调用不写任何文件。
- [x] 父子会话交错录进同一份录像后，各自回放拿到的仍是自己那条子序列。
- [x] 交错录制时打包不退化——行号穿插不再打断 run。
- [x] `replay.override.json` 能改掉某条调用的响应，且 `calls.jsonl` 一字节未变。
- [x] 覆盖文件下标越界 / 重复时报错，不静默忽略。
- [x] 实时调用数少于录像时，`unconsumed()` 报出是哪个会话、走了几条。
- [x] 改掉某个 system blob 后 `rerun` 发出去的是**改后的**请求，且录像未被写入。
- [x] rerun 的 diff 在响应未变时为空串，变了时同时给出两侧文本与工具序列。
- [x] 手改过的 blob 被再次录制时 warn 并写回正确内容，不留下内容与 id 矛盾的文件。

---

## 12. 数据敏感性

**一份录像 = 模型看到的一切，明文。** 系统提示词、完整对话、工具参数与结果，逐字节原
样，且**有意不过 `RedactionPolicy`**——那套脱敏是给 telemetry 事件用的，用在这里会直接
废掉录像的价值：录像值得留，正因为它说的是实际发出去的东西。

推论：

- 录像目录的敏感级别**等同于它对应的 transcript**，按同样的方式对待。
- 提交录像到仓库（测试固件就是这么用的）等于提交那次会话的完整提示词，包括环境快照
  里的路径、git 状态等。既有的 `tests/fixtures/cassettes/` 已在 `.gitignore` 中。
- **rerun 报告是录像的衍生物，同一敏感级别。** `tests/output/<用例>/rerun.md` 里有提示词
  与两侧回复的摘录（长值只报字符数，不整段贴出），`tests/output/` 同样已在 `.gitignore`
  中。往外发报告之前先当 transcript 看待。
- 默认不录。要录必须显式配置或设环境变量。

---

## 13. 明确的非目标

- **不跨录像去重 blob。** 会牺牲"删目录即干净"。
- **不做保留策略。** 上层负责。
- **不记录工具执行、权限提问、压缩细节。** 前者由下次调用的 messages 隐式覆盖；后
  两者属于 history 与 telemetry 的职责。
- **不追求从录像重建会话。** 录像是诊断数据，不是状态。会话状态只有 history 说了算。

  §6.1 的 rerun 重建的是**一次请求**，不是一个会话：它把第 k 条 call 原样发出去，不接
  着往下跑 turn 循环、不执行工具、不产生新的会话状态。这条边界是有意的——一旦 rerun 开
  始"接着跑"，录像就成了第二份会话状态，与 history 争夺权威。
