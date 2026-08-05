# 多 LLM 供应商配置指南

> **实施状态**：配置文件的三层位置（全局/scene/项目）、`providers`/`default_provider`/`task_models` 的解析、字段级合并、模型允许列表过滤、悬空引用降级 + 启动告警——**均已实施**（`crates/core/src/interface/settings.rs`::`Settings::load()`、`crates/core/src/provider.rs`）。settings.json 的解析现在统一走 `base::interface::settings::Settings::load()` 这一套（不再有 `daemon` 自己的一份 `SettingsFile`），并发布了 JSON Schema（`docs/schemas/settings.schema.json`，可在 settings.json 顶层加 `"$schema"` 指向它获得编辑器自动补全/校验）。新增了三个只读/写配置的 daemon RPC——`daemon.doctor`（诊断）、`config.setProvider`（写供应商配置）、`config.getProvider`（读回，`api_key` 默认脱敏）——见下方「通过 RPC 配置」一节。**尚未实施**：把解析出的 provider/task 配置接到实际发起 LLM 请求的代码路径（`SessionPool`/`AgentTool`/`team::coordinator` 等六处，仍然全部走原有的单一 Anthropic 客户端）。也就是说：**现在配置 `providers`/`task_models`，daemon 启动时（或调用 `config.setProvider` 后）会解析、校验、告诉你每个任务实际解析到哪个 provider/model，但实际对话请求暂时还是走原来的那一个供应商**，不会真的按任务分流。详见 `docs/design/2026-08-04-multi-provider-llm-migration.md` §4-§6。

---

## 这解决什么问题

默认情况下，AttaCore 全程只用**一个** LLM 供应商、一个 API Key 发起所有请求。本设计允许你：

1. 在配置文件里登记**多个**供应商（Anthropic 原生协议 / OpenAI 兼容协议均支持）；
2. 给每个供应商配一份**允许使用的模型列表**（纯过滤用，不是从供应商 API 拉取的实时目录）；
3. 给不同**任务类型**（主对话、子 agent、压缩摘要……）分别指定用哪个供应商、哪个模型；
4. 配置出错时软着陆——供应商被删除、模型名不在允许列表里，都是警告 + 自动回退，不是启动失败。

---

## 配置文件位置与优先级

**三层**，低到高：

```
~/.atta/settings.json              全局层：跨所有 scene 共享
        ↓ 被下面覆盖
~/.atta/scenes/<scene>/settings.json   scene 级：coding/chat/demo 各自独立
        ↓ 被下面覆盖
<repo>/.atta/settings.json         项目级：仅本项目生效
```

三层格式完全一致，都是在 `settings.json` 顶层加 `providers` / `default_provider` / `task_models` 三个字段。**同一个 provider id 在多层都出现时，逐字段合并**——比如全局层定义了某个 provider 的 `base_url`/`default_model`/`models`，项目级只想换 `api_key`，项目级文件只需要写这一个字段，其余字段沿用更低优先级层的值。`task_models` 则是按任务类型整条覆盖（不做字段级合并——override 是一次原子的"这个任务用哪个供应商/模型"的选择）。

**多供应商配置通常适合放在全局层**（`~/.atta/settings.json`）——API key 这类信息一般不随 scene 变化，放全局层能让所有 scene 共用同一份，不用每个 scene 目录都抄一遍。

---

## 字段说明

### `providers`：按 id 登记供应商

