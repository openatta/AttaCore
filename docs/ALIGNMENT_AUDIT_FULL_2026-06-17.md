# AttaCore vs Claude Code TS — 全 14 域行为对齐对比报告

> 日期: 2026-06-17
> 参考实现 (A): `3rds/claude-code-main/` (TypeScript, ground truth)
> 目标实现 (B): AttaCore (Rust, 17 crates + daemon)
> 方法: 逐域定位双侧代码、读取关键逻辑、带 `文件:行` 比对。审计阶段只读；修复见下方"修复与勘误"。
> 标记: ✅ 一致 | ⚠️ 等价/小偏差 | ❌ 行为偏差 | ➕ 目标增强(参考无) | ❓ 未确认

---

## 修复与勘误（2026-06-17 实施）

本节记录对齐建议的处理结果，以及对审计中发现的自误判的更正。

### 已修复（A 组，含 TDD 测试）
| 项 | 文件 | 改动 |
|---|---|---|
| A1 Dream MAX_TURNS 3→30 | `crates/task/src/dream.rs:34` | 对齐 `DreamTask.ts:12 MAX_TURNS=30` |
| A2 记忆文件数 200 cap | `crates/core/src/frozen/memory.rs:60` | `collect_memdir_files` 按 mtime 新→旧排序后 `take(200)`，对齐 `memoryScan.ts:21` |
| A3 MCP env 展开 | `crates/mcp/src/config.rs`(+fn) `connect.rs:377` | 新增 `expand_env_vars` 处理 `$VAR`/`${VAR}`/`${VAR:-default}`/`$$`，spawn 前 expand command/args/env/url/headers |
| A4 MCP 过时注释 | `crates/mcp/src/manager.rs:514` | 更正"仅 stdio"过时注释（实际 5 传输均 wire） |
| A5 token-budget 停止逻辑 | `crates/runtime/src/turn.rs:733` `agent.rs:144` | 100%+硬上限10 → 90% 阈值 + 收益递减(≥3 且 delta<500) + 无上限，对齐 `query/tokenBudget.ts` |
| A6 权限枚举补 DontAsk/Bubble | `crates/core/src/interface/settings.rs:76` | settings 枚举补两变体，修复 `bubble`/`dontAsk` 反序列化失败 |

### 撤销（再核后判定非缺陷）
- **A7 max_tokens 8000 地板**（`turn.rs:1370`）：首次升 64K 后 effective=64000，`<8000` 恒假，**死分支无行为影响**，不改。
- **A8 # Code style 冗余**（`coding.rs:429`）：`CODE_STYLE_BLOCK`("匹配周边代码风格") 与 Doing Tasks 的"少写注释"**非重复**，删除会丢有用指引，不改。

### 自误判更正（本审计原结论有误，已修正）
- **MCP 传输**：原域 6 称"仅 2-3 种可用"——**错误**。`config.rs:14-68` 有 Stdio/StreamableHttp/Sse/InProcess/WebSocket 全 5 变体，`connect.rs:200-208` `transport_kind()` 返回全部。唯一真缺口是 env 展开(已 A3 修)与过时注释(已 A4 修)。
- **MCP scope**：原 B1 称"仅工具过滤器"——**部分错误**。目标同时有两套：per-server `scope: Vec<String>`(工具过滤器, `config.rs:23`) **与** 配置源 scope(Enterprise/User/Project/Local, `config.rs:96+`)。非缺陷。
- **记忆 staleness**(B2)：目标数值罚分(`memory.rs:49`) vs 参考文本提示(`memoryAge.ts`)——**目标增强**，非偏差，不改。

### 仍开放（设计决策，非 bug）
- **B3 权限 external/internal 语义**：A6 已修复反序列化；参考区分 EXTERNAL(default/acceptEdits/bypassPermissions/dontAsk/plan) vs INTERNAL(auto/bubble) 的"用户可配 vs 内部"分层仍未实现，留作后续设计。

---

## 总览

