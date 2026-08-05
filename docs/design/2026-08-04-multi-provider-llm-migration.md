# 多 LLM 供应商 + 按任务分级路由 架构设计

**日期：** 2026-08-04（当日两轮：首版设计 → 用户调整需求后补充实施）
**基于需求：** 用户在对话中给出的 5 点设计结论（去环境变量、配置分层复用现状、多供应商两种协议、按任务类型分级、删除供应商自动降级）+ 密钥存储方式的用户决策（配置文件明文存储）+ 第二轮两点调整（新增跨 scene 全局配置层、供应商模型允许列表过滤）。
**状态：本文档记录设计过程，标注哪些部分已经落地。** 稳定参考版本在 `docs/LLM_PROVIDERS.md`（不含推理过程，日常查阅用这份）。

> 记号约定：**[现状]** 已通过阅读源码确认；**[建议]** 是尚未实现的设计方案；**[已实施]** 是本次会话里已经写完、编译通过、测试通过的代码，标注具体文件路径。

> **后续更新（2026-08-04 第四轮，见 `docs/CONFIG_LAYOUT.md` §13 / `docs/LLM_PROVIDERS.md`）**：本文档 §4.1/§8/§9 里提到的 `daemon::model_routing`（独立第三套实现）、`daemon::SettingsFile`/`merge_settings`（更窄的平行解析器）均已删除——`daemon::model_routing` 的内容迁移进 `crates/core/src/provider.rs`，与 §1.4 提到的 `ProviderRegistry`/`ModelResolver`/`ModelRegistry` 两套死代码一起被清理，现在只有唯一一套 `ProviderConfig`/`TaskModelOverride`/`resolve_task_models`；`SettingsFile` 被 `Settings::load()`（唯一权威解析器）取代。另外新增了 `daemon.doctor`（诊断）、`config.setProvider`（写供应商配置）两个 RPC，以及 settings.json 的 JSON Schema 发布——这些是本文档写作时还没有的能力。下文正文保留原始设计推理记录不做改写，实际现状以上述两份文档为准。

---

## 0. 范围声明

本次改造只涉及"引擎怎么决定用哪个 LLM 供应商/模型来发起请求"这一件事。以下内容**不在范围内**：

- `crates/auth`（Anthropic 账号 OAuth 登录流程）——登录鉴权，不是 API provider 选择，两者容易混淆但完全独立。
- `docs/design/2026-08-03-agents-config-migration.md` 里的目录布局迁移（`AGENTS.md`/`.agents/`/scope→scene）——已完成，与本次改造正交。本文档 §2 新增的全局配置层，写进了那份文档的 §10，作为它既定"settings 分层顺序"描述的延伸,不是重复决策。

---

## 1. 现状 [已确认]

### 1.1 唯一全局 Model 实例

`daemon/src/main.rs:151-164`：进程启动时读一次环境变量，构造**唯一**一个 client：

```rust
let api_key = std::env::var("ANTHROPIC_AUTH_TOKEN")
    .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
    .map_err(|_| anyhow::anyhow!("set ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY"))?;
let client: Arc<dyn AnthropicClient> = match std::env::var("ANTHROPIC_BASE_URL").ok() {
    // ... 自定义 base_url 分支
};
```

`daemon/src/session_pool.rs:105`：这个 client 被包一层 `AnthropicModel::new(client.clone())`，赋给 `SessionPool.model: Arc<dyn base::interface::model::Model>`（`session_pool.rs:58`）。此后全进程共享这一个实例：

- `crates/runtime/src/agent_tool.rs:529,650,762`（三处子 agent 生成点）：`.model(self.inner.model.clone())` / `.model(inner.model.clone())`，直接克隆父级实例。
- `crates/team/src/coordinator.rs`：`OrchestrateRequest.model: Arc<dyn Model>` 同样单一实例传给所有 team 成员。
- `crates/tools/src/secondary_llm.rs:31,39`：`AnthropicSecondaryLlm` 结构体**直接依赖具体类型** `model::client::AnthropicClient`，而不是 `Model` trait——这是本次改造里唯一需要先解耦类型依赖、才能接入分级路由的调用点。