```jsonc
"providers": {
  "<provider_id>": {
    "api_type": "anthropic",        // 或 "openai_compatible"
    "base_url": "...",
    "api_key": "...",               // 明文存储，见下方"安全提示"
    "default_model": "...",         // 未指定具体模型时使用的模型
    "models": ["...", "..."]        // 可选：允许列表，见下方
  }
}
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `api_type` | 是 | `anthropic`：Anthropic Messages API 原生协议。`openai_compatible`：OpenAI Chat Completions 兼容协议（DeepSeek、多数第三方网关走这个）。 |
| `base_url` | 是 | API 端点地址。 |
| `api_key` | 是 | 明文写在配置文件里。**不做加密/脱敏**——见下方安全提示。 |
| `default_model` | 是 | 任务没有指定具体模型时使用的模型；也是模型名被允许列表过滤掉之后的回退目标。 |
| `models` | 否 | 该供应商可用的模型名允许列表，**纯粹用于过滤** `task_models` 里显式指定的模型名，不是从供应商 API 实时查询的目录。**不填或填空数组 = 不过滤，任意模型名都直接采用。** |

### `default_provider`：兜底供应商

```jsonc
"default_provider": "anthropic"
```

必须指向 `providers` 里存在、且配了 `default_model` 的一项——这是最后一道防线,如果这条本身失效，daemon **启动失败**（没有更底层的默认值可退）。所有没有在 `task_models` 里单独配置的任务类型，都走这个供应商 + 它的 `default_model`。

### `task_models`：按任务类型分级路由

```jsonc
"task_models": {
  "<task_type>": "<provider_id>",
  // 或者需要指定该供应商下的具体模型：
  "<task_type>": { "provider": "<provider_id>", "model": "<model_name>" }
}
```

`<task_type>` 是自由字符串，不强制枚举——拼错或者写了个引擎还不认识的任务类型名，不会导致配置报错，只是这条 override 不会被任何调用点用到（因为还没有代码去查它，见文首的实施状态说明）。等运行时接线完成后（见设计文档 §6 阶段 4）实际会用到的任务类型：

| 任务类型 | 对应场景 |
|---|---|
| `main` | 主对话 |
| `subagent` | 子 agent（Agent 工具生成的子任务） |
| `team` | 多 agent 协作 |
| `classifier` | 权限判断用的内部分类调用 |
| `compact` | 长对话上下文压缩摘要 |
| `web_fetch` | 网页抓取内容摘要 |

---

## 两条独立的"出错就降级"路径

这是本设计里最容易搞混的一点，务必分清楚：

### 路径 A：`task_models` 引用的 provider 不存在

→ 警告 + **整条**回落到 `default_provider` 的 `default_model`（供应商和模型都换成默认的）。

### 路径 B：`task_models` 指定的 model 不在该 provider 的 `models` 允许列表里

→ 警告 + **只换模型**，供应商本身不变，回落到**同一个** provider 自己的 `default_model`（不会跳去 `default_provider`——供应商选择本身没错，错的只是模型名）。

用户举的例子刚好对应路径 B：DeepSeek 配置了 `"models": ["deepseek-pro", "deepseek-flash"]`，某任务写了 `"model": "deepseek-max"` —— MAX 不在列表里，警告，该任务实际用的是 DeepSeek 自己的 `default_model`（比如配置里写的 `deepseek-pro`），**不会**因此改用 `default_provider` 指向的 Anthropic。

---

## 完整示例

### 示例 1：只用 Anthropic（最简配置）

`~/.atta/settings.json`（全局层，所有 scene 共用）：

```json
{
  "providers": {
    "anthropic": {
      "api_type": "anthropic",
      "base_url": "https://api.anthropic.com",
      "api_key": "sk-ant-xxxxxxxx",
      "default_model": "claude-sonnet-4-6"
    }
  },
  "default_provider": "anthropic"
}
```

不写 `models` 允许列表、不写 `task_models` 都完全合法。

### 示例 2：主对话用 Anthropic，子 agent 用 DeepSeek 降成本，带允许列表

`~/.atta/settings.json`：

```json
{
  "providers": {
    "anthropic": {
      "api_type": "anthropic",
      "base_url": "https://api.anthropic.com",
      "api_key": "sk-ant-xxxxxxxx",
      "default_model": "claude-sonnet-4-6"
    },
    "deepseek": {
      "api_type": "openai_compatible",
      "base_url": "https://api.deepseek.com/v1",
      "api_key": "sk-xxxxxxxx",
      "default_model": "deepseek-pro",
      "models": ["deepseek-pro", "deepseek-flash"]
    }
  },
  "default_provider": "anthropic",
  "task_models": {
    "subagent": "deepseek",
    "web_fetch": { "provider": "deepseek", "model": "deepseek-flash" }
  }
}
```

`main`/`team`/`classifier`/`compact` 未出现在 `task_models` 里，走 `default_provider`（anthropic）。

### 示例 3：项目级只覆盖一个字段

全局层已经登记了 `deepseek`（如示例 2）。某个项目想换一把专属 key，`<repo>/.atta/settings.json` 只需要：

```json
{
  "providers": {
    "deepseek": {
      "api_key": "sk-project-specific-key"
    }
  }
}
```

`api_type`/`base_url`/`default_model`/`models` 沿用全局层的值，字段级合并、不用整段重写。

### 示例 4：模型不在允许列表里（路径 B）

接示例 2 的配置，把 `web_fetch` 的模型改成一个允许列表里没有的名字：

```json
"task_models": {
  "web_fetch": { "provider": "deepseek", "model": "deepseek-max" }
}
```

启动日志（大意）：

```
[WARN] task_models.web_fetch 指定的模型 'deepseek-max' 不在 provider 'deepseek' 的 models 列表 ["deepseek-pro", "deepseek-flash"] 中，已回退到该 provider 的默认模型 'deepseek-pro'
[INFO] model routing resolved task=web_fetch provider=deepseek model=deepseek-pro
```

`web_fetch` 仍然用 DeepSeek（供应商没变），只是模型换成了 DeepSeek 自己的 `default_model`。

### 示例 5：供应商被整个删除（路径 A）

接示例 2，把 `deepseek` 整段从 `providers` 里删掉，但忘了同步清理 `task_models` 里对它的引用：

```json
{
  "providers": { "anthropic": { "...": "..." } },
  "default_provider": "anthropic",
  "task_models": {
    "subagent": "deepseek",
    "web_fetch": { "provider": "deepseek", "model": "deepseek-flash" }
  }
}
```

启动日志（大意）：

```
[WARN] task_models.subagent 引用的 provider 'deepseek' 不存在，已降级到 default_provider 'anthropic'
[WARN] task_models.web_fetch 引用的 provider 'deepseek' 不存在，已降级到 default_provider 'anthropic'
```

这次是路径 A：连供应商一起换成了 `default_provider`（anthropic），不只是换模型。

---

## 出错行为

| 情况 | 行为 |
|---|---|
| `task_models.<task>` 引用的 provider 不存在 | 路径 A：警告 + 整条回落到 `default_provider` |
| `task_models.<task>` 指定的 model 不在该 provider 的 `models` 允许列表里 | 路径 B：警告 + 只换模型，回落到同一 provider 的 `default_model` |
| `default_provider` 未设置 / 不在 `providers` 里 / 对应 provider 没有 `default_model` | **daemon 启动失败**——只在你已经配置了 `providers` 的前提下才会触发这条检查；完全不配置 `providers` 的用户不受影响 |
| 不配置 `providers`（保持现状） | 无任何变化，daemon 行为和这个功能上线前完全一样 |

---

## 通过 RPC 配置

除了手写 settings.json，也可以通过 daemon 的 JSON-RPC 接口写入/诊断配置——适合 IDE 插件或 CLI 命令封装成"添加供应商"的交互式向导，不用用户直接碰 JSON 文件。daemon 完整的 RPC 方法清单（`session.*`/`mcp.*`/`import.*` 等，不只是这里的 `config.*`）见 `docs/DAEMON_RPC.md`。

> **信任边界提示**：这些 RPC 和 `session.run_turn`/`daemon.shutdown` 等其他方法一样，daemon 的 JSON-RPC 接口**没有单独的方法级鉴权**——Unix socket 的文件权限（本地）/ `--token`（TCP）是唯一的访问控制。能连上这个 socket 的调用方，就能调用 `config.setProvider` 把 `providers.<id>.api_key`/`base_url` 改成任意值（包括指向一个攻击者控制的端点，从而劫持之后所有 LLM 请求），也能对 `config.getProvider` 传 `include_secrets: true` 直接读出明文密钥。对待 daemon socket/token 的暴露面，应该和对待 LLM 凭证本身一样谨慎。

### `config.setProvider`：写供应商配置

请求写的是**当前项目**的项目层 `settings.json`（`<repo>/.atta/settings.json`），语义是**局部 patch**（只覆盖你传的字段，不动其余字段），和手写项目层文件做字段级覆盖是同一套合并规则。写入后，daemon 会立即重新加载全局→scene→项目三层合并出的完整配置并生效于**之后新建的** session（已经在运行的 session 不受影响，除非重启 daemon）。

```jsonc
// 请求
{"jsonrpc":"2.0","method":"config.setProvider","id":1,"params":{
  "provider_id": "deepseek",
  "config": {
    "api_type": "openai_compatible",
    "base_url": "https://api.deepseek.com/v1",
    "api_key": "sk-xxxxxxxx",
    "default_model": "deepseek-pro",
    "models": ["deepseek-pro", "deepseek-flash"]
  },
  "default_provider": "deepseek",           // 可选：同时设置兜底供应商
  "task_models": { "subagent": "deepseek" }  // 可选：局部 patch task_models
}}