| # | 域 | 评级 | 一句话 |
|---|---|---|---|
| 1 | 主处理流程 | ⚠️ | `max_tokens` 恢复逐字对齐；`token-budget` 续写停止逻辑偏差 |
| 2 | 提示词 | ✅ | 20 section 齐全，含逐字短语；Doing Tasks 忠实重组 |
| 3 | 工具 | ✅ | 注册表对齐；FileEdit 过时检查行为一致 |
| 4 | 记忆 | ⚠️ | 4 类型/头部扫描/提取对齐；**文件数上限缺失**；staleness 机制不同 |
| 5 | 压缩 | ✅ | 5 策略 + 反应式默认开 + 警告钩子齐全 |
| 6 | MCP | ⚠️ | **传输仅 2-3 种可用(非 5)**；env 展开未确认；scope 语义不同 |
| 7 | HOOKS | ⚠️ | 事件 14 种(非"30")；600s 超时对齐；多 4 个目标增强事件 |
| 8 | 权限 | ⚠️ | 7 参考模式皆在；`Yolo` 误标；双枚举不一致 |
| 9 | 插件 | ✅ | TOML(有意) + 版本缓存 + 同形异义对齐 |
| 10 | 会话 | ✅ | 父跟踪 + JSONL/EnvelopedEntry + PasteStore 对齐 |
| 11 | SKILLS | ✅ | frontmatter + 核心技能重叠；平台特定技能差异(预期) |
| 12 | 任务 | ⚠️ | background/TaskOutput/cron 对齐；**Dream 上限 3 vs 参考 30** |
| 13 | 团队 | ✅ | mailbox + protocol + coordinator + remote 对齐 |
| 14 | 遥测 | ✅ | VCR + GrowthBook + CostTracker + 双路由 + 脱敏齐全 |

> **结论**: 能力覆盖度高（与 06-15 自报审计 ~92% 大致相符），但**行为精度被系统性高估**。共发现 **3 项 P1 行为偏差**、**4 项 P2 偏差/误标**、**4 项 P3 差异**。核心提示词/工具/会话/团队/遥测/压缩确属高质量对齐；主流程续写、记忆上限、MCP 传输、Dream 回合、权限枚举是实质差距。

---

## 域 1：主处理流程 — ⚠️

### 定位
- A: `3rds/.../src/query.ts`（主循环）、`query/tokenBudget.ts`
- B: `crates/runtime/src/turn.rs`（2088 行）

### ✅ `max_tokens` 恢复 — 逐字对齐
| 行为 | 参考 | 目标 | 判定 |
|---|---|---|---|
| 首次升 64K | `query.ts:1199` `ESCALATED_MAX_TOKENS`(`utils/context.ts:25 = 64_000`) | `turn.rs:1358,1365` `ESCALATED_64K=64000`，`recovery==1` 升级 | ✅ |
| 恢复上限 3 | `query.ts:164` `MAX_OUTPUT_TOKENS_RECOVERY_LIMIT=3` | `turn.rs:1356` `MAX_TOKENS_RECOVERY_LIMIT=3` | ✅ |
| 恢复消息 | `query.ts:1226` "Output token limit hit. Resume directly — no apology, no recap..." | `turn.rs:1376-1380` **逐字相同** | ✅ |
| 门控 | `query.ts:1199` 受 `tengu_otk_slot_v1` feature 门控 | `turn.rs:1365` 未门控 | ⚠️ |
| 后续重试上限 | `query.ts:1235` `maxOutputTokensOverride: undefined`（回默认） | `turn.rs:1370` 设地板 `8000` | ⚠️ |

### ❌ `token-budget` 续写停止逻辑 — 行为偏差
| 维度 | 参考 (`query/tokenBudget.ts`) | 目标 (`turn.rs:733-779`) | 判定 |
|---|---|---|---|
| 继续条件 | `turnTokens < budget*0.9`（**90%**，tokenBudget.ts:35） | `accumulated < target`（**100%**） | ❌ |
| 硬上限 | **无**（靠 90% + 收益递减） | `count < 10`（**硬上限 10**，turn.rs:739,767） | ❌ |
| 收益递减早停 | `continuationCount>=3 && delta<500`（tokenBudget.ts:16-19） | **无** | ❌ |
| nudge 文本 | `getBudgetContinuationMessage(pct,...)` | "Continue working. Used X/Y..."（turn.rs:744） | ⚠️ |

> 06-15 审计把"最多 10 次自动延续"当作参考对齐——**不正确**。参考无硬上限。差异使目标在长预算下更早停（10 次）或更晚停（100% vs 90%）。

---

## 域 2：提示词 — ✅

