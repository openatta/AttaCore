# 配置与状态目录规范

> 本文档是 AttaCore **配置/状态目录布局**的稳定参考——只给定论和示例，不含决策取舍推理。决策过程与理由见 `docs/design/2026-08-03-agents-config-migration.md`。**目录布局（Phase 1-2）与跨工具导入功能（Phase 3+4：检测 Claude Code/Codex/Cursor 并单选导入）均已实施**——见该文档 §8。手动触发是 `/import`（`ImportTool`）；`daemon` 里最初 v1 暂未接 `ImportCallback` 真实回调（见 §8.8 历史记录），**第五轮起改为不依赖该回调的异步通知 + 直接 RPC 路径，见 §14.4**。**用户级命名空间不再叫"scope"，而是直接复用 `AgentScene`（见 §9.1）**：daemon 用 `--scene` 指定，值必须是代码里真实注册的场景（`coding`/`chat`/`demo`），不支持的值会让 daemon 启动失败，不存在自由填写的 `--scope` 字符串。**2026-08-04 第二轮：用户级 `.atta/` 扁平化 + `scenes/` 子目录（已实施，见 §11）**——用户级目录树现在和项目级几乎同构：只多一个 `scenes/<scene>/` override 目录（config 类资源）和一个 `skills/` 目录（项目级已迁到 `.agents/`）；`workflows/` 整体删除；`memory/sessions/vcr/mcp` 不再挂 scene，只分全局/项目两层。**2026-08-04 第四轮：settings.json 单一化 + schema 发布 + `daemon.doctor`/`config.setProvider` RPC（已实施，见 §13）**——`daemon` 自己维护的一份平行 `SettingsFile` 被彻底删除，`Settings::load()` 是唯一权威解析入口；发布了 `docs/schemas/settings.schema.json`；新增只读诊断 RPC `daemon.doctor` 与写供应商配置 RPC `config.setProvider`（局部 patch 语义，写入项目层）；顺带补齐了 session 持久化、`AgentTool` scope 继承、`AgentTypeDefinition.model` 覆盖、Prompt/Agent 型 hook executor 等此前已知的"配了不生效"缺口。**2026-08-04 第五轮：MCP 真正接线 + RPC 面从 6 个扩到 13 个 + 新增异步通知机制（已实施，见 §14）**——`Settings.mcp_servers` 不再是解析了没人用的死配置，daemon 启动时后台异步连接（不阻塞启动，失败告警+通知）；新增 `daemon.subscribeEvents`（通用异步事件订阅）、`mcp.status`/`mcp.addServer`、`import.list`/`import.run`（跨工具导入终于不用绕一圈 LLM turn 了）、`session.close`、`config.getProvider`（默认脱敏 `api_key`）。

## 核心原则

1. **只有"其他 agent 工具真的认得"的东西才进 `.agents/`。** 目前只有 `AGENTS.md` 本身和 `.agents/skills/` 满足这条（Codex 官方约定）。其余一切——包括 AttaCore 自己发明的 `agents/rules/hooks`——都是私有扩展，进 `.atta/`，不管它是不是"人也想看"。
2. **项目级共享、用户级私有。** 项目目录（`repo/`）可能被团队、被 Codex/Cursor 等别的工具打开，`.agents/` 是这个场景下的公共接口；用户主目录（`$HOME`）不存在"被别的工具扫描"的场景，不需要为了对齐外部标准而多开目录。
3. **两类资源，两种"要不要分 scene"的答案（2026-08-04 第二轮确立）：**
   - **config 类**（`settings.json`/`skills/`/`plugins/`/`agents/`/`rules/`/`hooks/`）——按名字/字段索引，天然适合"全局默认 + scene 覆盖"：用户级放在 `~/.atta/<resource>/`（全局默认）+ `~/.atta/scenes/<scene>/<resource>/`（scene 覆盖，同名后者赢）。
   - **状态/历史类**（`memory/`/`sessions/`/`vcr/`/`mcp/`）——**不分 scene**，只分"全局"与"项目"两层：`~/.atta/<resource>/`（全局，用于未来"没有具体项目"的场景，如桌面端）+ `<repo>/.atta/<resource>/`（项目，当前唯一实际生效的一层）。不分 scene 的理由：一次对话属于哪个项目、该被哪份 memory/session 记住，跟这次对话用的是哪个 scene（工具集/系统提示词配置）无关——借用其他 scene 的记录是正确性隐患（比如用错误的工具配置恢复一个 session，或 VCR 磁带对不上当前工具集）。
   - `workflows/`（命名工作流）**已整体删除**——这个功能在有 loader 之前就被砍掉了，不再是任何目录树的一部分。
4. **引擎层（`base::paths::ConfigPaths` 等函数）不内置任何默认值**——函数签名要求显式传一个 `scope: &str` 参数（这个底层参数名字仍叫 `scope`，纯粹是"给我一个命名空间字符串"的通用抽象，不关心调用方从哪来的）。**本仓库自带的 `daemon` 把这个字符串定为"经过校验的 `AgentScene::id()`"**：`--scene` CLI flag（默认 `coding`）只接受代码里真实注册的场景名，传别的值 daemon 直接启动失败——不存在"自由字符串 + 事后校验格式"这种设计，非法值根本到不了路径拼接那步。

---

## 项目级目录树