// 响应
{"jsonrpc":"2.0","id":1,"result":{
  "written_to": "/path/to/repo/.atta/settings.json",
  "providers": ["deepseek"],
  "default_provider": "deepseek",
  "task_models": ["subagent"],
  "routing": { "ok": true, "warnings": [], "error": null }
}}
```

删除一个供应商：传 `"delete": true`（此时忽略 `config`）。按前文「路径 A」的软降级规则，任何还引用这个 id 的 `task_models` 条目**不需要额外清理**——下次解析路由时会自动警告 + 回落到 `default_provider`。

```json
{"jsonrpc":"2.0","method":"config.setProvider","id":2,"params":{"provider_id":"deepseek","delete":true}}
```

`config`/`task_models` 校验失败（比如 `config` 不是 JSON 对象，或字段类型不对）时返回 JSON-RPC 标准的 `INVALID_PARAMS`（-32602）错误，不会写文件。写入本身成功之后，`result.routing.ok=false` 表示合并后的整体配置有硬性问题（比如 `default_provider` 指向一个不存在的 provider）——这种情况**仍然会写入磁盘**（你传的 patch 本身是合法的，只是搭配现有配置后整体不自洽），需要你根据 `routing.error` 再修一次。

### `config.getProvider`：读回当前配置

`config.setProvider` 的读回对称方法，返回当前生效（三层合并后）的 `providers`/`default_provider`/`task_models`。`api_key` **默认脱敏**为 `***<末 4 位>`（太短的 key 整体脱敏成 `***`）——只是想确认"配了哪些 provider"的场景不会意外把明文密钥带出去；需要明文时传 `include_secrets: true`。

```jsonc
// 请求（默认脱敏）
{"jsonrpc":"2.0","method":"config.getProvider","id":1}

