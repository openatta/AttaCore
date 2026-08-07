# 编写 Skills（简明指南）

一个 skill 是一个 `SKILL.md` 文件：YAML frontmatter + Markdown 正文。正文只在 skill 被真正调用时才进入上下文（"渐进展开"），不会一次性把所有 skill 全文塞给模型——模型平时只看到 `name` + `description` 的目录清单。

## 存放位置

| 位置 | 作用域 |
|---|---|
| `~/.atta/skills/<name>/SKILL.md` | 全局，所有场景共享 |
| `~/.atta/scenes/<scope>/skills/<name>/SKILL.md` | 场景覆盖（同名覆盖全局） |
| `<project>/.agents/skills/<name>/SKILL.md` | 项目级（同名覆盖以上两者） |

支持子目录格式（`<name>/SKILL.md`，推荐）和历史遗留的平铺格式（`<name>.md`）。两种格式都在**启动时**扫描进内存，运行中新增/编辑/删除也会被文件监听器自动捡起，不需要重启或重建 session。

## Frontmatter 字段

```yaml
---
name: code-review
description: Review code changes for correctness, architecture, security, and tests.
when_to_use: Use after a diff has been staged or a PR is being prepared.
arguments: [issue, branch]
argument_hint: "[issue-number] [branch]"
allowed_tools: Read, Grep, Glob
disallowed_tools: AskUserQuestion
model: claude-opus-4-8
effort: high
context: fork
agent: Explore
disable_model_invocation: false
user_invocable: true
paths: ["src/**/*.rs"]
---
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `name` | 否 | 缺省取目录名 |
| `description` | 推荐 | 缺省取正文**第一段**（不是第一行）。这是模型判断"要不要调用这个 skill"唯一的依据，写清楚触发场景 |
| `when_to_use` | 否 | 追加在 description 后面一起展示。两者合计有 **1536 字符**的固定上限（跟清单预算是两层独立的限制） |
| `argument_hint` | 否 | 自动补全时显示的参数提示，纯展示用 |
| `arguments` | 否 | 具名位置参数列表，配合正文里的 `$name` 占位符使用（见下） |
| `allowed_tools` | 否 | skill 激活期间，给列出的每个工具在真实权限引擎（`PermissionGate`/`RuleSet`）里临时注入一条 Allow 规则——跳过原本会弹出的确认（Ask），下一条用户消息后自动清空。只对真正会走到 `Permission::check` 判定的工具有效（也就是几乎所有内置工具）；不会绕过显式 Deny 规则或 bypass-immune 的敏感路径检查，那两层优先级更高 |
| `disallowed_tools` | 否 | 该 skill 激活期间从工具池移除这些工具（下一条用户消息后自动清空） |
| `model` | 否 | 该 skill 激活时临时切换模型 |
| `effort` | 否 | `low`/`medium`/`high`/`xhigh`/`max`，映射到 AttaCore 的 `ThinkingMode`（`Off`/`Auto`/`On`，是有损映射，不是新增了一个独立的"effort"概念） |
| `context` | 否 | 设为 `fork` 让 skill 正文作为任务 prompt，在子 agent 里执行 |
| `agent` | 否 | `context: fork` 时选用哪个 agent 类型（缺省 general-purpose） |
| `background` | 否 | `context: fork` + `background: true` 时不再阻塞等待子 agent 的结果，而是立即返回一个 task id，模型需要自己用 `TaskOutput`/`TaskStop` 轮询——跟 `Agent` 工具自己的 `background: true` 参数走的是同一套后台任务机制 |
| `disable_model_invocation` | 否 | `true` 时 description **完全不出现**在模型看到的清单里（不是"能看见但不让调"），只能用户手动 `/name` 调用 |
| `user_invocable` | 否 | `false` 时从 `/` 菜单隐藏，但模型仍可调用——跟上面那个字段是两个独立的开关 |
| `paths` | 否 | glob 列表，命中路径时自动激活（已实现） |

## 参数替换

正文里可以用：`{ARGS}` / `$ARGUMENTS` / `$@`（整串参数）、`$1`..`$9`（位置参数，空格分隔）、`${ATTA_SKILL_DIR}` / `${ATTA_SESSION_ID}`，以及配合 `arguments:` frontmatter 声明的具名参数 `$name`。都没用到时，参数会以 `\n\nUser arguments: {args}` 的形式追加在正文末尾。

## 动态内容注入

正文里可以用 `` !`command` ``（`!` 必须在行首或前面有空格/tab，紧贴着别的字符时不生效，比如 `KEY=!\`cmd\`` 不会被展开）或围栏形式：