### 定位
- A: `3rds/.../src/constants/prompts.ts:444` `getSystemPrompt()`
- B: `crates/core/src/interface/prompt.rs:70` `assemble_prompt()` + `crates/scene/src/scene/coding.rs:29`

### 逐 section
| Section | 参考 | 目标 | 判定 |
|---|---|---|---|
| Identity | `prompts.ts:175` | `coding.rs:343` `identity_block` | ✅ 安全测试指引+URL 猜测警告匹配；品牌→AttaCode |
| System | `prompts.ts:186` | `coding.rs:358` `SYSTEM_INFO_BLOCK` | ✅ hooks/system-reminder/自动压缩 |
| Doing tasks | `prompts.ts:199-253`（`prependBullets`） | `coding.rs:385` `DOING_TASKS_BLOCK` | ✅ 忠实重组为加粗分类，覆盖相同指令 |
| Parallelism | `prompts.ts`(短语) | `coding.rs:404` | ✅ "Parallelism is your superpower" **逐字** |
| Code style | (并入 Doing Tasks `codeStyleSubitems`) | `coding.rs:429` 单列段 | ⚠️ 冗余但内容已覆盖 |
| Actions | `prompts.ts:255` | `coding.rs:435` | ✅ 可逆性/爆炸半径 |
| Tone & style | `prompts.ts:430` | `coding.rs:460` | ✅ `file_path:line_number`、无 emoji |
| 动态段 frc/summarize/scratchpad/output_style/session_guidance/memory/language/env/mcp | `prompts.ts:491-555` | `coding.rs:303-333` `memoized_section` | ✅ 全部注入；`CacheStrategy`(`prompt.rs:27`) 语义等价 |

### 小偏差
- 目标把参考 `USER_TYPE==='ant'` 门控指令（最少注释/先验证再宣称/报告保真/`/issue`）**无条件应用于所有用户**——更严格，非偏差但行为不同。

---

## 域 3：工具 — ✅

### 定位
- A: `3rds/.../src/tools.ts:196` `getAllBaseTools()`
- B: `crates/tools/src/lib.rs:51` `assemble_tool_pool()`

### 工具清单
参考集（Agent/Bash/Glob/Grep/FileRead/FileEdit/FileWrite/Notebook/WebFetch/WebSearch/TodoWrite/TaskStop/TaskOutput/AskUserQuestion/Skill/EnterPlanMode/ExitPlanMode/Config/SendMessage/TeamCreate/Delete/PowerShell）在目标均有对应模块，并扩展 LSP/Monitor/RemoteTrigger/Cron 等。`assemble_tool_pool` 内置优先去重（BTreeMap）与参考 `assembleToolTool` 一致。

### FileEdit 过时检查
| 行为 | 参考 | 目标 | 判定 |
|---|---|---|---|
| 编辑前必须先 Read | `FileEditTool.ts:281` "File has not been read yet..." | `file_edit.rs:147` `check_read_staleness()` | ✅ |
| 读时间戳失效 | `FileEditTool.ts:519` | `file_edit.rs:750` 预填读缓存 | ✅ |

- ❌ `WebBrowser`（参考 TS 内部/Chrome 扩展）目标无——可接受。
- 注：未对全部 47+ 工具逐行为核对；Bash 安全/sed 校验、FileRead 去重未深核（❓）。

---

## 域 4：记忆 — ⚠️

### 定位
- A: `memdir/{memoryScan,memoryAge,memoryTypes,findRelevantMemories}.ts`、`services/extractMemories/`、`services/SessionMemory/`
- B: `crates/core/src/interface/memory.rs`、`crates/core/src/frozen/memory.rs`

### 核对
| 特性 | 参考 | 目标 | 判定 |
|---|---|---|---|
| 4 类型 user/feedback/project/reference | `memoryTypes.ts:15-18` | `memory.rs:79-88` `MemoryType` | ✅ |
| 头部扫描 | `memoryScan.ts` read-then-sort | `memory.rs:148` `scan_memory_headers()` | ✅ |
| 提取游标+节流+agent 推进 | `extractMemories.ts:305,376,397,429` | `frozen/memory.rs`（机制在） | ✅ |
| staleness | `memoryAge.ts` **文本式 "X days ago" 提示**（>1 天） | `memory.rs:49` `staleness_penalty` **数值 0-1 罚分**（7 天新鲜，7→90 衰减） | ⚠️ 机制不同，目标自创数值评分却标注"TS parity" |
| **MAX_MEMORY_FILES=200（文件数上限）** | `memoryScan.ts:21,73` `.slice(0,200)` | **未实现**——目标的 `200` 是内容行上限 `MAX_LINES=200`(`memory.rs:544`)，非文件数 | ❌ |
| 团队记忆 + 秘密扫描 | `memdir/teamMem*` | `crates/team/src/team_memory.rs` | ✅（结构对齐） |

