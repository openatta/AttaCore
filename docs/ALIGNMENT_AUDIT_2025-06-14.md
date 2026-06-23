# 对齐审计报告 — AttaCore vs Claude Code TS — 2025-06-14

## 比较范围
- **参考实现 (A)**: `3rds/claude-code-main/src/` — Anthropic Claude Code CLI (TypeScript, Bun 运行时)
- **目标实现 (B)**: `AttaCore/` — AttaCode Rust 实现 (Tokio 异步运行时)
- **涉及文件**: A 侧 ~150 个源文件, B 侧 ~200 个源文件 (20 个 crate)
- **比较维度**: 主处理流程、提示词、工具、记忆、压缩、MCP、HOOKS、权限、插件、会话、SKILLS、任务、团队、遥测

---

## 总评

| 维度 | 能力对齐 | 行为对齐 | 提示词对齐 | 流程对齐 | 综合评级 |
|------|---------|---------|-----------|---------|---------|
| 主处理流程 | ✅ | ✅ | — | ✅ | ✅ 高度一致 |
| 系统提示词 | ✅ | ✅ | ⚠️ | ✅ | ✅ 高度一致 |
| 上下文构建 | ⚠️ | ⚠️ | ⚠️ | ⚠️ | ⚠️ 差异可修 |
| 工具系统 | ✅ | ✅ | ✅ | ✅ | ✅ 高度一致 |
| 记忆系统 | ⚠️ | ❌ | ✅ | ⚠️ | ⚠️ 存在重要差异 |
| 上下文压缩 | ⚠️ | ⚠️ | ⚠️ | ⚠️ | ⚠️ 存在差异 |
| MCP | ✅ | ✅ | ✅ | ✅ | ✅ 高度一致 |
| HOOKS | ⚠️ | ⚠️ | ✅ | ⚠️ | ⚠️ 部分对齐 |
| 权限 | ⚠️ | ✅ | — | ✅ | ⚠️ 部分对齐 |
| 插件 | ❌ | ❌ | — | ❌ | ❌ 显著差距 |
| 会话管理 | ⚠️ | ⚠️ | — | ⚠️ | ⚠️ 架构差异 |
| SKILLS | ✅ | ⚠️ | ❌ | ✅ | ⚠️ 差异可修 |
| 任务系统 | ⚠️ | ❌ | — | ❌ | ❌ 显著差距 |
| 团队/多Agent | ⚠️ | ❌ | ✅ | ❌ | ❌ 显著差距 |
| 遥测 | ✅ | ✅ | — | ✅ | ✅ B 侧更优 |

**整体结论**: AttaCore Rust 实现在核心引擎（主循环、工具、MCP）上与参考实现高度一致；在记忆、压缩、权限、技能上部分对齐；在插件、任务、团队协调上存在显著差距。B 侧在遥测/可观测性上比 A 侧更完善。

---

## 一、主处理流程

### ✅ 已对齐

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| `query.ts:307-708` — turn 循环: build→call→stream→execute→continue | `turn.rs:288-556` — 相同六步生命周期 | 核心流程一致 |
| `query.ts:560-568` — StreamingToolExecutor 流式并发 | `streaming.rs:77-239` — FuturesUnordered 并发执行 | 并发模型一致 |
| `query.ts:662` — AbortController 取消 | `agent.rs:176-179` — CancellationToken | 取消语义一致 |
| `query.ts:834-952` — 连续决策 (tool_use, max_tokens, fallback) | `turn.rs:498-566` — has_tool_uses, budget, recovery | 连续逻辑一致 |
| `query.ts:301-304` — 记忆预取 | `turn.rs:216-285` — LLM 记忆选择 | 功能等价 |

### ⚠️ 差异

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `query.ts:219-239` — 异步生成器模式 | `agent.rs:134` — `tokio::select!` channel 驱动 | Rust 惯用写法替代 TS 生成器 | 等价 |
| `query.ts:558-566` — max_tokens 恢复通过 query 重试 | `turn.rs:559` — `handle_max_tokens_recovery()` 内联 | 实现路径不同，语义等价 | 等价 |