```
repo/
├── AGENTS.md                  # 项目常驻指令入口；逐级向上叠加读取（子目录优先于父目录/repo root）
├── .agents/                   # 外部事实标准区——只放别的 agent 工具认识的东西
│   └── skills/
│       └── <name>/
│           └── SKILL.md
└── .atta/                     # AttaCore 私有区：运行时状态 + 自家扩展概念
    ├── .imported.json          # 跨工具配置导入的决策记录（是否已导入/跳过/待定）
    ├── settings.json           # 项目级 settings 覆盖（分层：内置默认 < 全局 < scene 级 < 项目级 < CLI，见下方"用户级目录树"）
    ├── memory/                 # 跨会话 memory 文件——不分 scene，见核心原则 3
    ├── mcp/                    # MCP server 连接状态/缓存——不分 scene
    ├── sessions/                # 会话记录——不分 scene
    ├── vcr/                     # 录制/回放测试用的 VCR 磁带——不分 scene
    ├── agents/                   # subagent 定义（AttaCore 扩展）
    ├── rules/                    # 长文档规则，由 AGENTS.md 显式引用，不自动加载
    └── hooks/                    # hook 脚本本体（是否启用/匹配什么事件仍在 settings.json 里配置）
```

**判定"这是不是已经处理过的 AttaCore 项目"**：存在 `.agents/` 目录（不论是否为空）即视为是。首次打开检测到别的工具痕迹（`CLAUDE.md`/`.claude/`、`.cursorrules`/`.cursor/rules/*.mdc`）但没有 `.agents/` 时，才会触发导入提示；具体规则见迁移设计文档 §3。

---

## 用户级目录树

```
$HOME/
├── AGENTS.md?                  # 可选：用户级全局指令（是否接入 walk-up 待定，见迁移文档开放问题）
└── .atta/                      # 扁平——config 类资源的全局默认层
    ├── settings.json            # 跨 scene 全局配置，优先级最低——被 scenes/<scene>/settings.json 覆盖
    ├── skills/                   # 全局默认技能——不迁移到 .agents/，用户级根本不存在 .agents/
    ├── plugins/                  # 全局已安装插件
    ├── agents/                   # 全局默认 subagent 定义（已实施，见 §12）
    ├── rules/                    # 全局默认长文档规则，惰性发现索引（已实施，见 §12）
    ├── hooks/                    # 全局默认 hook 脚本（已实施，见 §12）
    ├── memory/                   # 跨 scene 全局 memory——不分 scene，用于"没有具体项目"场景（未来桌面端），daemon 今天始终有项目，实际读的是项目层
    ├── sessions/                  # 同上，不分 scene
    ├── vcr/                       # 同上，不分 scene
    ├── mcp/                       # 同上，不分 scene
    └── scenes/                    # config 类资源的 scene 覆盖层——只有需要"这个 scene 要特化"时才会有内容
        └── <scene>/                 # 本仓库里就是 AgentScene::id()：coding/chat/demo/research 之一
            ├── settings.json         # 覆盖全局同名字段
            ├── skills/                # 覆盖/新增全局同名技能
            ├── plugins/               # scene 专属插件
            ├── agents/                # 覆盖/新增全局同名 subagent 定义
            ├── rules/                 # 覆盖/新增全局同名规则文档
            └── hooks/                 # 覆盖/新增全局同名 hook 脚本
```

举例：本仓库的 `daemon` 用 `--scene coding` 启动，scene 覆盖层落地路径就是 `~/.atta/scenes/coding/...`；用 `--scene chat`，就落在 `~/.atta/scenes/chat/...`，两者互不干扰，但都共享同一份 `~/.atta/skills/` 等全局默认。如果你基于 AttaCore 构建别的产品，不一定非要叫"scene"或复用 `AgentScene`——引擎层的 `scope: &str` 参数本身还是通用的，你可以传任何你自己中意的命名空间字符串；本仓库只是选择了"复用已有的 AgentScene 概念、并对其做校验"这个具体做法，而不是接受自由字符串。

**优先级链**：`内置默认 < ~/.atta/<resource>（全局） < ~/.atta/scenes/<scene>/<resource>（scene 覆盖） < <repo>/.atta/<resource>（项目） < CLI 参数`——仅对 config 类资源成立；`memory/sessions/vcr/mcp` 没有 scene 这一环，是 `全局 < 项目`。`settings.json` 的合并实现在 `daemon/src/config.rs::load_daemon_config()`（`DaemonPaths::{global_root, config_root, project_root}`），详见 `docs/design/2026-08-03-agents-config-migration.md` §10-§11。

---

## `.atta/` 子目录一览

| 目录/文件 | 内容 | Scene 覆盖层？ | 是否随导入功能填充 |
|---|---|---|---|
| `settings.json` | 引擎运行参数（model/permission/mcp_servers/providers 等） | 是 | 否 |
| `skills/` | 技能定义（项目级已废弃，见下方"skills 去哪了"） | 是（仅用户级） | 用户级导入不涉及；项目级技能走 `.agents/skills/` |
| `plugins/`（仅用户级） | 已安装插件 | 是 | 否 |
| `agents/` | subagent（`AgentTool` 的 `subagent_type`）定义 | 是 | 否——见 §12（未接入 `crates/plugin::agent_registry`，那是独立的 plugin-manifest 来源） |
| `rules/` | 长文档规则，`AGENTS.md` 可引用，**且有惰性发现索引**（见 §12） | 是 | Cursor `.mdc` 导入的目标位置 |
| `hooks/` | hook 脚本本体，`command` 可用裸文件名引用（见 §12） | 是 | 否 |
| `memory/` | 跨会话 memory 文件（`MEMORY.md` 索引 + 主题文件） | **否** | 否 |
| `mcp/` | MCP server 连接态/缓存 | **否** | 否 |
| `sessions/` | 会话历史 | **否** | 否 |
| `vcr/` | 测试用录制/回放磁带 | **否** | 否 |
| `.imported.json`（仅项目级） | 导入决策记录 | — | 是——导入流程本身写入 |

`workflows/` 已删除，不在此表中——见核心原则 3。

### skills 去哪了？

这是唯一一处项目级答案和用户级不同的地方（用户级本身还多一层 scene 覆盖），容易记混，专门列出来：

| 层级 | Skills 权威位置 | 原因 |
|---|---|---|
| 项目级 | `.agents/skills/` | 需要能被 Codex 等外部工具直接扫到 |
| 用户级 · 全局默认 | `~/.atta/skills/` | 用户主目录不存在 `.agents/`，没有对外扫描的需求 |
| 用户级 · scene 覆盖 | `~/.atta/scenes/<scene>/skills/` | 同名时覆盖全局默认，给需要按 scene 定制技能的场景用 |