**这一节描述的现状在本轮之后依然成立**——本次只新增了配置的解析与校验（§2、§4），没有改动实际发起 LLM 请求的路径,见 §5 的范围说明。

### 1.2 配置分层机制：勘误——真正生效的不是 `Settings::load()`

首版设计文档在这里写的是"`crates/core/src/interface/settings.rs::Settings::load()` 已经存在、可以直接复用"——**这个判断是错的**，写完 §2/§4 的实现前重新核对代码库时发现：

- `grep -rn "Settings::load(" --include="*.rs"` 全仓库**零命中**。`Settings::load()`/`Settings::merge()` 是没有任何调用点的死代码，和 §1.4 提到的两套死 registry 属于同一类问题。
- daemon 真正启动时用的是 `daemon/src/main.rs:203` 处的 `Settings { ... }` **结构体字面量**直接构造，字段值来自 `daemon_config`（`load_daemon_config()` 的返回值），根本不经过 `crates/core` 那个 `Settings::load()`。
- 真正做"user → project 分层合并"的，是 `daemon/src/config.rs::load_daemon_config()` + 内部私有的 `SettingsFile` 结构体 + `merge_settings()` 函数——这是一个**更小、更朴素**的解析器，只认 `model`/`max_tokens`/`mcp_servers` 三个字段（本次新增 `providers`/`default_provider`/`task_models`），不是 `crates/core::Settings` 那个大结构体的镜像。

本次改造（§2、§4）的落点因此是 `daemon/src/config.rs`，不是 `crates/core/src/interface/settings.rs`。`crates/core::Settings::load()` 依旧是死代码，本次没有动它，是否要一并清理见 §8 开放问题。

原有分层（低→高）：`config_root/settings.json`（scene 级，即 `~/.atta/<scene>/`）→ `project_root/.atta/settings.json`（项目级）。本次在最下面新增一层，见 §2。

### 1.3 当前 `Settings.model`（daemon 构造出来的那份）是单一供应商结构

`daemon/src/main.rs:203-212`：

```rust
model: ModelSettings {
    api_type: base::provider::ApiType::Anthropic,   // 硬编码，全代码库 10 处这样写死
    base_url: String::new(),
    auth_token: String::new(),
    model_name: daemon_config.model.clone(),
    max_tokens: daemon_config.max_tokens,
    thinking_mode: ThinkingMode::Auto,
    fallback_model: None,
},
```

不支持多供应商并存，`fallback_model` 只是"同供应商内降级重试"的模型名（不是供应商切换）。**本次改造没有改这里**——§4 新增的 `providers`/`default_provider`/`task_models` 目前只被解析、校验、记日志（`main.rs` 新增的那一段），还没有接回这个 `ModelSettings` 或任何实际发起请求的路径,见 §5。

### 1.4 已经存在但零调用点的两套死代码

**(a) `crates/core/src/provider.rs`** —— `ApiType`（含 `Anthropic`/`OpenAICompatible` 两个变体）、`ProviderDef`/`ModelConfig`/`ProviderRegistry`/`ModelResolver`（opus/sonnet/haiku 等九个模型槽位，`ProviderRegistry::activate()` 是"同一时刻只有一个 provider 生效"的单选模型）。

**(b) `crates/model/src/registry.rs`** —— `ModelRegistry`，从 `settings.json` 顶层 `providers` **数组**解析 `ProviderConfig{id,name,model,sonnet,opus,haiku}`。挂在 `crates/tools/src/config.rs:28` 的全局 `OnceLock`，初始化函数零调用点，registry 永远是空的。

两套都是"槽位/档位"思路，不是"任务类型"思路——本次 §4 的实现是**第三套**，与这两套都不同（也不共享代码），是否清理前两套见 §8。

### 1.5 `Model` trait 是唯一值得直接复用的抽象

`crates/core/src/interface/model.rs`：`Model` trait（`api_type()` + `stream()`）协议无关。目前只有一个实现 `AnthropicModel`（`crates/model/src/adapter.rs`）。本次未涉及，留给 §5/§6 阶段 2。