---

## 二、系统提示词

### ✅ 已对齐

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| `prompts.ts:444-577` — `getSystemPrompt()` 静态+动态段 | `coding.rs:29-83` — `build_system_prompt()` | 静态/动态分离一致 |
| `systemPromptSections.ts:20-25` — 按段 cache 缓存 | `coding.rs:121-141` — `cached_section()` / `memoized_section()` | 缓存策略一致 |
| `prompts.ts:114` — `SYSTEM_PROMPT_DYNAMIC_BOUNDARY` | `coding.rs:198-225` — per-block `CacheStrategy::Global/Ephemeral` | cache_control 放置一致 |

### ⚠️ 差异

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `prompts.ts` — 所有段内联 | `coding.rs:231-336` — 拆分为 parallelism, sub_agents, code_style 独立段 | B 更细粒度，内容等价 | 等价 |
| `prompts.ts` — 有 `ant_model_override`, `brief`, `proactive` 段 | B 侧缺失 | Anthropic 内部特性，B 不需要 | 合理缺失 |

---

## 三、上下文构建

### ✅ 已对齐

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| `context.ts:36-111` — git status + recent commits, 2000 字符截断 | `frozen/mod.rs:144-150` — 相同逻辑 | 一致 |
| `context.ts:170-172` — CLAUDE.md `<system-reminder>` 注入 | `turn.rs:166-188` — 相同格式，仅首次注入 | 一致 |

### ⚠️ 差异

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `context.ts` — 上下文注入到 system prompt 前缀 | `turn.rs:157-188` — 注入为合成 user message | **重要**: A 利用 prompt cache，B 不缓存上下文 | 偏差 |
| `context.ts` — Intent 用于路由决策 | `intent.rs:18-117` — 仅注入 style fragment | B 未用 intent 做路由 | 偏差 |

---

## 四、工具系统

### ✅ 已对齐 (核心工具)

全部 25 个核心工具的 name、schema、description、默认值、超时、安全限制均与参考一致：

- **BashTool**: 超时 120s/最大 600s, 4MB 输出限制, sleep>=2s 阻塞, sandbox-exec/bwrap
- **ReadTool**: offset/limit/pages, 图片/PDF/Jupyter 支持, cat -n 行号格式
- **EditTool**: old_string/new_string/replace_all, 多编辑 batch, 结构化 diff, 1GiB 限制
- **WriteTool**: 100MB 限制, 敏感文件保护
- **GrepTool**: content/files_with_matches/count 模式, head_limit=250, 1MB 输出
- **GlobTool**: gitignore 感知, pattern/path/respect_gitignore
- **WebFetchTool**: 5MB/50K chars/15s 超时, HTML→文本
- **WebSearchTool**: allowed_domains/blocked_domains 互斥验证
- **LSPTool**: 9 种操作完全一致
- **TaskCreate/Get/Update/List/Stop/Output**: 全部 6 个工具, TaskStatus 5 状态
- **TodoWriteTool**: content/status/activeForm
- **EnterPlanMode/ExitPlanMode**: plan 存储一致
- **SkillTool**: skill + args, 8000 字符截断
- **AskUserQuestionTool**: question/header/options
- **MonitorTool**: command/timeout_ms/persistent
- **CronCreate/Delete/List**: 5 字段 cron, durable 持久化
- **PushNotificationTool**: osascript/stderr

### ⚠️ 差异

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| GrepTool shell out 到 rg | `grep.rs:3-7` — 原生 Rust regex+ignore | B 无外部依赖但性能低 2-5x | 等价 |
| GlobTool max_results=100 (可配置) | `glob.rs` — 硬编码 1000 | B 不可配置 | 偏差 |
| WebFetchTool 无二次提取 | `web_fetch.rs:42-56` — SecondaryLlm 提取 | B 独有特性 | 增强 |

### ❌ B 独有工具