````markdown
```!
git status --short
git diff --stat
```
````

skill 正文发给模型**之前**，这些命令会先在本地真实跑一遍，输出（去掉末尾一个换行,其余原样保留）内联替换进去——模型看到的是命令的真实结果,不是命令本身,也不会自己去执行这些命令。**只跑一遍**,命令输出里如果碰巧长得像 `` !`...` `` 也不会被二次展开。执行走的是跟模型自己调用 Bash 工具**完全一样**的路径（同一份 `ToolContext`,同一套沙盒/权限设置——不是另开一条不受管的 shell-out),把 settings.json 里的 `disable_skill_shell_execution` 设为 `true` 可以整体关掉这个功能,每个占位符改成显示一句固定的"disabled by policy"提示。

## 运行期行为

- **调用即全文注入**：Skill 工具被调用时，正文全文作为一条 `user` 消息注入，此后整个 session 都留在上下文里，不会每轮重新读取。
- **重复调用去重**：同一个 session 里，如果同一 skill 的渲染内容跟上次一模一样，第二次调用只会返回一句"already loaded"提示，不会再塞一份重复正文。
- **清单预算超限时怎么办**：skill 很多、超出清单字符预算时，**优先保留最近/最常调用过的 skill 的完整描述**，冷门的降级成只显示名字（不是把所有描述一起等比例缩短）。
- **Compaction（压缩）之后**：最近调用过的 skill，其正文会按"最近调用优先"重新贴回上下文（每个最多约 20000 字符，所有 skill 共享约 100000 字符预算），而不只是提一句名字。

## 权限判定现在是怎么接起来的（供理解 `allowed_tools` 语义用）

引擎的主派发路径（`runtime::turn::execute_tool_inner`，daemon/CLI 每次工具调用都走这里）现在真的会调用 `Permission::check`——之前这条路径完全不咨询任何权限逻辑，`Settings.permission_mode`/`permission_rules` 对真实工具调用没有任何效果。`Permit`/`Deny` 立即生效；`Ask`（比如 `Default` 模式下没有规则命中的普通工具调用）会发出一次 `AgentEvent::PermissionPrompt` 并短暂等待宿主的 `PermissionResponse`——目前还没有任何宿主实现会真的回应这个事件，所以等待会在很短的兜底超时后自动按 Permit 处理，跟以前"从不真正询问、直接放行"的实际行为一致，不会让工具调用卡住。`allowed_tools` 正是靠往这套引擎注入临时 Allow 规则来生效的。

## 已知缺口（还没做，别当成已生效的功能）

- **`disable_skill_shell_execution`/`dangerously_disable_sandbox` 现在会真的从 `Settings` 派生生效**（`EngineConfig::from_settings`），不再是本节曾经记录的"始终读到 `defaults_for("unknown")` 的兜底值"——这条缺口已经修复。
- **权限确认目前还没有真正的宿主侧交互实现**：`AgentEvent::PermissionPrompt`/`InputMessage::PermissionResponse` 这套协议骨架和挂起/唤醒逻辑都已经接好，但目前没有任何 daemon/CLI 实现会真的回应它——`Ask` 结果统一走短暂超时后兜底 Permit，行为上等价于"暂时还是不会真的弹确认框"，只是现在 Allow/Deny 规则本身已经真实生效。
- **daemon 默认仍然是 `AllowAllPermission`**（"IDE 插件自己管沙盒"这个既有设计决定，见 `daemon/src/main.rs`），没有改成新的 `RuleSetPermission`——想让 `allowed_tools`/`permission_rules` 在 daemon 服务的 session 里真正生效，需要宿主主动切换成 `permissions::rule_set_permission::RuleSetPermission`,这是一个产品/安全策略决定,这次没有代为决定。
- **`metadata`（自由字段）/ `license` / `compatibility`**：Claude Code 打包/市场专用字段，AttaCore 没有对应的 skill 市场，不做。
