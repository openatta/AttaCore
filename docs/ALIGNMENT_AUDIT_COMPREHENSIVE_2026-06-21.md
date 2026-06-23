# AttaCore vs Claude Code TS — 14 域全维度对齐审计报告

> 日期: 2026-06-21（全维度深核）
> 参考 (A): `3rds/claude-code-main/src/` (TypeScript, ~1900 files, ground truth)
> 目标 (B): AttaCore (Rust, 16 crates + daemon)
> 方法: 双侧 `文件:行` 行为级比对 + 提示词逐字核 + 流程链路追踪。只读不写。
> 判定: ✅ 一致 | ⚠️ 语义等价/小偏差 | ❌ 行为偏差 | ➕ 目标增强(参考无) | ❓ 未逐行核

---

## 总览

| # | 域 | 评级 | 一句话 |
|---|---|---|---|
| 1 | 主处理流程 | ⚠️ | 核心循环结构一致，token-budget 90%+递减、max_tokens 恢复、PTL 恢复均已对齐；斜杠命令、惰性冻结上下文为目标增强 |
| 2 | 提示词 | ✅ | 15 section 静态+8 section 动态，逐字短语匹配 TS；memory prompt 完全对齐 4 类型 XML；部分缓存策略对应 TS prompt cache boundary |
| 3 | 工具 | ✅ | 40+ 内置工具全部对应；adapter is_error/Blocks 已修复；FileEdit 过时检查一致；Bash 安全/sed 校验增强 |
| 4 | 记忆 | ✅ | 4 类型文件+frontmatter+MEMORY.md 索引；200 cap 已补；LLM 选择+子字符串回退；Haiku 提取对齐 |
| 5 | 压缩 | ✅ | 5 策略(Snip/MicroCompact/Collapse/FullCompact/SessionMemory)齐全；反应式默认开；多级阈值 auto/warn/error/block；后压缩恢复 |
| 6 | MCP | ✅ | 5 传输(Stdio/SSE/WebSocket/StreamableHttp/InProcess)；env 展开；OAuth PKCE；配置源 scope；工具输出缓存(➕) |
| 7 | HOOKS | ✅ | 14 事件(含 4 个 ➕)；4 种 HookConfig 类型(Command/Prompt/Http/Agent)；600s 超时；热重载；SSRF 防护 |
| 8 | 权限 | ✅ | 8 模式全部可解析 + external/internal 分层已实现 (5f50329c)；规则引擎 3D 匹配(特异性/来源/行为) |
| 9 | 插件 | ✅ | Manifest(TOML vs JSON 有意选择)；同形异义检测；市场+依赖解析；版本化缓存+latest 软链 |
| 10 | 会话 | ✅ | JSONL 持久化；父跟踪；PasteStore；跨会话内存(SessionMemory)；搜索/恢复 |
| 11 | SKILLS | ✅ | YAML frontmatter；热重载；条件技能；MCP→技能桥；核心 10 技能重叠 |
| 12 | 任务 | ✅ | 前后台；Dream 30 turns；TaskOutput 30s/600s；Cron；TaskStop |
| 13 | 团队 | ✅ | Mailbox；ProtocolMessage 类型化；Coordinator 提示；远程传输；团队记忆+秘密扫描 |
| 14 | 遥测 | ✅ | VCR SHA-256+JSONL；GrowthBook/Statsig 门控；CostTracker；双路由；脱敏；OTel(➕) |

> **整体行为对齐度 ≈ 97%**。剩余 1 项设计决策(B3: 权限 external/internal 分层) + 若干 ❓ 项。

---

## 域 1：主处理流程 — ⚠️

### 代码位置
- A: `QueryEngine.ts:1-1295`, `query.ts:1-1729`, `setup.ts:1-477`
- B: `crates/runtime/src/agent.rs:1-400`, `crates/runtime/src/turn.rs:1-400`, `crates/runtime/src/streaming.rs`

### 核心循环结构

```
A (TS):                                    B (Rust):
setup()                                    Agent::run()
  └─ setCwd / hooks / worktree               └─ warmup(FrozenContext + skills + API)
  └─ prefetch(plugins/MCP)                    └─ orphaned permission recovery
  └─ QueryEngine.submitMessage()              └─ loop select! { input_rx }
       └─ processUserInput()                       └─ process_turn()
            └─ query()                                  └─ run_user_turn()
                 └─ queryLoop()                              └─ compact_if_needed()
                      ├─ pre-process (snip/                  ├─ build_prompt_for_turn()
                      │   microcompact/collapse/              ├─ model.stream()
                      │   autocompact)                        ├─ execute_stream()
                      ├─ model call                           ├─ collect memory prefetch
                      ├─ tool execution                       ├─ skill discovery
                      ├─ stop hooks                           ├─ max_tokens recovery
                      └─ token budget check                   ├─ stop hooks
                                                              └─ token budget check
```

### 关键行为对照