- ScheduleWakeupTool (`schedule_wakeup.rs`) — loop 模式定时唤醒
- PingTool — URL 活性检测
- StructuredOutputTool — 结构化数据返回
- VerifyPlanExecutionTool — plan vs diff 验证

### ❌ A 有 B 无 (23 个)

大部分是特性门控 (KAIROS, COORDINATOR_MODE) 或 SaaS 专属 (ConfigTool, TungstenTool, REPLTool)。值得关注的缺失:
- **ListMcpResourcesTool / ReadMcpResourceTool** — 虽在 mcp crate 中有实现但未注册为标准工具
- **BriefTool** — 简洁任务摘要
- **SendMessageTool** — Agent 间消息 (在 team/mailbox 中有独立实现)

---

## 五、记忆系统

### ✅ 已对齐

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| `memoryTypes.ts:14-19` — 4 种类型 + XML 提示词 | `interface/memory.rs:77-89, 508-603` — 完全一致 | 逐字匹配 |
| `memdir/memdir.ts:34-103` — MEMORY.md 200行/25KB 截断 | `interface/memory.rs:449-500` — 相同逻辑 | 一致 |

### ❌ 重要差异

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `extractMemories.ts` — `.md` 前端内容文件 | `memory.rs:83-227` — JSON 文件 (`topic-hash.json`) | **严重**: LLM 不可读/写的 JSON 格式 | ❌ |
| `sessionMemory.ts` — LLM 驱动的子 Agent 提取 | `session_memory.rs:1-289` — 仅文件管理器 | **严重**: 无自动提取, 无后台子Agent | ❌ |

---

## 六、上下文压缩

### ✅ 已对齐

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|---------|------|
| `microCompact.ts:41-50` — 8 种可压缩工具 + 占位符 | `compact.rs:125-167` — 相同白名单和占位符 | 一致 |
| `compact.ts:660-818` — 压缩后重新注入 (最近文件/计划/任务/技能) | `compact.rs:660-791` — 相同类别和限制 | 一致 |
| `microCompact.ts:52-128` — API 级 cache_edits | `cached.rs:100-149` — build_cache_edits | 一致 |

### ❌ 重要差异

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `autoCompact.ts:62-91` — 多级阈值 (auto/warn/error/block) + 断路器 | `reactive.rs:41-65` — 二元阈值 50000 tokens | **严重**: 无渐进降级 | ❌ |
| `compact.ts:387` — hook 集成 (pre/post compact) | `compact.rs:617-656` — 无 hook | 无用户交互路径 | ❌ |
| `postCompactCleanup.ts:31-77` — 缓存清除/状态重置 | `cleanup.rs:27-60` — 仅重新注入记忆 | 压缩后缓存污染风险 | ❌ |
| `compact.ts:243-291` — PTL 重试 snipping 恢复 | `compact.rs:90-122` — 仅丢弃轮次, 无 PTL 集成 | 恢复机制不完整 | ⚠️ |
| `timeBasedMCConfig.ts:18-43` — 60min/disabled 默认 | `time_based_mc.rs:15-41` — 15min/enabled 默认 | 默认值不同 | ⚠️ |
| `compact.ts:145-200` — 图片剥离 | `compact.rs:834-840` — 空桩 | 图片支持后需补齐 | ⚠️ |

---

## 七、MCP

### ✅ 已对齐 (7/11)

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| 连接生命周期 + 指数退避重试 (5次, 1s→30s) | `connect.rs:88-140, manager.rs:320-356` | 一致 |
| `mcp__server__tool` 命名规范 | `adapter.rs:43` | 一致 |
| resources/list + resources/read | `tools.rs:28-268` | 一致 |
| 输出缓存 (TTL 30s, 100条) | `output_cache.rs:1-247` | 一致 |
| 官方注册表 (8 个 server) | `registry.rs:1-258` | 一致 |
| 5 状态连接模型 (Connected/Failed/NeedsAuth/Pending/Disabled) | `manager.rs:33-39` | 一致 |
| prompts/list + prompts/get | `client.rs:110-124` | 一致 |