---

## `AGENTS.md` 模板

```markdown
# 项目指令

## 项目结构
...

## 构建与测试命令
...

## 编码约束
...

## 修改完成标准
...

## 详细规则

改动架构相关代码前，阅读：
- `.atta/rules/architecture.md`
- `.atta/rules/security.md`

完成编码任务前，遵循：
- `.atta/rules/testing.md`

## 可用 Skills

见 `.agents/skills/`，Agent 会按需自动发现并调用。
```

## `.agents/skills/<name>/SKILL.md` 最小示例

```markdown
---
name: code-review
description: Review code changes for correctness, architecture, security, and tests.
---

# Code Review

1. Inspect the changed files.
2. Identify correctness and architecture issues.
3. Run relevant tests.
4. Report findings by severity.
```

---

## 与其他工具的关系速查

| | AttaCore | Codex | Claude Code | Cursor |
|---|---|---|---|---|
| 指令文件 | `AGENTS.md` | `AGENTS.md` | `CLAUDE.md` | `.cursorrules` / `.cursor/rules/*.mdc` |
| 项目级 skills | `.agents/skills/` | `.agents/skills/` | `.claude/skills/` | — |
| 项目级私有状态 | `.atta/`（扁平） | `.codex/`（扁平） | `.claude/`（扁平，且部分内容常被提交） | `.cursor/`（扁平） |
| 用户级私有状态 | `~/.atta/`（扁平，全局）+ `~/.atta/scenes/<scene>/`（覆盖） | `~/.codex/` | `~/.claude/` | — |

AttaCore 对 `CLAUDE.md`/`.claude/skills/`/`.cursorrules`/`.cursor/rules/*.mdc` 提供**单选、用户确认后执行**的导入功能（同时检测到多个来源时，只能选一个，不支持合并导入），把内容迁移进 `AGENTS.md` + `.agents/skills/` + `.atta/rules/`；已经是 `AGENTS.md` + `.agents/skills/` 形态的项目（如 Codex 项目）直接原样兼容，只在缺 `.agents/skills/` 时补目录。用户手动触发用 `/import`（内置 skill + `Import` Tool）；宿主也可以注册 `ImportCallback` 实现进程启动时自动检测+询问（daemon 本身 v1 未接，见迁移文档 §8.8）。完整的检测规则、字段映射表、落地阶段见 `docs/design/2026-08-03-agents-config-migration.md`。

---

## 面向 AttaCore 集成方的说明

AttaCore 是一个通用 agent 库，**引擎层本身不预设、不内置任何具体命名空间**——`base::paths::ConfigPaths::from_env(cwd, scope)` 等函数的 `scope: &str` 参数是必填的，不存在"AttaCore 自带的默认产品身份"这回事。任何基于 AttaCore 构建产品的调用方，在自己的公开接口层（CLI flag、配置文件、环境变量等）决定这个字符串怎么来、要不要校验、要不要给默认值——**本仓库自带的 `daemon` 选择了"复用已有的 `AgentScene` 概念，并对输入做封闭集合校验"**这个具体做法：`--scene` 只接受代码里注册过的场景（`coding`/`chat`/`demo`），传别的值 daemon 直接启动失败，不是"接受任意字符串再兜底"。这不代表所有基于 AttaCore 的产品都必须复用 `AgentScene` 作为命名空间来源——你可以按自己的产品形态设计校验规则，引擎不替你做这个选择。项目级目录（`.atta/`、`.agents/`）不需要设置这个命名空间，天然按仓库隔离。（本仓库注册的场景集合会随时间增加——写这段时是 `coding`/`chat`/`demo`，当前实际是 `coding`/`chat`/`demo`/`research`，见 `crates/scene/src/scene/mod.rs::register_builtin`——校验规则本身不变，只是集合内容不是本文档要跟踪的东西。）

`ConfigPaths` 暴露 `user_data_dir`（scene 覆盖根，`{global_data_dir}/scenes/<scope>/`）和 `global_data_dir`（扁平全局根）两个字段——`memory`/`sessions`/`vcr`/`mcp` 只应该从 `global_data_dir`（+ 项目级 `local_data_dir`）派生，不应该从 `user_data_dir` 派生，否则会意外引入 scene 隔离，见核心原则 3。

---

## 11. 第二轮：用户级 `.atta/` 扁平化 + `scenes/` 子目录（2026-08-04，已实施）

在实现多 LLM 供应商配置（`docs/design/2026-08-04-multi-provider-llm-migration.md`）的过程中，用户进一步提出：个人配置目录 `.atta/` 也应该扁平化，和项目级目录树同构，只是多一个 `scenes/` 子目录承载"确实需要按 scene 特化"的部分。决策与实施：