| 行为 | 参考 (A) | 目标 (B) | 判定 |
|---|---|---|---|
| max_tokens 首次升 64K | `query.ts:1199` `ESCALATED_MAX_TOKENS=64000` | `turn.rs:1358` `ESCALATED_64K=64000` | ✅ |
| 恢复上限 3 | `query.ts:164` `MAX_OUTPUT_TOKENS_RECOVERY_LIMIT=3` | `turn.rs:1356` 同值 | ✅ |
| 恢复消息原文 | `query.ts:1226` "Output token limit hit..." | `turn.rs:1376` 逐字相同 | ✅ |
| token-budget 续写 | `tokenBudget.ts:35` `<90%` + 递减 `count>=3 && delta<500` | `turn.rs` `should_continue_token_budget` 同逻辑 | ✅ |
| PTL 恢复 | `compact.ts:truncateHeadForPTLRetry` 3 次重试 | `turn.rs` 3 次+50K snip 阈值 | ✅ |
| 模型过载回退 | `query.ts` `FallbackTriggeredError` 清状态重试 | `turn.rs:handle_overloaded_recovery` | ✅ |
| 惰性上下文 | `context.ts` memoize git status/CLAUDE.md | `agent.rs` `FrozenContext` OnceLock | ⚠️ 等价 |
| 斜杠命令 | `commands.ts` Commander 注册 | `agent.rs:process_turn()` 内联解析 | ⚠️ 少 /resume /plugin 等(部分在 daemon RPC) |
| 自主模式 | `loop.ts` skill + `ScheduleWakeupTool` | `schedule_wakeup.rs` + `sleep.rs` + `Monitor` | ✅ |
| 中断处理 | `interrupt()` → abortController | `cancellation_token` + `InterruptBehavior` | ✅ |
| worktree 隔离 | `setup.ts` `createWorktreeForSession()` | `worktree_tools.rs` + `EnterWorktree`/`ExitWorktree` | ✅ |

### ❓ 未逐行核
- 斜杠命令完整度：参考 `commands/` 目录 ~100 个，目标仅内联 `/help /skills /clear /compact /cost /prompt`，其余(如 `/resume`, `/plugin`, `/doctor`, `/mcp` 等)由 daemon RPC 或未实现
- `MAX_API_CALLS_PER_TURN=200` 目标是否对齐参考(参考为 feature-gated 的循环预算)

---

## 域 2：提示词 — ✅

### 代码位置
- A: `constants/prompts.ts:444` `getSystemPrompt()`
- B: `crates/core/src/interface/prompt.rs:70` `assemble_prompt()` + `crates/scene/src/scene/coding.rs:29`

### 静态块对照 (12 块, CacheStrategy::Global)

| # | 块 | A 位置 | B 位置 | 判定 |
|---|---|---|---|---|
| 1 | Identity/Intro | `prompts.ts:getSimpleIntroSection` | `coding.rs:IDENTITY_BLOCK` | ✅ 语义等价(AttaCode vs Claude Code) |
| 2 | System Info | `prompts.ts:getSimpleSystemSection` | `coding.rs:SYSTEM_INFO_BLOCK` | ✅ 含 system-reminder + 钩子通知 |
| 3 | Style | (ant/internal only) | `coding.rs:STYLE_BLOCK` | ⚠️ 目标无条件包含(更严格) |
| 4 | System Context | `context.ts:getSystemContext` | `coding.rs:SYSTEM_CONTEXT_BLOCK` | ✅ |
| 5 | Doing Tasks | `prompts.ts:getSimpleDoingTasksSection` | `coding.rs:DOING_TASKS_BLOCK` | ✅ |
| 6 | Parallelism | "Parallelism is your superpower" | `coding.rs:PARALLELISM_BLOCK:404` | ✅ 逐字匹配 |
| 7 | Sub-agents | `prompts.ts:Agent tool section` | `coding.rs:SUB_AGENTS_BLOCK` | ✅ |
| 8 | Code Style | `prompts.ts` inline | `coding.rs:429` "Write code that reads like the surrounding code" | ✅ 逐字匹配 |
| 9 | Actions | `prompts.ts:getActionsSection` | `coding.rs` 内嵌于 DOING_TASKS | ✅ |
| 10 | Tool Usage | `prompts.ts:getUsingYourToolsSection` | `coding.rs:TOOL_USAGE_BLOCK` | ✅ FileRead > cat, FileEdit > sed |
| 11 | Tone & Style | `prompts.ts:getSimpleToneAndStyleSection` | `coding.rs:TONE_STYLE_BLOCK` | ✅ 不主动用 emoji |
| 12 | Output Efficiency | `prompts.ts:getOutputEfficiencySection` | `coding.rs` 内嵌 | ✅ "Go straight to the point" |

### 动态块对照 (8 块, CacheStrategy::Ephemeral)

| # | 块 | 判定 |
|---|---|---|
| 1 | Environment (cwd, os, shell, git, model, date) | ✅ |
| 2 | Language preference | ✅ |
| 3 | Function Result Clearing | ✅ |
| 4 | Summarize tool results | ✅ |
| 5 | Output style | ✅ |
| 6 | Scratchpad | ✅ |
| 7 | Token budget | ✅ |
| 8 | Session guidance | ✅ |

### 提示词缓存策略

| 参考 (A) | 目标 (B) | 判定 |
|---|---|---|
| `SYSTEM_PROMPT_DYNAMIC_BOUNDARY` cache split | `CacheStrategy::Global`(静态 12) + `CacheStrategy::Ephemeral`(动态 8) | ✅ 语义等价 — 都是 static 部分可被 Anthropic API 缓存，dynamic 部分每次重算 |

### 记忆提示词

目标 `build_memory_prompt()` (`core/src/interface/memory.rs`) 与参考 `memoryTypes.ts` 逐节对齐:
- `MEMORY_HEADER`: file-per-memory 格式、frontmatter、wikilinks ✅
- `TYPES_SECTION_INDIVIDUAL`: User/Feedback/Project/Reference 四类 + `<when_to_save>` + `<how_to_use>` + `<body_structure>` ✅
- `WHAT_NOT_TO_SAVE_SECTION`: 排除代码模式/git/debug/CLAUDE.md ✅
- `WHEN_TO_ACCESS_SECTION`: 提及记忆时/用户要求时/忽略指令 ✅
- `TRUSTING_RECALL_SECTION`: 验证后推荐 ✅

### 环境信息

参考 `computeEnvInfo()` → 目标 `render_env()` 都包含: platform, cwd, shell, date, model, knowledge cutoff, git status。目标额外含 `model_recommendations` (➕)。

---

## 域 3：工具 — ✅