### ⚠️ 差异

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| 7 种传输: stdio/sse/sse-ide/http/ws/sdk/claudeai-proxy | `config.rs:10-40` — Stdio/StreamableHttp/Sse (3种) | 缺少 ws, sdk, sse-ide, proxy | ⚠️ |
| 完整 OAuth 流程 (browser popup, redirect, code exchange, revocation) | `oauth.rs:31-148` — trait 抽象, 无完整流程 | OAuth 委托给外部实现 | ⚠️ |
| 7 种配置 scope (user/project/enterprise/claudeai/managed) | `config.rs:8-40` — 无 scope 字段 | 无法区分配置来源 | ⚠️ |

---

## 八、HOOKS

### ✅ 已对齐

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| `if` 模式匹配 (Bash, Bash(content), mcp__ 前缀) | `runner/matcher.rs:1-73` — 相同语法 | 一致 |
| stdin-JSON/stdout-JSON 协议 | `payload.rs:1-57` — HookInput/HookResponse | 一致 |
| 文件变更监听 (debounce 300ms) | `watcher.rs:1-190` — notify crate | 一致 |
| Command/Prompt/Http/Agent 四种 hook 类型定义 | `config.rs:12-55` — 全部四种 | 一致 |

### ⚠️ 差异

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| 27 种 hook 事件 | `config.rs:60-113` — 30 种 (多了 TurnStart/TurnComplete/PostSampling) | B 多了 3 个事件 | ⚠️ |
| Agent hook 完整实现 | `runner/mod.rs:324-365` — Agent hook 被跳过 (仅占位) | Agent hook 未实现 | ⚠️ |
| PermissionRequest (A) | PermissionRequested (B) | 命名差异, 功能等价 | ⚠️ |
| `PermissionDenied` 事件处理 | B 无对应实现 | 缺少权限拒绝 hook | ❌ |

---

## 九、权限

### ✅ 已对齐

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| `TOOL(CONTENT)` 规则语法 + 双向解析 | `rule.rs:21-62` — parse/format | 一致 |
| 特异性 > 来源优先级 > 行为排名的匹配算法 | `ruleset.rs:68-106` — MatchScore | 一致 |
| settings.json 原子追加权限规则 | `settings_patch.rs:1-108` | 一致 |

### ⚠️ 差异

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| 6 种权限模式 (default/plan/acceptEdits/bypass/dontAsk/auto) | `gate.rs:178-253` — 8 种 (增加 Yolo, Bubble) | B 多了 2 个模式 | ⚠️ |
| LLM 驱动的 YOLO 分类器 | `yolo.rs:1-133` — 纯规则启发式 | B 无 LLM 调用 | ⚠️ |
| 10+ 种 DecisionReason | `gate.rs:151-156` — 仅 4 种 (Rule/Mode/Other/ToolBuiltin) | 决策溯源较粗 | ⚠️ |
| 拒绝追踪 (连续拒绝/总拒绝计数 + 断路器) | ❌ 无对应 | 缺少自动模式降级机制 | ❌ |
| 文件系统保护以 git/vscode/claude 配置目录为主 | `path_safety.rs:9-36` — 以凭证文件和系统目录为主 | 保护范围不同 | ⚠️ |

---

## 十、插件

### ⚠️ 部分对齐

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `plugin.json` (Zod 校验) | `plugin.toml` (serde) | 贡献类型等价 (skills/commands/mcp/hooks/agents) | ⚠️ |
| 依赖解析 (semver + marketplace 限定) | `resolver.rs` — 拓扑排序仅词法比较 | 无 semver 范围支持 | ⚠️ |
| 内置插件生态 | `bundled.rs` — 仅 2 个示例插件 | 规模差距 | ⚠️ |
| CLI 命令 (install/uninstall/enable/disable/update/list) | `cli.rs` — 缺 enable/disable, install 为桩 | 命令不完整 | ⚠️ |

### ❌ 缺失