---

## 2. 已实施：跨 scene 全局配置层 [已实施，2026-08-04]

### 2.1 需求

用户级配置原来只有 scene 级一层（`~/.atta/<scene>/settings.json`，多个 scene 之间互不共享）。需要新增一层**跨 scene 的公用配置**，优先级低于 scene 级——适合放"不管跑 coding 还是 chat scene 都通用"的配置项（比如本次的 `providers`）。

### 2.2 实现

`daemon/src/config.rs`：

- `DaemonPaths` trait 新增 `global_root()` 方法，默认实现是 `config_root()` 的文件系统 parent（`config_root()` 是 `$HOME/.atta/<scene>`，parent 就是 `$HOME/.atta`）。
- `DefaultDaemonPaths`（daemon 真实启动路径用的实现）不需要覆写，直接吃默认实现，落地路径是 `~/.atta/settings.json`。
- `StaticDaemonPaths`（测试用）**不用默认实现**——测试的 `config_root` 通常来自 `tempfile::tempdir()`，它的文件系统 parent 是系统共享的临时目录根，用默认实现有极小概率读到宿主机上无关的 `settings.json`。改为显式字段：不调用 `.with_global(...)` 时 `global_root() == config_root()`（等于该层被读两次，幂等无害）；需要测试全局层时显式 `.with_global(path)`。
- `load_daemon_config()` 合并顺序改为三层：`global_root()/settings.json`（新增，最低优先级）→ `config_root()/settings.json`（scene 级）→ `project_root/.atta/settings.json`（项目级，最高）。

```rust
let global_path = paths.global_root().join("settings.json");
let mut merged = load_single(&global_path).unwrap_or_default();

let user_path = config_root.join("settings.json");
if let Some(user) = load_single(&user_path) {
    merged = merge_settings(merged, user);
}

let project_path = project_root.join(".atta").join("settings.json");
if let Some(proj) = load_single(&project_path) {
    merged = merge_settings(merged, proj);
}
```

### 2.3 测试

`daemon/src/config.rs` 测试模块新增：`static_paths_global_root_defaults_to_config_root`、`static_paths_global_root_respects_explicit_override`、`default_daemon_paths_global_root_is_parent_of_config_root`、`load_daemon_config_global_is_lowest_priority`（三层同时给不同值，验证优先级链）、`load_daemon_config_scene_overrides_global_which_is_the_only_layer_set`。全部通过，见 `cargo test -p daemon --lib`。

### 2.4 与 §1.2 勘误的关系

这一层加在**真正生效**的 `daemon/src/config.rs` 里，不是加在死代码 `crates/core::Settings::load()` 里——这是为什么本节先于原本"§1 现状"里对 `Settings::load()` 的错误描述被发现：实现这一层时去找"现有分层机制在哪"，才顺带发现了 §1.2 的勘误。

---

## 3. 配置 schema [建议 + 已实施解析部分，见 §4]

### 3.1 顶层结构

```jsonc
{
  "providers": {
    "<provider_id>": {
      "api_type": "anthropic",       // "anthropic" | "openai_compatible"（字符串，非法值不报错，见 §4）
      "base_url": "https://api.anthropic.com",
      "api_key": "sk-ant-...",        // 明文，见 §3.4
      "default_model": "claude-sonnet-4-6",
      "models": ["claude-sonnet-4-6", "claude-opus-4-8"]   // 可选，允许列表，见 §3.3
    }
  },
  "default_provider": "<provider_id>",
  "task_models": {
    "<task_type>": "<provider_id>",                                          // 简写
    "<task_type>": { "provider": "<provider_id>", "model": "<model_name>" }  // 或详细写法
  }
}
```

`providers` 用按 id 为 key 的 map（不用数组）——project 级配置需要能只覆盖某个 provider 的一个字段（比如只换 `api_key`），map 才能做字段级合并。

### 3.2 任务类型