---

## 域 5：压缩 — ✅

### 定位
- A: `services/compact/`（autoCompact/compact/microCompact/sessionMemoryCompact/timeBasedMCConfig/compactWarningHook/postCompactCleanup）
- B: `crates/compaction/src/`（compact/cached/cleanup/grouping/reactive/session_memory/time_based_mc）

### 核对
| 特性 | 参考 | 目标 | 判定 |
|---|---|---|---|
| 策略 Snip/MicroCompact/SessionMemory/timeBasedMC | `autoCompact.ts:164`(Snip)、`microCompact.ts`、`sessionMemoryCompact.ts`、`timeBasedMCConfig.ts` | `compact.rs`、`cached.rs`、`session_memory.rs`、`time_based_mc.rs` | ✅ |
| 反应式默认开 | `autoCompact.ts:189-207` reactiveCompact alive | `reactive.rs:39` `enabled:true` | ✅ |
| 多级阈值 auto/warn/error/block | `autoCompact.ts` | `reactive.rs:8,103` | ✅ |
| 压缩警告钩子 | `compactWarningHook.ts` | `reactive.rs` warn(`auto-20K`) | ✅（⚠️ 目标用 token 缓冲非"80%"，审计"80%"表述不准） |
| 后压缩清理 | `postCompactCleanup.ts` | `cleanup.rs` | ✅ |

---

## 域 6：MCP — ⚠️

### 定位
- A: `services/mcp/{config,client,auth}.ts`、`tools/{MCPTool,McpAuthTool,ListMcpResourcesTool,ReadMcpResourceTool}`
- B: `crates/mcp/src/`（config/connect/client/manager/oauth/output_cache/registry）

### 核对
| 特性 | 参考 | 目标 | 判定 |
|---|---|---|---|
| env 展开 `$VAR/${VAR}/${VAR:-default}/$$` | `config.ts:44,556` `expandEnvVarsInString`（envExpansion.js） | **未在 config/connect/manager 找到** | ❓ 疑缺失，需确认 |
| 传输 stdio/StreamableHTTP/SSE/WebSocket/InProcess | `config.ts:52-54` Stdio+SSE+WebSocket+Streamable+InProcess | config.rs 定义 Stdio/StreamableHttp/SSE；**`manager.rs:514` 注明"当前只有 stdio 真接入（StreamableHttp 尚未 wire 进 connect.rs）"**；WebSocket/InProcess 未见 | ❌ 实际可用 2-3 种，非 5 |
| 配置源 scope(enterprise/user/project/local) | `config.ts:46,55,62,71` `ConfigScope`/`ScopedMcpServerConfig` | `config.rs:23` `scope: Option<Vec<String>>` ——**语义是工具过滤器，非配置源** | ⚠️ 概念不同 |
| 工具输出缓存 30s/100 | 参考仅见 AUTH 缓存 15min(`client.ts:257`) | `output_cache.rs:15-16` `CACHE_TTL=30s`/`MAX=100` | ➕ 疑目标增强 |
| OAuth PKCE | `services/mcp/auth.ts` | `crates/mcp/src/oauth.rs` | ✅ |

> 06-15 审计"MCP 92%/5 传输/4 scope"明显高估。

---

## 域 7：HOOKS — ⚠️

### 定位
- A: `types/hooks.ts`（事件 zod schema）、`utils/hooks.ts`（runner）
- B: `crates/hooks/src/`（config/payload/runner/watcher）