| 参考 (A) | 缺失内容 | 影响 |
|-----------|---------|------|
| `pluginLoader.ts` — 多源发现 (marketplace/--plugin-dir/SDK/seed) | 无多源发现 | 仅支持单文件 plugin.toml |
| `marketplaceManager.ts` — 完整市场 (known_marketplaces, 防冒充, 离线缓存) | 无市场系统 | 无法远程安装插件 |
| `schemas.ts:19-101` — 官方名称保护/同形词防护 | 无安全验证 | 可被冒充 |
| `installedPluginsManager.ts` — 安装/启用分离, V1→V2 迁移 | 无启用/禁用管理 | 无法按项目开关插件 |

---

## 十一、会话管理

### ✅ 已对齐

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| JSONL 格式持久化 | `history/src/store.rs` — `JsonlHistoryStore` | 一致 |
| 转录投影 (用于压缩/恢复) | `history/src/transcript.rs` — `ResumeProjectionReport` | 一致 |

### ⚠️ 架构差异

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| CLI 单会话模型 | `session_pool.rs:49-548` — Daemon 多会话池 + LRU 驱逐 | B 是守护进程模型, 基础不同 | ⚠️ |
| UUID session ID | BASE58(UUID v4) session ID | B 遵循项目 ID 铁律 | ⚠️ |
| `bootstrap/state.ts` — 60+ 运行时状态字段 | `session.rs:136-148` — SessionSummary 基本字段 | B 更精简, 适合多会话 | ⚠️ |
| `sessionStorage.ts:139-178` — 复杂的恢复 (parentUuid 链修复) | `session.rs:103-118` — 基本 load | B 恢复逻辑较简单 | ❌ |

---

## 十二、SKILLS

### ✅ 已对齐

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| SKILL.md + YAML 前端内容格式 (15+ 字段) | `frozen/frontmatter.rs:85-173` — 相同字段 | 一致 |
| 发现路径 (~/.claude/skills/ ↔ ~/.atta/code/skills/) | `skill.rs:117-134` — 用户/项目/插件三源 | 一致 |
| 目录树遍历 + 去重 | `manager.rs:170-214` — discover_for_paths | 一致 |
| MCP→Skill 自动生成 (`mcp__server__tool`) | `mcp_builder.rs:31-100` | 一致 |
| 10 个内置技能 (simplify/verify/debug/batch/stuck/loop/remember/skillify/updateConfig/loremIpsum) | `bundled.rs:32-131` | 一致 |

### ❌ 重要差异

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| LLM `Skill` 工具调用 (权限检查 + 分析日志 + fork 执行) | Slash 命令展开 (`/<name> args`) | **严重**: B 无 LLM 驱动的 Skill 工具, 技能仅通过 slash 命令触发 | ❌ |
| `prompt.ts:1-241` — 基于上下文窗口比例的预算管理 | `manager.rs:241-251` — 简单 name:description 列表 | **严重**: B 无预算感知的技能提示, 无截断 | ❌ |

---

## 十三、任务系统

### ✅ 已对齐

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| Dream 任务 (后台思考, 自动停止) | `dream.rs:17-178` — 功能完整 | 一致 |
| 任务持久化 + 崩溃恢复 | `running.rs:111-137` — scan_and_mark_stale | B 更完善 |

### ❌ 重要差异

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `Task.ts:6-13` — 7 种 TaskType 联合类型 + 类型前缀 ID | `task.rs:16-27` — 仅 RunningStatus 枚举, 无 TaskType | **严重**: 无法区分任务类型 | ❌ |
| `tasks.ts:22-39` — 按类型注册 + 查找的 Task 接口 | ❌ 无 task 注册表/trait | 无类型分发 | ❌ |
| `LocalShellTask.tsx:24-42` — 停滞看门狗 (45s) + 交互式提示检测 | ❌ 无实现 | Shell 任务缺乏监控 | ❌ |
| `<task-notification>` XML 通知 | ❌ 无通知机制 | 子任务静默完成 | ❌ |

