# 配置与状态目录规范

> 本文档是 AttaCore **配置/状态目录布局**的稳定参考——只给定论和示例，不含决策取舍推理。决策过程与理由见 `docs/design/2026-08-03-agents-config-migration.md`。**目录布局（Phase 1-2）与跨工具导入功能（Phase 3+4：检测 Claude Code/Codex/Cursor 并单选导入）均已实施**——见该文档 §8。手动触发是 `/import`（`ImportTool`）；自动触发（`ImportCallback`）在 daemon 里 v1 暂未接真实回调，见 §8.8。**用户级命名空间不再叫"scope"，而是直接复用 `AgentScene`（见 §9.1）**：daemon 用 `--scene` 指定，值必须是代码里真实注册的场景（`coding`/`chat`/`demo`），不支持的值会让 daemon 启动失败，不存在自由填写的 `--scope` 字符串。

## 核心原则

1. **只有"其他 agent 工具真的认得"的东西才进 `.agents/`。** 目前只有 `AGENTS.md` 本身和 `.agents/skills/` 满足这条（Codex 官方约定）。其余一切——包括 AttaCore 自己发明的 `workflows/agents/rules/hooks`——都是私有扩展，进 `.atta/`，不管它是不是"人也想看"。
2. **项目级共享、用户级私有。** 项目目录（`repo/`）可能被团队、被 Codex/Cursor 等别的工具打开，`.agents/` 是这个场景下的公共接口；用户主目录（`$HOME`）不存在"被别的工具扫描"的场景，不需要为了对齐外部标准而多开目录。
3. **项目级扁平，用户级按 scene 隔离。** 一个项目通常只被一种 Atta 系产品使用，`.atta/` 项目级不再嵌套场景名；但一台机器上可能同时装有多个基于 AttaCore 构建的不同产品（不同 `AgentScene`），用户级 `.atta/<scene>/` 用 scene 隔离，避免互相覆盖。**引擎层（`base::paths::ConfigPaths` 等函数）不内置任何默认值**——函数签名要求显式传一个 `scope: &str` 参数（这个底层参数名字仍叫 `scope`，纯粹是"给我一个命名空间字符串"的通用抽象，不关心调用方从哪来的）。**本仓库自带的 `daemon` 把这个字符串定为"经过校验的 `AgentScene::id()`"**：`--scene` CLI flag（默认 `coding`）只接受代码里真实注册的场景名，传别的值 daemon 直接启动失败——不存在"自由字符串 + 事后校验格式"这种设计，非法值根本到不了路径拼接那步。

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
    ├── settings.json           # 项目级 settings 覆盖（分层：内置默认 < 用户级 < 项目级 < CLI）
    ├── memory/                 # 跨会话 memory 文件
    ├── mcp/                    # MCP server 连接状态/缓存
    ├── sessions/                # 会话记录
    ├── vcr/                     # 录制/回放测试用的 VCR 磁带
    ├── workflows/                # 命名工作流（AttaCore 扩展，无外部标准）
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
└── .atta/
    └── <scene>/                 # 本仓库里就是 AgentScene::id()：coding/chat/demo 之一，daemon 用 --scene 选，非法值启动失败
        ├── settings.json
        ├── memory/
        ├── mcp/
        ├── sessions/
        ├── vcr/
        ├── plugins/               # 已安装插件的 manifest/缓存
        ├── skills/                 # 用户级技能——不迁移到 .agents/，用户级根本不存在 .agents/
        ├── workflows/
        ├── agents/
        └── hooks/
