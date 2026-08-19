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
 "name":"…","session_id":"…","parent":"…",
 "created_at":1755500000123,
 "engine_version":"0.1.8"}
```

`version` 用于将来真正的破坏性变更。**新增记录类型不动它**——见 §4.5 的兼容约定。

### 4.2 `call` —— 请求

在**发起流之前**写，因为此时请求已经完全确定。这样即使进程崩在流中间，录像里也留着
"崩之前发出去的是什么"——恰恰是诊断崩溃最需要的一条。

```jsonc
{"type":"call","seq":12,"ts":1755500000123,
 "turn":3,"step":2,
 "provider":"anthropic","api_type":"anthropic_messages",
 "params":{"model":"claude-opus-5","max_tokens":8192,
           "thinking_mode":"off","fallback_model":"claude-sonnet-5",
           "cache_edits":[]},
 "system":["a1b2c3d4e5f6a7b8","c3d4e5f6a7b8c9d0"],
 "tools":"e5f6a7b8c9d0e1f2",
 "messages":["1122334455667788","99aabbccddeeff00"]}
```

`system` 是**按块的引用数组，不是 join 后的整串**。把 `PromptBlock` 拼成一个整串会让
块的边界与来源消失——系统提示词、skills 清单、MCP 指令混成一坨。按块存是这份设计对
"组合可区分"这条要求的直接回答。

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
}

pub struct CallOrigin {
    pub session_id: String,
    pub turn: u32,
    pub step: u32,
}
```

比在装饰器上开 setter 干净：每个 stream 调用点都必须显式传，漏不掉。`step` 就是 turn
循环里已有的 `api_calls` 计数。

**`None` 是有意义的取值**：压缩摘要、权限分类、会话命名这些辅助调用不属于任何 turn。
它们照样是真实的 LLM 调用、照样值得录，但给它们编 turn/step 会让坐标说谎。

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

pub enum RecorderMode { Record, Replay }
pub enum Divergence { Strict, Warn }
```

`name: None` 表示用 session id。

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
| `crates/telemetry/src/recorder/mod.rs` | `RecorderModel` 装饰器：录制、序列回放、分歧 diff |
| `crates/telemetry/src/recorder/blob.rs` | 内容寻址存储 |
| `crates/telemetry/src/recorder/format.rs` | `calls.jsonl` 的记录类型 |
| `crates/telemetry/src/recorder/pack.rs` | 增量打包编解码 |
| `crates/telemetry/src/recorder/writer.rs` | 追加写入与 flush 纪律 |
| `crates/telemetry/src/recorder/reader.rs` | 读回，跳过读不懂的行并计数 |
| `crates/core/src/interface/settings.rs` | `RecorderConfig` / `RecorderMode` / `Divergence` |
| `crates/core/src/interface/model.rs` | `StreamParams::origin`（`CallOrigin`）、`ModelEvent::ToolArgsDelta` |
| `crates/model/src/adapter.rs` | 旁路转发工具参数原始片段 |
| `crates/runtime/src/turn.rs` | 在三个 stream 调用点填 `turn`/`step` |
| `daemon/src/rpc.rs` / `session_pool.rs` | `RecorderOptions` 与接线 |

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

既有的录制数据是"哈希 → 罐头响应"的查找表，与时间线格式不兼容，**必须重录**。

规模可控：6 个场景（`000`–`004`、`skills`），每场景 2–3 个模式，只有各自**最新轮次**
需要重录，约 15 份。旧轮次目录原样留在磁盘上，不删——轮次机制本来就是"不同轮次物理
隔离、互不覆盖"，让它继续这样。

重录后 `tests/runner` 的用例语义完全不变：仍然是"跑这个场景、比对输出"。变的只是命
中方式，而序列匹配比哈希匹配更稳——换模型名不再全 miss，只是报一条 `params` 分歧。

---

## 11. 验收标准

- [ ] 无配置时 `RecorderModel` 是 pass-through，`calls.jsonl` 不产生，无额外开销。
- [ ] 录制一个含工具调用、压缩、子 Agent 的会话；`calls.jsonl` 里每次模型调用（含
      overloaded 失败重试）各有一条 `call` + 一条 `end`。
- [ ] 打包与不打包两种布局，解码结果逐字段相同；读端不依赖任何写侧开关。
- [ ] 展开后的 chunk 序列与录制时经过 `RecorderModel` 的事件序列**逐条相同**，含
      token 边界与时间戳。
- [ ] 一个会话录像内，未变化的 system 块与工具表各只有一份 blob 文件。
- [ ] 进程在流中间被 kill：录像可读，且含崩溃前那条 `call` 的完整请求。
- [ ] 含未知 `type` 行的录像能读出其余部分，并 warn 出行号。
- [ ] 回放同一录像两次，产生的 turn 结果一致。
- [ ] 实时请求与录像分歧时，`strict` 报出**差异所在的字段**，不只是"不匹配"。
- [ ] 删掉整个录像目录，会话的 resume / fork / history / 压缩行为不受任何影响。

---

## 12. 数据敏感性

**一份录像 = 模型看到的一切，明文。** 系统提示词、完整对话、工具参数与结果，逐字节原
样，且**有意不过 `RedactionPolicy`**——那套脱敏是给 telemetry 事件用的，用在这里会直接
废掉录像的价值：录像值得留，正因为它说的是实际发出去的东西。

推论：

- 录像目录的敏感级别**等同于它对应的 transcript**，按同样的方式对待。
- 提交录像到仓库（测试固件就是这么用的）等于提交那次会话的完整提示词，包括环境快照
  里的路径、git 状态等。既有的 `tests/fixtures/cassettes/` 已在 `.gitignore` 中。
- 默认不录。要录必须显式配置或设环境变量。

---

## 13. 明确的非目标

- **不跨录像去重 blob。** 会牺牲"删目录即干净"。
- **不做保留策略。** 上层负责。
- **不记录工具执行、权限提问、压缩细节。** 前者由下次调用的 messages 隐式覆盖；后
  两者属于 history 与 telemetry 的职责。
- **不追求从录像重建会话。** 录像是诊断数据，不是状态。会话状态只有 history 说了算。