// 响应
{"jsonrpc":"2.0","id":1,"result":{
  "providers": {
    "deepseek": {
      "api_type": "openai_compatible",
      "base_url": "https://api.deepseek.com/v1",
      "api_key": "***5678",
      "default_model": "deepseek-pro",
      "models": ["deepseek-pro", "deepseek-flash"]
    }
  },
  "default_provider": "deepseek",
  "task_models": { "subagent": "deepseek" }
}}

// 要明文，显式传 include_secrets
{"jsonrpc":"2.0","method":"config.getProvider","id":2,"params":{"include_secrets":true}}
```

`include_secrets: true` 本身不需要额外权限——和 `config.setProvider` 一样，能连上 daemon socket 就能用，见下方「安全提示」与 `daemon/src/server.rs` 模块文档的信任边界说明。

### `daemon.doctor`：只读诊断

不需要参数，返回当前 daemon 实例的配置/接线状态一览——排查"为什么我的 provider 没生效"时不用去翻日志：

```jsonc
// 请求
{"jsonrpc":"2.0","method":"daemon.doctor","id":1}

// 响应（节选）
{"jsonrpc":"2.0","id":1,"result":{
  "scene": "coding",
  "settings_tiers": [
    {"tier":"global","path":"/home/u/.atta/settings.json","exists":true,"parses":true,"error":null},
    {"tier":"scene","path":"/home/u/.atta/scenes/coding/settings.json","exists":false,"parses":true,"error":null},
    {"tier":"project","path":"/repo/.atta/settings.json","exists":true,"parses":true,"error":null}
  ],
  "providers": {
    "configured": ["deepseek"],
    "default_provider": "deepseek",
    "task_models": ["subagent"],
    "ok": true,
    "warnings": [],
    "error": null
  },
  "hooks": { "configured": false, "ok": true, "error": null },
  "session_persistence": { "history_store_wired": true },
  "permission_rules_count": 0,
  "model": { "model_name": "claude-sonnet-4-6", "api_type": "Anthropic" }
}}
```

`settings_tiers[].exists=false` 只是说明那一层没有文件（完全正常，不是每层都必须存在）；`parses=false` 才说明那一层的 JSON 语法有问题，`error` 字段给出具体原因。`providers.ok=false` 对应本文「出错行为」表格里 `default_provider` 那一行的硬错误。

---

## 安全提示

- `api_key` 明文存储，不加密。**放在全局层 `~/.atta/settings.json` 或 scene 级 `~/.atta/scenes/<scene>/settings.json` 相对安全**（都在用户主目录下，不在任何代码仓库里）；**项目级 `<repo>/.atta/settings.json` 如果被提交到共享仓库会泄露密钥**——如果需要项目级 override，尽量只覆盖非密钥字段（比如切换 `default_model`），密钥本身留在全局/scene 层。
- 不支持通过环境变量传入密钥作为这套多供应商配置的来源——所有凭证必须显式出现在某一级 `settings.json` 里。（注意：daemon 原有的单供应商 Anthropic 客户端目前仍然读取 `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` 环境变量,这是本设计尚未接管的旧路径，见文首的实施状态说明。）

---

## 与现状的差异对照

| | 现状（原有单供应商路径） | 本设计（新增，尚未接管实际请求） |
|---|---|---|
| 密钥来源 | 环境变量 `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` | `settings.json` 的 `providers.<id>.api_key` |
| 供应商数量 | 全进程唯一一个 | 任意多个，按 id 登记 |
| 任务级路由 | 无 | 已解析、已校验、已记日志；**尚未接到实际请求路径** |
| 模型名校验 | 无 | `models` 允许列表过滤 + 回退 |
| 供应商配置出错 | 不适用 | 分两条路径（provider 悬空 vs 模型不在允许列表），均软降级 + 警告，不阻塞启动（除非连 `default_provider` 本身都无效） |