---

## 十四、团队/多Agent

### ✅ 已对齐

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| 协调器系统提示 (6-7 段) | `prompt.rs:9-153` — 忠实移植 | 一致 |
| 权限桥接 (leader→worker) | `coordinator.rs:28-205` — PermissionBridge | 一致 |
| Agent 间邮箱 (SendMessage/ReadMail/ListPeers) | `mailbox.rs:33-455` — JSONL 持久化 | B 更完善 |

### ❌ 重要差异

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `coordinatorMode.ts:36-41` — env var 门控 + 会话模式匹配 | `coordinator.rs:273-401` — 无模式检测 | 无法切换模式 | ❌ |
| `LocalAgentTask.tsx:270-303` — 子Agent 生成 | `coordinator.rs:313-315` — `// TODO: disabled (circular dep)` | **阻塞**: 子Agent 生成已禁用 | ❌ |
| `InProcessTeammateTask/types.ts:22-76` — 丰富的 teammate 生命周期状态 | ❌ 无对应 | 无 idle/plan/shutdown 管理 | ❌ |
| `teamHelpers.ts:64-89` — TeamFile 成员元数据持久化 | 仅 SCRATCHPAD.md | 无可恢复的团队状态 | ❌ |

---

## 十五、遥测

### B 侧领先

| 维度 | 参考 (A) | 目标 (B) | 判定 |
|------|---------|---------|------|
| 事件类型系统 | 临时 string-key 字典 | 36+ 种类型化 EventPayload 枚举 + UUID v7 | B 更优 |
| 导出模式 | AnalyticsSink 接口 (noop 直到 attach) | Remote/Disabled + spawn() 管道 | 等价 |
| HTTP 批量导出 | 基础 | 指数退避重试 + 磁盘回退 + 启动重试 | B 更优 |
| OTLP 导出 | 无 | gRPC/HTTP+Protobuf, 4 个指标仪器 | B 独有 |
| PII 脱敏 | Type-level marker + 名称前缀剥离 | 8 种正则模式的 RedactionPolicy | B 更全面 |
| Token/成本追踪 | ProgressTracker (仅显示) | UsageAccumulator + per-model + 成本估算 + /cost 报告 | B 更优 |
| VCR (录制/回放) | 外部 vcr.ts | SHA-256 匹配 + JSONL + dehydrate/hydrate + CI 保护 | B 更优 |

---

## 对齐建议

### P0 — 阻塞级 (影响核心功能正确性)

| # | 问题 | 参考位置 | 目标位置 | 建议 |
|---|------|---------|---------|------|
| 1 | **子Agent 生成已禁用** — 循环依赖 tools↔runtime | `LocalAgentTask.tsx` | `coordinator.rs:313` | 重构以解除循环依赖; 或将 AgentTool 移到独立 crate |
| 2 | **Skill 无 LLM 工具调用** — 技能仅通过 slash 触发 | `SkillTool/` | `skill_tool.rs` | 实现完整的 SkillTool (权限检查 + fork 执行 + 分析) |
| 3 | **记忆存储格式为 JSON** — LLM 不可读写 | `extractMemories.ts` | `memory.rs:83-227` | 改为 `.md` + YAML 前端内容格式 |
| 4 | **无自动压缩断路器** — 无渐进降级 | `autoCompact.ts:62-91` | `reactive.rs:41-65` | 实现多级阈值 (auto/warn/error/block) + 3 次失败断路器 |

### P1 — 重要级 (影响用户体验/安全性)