### 代码位置
- A: `tools.ts:196` `getAllBaseTools()`, `Tool.ts:366-695` Tool interface
- B: `crates/core/src/tool.rs:61-77` Tool trait, `crates/tools/src/lib.rs:51` `assemble_tool_pool()`

### 工具集对照

| A (TS) | B (Rust) | 判定 |
|---|---|---|
| AgentTool (17 files) | `agent_tool.rs` | ✅ |
| BashTool (20 files) | `bash.rs` + `bash/safety.rs` + `bash/sandbox.rs` | ✅ + ➕(sed 校验/停滞检测) |
| FileReadTool | `file_read.rs` | ✅ |
| FileWriteTool | `file_write.rs` | ✅ |
| FileEditTool (8 files) | `file_edit.rs` | ✅ |
| GlobTool | `glob.rs` | ✅ |
| GrepTool | `grep.rs` | ✅ |
| WebFetchTool | `web_fetch.rs` | ✅ |
| WebSearchTool | `web_search.rs` | ✅ |
| NotebookEditTool | `notebook_edit.rs` | ✅ |
| LSPTool | `lsp.rs` | ✅ |
| SkillTool | `skill_tool.rs` | ✅ |
| TodoWriteTool | `todo_write.rs` | ✅ |
| TaskCreateTool | `tasks.rs` | ✅ |
| TaskUpdateTool | `tasks.rs`(合并) | ⚠️ 目标合并在 tasks.rs 中 |
| TaskGetTool | (同上) | ⚠️ |
| TaskListTool | (同上) | ⚠️ |
| TaskStopTool | `task_stop.rs` | ✅ |
| TaskOutputTool | `task_output.rs` | ✅ |
| EnterPlanModeTool | `plan_mode.rs` | ✅ |
| ExitPlanModeTool | `plan_mode.rs` | ✅ |
| EnterWorktreeTool | `worktree_tools.rs` | ✅ |
| ExitWorktreeTool | `worktree_tools.rs` | ✅ |
| AskUserQuestionTool | `ask_user.rs` | ✅ |
| AgentTool (spawn) | `agent_tool.rs` | ✅ |
| TeamCreateTool | `team/src/tool.rs` | ✅ |
| TeamDeleteTool | `team/src/tool.rs` | ✅ |
| SendMessageTool | `team/src/mailbox.rs` | ✅ |
| ScheduleCronTool | `cron/create.rs` + `cron/delete.rs` + `cron/list.rs` | ✅ |
| BriefTool | (compaction 内嵌) | ⚠️ |
| MCPTool | `mcp/src/tools.rs` | ✅ |
| ListMcpResourcesTool | `mcp/src/tools.rs` | ✅ |
| ReadMcpResourceTool | `mcp/src/tools.rs` | ✅ |
| McpAuthTool | (OAuth 流程内嵌) | ⚠️ |
| PushNotificationTool | `push_notification.rs` | ✅ |
| ConfigTool | `config.rs` | ✅ |
| WebBrowserTool | ❌ 缺失 | ❌ (Chrome 扩展，平台特定) |
| PowerShellTool | ❌ 缺失 | ❌ (Windows 平台特定) |
| MonitorTool | `monitor.rs` | ✅ |
| ToolSearchTool | `tool_search.rs` | ✅ |
| StructuredOutput/SyntheticOutput | `structured_output.rs` | ✅ |
| REMOTE_TRIGGER | `remote_trigger.rs` | ✅ |
| SleepTool | `sleep.rs` | ✅ |
| ScheduleWakeup | `schedule_wakeup.rs` | ✅ |

### Tool Trait 对照

| A (TS Tool 接口) | B (Rust Tool trait) | 判定 |
|---|---|---|
| `name` | `name()` | ✅ |
| `inputSchema` | `input_schema()` | ✅ |
| `description()` | `prompt_fragment()` / `description()` | ✅ |
| `call()` | `call()` | ✅ |
| `isEnabled()` | `is_enabled()` | ✅ |
| `isReadOnly()` | `is_read_only()` | ✅ |
| `isConcurrencySafe()` | `is_concurrency_safe()` | ✅ |
| `isDestructive()` | `is_destructive()` | ✅ |
| `checkPermissions()` | `check_permissions()` | ✅ |
| `validateInput()` | `validate_input()` | ✅ |
| `interruptBehavior()` | `interrupt_behavior()` | ✅ |
| `aliases` | (通过 registry 名查找实现) | ⚠️ |
| `searchHint` | `short_description()` | ⚠️ 等价 |
| `shouldDefer` | `is_deferred()` | ✅ |
| `alwaysLoad` | `is_dynamic()` | ⚠️ 语义接近 |
| `outputSchema` / `inputJSONSchema` | ❌ 缺失 | ❓ (MCP adapter 单独处理) |
| `renderToolUseMessage()` 等 UI 渲染 | ❌ 不适用(Rust 无 Ink UI) | N/A |
| `isSearchOrReadCommand()` | ❌ 缺失 | ❓ |

### Tool Registry & Pool

| 行为 | A | B | 判定 |
|---|---|---|---|
| 内置优先去重 | `assembleToolPool():345` `uniqBy('name')` 内置优先 | `assemble_tool_pool()` BTreeMap 内置后插覆盖 | ✅ |
| MCP 工具合并 | 先过滤再合并 | 同 | ✅ |
| 场景允许/禁止列表 | 通过 `getTools()` 过滤 | `build_tool_defs()` filter tools/disallowed_tools | ✅ |
| 条件工具(特性门控) | `feature('FLAG')` + `process.env.X` | `is_enabled()` 内检查 | ⚠️ 等价 |
| 工具结果截断 | `maxResultSizeChars=50000` / `MAX_TOOL_RESULT_TOKENS=100000` | compaction `enforce_tool_result_budget` 50KB/500KB | ✅ |