1. **`workflows/` 整体删除**——这个功能从未有过 loader，直接砍掉，不再出现在任何目录树、`ConfigPaths` 方法或文档里。
2. **`sessions/`/`vcr/`/`memory/`/`mcp/` 不再挂 scene**，只分"全局"（`~/.atta/<resource>/`）与"项目"（`<repo>/.atta/<resource>/`）两层——理由见核心原则 3。全局层目前主要是"未来没有具体项目时"的占位（比如桌面端），daemon 今天启动时永远有 cwd/项目，实际生效的是项目层。
3. **`skills/`/`plugins/`/`agents/`/`rules/`/`hooks/`/`settings.json` 保留 scene 覆盖能力**，但物理结构从"`~/.atta/<scope>/<resource>/`"改为"`~/.atta/<resource>/`（全局默认）+ `~/.atta/scenes/<scope>/<resource>/`（scene 覆盖，同名后者赢）"。
4. `crates/core/src/paths.rs::ConfigPaths` 重构为 `user_data_dir`（= scene 覆盖根）+ `global_data_dir`（= 扁平全局根）两个字段；`atta_scope_dir(scope)` 现在返回 `$HOME/.atta/scenes/<scope>/`，新增 `atta_global_dir()` 返回 `$HOME/.atta/`。
5. `daemon/src/config.rs::DefaultDaemonPaths` 同步调整：`config_root()` 现在是 `{global_root}/scenes/<scope>/`，`global_root()` 独立存储（不再靠"取 config_root 的 parent"推导，因为现在要跳两层）。`ATTA_CONFIG_HOME` 的含义相应变化：现在覆盖的是全局根，不是 scene 根。
6. 接线改动：`crates/runtime/src/agent.rs`（skills 加载、memory store 构造）、`crates/core/src/frozen/{skill,memory}.rs`（skills 扫描新增全局层；memory 系列函数去掉 `scope` 参数）、`crates/mcp/src/config.rs`（mcp 配置改走全局层，enterprise policy 路径改用 `global_data_dir` 直接拼接）、`crates/tools/src/bash/sandbox.rs`（sandbox 写保护规则新增全局 `settings.json` 一条，scene 规则路径加 `scenes/` 段）、`daemon/src/main.rs`（memory store 构造改用全局层）。
7. **未做**：`agents/`/`rules/`/`hooks/` 的 loader 本来就不存在（零调用点，纯路径占位），本轮只是让这些路径方法反映新形状，不涉及新增任何加载逻辑——留给未来。`team_memory.rs` 使用的 `atta_scope_dir()` 未做语义调整（该文件本身未被 `crates/team` 的 `lib.rs` 声明为模块，不参与编译，不影响本轮）。

验证：`cargo build --workspace`、`cargo clippy --workspace --lib --tests`、`cargo test --workspace --no-fail-fast` 均已跑过；失败项均为已知与本次改动无关的网络环境问题（`ping`/`web_fetch` 本地 sandbox 网络异常、`team::remote_agent` HTTP 测试）。

---

## 12. 第三轮：agents / rules / hooks 从路径占位变成真正生效（2026-08-04，已实施）

上一轮只是让 `agents/`/`rules/`/`hooks/` 的路径方法反映新形状，加载逻辑本身是空白。这一轮把三者分别补齐——但补的方式不一样，因为三者的现状和设计意图完全不同（分析过程见调研记录，此处只记结论）。

### 12.1 `hooks/` —— 引擎本来就是成品，缺的是接线

`crates/hooks::HookRunner` 一直是功能完整、测试充分的引擎，但 `Settings.hooks_config`（settings.json 的 `hooks` 字段）在所有生产代码路径里都是硬编码 `None`，`Builder::hooks(...)` 注入口全仓库零调用——配了也不生效。这一轮：

- `daemon/src/config.rs`：`SettingsFile`/`DaemonConfig` 新增 `hooks: Option<serde_json::Value>`，随 global→scene→project 三层合并（整段替换，和 `mcp_servers` 同规则）。**（`SettingsFile` 本身已在下方 §13 被整体删除——这里保留原始记录，字段现在直接是 `Settings.hooks_config`。）**
- `daemon/src/main.rs`：`Settings.hooks_config` 改为读 `daemon_config.hooks`，不再硬编码 `None`。
- `crates/runtime/src/agent.rs::Builder::build()`：新增 `build_hook_runner(&settings)`，把 `hooks_config` 解析成 `hooks::HooksSettings` 构造真正的 `HookRunner`（解析失败 → 警告 + 空 hooks，不阻断启动）。
- `crates/hooks::HookRunner` 新增 `with_hooks_search_dirs(dirs)`：`Command` 型 hook 执行子进程时，把 `.atta/hooks/` 三层目录（project > scene > global）prepend 进子进程的 `PATH`，`command` 字段可以直接写裸文件名（如 `check.sh`）走标准 shell 查找，不需要写全路径——没有引入第二套 hook 配置协议，`command` 依然是普通 shell 字符串，只是多了个查找路径。

### 12.2 `agents/` —— 整合两份重复实现，而不是各自补功能

现状是三份互不相认的东西：`crates/plugin::agent_registry`（数据源零调用点的死代码，读的是 plugin manifest 不是 `.atta/agents/`）、`crates/tools/src/agent_tool.rs`（920 行，未被 `crates/tools/src/lib.rs` 声明为模块，完全不参与编译）、`crates/runtime/src/agent_tool.rs`（真正编译、但 5-6 个 subagent 类型全硬编码在两个独立的 `match` 里，且 `.atta/agents/*.md` 的解析器写好了没人调）。更严重的是：**`AgentTool` 本身在 `Builder::build()` 里从未被构造/注册过**——`CodingScene` 的系统提示词明确告诉模型"用 Agent 工具生成子任务"，但这个工具压根不在模型能调用的工具列表里。

处理方式：删除孤儿文件 `crates/tools/src/agent_tool.rs`；`crates/runtime/src/agent_tool.rs` 里把 `type_prompt()`/`resolve_tools()` 两个硬编码 `match` 重构成查一份合并后的 `HashMap<String, AgentTypeDefinition>`（新增 `merge_agent_types()`，`load_agent_types_from_dir()` 改成同步 `std::fs`，可以在 `Builder::build()` 的同步上下文里直接调用）——6 个内置类型（explore/plan/general-purpose/claude/code-reviewer/worker）作为最低优先级默认值，`.atta/agents/*.md` 按 全局 < scene < 项目 覆盖，同名整条替换。`AgentTool::description()` 现在会把当前实际可用的类型列表（内置 + 自定义）拼进工具描述文本，让模型能看到有效的 `subagent_type` 取值，不用瞎猜。`Builder::build()` 里新增真正的构造 + 注册，用 `settings.model` 派生一个最小 `EngineConfig`（`EngineConfig` 本身尚未整体接入 `Builder`，这是已知的更大缺口，不在本轮范围）。