### 核对
| 特性 | 参考 | 目标 | 判定 |
|---|---|---|---|
| 事件类型 | `types/hooks.ts:73-117` PreToolUse/PostToolUse/PostToolUseFailure/UserPromptSubmit/SessionStart/Notification/Stop/SessionEnd/PreCompact/SubagentStop（≈10-11） | `config.rs:65-117` **14 种**：上述 + TurnStart/TurnComplete/StopFailure/PostSampling | ⚠️ 目标 14（非审计所称"30"）；多 4 个为 ➕ 增强 |
| 超时 600s | `utils/hooks.ts:166` `TOOL_HOOK_EXECUTION_TIMEOUT_MS=10*60*1000` | `config.rs:21,40,50,58` 每 hook `timeout: Option<u64>` + runner default | ✅ 机制对齐（默认值待确认） |
| HookSpecificOutput 改写权限/入参 | `types/hooks.ts` | `payload.rs:42,50` | ✅ |
| SessionEnd 短超时 1.5s | `utils/hooks.ts:175` `1500` | 未单独确认 | ❓ |

---

## 域 8：权限 — ⚠️

### 定位
- A: `types/permissions.ts:24-29`
- B: `crates/core/src/permission.rs:12`、`crates/core/src/interface/settings.rs:76`、`crates/permissions/src/gate.rs`

### 核对
| 特性 | 参考 | 目标 | 判定 |
|---|---|---|---|
| 模式集合 | `types/permissions.ts:24` `EXTERNAL=['acceptEdits','bypassPermissions','default','dontAsk','plan']` ∪ `['auto','bubble']` = **7 种，无 yolo** | `permission.rs:12` **8 变体**（含 `Yolo`） | ⚠️ 7 参考模式皆在；`Yolo` ➕ 目标增强，**06-15 误标为对齐** |
| 双枚举不一致 | — | `permission.rs`(8 变体) vs `settings.rs:76`(**仅 6 变体，缺 DontAsk/Bubble**) | ❗ 配置若走 settings 枚举将无法解析 dontAsk/bubble |
| Bubble（子代理冒泡） | `runAgent.ts:443` `'bubble'` | `gate.rs:331` | ✅ |
| DontAsk | `types/permissions.ts:24` | `gate.rs:323` `DontAsk=>Deny` | ✅ |
| shadow/dangerous/llm_classifier | 未在参考定位到对应物 | `shadow.rs`(582)/`dangerous.rs`(517)/`llm_classifier.rs`(570) | ❓ 疑目标增强，需确认参考是否有 |

---

## 域 9：插件 — ✅

### 定位
- A: `services/plugins/{PluginInstallationManager,pluginOperations}.ts`、`plugins/builtinPlugins.ts`、`utils/plugins/schemas.ts`
- B: `crates/plugin/src/`（manifest/cache/homograph/marketplace/resolver）

### 核对
| 特性 | 参考 | 目标 | 判定 |
|---|---|---|---|
| 清单格式 | `builtinPlugins.ts:80` **JSON** | `manifest.rs`/`cache.rs:10` **TOML**（`plugin.toml`） | ⚠️ 有意选择（生态独立） |
| 版本化缓存 + latest 软链 | `PluginInstallationManager.ts` | `cache.rs:9-14` `{version}/` + `latest -> {version}` + `registry.json` | ✅ |
| 同形异义防护 | `utils/plugins/schemas.ts`（含 confusable） | `homograph.rs`（CONFUSABLE_MAP） | ✅ 参考确有 |
| 启用/禁用 + 市场 + 依赖解析 | `pluginOperations.ts` | `marketplace.rs`/`resolver.rs` | ✅ |

---

## 域 10：会话 — ✅

### 定位
- A: `utils/{pasteStore,sessionState,sessionStorage}.ts`、`history.ts`
- B: `crates/session/src/session.rs`、`crates/history/src/{entry,store,transcript}.rs`

### 核对
| 特性 | 参考 | 目标 | 判定 |
|---|---|---|---|
| 父会话跟踪 | `sessionState.ts` | `session.rs:32,157,186` `parent_session_id` + `LogEntry::Meta` | ✅ |
| JSONL 持久化 | `history.ts:115,316,319` append `history.jsonl` | `session.rs:23` `JsonlHistoryStore`、`session.rs:151` `EnvelopedEntry` | ✅ |
| PasteStore SHA-256 | `pasteStore.ts:22` `sha256.slice(0,16)` | `session.rs`/`history`（机制在） | ✅（注意参考截 16 字符） |
| MAX_PASTED 1024 | `history.ts:20` `MAX_PASTED_CONTENT_LENGTH=1024` | 未单独确认 | ❓ |

---

## 域 11：SKILLS — ✅