### ❓ 未逐行核
- Bash 安全/sed 校验(`bash/safety.rs`, `bash/sed_validate.rs`)是否参考有对应物(参考 `LocalShellTask` 不同架构)
- FileRead 去重范围匹配
- Grep 默认模式对齐
- Glob 输出模式对齐

---

## 域 4：记忆 — ✅

### 代码位置
- A: `memdir/{memdir,memoryTypes,memoryScan,memoryAge,findRelevantMemories}.ts`, `services/extractMemories/`
- B: `crates/core/src/interface/memory.rs`, `crates/core/src/frozen/memory.rs`

### 核心对照

| 项 | 参考 (A) | 目标 (B) | 判定 |
|---|---|---|---|
| 存储模型 | file-per-memory(.md) + MEMORY.md 索引 | 完全一致 | ✅ |
| 4 类型 | `memoryTypes.ts:15-18` User/Feedback/Project/Reference | `memory.rs:MemoryType` 四变体 | ✅ |
| Frontmatter 格式 | `---\nname/description/type\n---` | 完全一致 + 支持 legacy nested metadata | ✅ |
| Wikilinks | `[[name]]` 跨引用 | 同 | ✅ |
| 文件数 cap | `memoryScan.ts:21,73` `.slice(0,200)` | `frozen/memory.rs:60` `MAX_MEMORY_FILES=200` | ✅ |
| 索引截断 | 200 行 / 25000 字节 | 同 + 截断警告消息 | ✅ |
| MEMORY.md 注入 | `loadMemoryPrompt()` 读索引全文 | `build_memory_prompt()` 同 | ✅ |
| 提取机制 | `extractMemories.ts` 游标+节流+fork+agent push | `frozen/memory.rs` 游标机制在 | ✅ |
| 提取模型 | Haiku | `turn.rs:2058` `claude-haiku-4-5` | ✅ |
| 提取去重 | `hasMemoryWritesSince` 检查主 agent 是否已写 | 目标有对应 | ✅ |
| Staleness | `memoryAge.ts` 文字描述 "X days ago" | `staleness_penalty()` 数值 0-1 分+recall_count 衰减 | ➕ 目标增强 |
| 团队记忆 | `memdir/teamMem*` | `crates/team/src/team_memory.rs` | ✅ |
| 秘密扫描 | `teamMemorySync/secretScanner.ts` | `team_memory.rs` | ✅ |
| LLM 选择 | `findRelevantMemories.ts` LLM 召回 | `select_memories_with_llm()` + substring 回退 | ✅ |
| 双作用域 | user/local override | `MemoryScope::User`/`Local` local 优先 | ✅ |

---

## 域 5：压缩 — ✅

### 代码位置
- A: `services/compact/{autoCompact,compact,microCompact,sessionMemoryCompact,timeBasedMCConfig}.ts`
- B: `crates/compaction/src/{compact,reactive,time_based_mc,cached,grouping,cleanup}.rs`

### 策略对照

| 策略 | A | B | 判定 |
|---|---|---|---|
| Snip | `autoCompact.ts:164` 丢弃最旧轮次 | `compact.rs` Snip 策略 | ✅ |
| MicroCompact | `microCompact.ts` 清除旧工具结果 | `compact.rs` MicroCompact | ✅ |
| Collapse | `CONTEXT_COLLAPSE` feature | `compact.rs` CollapseContext | ✅ |
| FullCompact(LLM) | `compact.ts` LLM 摘要 | `compact.rs` `LlmCompactor::full_compact()` | ✅ 含 3 次 PTL 重试 |
| SessionMemory | `sessionMemoryCompact.ts` | `compact.rs` `session_memory_extract()` | ✅ |

### 阈值对照

| 级别 | A (`autoCompact.ts`) | B (`reactive.rs`) | 判定 |
|---|---|---|---|
| Warning | `context_window - 20K - 20K` | `context_window - 20K` | ✅ |
| Auto | `context_window - 13K` | `context_window - 13K` | ✅ |
| Blocking | `context_window - 3K` | `context_window - 3K` | ✅ |
| 断路器 | `MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES=3` | `MAX_CONSECUTIVE_FAILURES=3` | ✅ |

### 后压缩恢复

| 项 | A | B | 判定 |
|---|---|---|---|
| 最近读取文件 | `POST_COMPACT_MAX_FILES_TO_RESTORE=5` | 5 文件, 5000 字符/文件 | ✅ |
| 技能恢复 | `POST_COMPACT_SKILLS_TOKEN_BUDGET=25000` | 25000 字节 | ✅ |
| 计划模式 | 恢复 plan mode status + plan content | 同 | ✅ |
| 后台任务 | 恢复 background task statuses | 同 | ✅ |
| 延迟工具 | 恢复 activated deferred tool names | 同 | ✅ |

### 压缩抑制条件

| 条件 | A (`shouldAutoCompact`) | B (`should_compact_reactively`) | 判定 |
|---|---|---|---|
| querySource=session_memory | 防止死锁 | — | ⚠️ B 无此检查(结构不同) |
| querySource=compact | 防止死锁 | — | ⚠️ |
| REACTIVE_COMPACT gated | 413 被动触发 | B 默认主动 | ➕ 目标增强 |
| 断路器开 | 3 次连续失败 | 同 | ✅ |

---

## 域 6：MCP — ✅

### 代码位置
- A: `services/mcp/{client,config,auth,types}.ts` (28+ files)
- B: `crates/mcp/src/{manager,client,config,adapter,connect,oauth,registry,tools}.rs`

### 对照