**一个新增的安全考虑**：把 `AgentTool` 真正接上之后，"full access"类型（如 `general-purpose`）会拿到父级完整工具集——而这个工具集现在包含 `AgentTool` 自己，会导致子 agent 可以无限递归再生成子 agent。`resolve_tools()` 因此在返回"完整工具集"时，显式排除 "Agent" 这个名字本身，把递归深度硬性锁死在 1 层，不依赖 `EngineConfig.max_agent_depth`（那个字段本来就没被真正串起来）。

**未采纳用户最初"内置用 AgentTool、自定义用 tools::agent_tool 分开加载"的字面提议**：改为单一 `AgentTool`（对齐 skills 的单一 `SkillTool` 先例），内置与自定义只是同一张表里两种优先级不同的数据来源，理由是把自定义类型做成独立的、逐个注册的 LLM 工具会导致工具列表随自定义 agent 数量线性膨胀（每个工具定义都要素随请求发送，增加 token 成本），且模型只需要认识一个稳定的调度入口，不需要认识 N 个几乎同构的工具名字。

**未做**：`crates/plugin::agent_registry` 仍未接入（它读的是 plugin manifest，跟 `.atta/agents/*.md` 是两个概念，是否要合并成同一份仍是开放问题）。

### 12.3 `rules/` —— 保持惰性设计，只补"看不见"的缺口

确认现状是刻意设计（不自动加载，`AGENTS.md` 手写引用，模型用 Read 按需读取），不是没做完。唯一的缺口：放进 `.atta/rules/` 但没被 `AGENTS.md` 引用的文件，对模型完全不可见。新增 `crates/core/src/interface/rules.rs::{discover_rules, build_rules_prompt}`：扫描 `.atta/rules/` 三层（不分 scene 覆盖），每个文件只读**第一行**当描述（不读全文），拼成一个"Available Rules"清单块，接入 `crates/core/src/interface/prompt.rs::assemble_prompt()`（和 Memory 块同级，真实生产路径，不是容易被绕过的 `Tool::prompt()` 机制）。目录下没有任何规则文件时整个块省略，不产生任何 token 开销。内容依然是惰性的——模型看到的只是"文件名 + 一行描述"，要看正文还是得显式 `Read`。

### 12.4 验证

新增/改动测试：`crates/hooks`（PATH 解析 2 个）、`daemon/src/config.rs`（hooks 字段合并 2 个）、`crates/runtime/src/agent.rs`（`build_hook_runner` 3 个）、`crates/runtime/src/agent_tool.rs`（`merge_agent_types`/`resolve_tools` 递归防护等 9 个）、`crates/core/src/interface/rules.rs`（7 个）。`cargo build --workspace`、`cargo clippy --workspace --lib --tests`、`cargo test --workspace --no-fail-fast` 全部跑过，失败集合与前几轮记录的一致（`team::remote_agent` HTTP 测试 + `ping`/`web_fetch` 本地网络环境问题），均确认与本轮无关。

---

## 13. 第四轮：settings.json 单一化 + schema 发布 + 诊断/写配置 RPC + 收尾（2026-08-04，已实施）

前几轮把 `agents`/`rules`/`hooks` 从路径占位变成真正生效，但发现一个更底层的结构性风险：`daemon` 自己维护一份平行的 `SettingsFile`（字段集比 `core::Settings`窄得多，只认 `model`/`max_tokens`/`mcp_servers`/`providers`/`task_models`/`hooks` 六个），每新增一个字段要在 `SettingsFile`、`merge_settings`、`DaemonConfig`、`daemon/src/main.rs` 里手搭的 `Settings{}` 字面量四处同步改——`permission_rules`/`permission_mode` 就是因为这样才一直被解析成"配了也不生效"。这一轮把它彻底拆掉，改为唯一权威实现，并补齐诊断与写配置能力：

### 13.1 唯一权威 `Settings::load()`

- `crates/core/src/interface/settings.rs` 新增通用递归 JSON 合并 `pub fn merge_json_values(base, over)`——对象按 key 递归合并，其余类型（数组/标量/null）整体替换；`Settings::load(global_dir, scene_dir, local_dir, scope, default_model)` 现在是**唯一**入口：以 `Settings::defaults_for(default_model)` 序列化成 JSON 作为起点，依次读 global→scene→project 三层 `settings.json`，逐层用 `merge_json_values` 叠加（跳过 `paths` 键，`paths` 由 `Settings::load` 自己算，不受任何一层文件影响），最后反序列化回 `Settings`。任意一层文件缺失（正常情况）静默跳过；存在但读不出/解析不出时 `tracing::warn!` 打印告警并跳过该层，不会导致整个 daemon 启动失败；最终反序列化失败时同样告警并回退到纯默认值。
- `daemon/src/config.rs`：删除 `SettingsFile`/`merge_settings`/`load_single`，`DaemonConfig` 直接持有 `pub settings: base::interface::settings::Settings` 一个字段（外加 socket/lock 路径、session 容量等纯进程级字段）；`load_daemon_config()` 变成对 `Settings::load()` 的一层薄包装。`daemon/src/main.rs` 里原本手写的约 70 行 `Settings{}` 字面量整体删除，改为 `daemon_config.settings.clone()`。
- 影响：`permission_rules`（`Bash(git push:*)` 这类规则）现在真的会被 `Settings::load()` 解析出来——之前是配了也没用的死字段，见 §7 遗留问题的根因分析。**注意区分"解析"和"生效"**：解析确实修好了（`Settings.permission_rules` 现在能从 settings.json 读到真实值），但**尚未接入实际权限判断**——`daemon/src/config.rs::DaemonConfig.permission_rules: RuleSet` 至今仍硬编码 `RuleSet::empty()`，`daemon/src/main.rs` 也仍硬编码 `AllowAllPermission`/`PermissionMode::BypassPermissions`（"IDE 插件自己管沙盒"）。也就是说：今天配置 `permission_rules` 对 daemon 的实际工具调用判断**零效果**，只会体现在 `daemon.doctor` 的 `permission_rules_count` 计数里。把 `Settings.permission_rules` 转换成真正生效的 `RuleSet` 并接入判断路径，是独立于本轮的后续工作。
- 另外两处顺带发现的"配了但没消费"缺口，一并记在这里避免重蹈覆辙：`Settings.feature_flags` 8 个标志里有 6 个（`team_mode`/`plugin_marketplace`/`extended_memory`/`experimental_agent`/`vcr_auto_detect`/`dream_task`）在 `crates/core/src/features.rs` 之外零调用点，只有 `reactive_compact`（`crates/runtime/src/turn.rs`）和 `cached_microcompact`（`crates/runtime/src/agent.rs`）是真正被读取的；`crates/tools/src/config.rs::ConfigTool` 暴露给模型的 `SETTINGS_STORE`（`theme`/`auto_memory` 等字段）是一套完全独立的运行时内存态存储，字段名和 `Settings` 结构体本身（比如真实字段是 `memory_enabled` 不是 `auto_memory`）没有对应关系，不要把两者当成同一件事。