不再是编译期封闭枚举——**这是第二轮相对首版的一个调整**：`task_models` 的 key 现在是普通字符串，不识别的任务类型不报错，也不生效（没有任何调用点会去查一个不存在的任务类型名），和 §3.3 的模型过滤走同一套"宽进严出、拼错不崩溃、只警告"哲学，取代首版"封闭枚举、非法值配置加载期报错"的设计（首版参照了 `AgentScene` 的强校验先例，现改为更宽松的语义,原因是 task_models 的 key 集合会随 §6 阶段 4 的接线逐步增加，用字符串避免每加一个任务类型都要发版本更新一次校验枚举）。当前实际会被消费的任务类型（等 §6 阶段 4 落地后）仍是这几个：`main`/`subagent`/`team`/`classifier`/`compact`/`web_fetch`，对应关系不变，见首版表格（本节不再重复）。

### 3.3 模型允许列表过滤 [第二轮新增需求]

`providers.<id>.models` 是一个**纯过滤用的允许列表**，不是从供应商 API 实时拉取的真实可用模型目录。语义：

- 列表为空（未配置）→ 不过滤，`task_models` 里指定什么模型名都直接采用。
- 列表非空 → `task_models.<task>.model` 指定的模型名必须在列表里才生效；不在列表里时**不是报错**，而是：
  1. 记一条启动 WARN；
  2. 回退到**该 provider 自己的** `default_model`（不是 `default_provider`——供应商本身的选择是对的，只是模型名不对）。

用户给的例子：DeepSeek 的 `models` 配了 `["deepseek-pro", "deepseek-flash"]`，某个任务的 `task_models` 写了 `deepseek-max`——启动时警告"deepseek-max 不在允许列表里"，该任务实际用 DeepSeek 的 `default_model`（比如配置里写的 `deepseek-pro`），**不会**整体降级到 `default_provider`（可能是 Anthropic）。这与"provider id 本身不存在"的降级路径（回落到 `default_provider`，见 §3.1 首版的降级设计）是两条不同的回退路径，不要混在一起。

### 3.4 密钥存储

`api_key` 明文写在 `providers.<id>.api_key` 里，不做加密。**项目级 `<repo>/.atta/settings.json` 如果被提交到共享仓库会泄露密钥**，需要用户自行 `.gitignore`；本次新增的全局层 `~/.atta/settings.json` 因为在用户主目录下（不在任何仓库里），是存放密钥的更安全默认位置，见 `docs/LLM_PROVIDERS.md` 的建议用法。

---

## 4. 已实施：Provider 配置解析 + 任务分级解析 [已实施，2026-08-04]

### 4.1 新文件 `daemon/src/model_routing.rs`

- `ProviderConfig { api_type, base_url, api_key, default_model, models }`——对应 §3.1 的 provider 条目，`api_type` 保持 `Option<String>` 而非枚举（非法值不炸整份 settings.json 解析,和 §3.2 同一哲学）。
- `TaskModelOverride`——`#[serde(untagged)]` 枚举，同时接受简写字符串和 `{provider, model}` 详细写法（§3.1）。
- `merge_providers()` / `merge_task_models()`——供 `daemon/src/config.rs::merge_settings()` 调用的字段级合并：`providers` 按 provider id 逐字段合并（非空字段覆盖），`task_models` 按任务类型整条替换（一条 override 是原子选择，不做字段级合并）。
- `resolve_task_models(providers, default_provider, task_models) -> Result<(HashMap<task, ResolvedModel>, Vec<警告文本>), 错误文本>`——纯函数，核心解析逻辑：
  - `default_provider` 未设置 / 不在 `providers` 里 / 对应 provider 没有 `default_model` → **硬错误**（`Err`），无法降级——这是最后一道防线,没有更底层的默认值。
  - `task_models` 某条引用的 provider 不存在 → 警告 + 回落到 `default_provider` 的 `default_model`。
  - 某条指定了 `model`，该 provider 的 `models` 允许列表非空且不包含该模型名 → 警告 + 回落到**该 provider 自己的** `default_model`（§3.3 的语义,注意与上一条的回落目标不同）。
  - 某条没指定 `model` → 直接用该 provider 的 `default_model`。
  - `models` 允许列表为空，或指定的模型名在列表里 → 原样采用。