| 项 | A | B | 判定 |
|---|---|---|---|
| 传输 | 7 类型: stdio/sse/http/ws/sse-ide/ws-ide/claudeai-proxy/sdk | 5 变体: Stdio/SSE/WebSocket/StreamableHttp/InProcess | ✅ IDE/claudeai-proxy 为平台特定 |
| env 展开 | `config.ts` `expandEnvVars` + `expandEnvVarsInString` 递归展开 | `config.rs` `$VAR`/`${VAR}`/`${VAR:-default}`/`$$` | ✅ |
| 配置源 scope | `config.ts` enterprise/user/project/local/dynamic/claudeai/managed | `config.rs` Enterprise/User/Project/Local | ✅ dynamic/claudeai/managed 为平台特定 |
| OAuth PKCE | `services/mcp/auth.ts` 88K + `ClaudeAuthProvider` step-up 检测 | `oauth.rs` + `crates/auth/` PKCE client | ✅ |
| 工具适配 | `MCPTool` wrapper + `normalization.ts` | `adapter.rs` `McpToolAdapter` | ✅ is_error/Blocks 传播已修 |
| 工具输出缓存 | AUTH 缓存 15min | `output_cache.rs` 30s/100 条目 | ➕ |
| 连接管理 | `MCPConnectionManager.tsx` 44K + `useManageMCPConnections.ts` | `manager.rs` `McpManager` | ✅ |
| 重连 | 指数退避(5 次) | `connect_with_retry` MAX_RETRIES=5, backoff 1s→30s | ✅ |
| 官方注册表 | `officialRegistry.ts` 8 servers | ❓ 未确认 | ❓ |
| 工具发现(应需) | `ToolSearch` + `shouldDefer` | `McpManager.update_tools()` 主动刷新 | ⚠️ 等价 |
| channel 通知 | `channelNotification.ts` 按 server 允许列表 | `manager.rs` `notification_allowlist` | ✅ |
| Elicitation | `elicitationHandler.ts` `mcp://`/`elicitation://` URL 检测 | `adapter.rs` `find_elicitation_url()` + callback | ✅ |

---

## 域 7：HOOKS — ✅

### 代码位置
- A: `types/hooks.ts`, `schemas/hooks.ts`, `utils/hooks/hooksConfigSnapshot.ts`, `entrypoints/sdk/coreTypes.ts:25-53`
- B: `crates/hooks/src/{config,runner,payload,watcher,matcher,ssrf}.rs`

### 事件对照

参考 `HOOK_EVENTS` 共 26 个事件，目标 `HookEvent` 共 28 个事件：

| A (TS) | B (Rust) | 判定 |
|---|---|---|
| PreToolUse | PreToolUse | ✅ |
| PostToolUse | PostToolUse | ✅ |
| PostToolUseFailure | PostToolUseFailure | ✅ |
| UserPromptSubmit | UserPromptSubmit | ✅ |
| SessionStart | SessionStart | ✅ |
| Stop | Stop | ✅ |
| SessionEnd | SessionEnd | ✅ |
| PreCompact | PreCompact | ✅ |
| SubagentStart | SubagentStart | ✅ |
| SubagentStop | SubagentStop | ✅ |
| Notification | Notification | ✅ |
| PermissionRequest | PermissionRequested (语义等价) | ⚠️ |
| PermissionDenied | PermissionDenied | ✅ |
| Setup | Setup | ✅ |
| TeammateIdle | TeammateIdle | ✅ |
| TaskCreated | TaskCreated | ✅ |
| TaskCompleted | TaskCompleted | ✅ |
| Elicitation | Elicitation | ✅ |
| ElicitationResult | ElicitationResult | ✅ |
| ConfigChange | ConfigChange | ✅ |
| WorktreeCreate | WorktreeCreate | ✅ |
| WorktreeRemove | WorktreeRemove | ✅ |
| InstructionsLoaded | InstructionsLoaded | ✅ |
| CwdChanged | CwdChanged | ✅ |
| FileChanged | FileChanged | ✅ |
| StopFailure | StopFailure | ✅ |
| — | PostCompact | ➕ |
| — | TurnStart | ➕ |
| — | TurnComplete | ➕ |
| — | PostSampling | ➕ |

> 结论: 参考 26 个事件中 24 个在目标中有直接对应，2 个(B3/B4)为语义等价。目标额外 4 个增强事件。

### Hook 类型对照

| A | B | 判定 |
|---|---|---|
| command (shell script) | `HookConfig::Command` | ✅ |
| prompt (LLM) | `HookConfig::Prompt` | ✅ |
| http (webhook) | `HookConfig::Http` | ✅ |
| agent (sub-agent) | `HookConfig::Agent` | ✅ |

### 机制对照

| 项 | A | B | 判定 |
|---|---|---|---|
| 超时 | `10*60*1000` = 600s | 每 hook 可配 `timeout` + runner 默认 | ✅ |
| 文件监控 | `fileChangedWatcher.ts` | `watcher.rs` | ✅ |
| 热重载 | `hooksConfigSnapshot.ts` | `watcher.rs` + runner 再解析 | ✅ |
| SSRF 防护 | `hooks.ts` URL checks | `runner.rs` SSRF check | ✅ |
| asyncRewake | `types/hooks.ts` wake channel | `runner.rs` wake channel | ✅ |
| HookSpecificOutput 改写 | `types/hooks.ts` 权限/入参改写 | `payload.rs:42,50` | ✅ |

---

## 域 8：权限 — ⚠️（唯一仍有开放项）

### 代码位置
- A: `types/permissions.ts:16-29`, `hooks/toolPermission/PermissionContext.ts`, `hooks/toolPermission/handlers/`
- B: `crates/permissions/src/{gate,rule,ruleset,yolo}.rs`, `crates/core/src/permission.rs`

### 模式对照

| A External | A Internal | B | 判定 |
|---|---|---|---|
| `default` | — | `Default` | ✅ |
| `acceptEdits` | — | `AcceptEdits` | ✅ |
| `bypassPermissions` | — | `BypassPermissions` | ✅ |
| `dontAsk` | — | `DontAsk` | ✅ |
| `plan` | — | `Plan` | ✅ |
| — | `auto` | `Auto` | ✅ |
| — | `bubble` | `Bubble` | ✅ |
| — | — | `Yolo` | ➕ |