### 13.2 settings.json 的 JSON Schema

`Settings` 及其可达的每个嵌套类型（`ModelSettings`/`PathSettings`/`ProviderConfig`/`TaskModelOverride`/`PermissionMode` 等）加上 `#[derive(schemars::JsonSchema)]`；`crates/core/src/interface/settings.rs::settings_json_schema()` 用 `schemars::schema_for!(Settings)` 生成，发布为 `docs/schemas/settings.schema.json`（`cargo test -p base settings_schema_matches_committed_file -- --ignored` 重新生成并落盘）。Schema 直接从类型定义推导，不手工维护，不会和 `Settings::load()` 实际接受的字段脱节；可以在任意一层 `settings.json` 顶层加 `"$schema": "../../../docs/schemas/settings.schema.json"`（相对路径按实际层级调整）获得编辑器自动补全/校验。

### 13.3 `daemon.doctor`：只读诊断 RPC

新增 `daemon/src/doctor.rs::run_doctor()` + `daemon.doctor` RPC 方法（`daemon/src/server.rs`）。不需要参数，返回：三层 `settings.json` 各自是否存在/能否解析（连读取/JSON 语法错误一起报，不用翻日志）、`providers`/`default_provider`/`task_models` 用 `base::provider::resolve_task_models()` 实际跑一遍校验的结果（`ok`/`warnings`/`error`）、`hooks_config` 能否解析成 `hooks::HooksSettings`、session 持久化（`HistoryStore`）是否接上、`permission_rules` 条数、当前 model 配置摘要。详见 `docs/LLM_PROVIDERS.md`「通过 RPC 配置」一节的完整示例。

### 13.4 `config.setProvider`：写供应商配置 RPC

新增 `daemon.SessionPool::set_provider()` + `config.setProvider` RPC 方法。语义要点：

- 只写**项目层** `settings.json`（`<repo>/.atta/settings.json`），且是**局部 patch**——`config`/`task_models` 参数保持原始 `serde_json::Value`（不反序列化成带 `#[serde(default)]` 的类型结构体再序列化回去），否则调用方没提到的字段会被强制置 `null` 顺带覆盖掉已有值。patch 通过 §13.1 的 `merge_json_values` 合并进现有项目层文件内容，语义与手写多层 `settings.json` 做字段级覆盖完全一致。
- `delete: true` 直接从 JSON 里删掉整个 `providers.<id>` 键（不是合并出一个 `null`）；引用了这个 id 的 `task_models` 条目**不需要额外处理**——`resolve_task_models()` 本身的软降级（未知 provider → 警告 + 回落 `default_provider`）在下次路由解析时自动生效，对应最初"供应商被删除时任务自动降级"的需求。
- 写入始终落盘（哪怕合并后的整体配置校验不通过），响应里的 `routing.ok`/`routing.error` 单独报告校验结果——校验失败不回滚已写入的 patch，因为 patch 本身通常是合法的，只是和现有配置搭配后不自洽，需要调用方再修一次，而不是静默丢弃用户刚提交的输入。
- 写入成功后立即用 `Settings::load()` 重新加载全局→scene→项目三层合并出的完整配置，替换 `SessionPool` 内部一个新增的 `tokio::sync::RwLock<Arc<Settings>>`（原来是不可变 `Arc<Settings>`）——之后新建的 session 会看到更新后的配置；已经在运行的 session 不受影响。

### 13.5 顺带修的完备性缺口

- **Session 持久化**：`daemon/src/main.rs` 原来硬编码 `history_store: None`，`SessionManager::persist()`/`resume()` 也只是签名存在、实现是空壳。现在 `main.rs` 真正构造 `JsonlHistoryStore`（失败降级为纯内存 + 告警，不阻断启动），`SessionManager::persist()`/`resume()` 补上真实的增量写入/从磁盘重建逻辑（`crates/session/src/session.rs`）。
- **`AgentTool` 不再硬编码 `scope: "code"`**：§12.2 把 `AgentTool` 真正接上生产路径之后，它原有的 `sub_settings()`/`run_sub_inner()` 里硬编码的 `scope: "code"` 就从"从未被走到的死代码"变成了真实 bug（父 session 用别的 scene 时，子 agent 会扫错 skills/hooks/agents 目录）。新增 `AgentTool::with_settings()` + `Inner.parent_settings`，子 agent 的 settings 现在从父 session 真实继承，只覆盖 model 相关字段。
- **`AgentTypeDefinition.model` 覆盖生效**：`.atta/agents/*.md` frontmatter 里的 `model` 字段之前解析了但没人用；现在 `run_sub()`/`run_sub_inner()` 会查这张表，命中时用于覆盖子 agent 的模型设置。
- **Prompt/Agent 型 hook 有了真正的 executor**：`crates/runtime/src/hook_executors.rs`（新文件）新增 `ModelPromptHookExecutor`（包一层 `Arc<dyn Model>`，跑一次 `stream()` 收集文本）和 `AgentSpawnerHookExecutor`（包一层 `Arc<dyn AgentSpawner>`，转发给 `spawn_agent`），`Builder::build_hook_runner()` 现在会注入这两个 executor——之前配了 `type: "prompt"`/`type: "agent"` 的 hook 永远走"未配置 executor"分支，等于配了不生效。
- **remember skill 里过时的路径文案**：`~/.atta/<scope>/memory/` 早在 §11 就已经改成扁平的 `~/.atta/memory/`（不分 scene），但面向模型的 skill 描述文案没跟着改，一直在教模型往错误路径找 memory 文件；已同步修正（`crates/skills/src/bundled.rs`、`crates/tools/src/skill_tool.rs`、`crates/tools/src/prompts/coding/skill.prompt.md`）。