- 11 个单元测试覆盖以上全部分支，含直接复刻用户给的 DS PRO/FLASH/MAX 例子（`model_outside_allow_list_falls_back_to_same_providers_default_and_warns`）。

### 4.2 `daemon/src/config.rs` 的接入

`SettingsFile` 新增 `providers`/`default_provider`/`task_models` 三个字段（`#[serde(default)]`，不写就是空/`None`，完全不影响没有配置这些字段的现状用户）。`merge_settings()` 对这三个字段分别调用 §4.1 的合并函数。`DaemonConfig` 相应新增三个字段，`load_daemon_config()` 把合并结果透传进去。新增测试 `load_daemon_config_parses_and_merges_providers`：验证 scene 级定义 provider、project 级只覆盖 `api_key` 一个字段、其余字段（`default_model`/`models`）保留 scene 级的值。

### 4.3 `daemon/src/main.rs` 的接入

`load_daemon_config()` 之后新增一段：`daemon_config.providers` 非空时才调用 `resolve_task_models()`——**完全没配置 `providers` 的用户走原来的路径，行为零变化**（这是本节改动最重要的性质：纯新增，不影响现状默认行为）。解析出的警告用 `tracing::warn!` 打出来，解析结果（每个任务类型实际会用哪个 provider/model）用 `tracing::info!` 打出来；`resolve_task_models` 返回 `Err`（即 §4.1 提到的硬错误情形）时，daemon **启动失败**（`anyhow::bail!`）——只有在用户已经配置了 `providers` 却把 `default_provider` 配错的情况下才会触发,不影响没碰这块配置的用户。

### 4.4 验证

`cargo build --workspace`、`cargo clippy -p daemon --lib`、`cargo test -p daemon --lib`（39 个测试全过，含本节新增的全部用例）均已跑过，无新增警告。

---

## 5. 尚未实施：运行时接线 [建议，未来阶段]

**§4 做的事只是"把配置读出来、校验、记日志"——目前解析出的 `providers`/`task_models` 还没有接到任何一个真正发起 LLM 请求的地方。** `SessionPool`/`AgentTool`/`team::coordinator` 等六处调用点（见 §1.1）现在依然是"读一次环境变量、造一个 `AnthropicModel`、全进程共享"的旧路径，完全没变。这不是遗漏，是本轮明确的范围边界——用户这一轮要求的是"先做 1（全局配置层），再做 2（供应商模型过滤）"，没有要求做运行时分发；下面记录的是**如果**要继续做下去、设计上大致的样子，留给后续决定。

### 5.1 Provider 工厂 [建议]

```rust
fn build_model(cfg: &ProviderConfig) -> Result<Arc<dyn Model>, String> {
    match cfg.api_type.as_deref() {
        Some("anthropic") | None => Arc::new(AnthropicModel::new(/* base_url + api_key */)),
        Some("openai_compatible") => Arc::new(OpenAICompatibleModel::new(/* 同上，新实现，见 §6 阶段 2 */)),
        Some(other) => return Err(format!("unsupported api_type: {other}")),
    }
}
```

### 5.2 Task-type resolver 接入六处调用点 [建议]

`SessionPool`/`AgentTool`×3/`team::coordinator`/`llm_classifier`/compact 逻辑/`secondary_llm.rs`（先做 §1.1 提到的类型解耦），从"持有一个 `Arc<dyn Model>` 字段"改为"持有一份 provider 实例 map + §4.1 的 `resolve_task_models` 结果，发起请求前查表"。

---

## 6. 落地阶段划分 [更新状态]

**阶段 0 — 跨 scene 全局配置层** ✅ **已实施**，见 §2。