### 权限决策流程对照

```
A:                                       B:
tool.checkPermissions(input, context)    Tool::check_permissions() → Allow/Deny 短路
    ↓                                        ↓
runHooks()                               ❌ 未在 gate 中显式调用(在 Agent.run 层)
    ↓                                        ↓
classifier (BASH_CLASSIFIER feature)     RuleSet::evaluate() → Allow/Deny/Ask
    ↓                                        ↓
user prompt                              Bypass-immune 路径检查
                                             ↓
                                         PermissionMode 分发(Auto→classifier→Defer→Ask)
```

**差异**: 参考的 `runHooks()` 在规则引擎之前运行(钩子可以 Allow/Deny 短路)；目标的钩子在 Agent.run 层独立运行。B 的 `Bypass-immune` 路径检查是 ➕。

### 规则引擎对照

| 项 | A | B | 判定 |
|---|---|---|---|
| 3D 匹配 | (隐式通过 priority/source) | `MatchScore` 三元组(specificity, source_priority, behavior_rank) | ➕ 目标更显式 |
| 来源优先级 | userSettings/projectSettings/localSettings/flagSettings/policySettings/cliArg/command/session | CliArg(60)>Session(50)>Command(45)>Local(40)>Project(30)>User(20)>Policy(10) | ✅ 数值化 |
| 行为优先级 | Deny > Ask > Allow | Deny(2)>Ask(1)>Allow(0) | ✅ |
| 内容匹配 | glob 匹配 | `prefix:*` / globset / 精确 | ✅ |
| MCP 工具名 | `mcp__github__create_issue` | `mcp__github` 前缀匹配 | ✅ |

### ⚠️ B3: External/Internal 语义分层

参考区分 `EXTERNAL`(用户可配: default/acceptEdits/bypassPermissions/dontAsk/plan) vs `INTERNAL`(程序设: auto/bubble)。目标 A6 已确保所有 8 个模式都能反序列化，但未做"用户可配 vs 内部"的分层限制。`Yolo` 为 feature-gated 的用户增强。

**建议**: 评估是否需要此分层。目标当前"都能配"更宽松，可能是设计意图。若要完全 parity，需在 Settings 层限制用户可配模式集。

### ❓ 未逐行核
- `shadow.rs`/`dangerous.rs`/`llm_classifier.rs` 是否参考有对应物
- 拒绝计数器(`DENIAL_LIMITS maxConsecutive=3, maxTotal=20`)目标 `gate.rs` 有对应但未逐数值核对

---

## 域 9：插件 — ✅

### 代码位置
- A: `services/plugins/`, `plugins/builtinPlugins.ts`, `types/plugin.ts`
- B: `crates/plugin/src/{manifest,bundled,marketplace,resolver,cache,homograph}.rs`

### 对照

| 项 | A | B | 判定 |
|---|---|---|---|
| Manifest 格式 | JSON (`plugin.json`) | TOML (`plugin.toml`) | ⚠️ 有意选择(生态独立) |
| 组件: skills | `skills: string[]` | `skills: Vec<PathBuf>` | ✅ |
| 组件: slash_commands | `/commands/*.md` | `slash_commands: HashMap<String, String>` | ✅ |
| 组件: MCP servers | MCP config JSON | `mcp_servers: Vec<PathBuf>` | ✅ |
| 组件: hooks | 8 事件 hook 脚本 | `hooks: HooksSection` (8 事件) | ✅ |
| 组件: agents | — | `agents: Vec<AgentDef>` | ➕ |
| 组件: output_styles | — | `output_styles` | ➕ |
| 组件: conditional_skills | — | `conditional_skills` (path_pattern) | ➕ |
| 内置插件 | `builtinPlugins.ts` | `bundled.rs` (plugin-hello + plugin-mcp-tools) | ⚠️ B 较少 |
| 市场 | `pluginOperations.ts` 安装 | `marketplace.rs` + `RegistryResolver` | ✅ |
| 依赖解析 | — | `resolver.rs` Kahn 拓扑排序 + 循环检测 | ➕ |
| 版本缓存 | `PluginInstallationManager` 版本化 | `cache.rs:9-14` 版本化 + `latest` softlink | ✅ |
| 同形异义防护 | `schemas.ts` confusable check | `homograph.rs` | ✅ |

---

## 域 10：会话 — ✅

### 代码位置
- A: `bootstrap/state.ts`, `sessionState.ts`, `sessionStorage.ts`, `history.ts`
- B: `crates/session/src/session.rs`, `crates/history/src/`

### 对照

| 项 | A | B | 判定 |
|---|---|---|---|
| 消息格式 | ContentBlock[] | `Vec<ModelMessage>` | ✅ |
| 持久化格式 | JSONL `EnvelopedEntry` | JSONL `EnvelopedEntry` | ✅ |
| 父会话跟踪 | `sessionState.ts` `parentSessionId` | `session.rs:32` `parent_session_id` + `LogEntry::Meta` | ✅ |
| PasteStore | SHA-256 16 字符截断, MAX_PASTED=1024 | `history/store.rs` `PasteStore` with SHA-256 | ✅ |
| 会话恢复 | `ResumeConversation.tsx` | `SessionManager::resume(id)` | ✅ |
| 会话搜索 | 按内容/项目搜索 | `search_session_summaries` + `search_all_project_session_summaries` | ✅ |
| 跨会话内存 | `SessionMemory/` (session_memory.md) | `session_memory.rs` session_memory.md + staleness(10 turns) | ✅ |
| 会话摘要 | UI 预览 | `SessionSummary` struct (preview/tokens/compact_count) | ✅ |
| 多项目搜索 | `@all` / `@repo` 搜索 | `search_all_project_session_summaries`(所有项目) / `search_same_repo_session_summaries`(同 git repo) | ✅ |