### 定位
- A: `skills/{bundledSkills,loadSkillsDir,mcpSkillBuilders}.ts`、`skills/bundled/`
- B: `crates/skills/src/{bundled,manager,mcp_builder,watcher}.rs`

### 核对
| 特性 | 参考 | 目标 | 判定 |
|---|---|---|---|
| YAML frontmatter | `loadSkillsDir.ts:40-42` `parseBooleanFrontmatter` | `manager.rs` | ✅ |
| 内置技能集 | 17 文件（simplify/verify/debug/batch/stuck/loop/remember/skillify/keybindings/loremIpsum/updateConfig + claudeApi/claudeInChrome/scheduleRemoteAgents 等 Ant/Chrome 专属） | simplify/verify/debug/batch/stuck/loop/remember/skillify/keybindings-help/init/security-review/rename | ✅ 核心重叠；平台特定差异（预期） |
| MCP→技能 | `mcpSkillBuilders.ts` | `mcp_builder.rs` | ✅ |
| 文件监控 | — | `watcher.rs` | ✅ |

---

## 域 12：任务 — ⚠️

### 定位
- A: `tasks/`（DreamTask/LocalAgentTask/RemoteAgentTask/...）、`Task.ts`、`tasks/types.ts`
- B: `crates/task/src/{running,store,dream,delete}.rs`、`crates/tools/src/{tasks,task_output,task_stop}.rs`

### 核对
| 特性 | 参考 | 目标 | 判定 |
|---|---|---|---|
| 前后台 `isBackgrounded` | `types.ts:37` `isBackgroundTask` | `running.rs:29,34,35` `is_backgrounded` | ✅ |
| TaskOutput 阻塞/非阻塞 + 500ms 轮询 + 30s 默认 + 600s 上限 | `tools/TaskOutputTool` | `task_output.rs:43` `30_000`、`:134` `500ms`、`:33` `max 600000` | ✅ |
| Cron 任务 | `tools/ScheduleCronTool` | `crates/tools/src/cron/` | ✅ |
| TaskStop CancellationToken | `tasks/stopTask.ts` | `task_stop.rs` | ✅ |
| **Dream 最大回合** | `DreamTask.ts:12` `MAX_TURNS = 30` | `dream.rs:7,26` **"Auto-stops after 3 turns"** | ❌ **3 vs 30，相差 10 倍** |

> 06-15 审计"Dream 最大 3 回合"当作对齐——**不正确**，参考是 30。

---

## 域 13：团队 — ✅

### 定位
- A: `coordinator/coordinatorMode.ts`（含 `ProtocolMessage` union）、`utils/teammateMailbox.ts`、`utils/swarm/`、`tools/{TeamCreateTool,SendMessageTool}`
- B: `crates/team/src/{coordinator,mailbox,protocol,polling,remote_agent,team_memory,tool}.rs`

### 核对
| 特性 | 参考 | 目标 | 判定 |
|---|---|---|---|
| 文件邮箱 | `teammateMailbox.ts:2,84` `readMailbox`/`getInboxPath` | `mailbox.rs` | ✅ |
| ProtocolMessage 类型化 | `coordinatorMode.ts`（union） | `protocol.rs:66` `ProtocolMessage` enum（注释自称 TS parity） | ✅ |
| 协调器系统提示 | `coordinatorMode.ts` | `coordinator.rs`/`prompt.rs` | ✅ |
| 远程传输 HttpRemote/SSE | `utils/swarm/` | `remote_agent.rs` | ✅ |
| 团队记忆 + 秘密扫描 | `memdir/teamMem*` | `team_memory.rs` | ✅ |

---

## 域 14：遥测 — ✅

### 定位
- A: `services/analytics/{sink,growthbook,firstPartyEventLogger,datadog,metadata}.ts`、`cost-tracker.ts`
- B: `crates/telemetry/src/{vcr,cost,feature_flags,first_party,redact,otel,env_metadata,remote}.rs`