**阶段 1 — Schema 与解析（provider/task_models 部分）** ✅ **已实施**，见 §4。**未做**：清理 §1.4 的两套死代码（`crates/core/src/provider.rs` 里 `ProviderDef`/`ModelConfig`/`ProviderRegistry`/`ModelResolver`；`crates/model/src/registry.rs` 整个文件；`crates/tools/src/config.rs:28` 的 `MODEL_REGISTRY`）——本轮新写的是第三套独立实现（`daemon::model_routing`），没有触碰前两套，仓库里现在同时存在三套"provider 配置"概念，是否清理见 §8。

**阶段 2 — OpenAI 兼容协议实现** [建议，未做]
- 新增 `OpenAICompatibleModel`（实现 `Model` trait），对标 `crates/model/src/adapter.rs::AnthropicModel`。

**阶段 3 — Provider 工厂 + 运行时 Router** [建议，未做]
- §5.1 落地，把 §4.1 已经写好的 `resolve_task_models` 结果接成真正可用的 `Arc<dyn Model>` 实例。

**阶段 4 — 六处接线** [建议，未做]
- §5.2，改动面最大，建议逐个模块独立提交。

**阶段 5 — 清理环境变量** [建议，未做]
- 删除 `daemon/src/main.rs:151-164` 的 env 读取逻辑，删除 10 处硬编码 `ApiType::Anthropic`。**必须放在阶段 4 完成之后**——阶段 0-1 完成的现在，`providers`/`task_models` 完全是可选的旁路配置，删环境变量会导致所有用户（包括没配置 `providers` 的）直接无法启动 daemon。

---

## 7. 破坏性说明

阶段 0-1（本轮已做的部分）**不是破坏性变更**——纯新增字段、`#[serde(default)]`、`providers` 为空时逻辑零介入，现有用户完全无感。破坏性变更在阶段 5，届时环境变量启动方式会被完全移除，见上一版设计的原文判断（不再重复）。

---

## 8. 开放问题（需要用户决策）

1. **§1.4 的两套死代码（`crates/core::provider` 里的 `ProviderRegistry`/`ModelResolver`，`crates/model::registry::ModelRegistry`）要不要顺手删掉？** 本轮新增的 `daemon::model_routing` 是第三套独立实现，三套并存容易让后来者疑惑"到底该用哪个"。建议删，但涉及改动 `crates/tools/src/config.rs` 里 `MODEL_REGISTRY` 这个全局 `OnceLock`（虽然零调用点,但删除前建议再跑一次全仓库引用确认）。
2. **`crates/core/src/interface/settings.rs::Settings::load()`/`merge()` 这两个零调用点的死函数，要不要一并清理或者标注 `#[deprecated]`？** §1.2 的勘误发现它们和本轮新实现的东西撞了名字/概念，容易被后来者误当成"现有机制"复用（就像本文档首版犯的错一样）。
3. **是否需要把阶段 2-5 排进日程？** 本轮明确只做了阶段 0-1，§4 产出的解析结果目前只用于启动时打日志，不影响任何实际请求。如果近期不打算做阶段 2-5，`providers`/`task_models` 配置本质上是"用户写了也没用"的状态，值得在 `docs/LLM_PROVIDERS.md` 里显著提示,避免用户配置后误以为已经生效。

---

## 9. 变更记录

- **2026-08-04 第一轮**：首版设计，基于用户 5 点需求 + 密钥存储决策（明文）。全文档 [建议]，未实施，且 §1.2 对"现有分层机制"的判断有误（见下一轮勘误）。
- **2026-08-04 第二轮**：用户提出两点调整——(1) 新增跨 scene 全局配置层 `~/.atta/settings.json`，优先级低于 scene 级；(2) provider 配置增加 `models` 允许列表，`task_models` 指定的模型不在列表里时警告 + 回落到该 provider 自己的默认模型（而非 `default_provider`）。两点均已实施（§2、§4），实施过程中发现并勘误了首版 §1.2 的错误判断（真正生效的合并逻辑在 `daemon/src/config.rs`，不是 `crates/core::Settings::load()`）。运行时接线（Provider 工厂、六处调用点分发、OpenAI 兼容协议实现、移除环境变量）明确未做，是后续独立阶段。