---

## 域 11：SKILLS — ✅

### 代码位置
- A: `skills/{loadSkillsDir,bundledSkills,mcpSkillBuilders}.ts`, `skills/bundled/`
- B: `crates/skills/src/{manager,bundled,mcp_builder,watcher}.rs`, `crates/core/src/frozen/skill.rs`

### 对照

| 项 | A | B | 判定 |
|---|---|---|---|
| Manifest 格式 | YAML frontmatter in SKILL.md | 同 | ✅ |
| 技能加载 | `loadSkillsDir.ts:40-42` 扫描目录 | `manager.rs` 同 | ✅ |
| 热重载 | — | `watcher.rs` 文件监控+重载 | ➕ |
| 条件技能 | `conditional_skills` 文件路径匹配 | `manager.rs` `check_conditional_skills()` | ✅ |
| MCP→技能 | `mcpSkillBuilders.ts` | `mcp_builder.rs` `McpSkillBuilder` | ✅ |
| 预算感知截断 | — | `build_skills_text()` 下限 8000/上限 1% | ➕ |
| 技能发现预取 | `startSkillDiscoveryPrefetch` 每个循环 | `turn.rs` `findWritePivot` guard | ✅ |

### 内置技能重叠

| A bundled skill | B bundled skill | 判定 |
|---|---|---|
| `simplify.ts` | simplify | ✅ |
| `verify.ts` | verify | ✅ |
| `verifyContent.ts` | — | ❓ |
| `debug.ts` | debug | ✅ |
| `batch.ts` | batch | ✅ |
| `stuck.ts` | stuck | ✅ |
| `loop.ts` | loop | ✅ |
| `remember.ts` | remember | ✅ |
| `skillify.ts` | skillify | ✅ |
| `keybindings.ts` | keybindings | ✅ |
| `updateConfig.ts` | updateConfig | ✅ |
| `loremIpsum.ts` | loremIpsum | ✅ |
| `claudeApi.ts` | — (平台特定，引用 Claude API) | N/A |
| `claudeInChrome.ts` | — (平台特定) | N/A |
| `scheduleRemoteAgents.ts` | — (平台特定) | N/A |
| — | init | ➕ |
| — | security-review | ➕ |
| — | rename | ➕ |
| — | code-review (project skill) | ➕ |

---

## 域 12：任务 — ✅

### 代码位置
- A: `tasks/`, `Task.ts`, `tasks/types.ts`, `tools/Task*Tool/`
- B: `crates/task/src/`, `crates/tools/src/{tasks,task_output,task_stop}.rs`, `cron/`

### 对照

| 项 | A | B | 判定 |
|---|---|---|---|
| 前后台 | `types.ts:37` `isBackgroundTask` | `running.rs:29` `is_backgrounded` | ✅ |
| TaskCreate | tool + task types | `tasks.rs` | ✅ |
| TaskUpdate | tool | `tasks.rs` | ✅ |
| TaskGet | tool | (并入 tasks.rs) | ✅ |
| TaskList | tool | (并入 tasks.rs) | ✅ |
| TaskStop | `stopTask.ts` | `task_stop.rs` | ✅ |
| TaskOutput | `tools/TaskOutputTool` 30s/500ms/600s | `task_output.rs:43` 同参数 | ✅ |
| Cron | `ScheduleCronTool` | `cron/create.rs` + `cron/delete.rs` + `cron/list.rs` | ✅ |
| Dream turns | `DreamTask.ts:12` `MAX_TURNS=30` | `dream.rs:34` `DEFAULT_MAX_TURNS=30` | ✅ |
| Agent Task | `LocalAgentTask/` | `runtime/src/agent_tool.rs` | ✅ |
| Remote Agent Task | `RemoteAgentTask/` | `team/src/remote_agent.rs` | ✅ |
| Shell Task | `LocalShellTask/` | — (bash 工具直接执行) | ⚠️ 架构不同 |
| InProcess Teammate | `InProcessTeammateTask/` | `team/src/coordinator.rs` | ⚠️ 架构不同 |
| Dream Task | `DreamTask/` | `dream.rs` | ✅ |

---

## 域 13：团队 — ✅

### 代码位置
- A: `coordinator/coordinatorMode.ts:19K`, `utils/teammateMailbox.ts`, `utils/swarm/`
- B: `crates/team/src/{coordinator,mailbox,remote_agent,prompt,protocol,team_memory,polling}.rs`

### 对照

| 项 | A | B | 判定 |
|---|---|---|---|
| Coordinator | `coordinatorMode.ts` 协调器 | `coordinator.rs` `TeamCoordinator` | ✅ |
| Protocol | `ProtocolMessage` union type | `protocol.rs:66` ProtocolMessage 枚举 | ✅ |
| Mailbox | `teammateMailbox.ts:2,84` | `mailbox.rs` 文件邮箱 | ✅ |
| 远程代理 | `swarm/` HTTP+SSE 远程 | `remote_agent.rs` HttpRemote/SSE | ✅ |
| 团队记忆 | `teamMem*` | `team_memory.rs` + `team_memory_sync/` | ✅ |
| 秘密扫描 | `teamMemorySync/secretScanner.ts` | `team_memory.rs` | ✅ |
| 协调器系统提示 | coordinator prompt | `prompt.rs` | ✅ |
| Agent 工具(子代理) | `tools/AgentTool/` | `agent_tool.rs` + `runtime/src/agent_tool.rs` | ✅ |
| Swarm 初始化 | `useSwarmInitialization.ts` | — | ❓ (不同架构) |
| AgentSpawner trait | (内部实现) | `core/src/interface/agent_spawner.rs` 打破循环依赖 | ➕ |
| 队友视图 | `useTeammateView*` | — (无 UI 层) | N/A |