### 核对
| 特性 | 参考 | 目标 | 判定 |
|---|---|---|---|
| VCR SHA-256 + JSONL | `vcr.ts` dehydrate/hydrate | `vcr.rs:4,21` `sha2::Sha256` + JSONL（注释自称 TS parity） | ✅ |
| GrowthBook/Statsig 特性门控 | `sink.ts:13,20` `checkStatsigFeatureGate` | `feature_flags.rs` | ✅（参考名 Statsig，目标名 GrowthBook，同概念） |
| CostTracker | `cost-tracker.ts:178` | `cost.rs` | ✅ |
| 双路由 Datadog + 1P | `sink.ts:5,11,63` | `remote.rs`（双路由） | ✅ |
| 脱敏 | `sink.ts:65` stripProtoFields | `redact.rs` | ✅ |
| OTel 导出 | — | `otel.rs`（feature-gated） | ➕ |

---

## 对齐建议

### P0（阻塞级）
- 无。核心架构（主循环、安全模型、工具生态、压缩、会话）均已对齐，无阻塞性架构缺失。

### P1（行为偏差，应修）
1. **`token-budget` 续写停止逻辑**（`turn.rs:733-779`）：把 100%→`budget*0.9`，去掉硬上限 10，加入收益递减启发式（`continuationCount>=3 && delta<500`），对齐 `query/tokenBudget.ts`。
2. **Dream MAX_TURNS**（`dream.rs:7,26`）：3→30，对齐 `DreamTask.ts:12`。
3. **记忆文件数上限**（`memory.rs`）：补 `MAX_MEMORY_FILES=200` 文件数 cap（`scan_memory_headers`/`collect_memory_files_with`），对齐 `memoryScan.ts:21,73`。

### P2（偏差/误标，应澄清或修）
4. **MCP 传输**（`crates/mcp/src/`）：完成 StreamableHttp wire 进 `connect.rs`（`manager.rs:514` 已标注未接）；补 WebSocket/InProcess 或在文档诚实标注实际支持 2-3 种。
5. **MCP env 展开**：确认/补齐 `$VAR/${VAR}/${VAR:-default}/$$`，对齐 `expandEnvVarsInString`。
6. **MCP scope 语义**：澄清目标 `scope: Vec<String>` 是工具过滤器；若要 parity 参考 `ConfigScope`(enterprise/user/project/local) 配置源，需另实现。
7. **权限双枚举**：统一为单一 `PermissionMode`（以 `permission.rs` 8 变体为准），删除/修正 `settings.rs:76` 的 6 变体副本。
8. **文档更正**：06-15 审计把 `Yolo`、`最多10次续写`、`Dream 3回合`、`MCP 5传输/4 scope`、`30 hooks 事件` 标为参考对齐——均应改为"目标侧增强/加严/实际值"。

### P3（改善）
9. 记忆 staleness：目标数值罚分（`memory.rs:49`）与参考文本提示（`memoryAge.ts`）机制不同；若要 parity 改为 "X days ago" 文本提示，或明确标注为目标增强。
10. `max_tokens` 升级门控（`tengu_otk_slot_v1`）是否引入；后续重试地板 8000 vs 参考回默认。
11. 提示词 `# Code style` 段与 Doing Tasks `codeStyleSubitems` 内容冗余，可合并。
12. Skills 内置集平台差异（claudeApi/claudeInChrome vs init/security-review）记录为预期。
13. 补验证：Bash 安全/sed 校验、FileRead 去重、`shadow/dangerous/llm_classifier` 是否参考有对应物、SessionEnd 1.5s 短超时、MAX_PASTED 1024。

### 安全观察（范围外但已发现）
- `turn.rs:1431` `dangerously_disable_sandbox: true` 在 `execute_tool_inner` 硬编码——Bash 沙箱默认关闭，与参考沙箱执行语义可能不符，建议单独评估。

---

## 附：核对涉及文件
- 参考 (A): `constants/prompts.ts` `tools.ts` `query.ts` `query/tokenBudget.ts` `utils/context.ts` `types/{permissions,hooks}.ts` `memdir/*` `services/{compact,mcp,extractMemories,SessionMemory,analytics,plugins}/*` `tasks/*` `coordinator/coordinatorMode.ts` `utils/{teammateMailbox,pasteStore,sessionState,hooks}.ts` `history.ts` `cost-tracker.ts` `tools/{FileEditTool,BashTool,DreamTask}/*`
- 目标 (B): `crates/core/src/{interface/{prompt,memory,settings},permission,frozen/memory}.rs` `crates/scene/src/scene/coding.rs` `crates/runtime/src/turn.rs` `crates/{tools,permissions,mcp,hooks,plugin,session,history,skills,task,team,telemetry,compaction}/src/*.rs`