| # | 问题 | 参考位置 | 目标位置 | 建议 |
|---|------|---------|---------|------|
| 5 | **无会话记忆自动提取** — 无 LLM 子Agent 提取 | `sessionMemory.ts` | `session_memory.rs` | 实现后台 fork Agent 自动提取会话记忆 |
| 6 | **上下文不缓存** — 注入为 user message 而非 system prompt | `context.ts` | `turn.rs:157-188` | 将静态上下文移至 system prompt 前缀以利用 prompt cache |
| 7 | **技能提示无预算管理** — 无上下文窗口比例截断 | `prompt.ts:1-241` | `manager.rs:241-251` | 按上下文窗口百分比控制技能提示长度 |
| 8 | **无 Task 类型系统** — 无法区分任务类型 | `Task.ts:6-13` | `task.rs:16-27` | 实现 TaskType 枚举 + 类型前缀 ID + Task trait 注册表 |
| 9 | **插件系统几乎为空** — 无市场/多源发现/安全验证 | `pluginLoader.ts`, `marketplaceManager.ts` | `manifest.rs`, `marketplace.rs` | 补齐插件加载管线 (多源发现→版本缓存→依赖解析→启用管理) |
| 10 | **压缩后无缓存清除** — 状态泄露风险 | `postCompactCleanup.ts` | `cleanup.rs:27-60` | 实现系统提示缓存清除 + 子Agent 安全检查 |

### P2 — 改善级 (锦上添花)

| # | 问题 | 参考位置 | 目标位置 | 建议 |
|---|------|---------|---------|------|
| 11 | MCP 缺少 ws/sdk/sse-ide 传输 | `types.ts:23-26` | `config.rs:10-40` | 添加 WebSocket transport |
| 12 | MCP 配置无 scope | `types.ts:10-20` | `config.rs:8-40` | 添加 scope 字段 (user/project/enterprise) |
| 13 | OAuth 无完整流程 | `auth.ts` | `oauth.rs` | 实现 browser popup + redirect server |
| 14 | Agent hook 未实现 | `execAgentHook.ts` | `runner/mod.rs:324-365` | 实现 Agent 类型 hook 执行 |
| 15 | Shell 任务无停滞检测 | `LocalShellTask.tsx:24-42` | 新增 | 实现 45s 停滞看门狗 + 交互式提示检测 |
| 16 | Teammate 无生命周期管理 | `InProcessTeammateTask/types.ts` | 新增 | 实现 idle/plan/shutdown 状态机 |
| 17 | Team 元数据无持久化 | `teamHelpers.ts:64-89` | 新增 | 实现 TeamFile 结构 + per-agent 元数据 |
| 18 | Glob 默认限制不可配置 | context.globLimits | `glob.rs` | 从 EngineConfig 读取限制 |
| 19 | 拒绝追踪缺失 | `denialTracking.ts` | 新增 | 实现连续拒绝计数 + 自动降级 |
| 20 | Skill 变量展开不支持 `!command` | `argumentSubstitution.ts` | `skill.rs:356-405` | 添加 `!` 前缀 shell 执行语法 |

---

## 整体评估

### 强项 (B 侧对齐良好的领域)
- **核心引擎**: 工具系统、主处理循环、MCP 集成 — 与参考实现高度一致
- **提示词工程**: 系统提示段结构、缓存策略、内容覆盖 — 几乎完全对齐
- **遥测/可观测性**: 实际上**超越**了参考实现 (类型化事件、OTLP、VCR、PII 脱敏)
- **压缩基础**: 微压缩、cache_edits、后压缩恢复 — 基础功能齐全

### 弱项 (B 侧需要改进的领域)
- **插件生态**: 几乎为桩实现 — 无市场、无多源发现、无安全验证
- **团队协调**: 子Agent 生成被禁用, 无 teammate 生命周期管理
- **任务系统**: 无类型区分, 无停滞检测, 无通知机制
- **记忆持久化**: JSON 格式使 LLM 无法直接读写记忆文件

### 架构差异 (由于 Rust/Daemon 模型造成的合理差异)
- Daemon 多会话池 vs CLI 单会话
- BASE58 UUID vs 标准 UUID
- Tokio channel 驱动 vs 异步生成器
- 原生 Grep vs shell-out ripgrep
- TOML plugin manifest vs JSON

---

*审计完成时间: 2025-06-14 | 审计工具: atta-compare skill | 涉及文件: A 侧 ~150 个, B 侧 ~200 个*