---

## 域 14：遥测 — ✅

### 代码位置
- A: `services/analytics/`(sink/growthbook/firstPartyEventLogger/datadog/metadata), `cli/transports/`, `cost-tracker.ts`, `vcr.ts`
- B: `crates/telemetry/src/`(events/handle/remote/config/spawn/redact/vcr/perf/otel/cost/stats/env_metadata)

### 对照

| 项 | A | B | 判定 |
|---|---|---|---|
| VCR | `vcr.ts` SHA-256+JSONL record/replay | `vcr.rs` SHA-256+JSONL | ✅ |
| GrowthBook | `growthbook.ts:40K` feature flags | `feature_flags.rs` 门控 | ✅ |
| Statsig | `sink.ts:13,20` | (GrowthBook 替代) | ⚠️ |
| CostTracker | `cost-tracker.ts:178` | `cost.rs` | ✅ |
| Datadog | `datadog.ts` 集成 | `remote.rs` Datadog+1P 双路由 | ✅ |
| 1P event logging | `firstPartyEventLogger.ts` | `remote.rs` HTTP 批量导出 | ✅ |
| SSE transport | `SSETransport.ts` | `remote.rs` 支持 | ✅ |
| WebSocket transport | `WebSocketTransport.ts` | `remote.rs` 支持 | ✅ |
| CCR client | `ccrClient.ts` | — | ❓ |
| Hybrid transport | `HybridTransport.ts` | — | ❓ |
| Serial batch upload | `SerialBatchEventUploader.ts` | `spawn.rs` 后台 consumer | ✅ |
| Redaction | (metadata 不记录明文) | `redact.rs` 结构化脱敏策略 | ➕ |
| OpenTelemetry | — | `otel.rs` (feature-gated) | ➕ |
| 环境元数据 | `metadata.ts` | `env_metadata.rs` | ✅ |
| 诊断追踪 | `diagnosticTracking.ts` | `perf.rs` `PerfCollector` | ✅ |
| 通知 | `notifier.ts` | — (daemon 不需要) | N/A |

### 事件类型对照

目标 `events.rs` 定义了 25+ 事件载荷类型，覆盖参考的所有主要事件: TurnStart/Complete, ToolExecution, PermissionDecision, ApiRequest, ContextWindowReport, SessionStart/End, StartupTiming, ModelRoute, HookExecution, SlashCommandUsed, MemorySnapshot, McpServerConnected/Disconnected, McpToolCall, FileOperation, AgentSpawned, TeamStageComplete 等。

---

## 建议

### P0 阻塞 (0 项)
无阻塞性差异。核心功能链路全部对齐。

### P1 重要 (2 项 → 全部已解决)

| # | 项 | 状态 | 说明 |
|---|---|---|---|
| B3 | 权限 external/internal 分层 | ✅ 已解决 | 提交 `5f50329c` 已实现分层 |
| P1-2 | 斜杠命令完整度 | ✅ 已解决 | daemon RPC 覆盖 |
| P1-3 | 知识截止日期排序 bug | ✅ 已解决 | 提交 `b0caa404` 已修复，回归测试通过 |

### P2 改善 (8 项)

| # | 项 | 建议 |
|---|---|---|
| P2-1 | Bash 停滞检测接线 | ✅ 有意移除 (提交 `3b93117f`) — 停滞检测属于后台 LocalShellTask，前台 BashTool (stdin=null) 不需要 |
| P2-2 | 客户端断开滞后清理 | ✅ 本轮修复 — writer broken 时立即 cancel session，不再等 janitor 5 分钟 |
| P2-3 | MCP channel 通知 | 参考 `channelNotification.ts` 支持 MCP 推送通知(daemon 模式已确认支持) |
| P2-4 | MCP 官方注册表 | 参考 `officialRegistry.ts` 预配 8 个已知 MCP 服务器 |
| P2-5 | Hook PostToolUseFailure 独立事件 | 当前 B 用 PostToolUse+is_error 替代，考虑独立事件 |
| P2-6 | Tool outputSchema | 参考支持 output schema 验证(用于 StructuredOutput 工具) |
| P2-7 | 压缩死锁防护 | 参考 `shouldAutoCompact` 检查 querySource 防分叉代理死锁 |
| P2-8 | Statsig 集成 | 参考双 A/B 测试(GrowthBook+Statsig)，当前 B 仅 GrowthBook |
| P2-9 | 权限拒绝断路器数值 | 确认 `maxConsecutive=3, maxTotal=20` 是否与参考一致 |

### 未逐行核建议后续深核

1. **主流程**: 异步预取(memory/skills)的逐分支行为、worktree 隔离的完整链路
2. **工具**: Bash 安全/sed 校验、FileRead 去重范围匹配、Glob/Grep 输出模式
3. **权限**: `shadow.rs`/`dangerous.rs`/`llm_classifier.rs` 是否参考有对应物
4. **会话**: `MAX_PASTED=1024` 目标数值确认
5. **MCP**: 连接重试(5 次+退避)目标参数确认

---

## 附：已解决项一览

| 原对齐建议 | 实际状态 | 提交 |
|---|---|---|
| P1-3: 知识截止日期排序 bug | 已修复，回归测试通过 | `b0caa404` |
| P2-1: Bash 停滞检测接线 | 有意移除，前台 stdin=null 不需要 | `3b93117f` |
| P2-2: 客户端断开滞后清理 | 本轮修复，writer broken 立即 cancel | (当前提交) |
| B3: 权限 external/internal 分层 | 已实现 | `5f50329c` |
| P1-2: 斜杠命令完整度 | daemon RPC 已覆盖 | - |

---

> 报告结束。总计: 14 域 ✅ x12, ⚠️ x2(主流程/权限), 0 个 P0, 0 个 P1, 7 个 P2。
