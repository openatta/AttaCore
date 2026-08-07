# 编写 Agent 类型（简明指南）

一个 agent 类型是一个 `.md` 文件：YAML frontmatter + Markdown 正文。正文成为该类型子 agent 的 system prompt。主 agent 只看到每个类型的 `name` + `description`，委派时才会真正拉起对应类型、加载它的完整配置。

## 存放位置

`.atta/agents/*.md`，三层：全局 `global_data_dir`、场景 `user_data_dir`、项目 `project_root()/.atta/agents`——同名后者覆盖前者。运行中新增/编辑会被文件监听器自动捡起（不需要重建 session）；新增一个此前不存在的顶层目录需要重启才能被监听到。

## Frontmatter 字段

```yaml
---
name: code-reviewer
description: Reviews code for quality and best practices. Use after writing or modifying code.
allowed_tools: Read, Grep, Glob, Bash
disallowed_tools: Write, Edit
model: claude-opus-4-8
permission_mode: plan
effort: high
max_turns: 20
skills: api-conventions, error-handling
mcp_servers: github
---

You are a code reviewer. When invoked, analyze the code and provide
specific, actionable feedback on quality, security, and best practices.
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `name` | 否 | 缺省取文件名 |
| `description` | 推荐 | 缺省取正文**第一段**。这是主 agent 判断"要不要委派给这个类型"的唯一依据——写得含糊，委派质量会明显下降,建议明确写清楚"什么任务用它" |
| `allowed_tools` | 否 | 允许使用的内置工具白名单，留空 = 全部工具（除 `Agent` 自身，禁止递归委派） |
| `disallowed_tools` | 否 | 黑名单，**先于** `allowed_tools` 生效——同时出现在两边的工具一定被排除。支持 `mcp__server` / `mcp__server__*` 这种按 server 整体匹配的写法 |
| `model` | 否 | 该类型子 agent 使用的模型，覆盖父 session 的模型 |
| `permission_mode` | 否 | `default` / `acceptEdits` / `bypassPermissions` / `plan` / `dontAsk`。覆盖父 session 的权限模式，跟父 session 用的是什么模式无关 |
| `effort` | 否 | `low`/`medium`/`high`/`xhigh`/`max`，映射到 `ThinkingMode`（有损映射，见 Skills 指南同一节的说明） |
| `max_turns` | 否 | 该子 agent 自己的 API 调用上限，覆盖父 session 的 `max_api_calls_per_turn`——**没设置的话默认继承父 session 的值**，配合后台子 agent（见下）建议主动给一个保守数字 |
| `skills` | 否 | 子 agent **启动时**就把这些 skill 的**完整正文**注入初始上下文,不需要它自己再调用 Skill 工具去发现。列出一个 `disable_model_invocation: true` 的 skill 会被跳过（打印警告，不会让整个 spawn 失败） |
| `mcp_servers` | 否 | 子 agent **可以使用哪些 MCP server 的工具**——默认是**零个**，MCP 访问是逐个 agent 类型显式授权的，不会因为 `allowed_tools` 留空（"全部内置工具"）就顺带继承 MCP。这里只能引用父 session 已经连上的 server 名字,不支持内联写一份新的 server 配置 |

## 子 agent 启动时实际拿到什么

- 自己的 system prompt（正文）+ 委派任务消息
- 项目规则/AGENTS.md 相关上下文、git 状态快照——这是子 agent **自己**独立重新计算的一份，不是父 session 快照的复制
- `skills:` 列出的 skill 全文（如果有）
- `mcp_servers:` 授权的 MCP 工具（如果有,复用父 session 已连接的实例,不会重新连接）

**拿不到**：父对话历史、父已调用过的 skill、父已读过的文件。每次委派都是全新上下文,这是设计使然,不是缺口。

## 前台 / 后台

已经实现,不需要额外配置字段：调用 `Agent` 工具时可以传 `background: true/false`（工具调用参数,不是这份 frontmatter 里的字段——这是每次调用时由调用方决定,比"写死在类型定义里"更灵活）。后台执行的结果通过 `TaskOutput`/`TaskStop` 轮询,同一套机制也支撑着后台 `Bash` 等其他工具。

## Worktree 隔离

同样已经实现,同样是工具调用参数（`worktree: <slug>`）而不是这份 frontmatter 字段——每次调用时按需指定要不要隔离,不是类型级别写死的默认值。

## 已知缺口 / 明确不做

- **跨 session 持久记忆**（Claude Code 的 `memory:` 字段）：AttaCore 已经有一整套 memory 基础设施可以复用,但目前没有"每个 agent 类型自己的记忆目录"这层封装,没有做,优先级较低。
- **`color`**（纯 UI 展示色）：AttaCore 目前是 CLI/daemon,没有消费这个字段的富文本任务面板,不做。
- **`initialPrompt` / "整个主 session 就是这个 agent 类型"模式**（对应 Claude Code 的 `--agent` 启动参数）：需要先确认 AttaCore 要不要做这个产品功能,不是简单的字段缺口,本轮不做。
- **`hooks:`（agent 类型作用域的 hooks）**：frontmatter 层面暂未支持,AttaCore 整体的 hooks 系统是存在的,但没有"某个 agent 类型专属 hooks"这一层覆盖。