```

举例：本仓库的 `daemon` 用 `--scene coding` 启动，落地路径就是 `~/.atta/coding/...`；用 `--scene chat`，就落在 `~/.atta/chat/...`，两者互不干扰。如果你基于 AttaCore 构建别的产品，不一定非要叫"scene"或复用 `AgentScene`——引擎层的 `scope: &str` 参数本身还是通用的，你可以传任何你自己中意的命名空间字符串；本仓库只是选择了"复用已有的 AgentScene 概念、并对其做校验"这个具体做法，而不是接受自由字符串。

---

## `.atta/` 子目录一览

| 目录/文件 | 内容 | 是否随导入功能填充 |
|---|---|---|
| `settings.json` | 引擎运行参数（model/permission/mcp_servers 等） | 否 |
| `memory/` | 跨会话 memory 文件（`MEMORY.md` 索引 + 主题文件） | 否 |
| `mcp/` | MCP server 连接态/缓存 | 否 |
| `sessions/` | 会话历史 | 否 |
| `vcr/` | 测试用录制/回放磁带 | 否 |
| `plugins/`（仅用户级） | 已安装插件 | 否 |
| `skills/` | 技能定义（项目级已废弃，见下方"skills 去哪了"） | 用户级导入不涉及；项目级技能走 `.agents/skills/` |
| `workflows/` | 命名工作流 | 否 |
| `agents/` | subagent 定义 | 否（未来考虑接入 `crates/plugin::agent_registry`） |
| `rules/` | 长文档规则，`AGENTS.md` 引用 | Cursor `.mdc` 导入的目标位置 |
| `hooks/` | hook 脚本本体 | 否 |
| `.imported.json`（仅项目级） | 导入决策记录 | 是——导入流程本身写入 |

### skills 去哪了？

这是唯一一处项目级和用户级**答案不同**的地方，容易记混，专门列出来：

| 层级 | Skills 权威位置 | 原因 |
|---|---|---|
| 项目级 | `.agents/skills/` | 需要能被 Codex 等外部工具直接扫到 |
| 用户级 | `.atta/<scene>/skills/` | 用户主目录不存在 `.agents/`，没有对外扫描的需求 |

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
| 用户级私有状态 | `~/.atta/<scene>/` | `~/.codex/` | `~/.claude/` | — |

AttaCore 对 `CLAUDE.md`/`.claude/skills/`/`.cursorrules`/`.cursor/rules/*.mdc` 提供**单选、用户确认后执行**的导入功能（同时检测到多个来源时，只能选一个，不支持合并导入），把内容迁移进 `AGENTS.md` + `.agents/skills/` + `.atta/rules/`；已经是 `AGENTS.md` + `.agents/skills/` 形态的项目（如 Codex 项目）直接原样兼容，只在缺 `.agents/skills/` 时补目录。用户手动触发用 `/import`（内置 skill + `Import` Tool）；宿主也可以注册 `ImportCallback` 实现进程启动时自动检测+询问（daemon 本身 v1 未接，见迁移文档 §8.8）。完整的检测规则、字段映射表、落地阶段见 `docs/design/2026-08-03-agents-config-migration.md`。

---

## 面向 AttaCore 集成方的说明

AttaCore 是一个通用 agent 库，**引擎层本身不预设、不内置任何具体命名空间**——`base::paths::ConfigPaths::from_env(cwd, scope)` 等函数的 `scope: &str` 参数是必填的，不存在"AttaCore 自带的默认产品身份"这回事。任何基于 AttaCore 构建产品的调用方，在自己的公开接口层（CLI flag、配置文件、环境变量等）决定这个字符串怎么来、要不要校验、要不要给默认值——**本仓库自带的 `daemon` 选择了"复用已有的 `AgentScene` 概念，并对输入做封闭集合校验"**这个具体做法：`--scene` 只接受代码里注册过的场景（`coding`/`chat`/`demo`），传别的值 daemon 直接启动失败，不是"接受任意字符串再兜底"。这不代表所有基于 AttaCore 的产品都必须复用 `AgentScene` 作为命名空间来源——你可以按自己的产品形态设计校验规则，引擎不替你做这个选择。项目级目录（`.atta/`、`.agents/`）不需要设置这个命名空间，天然按仓库隔离。