### 13.6 验证

新增/改动测试：`crates/core/src/interface/settings.rs`（`merge_json_values`/三层合并/告警降级/schema 一致性等 10 个）、`daemon/src/config.rs`（全部改写以适配单一 `Settings` 字段）、`daemon/src/doctor.rs`（4 个）、`daemon/tests/daemon_e2e.rs`（`daemon.doctor`/`config.setProvider` 共 5 个新增 e2e 测试，覆盖写入落盘、局部 patch 不清空未提及字段、删除、参数校验）、`crates/session/src/session.rs`（`persist`/`resume` 真实读写往返 3 个）、`crates/runtime/src/agent_tool.rs`（`custom_agent_type_model_override_is_applied_to_sub_settings` 等 catalog 测试）、`crates/runtime/src/hook_executors.rs`（新文件，含 stub Model/Spawner 测试）。`cargo build --workspace`、`cargo clippy -p daemon -p base --all-targets`、`cargo test -p daemon`/`-p base` 全部跑过；`daemon_e2e.rs` 里 `run_turn_without_session_id_creates_new`/`run_turn_nonexistent_session_errors` 两个需要真实发起 LLM 网络请求的测试在当前沙箱环境里挂起（无出网权限、请求不超时），与本轮改动无关，是运行环境限制而非代码问题。

---

## 14. 第五轮：MCP 真正接线 + daemon RPC 面扩容 + 异步通知机制（2026-08-04，已实施）

对四个子系统（配置架构、agents/skills/rules/hooks、daemon RPC、plugin）做完备性审查后发现：MCP 和 §13 之前的 `providers`/`task_models` 是同一种"配了但没人消费"缺口——`Settings.mcp_servers` 一直被正确解析，但从来没人把它喂给 `mcp::McpManager::connect_all()`；`daemon` 的 RPC 面也明显窄于底层已有能力（只有 6 个方法：`daemon.status`/`daemon.doctor`/`daemon.shutdown`/`session.list`/`session.run_turn`/`config.setProvider`），跨工具导入（`/import`）必须在一个跑着的 session 里才能触发，没有 daemon 直接可调的路径。这一轮把这些补上：

### 14.1 MCP 真正连接——异步、不阻塞启动、失败告警 + 通知

`daemon/src/main.rs` 在 `SessionPool` 构建完成、开始 `serve_unix`/`serve_tcp` 之前的窗口内，调用新增的 `SessionPool::connect_mcp_servers_in_background(mcp_servers)`——`tokio::spawn` 出去，**不阻塞启动**（这是本轮明确的设计要求：单个 server 连不上不该拖慢整个 daemon）。每个 `settings.mcp_servers` 条目（`HashMap<String, serde_json::Value>`）先 `serde_json::from_value::<mcp::config::McpServerConfig>()` 解析，解析失败或连接失败（`McpManager::connect_all` 内部已有 5 次指数退避重试，失败会 `tracing::warn!`）都不会中断其余 server 的连接，也都会各自触发一条 §14.3 的 `daemon.event` 通知（`mcp_connected` / `mcp_connect_failed`）。

`SessionPool` 新增字段 `mcp: AsyncRwLock<Arc<McpManager>>`（默认 `McpManager::empty()`，后台连接完成后整体替换）。**没有直接把这一个 `Arc<McpManager>` 塞进每个 session**——`McpManager::refresh_tools()` 需要 `&mut self`（每轮之间刷新 MCP 工具列表，`crates/runtime/src/turn.rs`），一个被多个 session 共享的 `Arc` 给不了这个可变性，尝试过这个方向被 `cannot borrow data in an Arc as mutable` 直接挡住。改为：每个新建 session 在 `SessionPool::create()` 里，用中心管理器已连接的 `McpClientHandle`（`mcp::client::McpClientHandle = Arc<dyn McpClient>`，clone 是廉价的 Arc clone，不是重新连接）通过 `McpManager::from_clients(...)` 建一份**该 session 自己独占**的 `McpManager`，再 `refresh_tools().await` 一次填充工具列表——每个 session 有一份轻量独立拷贝，但底层连接只建一次。`crates/runtime/src/agent.rs::Builder::mcp_manager()` 签名不变（仍是 owned `McpManager`），只是现在 `daemon` 真的调用它了。

### 14.2 新增 RPC：`mcp.status` / `mcp.addServer`

- `mcp.status`：包一层 `McpManager::server_statuses()`，返回每个已连接 server 的 `name`/`transport`/`tool_count`。
- `mcp.addServer {name, config}`：和 `config.setProvider` 同样的局部 patch 写入方式（写进项目层 `settings.json` 的 `mcp_servers.<name>`），写完立即用当前已连接的 client 列表 + 新 server 现场连接一次（`McpManager::add_server`），成功/失败都会走 §14.3 通知。`config` 是 `mcp::config::McpServerConfig`（`type` 字段做 tag：`stdio`/`streamable_http`/`sse`/`in_process`）。

### 14.3 新增 daemon 级异步通知机制：`daemon.subscribeEvents`

`SessionPool` 新增 `events_tx: tokio::sync::broadcast::Sender<serde_json::Value>`（容量 256，`send()` 失败——没有订阅者——不算错误，静默忽略）+ `subscribe_events()`/`emit_event()`。新增 RPC `daemon.subscribeEvents`：无参数，立即返回 `{"subscribed": true}` 确认，然后**在同一条连接上**、后台 spawn 一个转发循环，把之后每一条 `emit_event` 广播出来的通知都包成 `StreamFrame`（`method: "daemon.event"`）推给这个连接，直到连接断开——协议形态和 `session.run_turn` 推 `session.event` 帧完全一样，只是不属于任何具体 session/turn，且没有"结束"的概念（订阅是长期的）。**不补发历史事件**——订阅前发生的通知拿不到，需要在关心的事件可能发生之前就订阅（比如 daemon 刚启动、还没等 MCP 连接完成时）。

当前会触发通知的事件：`mcp_connected`/`mcp_connect_failed`（§14.1/14.2 的 MCP 连接结果）、`import_detected`（§14.4）。这是一个通用机制，不是 MCP 专用的——以后新的异步后台操作要上报结果，应该复用 `emit_event`，不要另开一套。

### 14.4 跨工具导入变成真正的 daemon RPC：`import.list` / `import.run`

原来 `/import` 只能在一个跑起来的 session 里、靠模型调用 `ImportTool` 才能触发——检测和执行本身其实是纯确定性的文件系统操作（`base::frozen::{detect_import_sources, execute_import, mark_imported}`），完全不需要 LLM 参与，只是被裹了一层"必须走 session.run_turn"的壳。这一轮直接在 `SessionPool` 上新增 `list_import_sources()`/`run_import()`，包成两个 RPC：

- `import.list`：调 `detect_import_sources(cwd)`，返回检测到的候选源（`source`: `claude_code`/`codex`/`cursor` + 一行描述）。
- `import.run {source}`：对某个已检测到的候选源执行导入（`execute_import` + `mark_imported`），返回执行摘要（`actions` 列表）。`source` 不在当前检测结果里、或者不是合法枚举值，都返回 `INVALID_PARAMS`，不会误伤空跑。

`daemon/src/main.rs` 原来那段"传 `None` 给 `ImportCallback`，等于没接"的代码整段删除——`ImportCallback` 这个抽象本来就是给"能同步问一个人"的宿主设计的，daemon 天生给不出这个同步答案，硬凑一个只会一直传 `None`。换成一段更贴合 daemon 实际能力的逻辑：`SessionPool` 构建完成后，`tokio::spawn` 一个后台任务，检查 `.imported.json` 标记（`import_already_decided`，已经是"是"就直接返回，不重复打扰）、检测候选源，检测到就发一条 `import_detected`（`daemon.event`，带候选源列表）——不阻塞等待决定，只是通知；调用方订阅到之后想处理就调 `import.list`/`import.run`，不想处理就忽略。手动 `/import` 斜杠命令（`ImportTool`）行为不变，仍然是独立路径，每次都重新检测、不看 `.imported.json` 标记。`base::interface::import_callback` 模块本身没删——它是给别的、真的能同步问人的宿主用的通用库 API，只是这个仓库自带的 `daemon` 不再是它的消费者。

### 14.5 新增 RPC：`session.close` / `config.getProvider`

- `session.close {session_id}`：包一层已经存在但从未暴露过 RPC 的 `SessionPool::shutdown_session()`——之前只有"杀掉全部 session"的 `daemon.shutdown`，没有单独关闭一个的入口。
- `config.getProvider {include_secrets?: bool}`：`config.setProvider` 的读回对称方法，返回当前生效的 `providers`/`default_provider`/`task_models`。`api_key` **默认脱敏**（`***<末 4 位>`，字符边界安全，非 ASCII 也不会 panic；≤4 字符的短密钥整体脱敏成 `***`），传 `include_secrets: true` 才会拿到明文——默认值是本轮明确的产品决策（避免一个只是想看看配了哪些 provider 的调用方，顺手把明文 key 打进自己的日志/UI）。和 `config.setProvider` 一样，`include_secrets: true` 本身没有比"能连上这个 socket"更高的门槛，见 `daemon/src/server.rs` 模块文档的信任边界说明。

### 14.6 RPC 方法总览（本轮结束时）

`daemon.status`、`daemon.doctor`、`daemon.subscribeEvents`（新）、`daemon.shutdown`、`session.list`、`session.close`（新）、`session.run_turn`、`config.setProvider`、`config.getProvider`（新）、`mcp.status`（新）、`mcp.addServer`（新）、`import.list`（新）、`import.run`（新）——从 6 个变成 13 个。完整的方法级参考（每个方法的参数/返回值/错误情况/示例）见新增的 `docs/DAEMON_RPC.md`。

### 14.7 验证

新增/改动测试：`daemon/src/session_pool.rs`（`redact_secret` 3 个单测，覆盖长密钥/短密钥/多字节字符边界安全）、`daemon/tests/daemon_e2e.rs`（12 个新增 e2e 测试：`mcp.status`/`mcp.addServer`（含真实连接失败场景）、`daemon.subscribeEvents`（用两条独立连接验证跨连接通知投递）、`config.getProvider`（默认脱敏 + `include_secrets` 还原）、`session.close`（正常 + 参数校验）、`import.list`/`import.run`（检测、执行、未知 source、当前未检测到的 source 四种场景））。`cargo build --workspace`、`cargo clippy -p daemon -p base -p runtime -p mcp --all-targets`、`cargo test -p daemon --lib`（36 个）+ `daemon_e2e`（20 个，除固定跳过的 2 个需要真实网络的用例）+ `-p base -p runtime -p session --lib`（212+49+22 个）全部通过。
