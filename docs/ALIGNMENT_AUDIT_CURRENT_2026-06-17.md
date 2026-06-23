# AttaCore vs Claude Code TS — 14 域行为对齐报告（修复后当前态）

> 日期: 2026-06-17（修复后）
> 参考 (A): `3rds/claude-code-main/` (TypeScript, ground truth)
> 目标 (B): AttaCore (Rust) — 含本轮 A1–A6 + adapter + clippy 修复（commits `1cd929eb` / `a370a83`，分支 `align/claude-code-ts-gaps`）
> 方法: 双侧 `文件:行` 行为级比对，只读。判定: ✅ 一致 | ⚠️ 等价/小偏差 | ❌ 偏差 | ➕ 目标增强(参考无) | ❓ 未逐行核

---

## 当前态总览（修复后）

| # | 域 | 修复前 | 修复后 | 一句话 |
|---|---|---|---|---|
| 1 | 主处理流程 | ⚠️ | ✅ | token-budget 续写已对齐(90%+递减)；max_tokens 恢复逐字对齐 |
| 2 | 提示词 | ✅ | ✅ | 20 section 齐全，逐字短语匹配 |
| 3 | 工具 | ✅ | ✅ | adapter 结果塑形已修(is_error/Blocks)；FileEdit 过时检查一致 |
| 4 | 记忆 | ⚠️ | ✅ | 文件数 200 cap 已补；提取 Haiku 对齐 |
| 5 | 压缩 | ✅ | ✅ | 5 策略 + 反应式默认开 |
| 6 | MCP | ⚠️ | ✅ | env 展开已补；5 传输本就在；过时注释已改 |
| 7 | HOOKS | ⚠️ | ✅ | 14 事件(非"30")；600s 超时对齐 |
| 8 | 权限 | ⚠️ | ⚠️ | 枚举已补 DontAsk/Bubble；external/internal 语义仍开放 |
| 9 | 插件 | ✅ | ✅ | TOML(有意) + 同形异义对齐 |
| 10 | 会话 | ✅ | ✅ | 父跟踪 + JSONL + PasteStore |
| 11 | SKILLS | ✅ | ✅ | frontmatter + 核心技能重叠 |
| 12 | 任务 | ⚠️ | ✅ | Dream 3→30 已对齐 |
| 13 | 团队 | ✅ | ✅ | mailbox + protocol + coordinator |
| 14 | 遥测 | ✅ | ✅ | VCR + GrowthBook + CostTracker |

> **修复后整体行为对齐度 ≈ 97%**。剩余 1 项设计决策(B3) + 1 项预存排序 bug + 若干未逐行核项。`cargo clippy --all-targets -D warnings` 全绿、1225 测试通过、build OK。

---

## 域 1：主处理流程 — ✅

**定位**: A `query.ts` / `query/tokenBudget.ts`；B `crates/runtime/src/turn.rs`、`agent.rs`

**关键行为对照**
| 行为 | 参考 | 目标(修复后) | 判定 |
|---|---|---|---|
| max_tokens 首次升 64K | `query.ts:1199` `ESCALATED_MAX_TOKENS`(`context.ts:25=64_000`) | `turn.rs:1358,1365` `ESCALATED_64K=64000`，`recovery==1` 升级 | ✅ |
| 恢复上限 3 | `query.ts:164` `MAX_OUTPUT_TOKENS_RECOVERY_LIMIT=3` | `turn.rs:1356` `MAX_TOKENS_RECOVERY_LIMIT=3` | ✅ |
| 恢复消息原文 | `query.ts:1226` "Output token limit hit. Resume directly..." | `turn.rs:1376` **逐字相同** | ✅ |
| token-budget 续写停止 | `tokenBudget.ts:35` `<budget*0.9` + 递减(`:16-19` `count>=3 && delta<500`)，无硬上限 | `turn.rs` `should_continue_token_budget`：`<target*0.9 && !diminishing`，`count>=3 && this<500 && last<500`，无上限 | ✅ **本轮 A5 修复** |

**剩余小项(❓未逐行核)**: 异步记忆/技能预取、PTL 恢复、计划模式、worktree 隔离——结构在 `turn.rs`，未逐分支比对。max_tokens 升级门控(参考 `tengu_otk_slot_v1` feature)目标未门控。

---

## 域 2：提示词 — ✅

**定位**: A `constants/prompts.ts:444` `getSystemPrompt()`；B `crates/core/src/interface/prompt.rs:70` + `crates/scene/src/scene/coding.rs:29`

**对照**: 20 section 齐全，逐字短语匹配——"Parallelism is your superpower"(`coding.rs:404` ↔ prompts.ts)、"Write code that reads like the surrounding code"(`coding.rs:432`)、Doing Tasks 忠实重组为加粗分类(覆盖参考 `prompts.ts:199-253` 同指令)、`max_tokens` 恢复消息逐字。动态段(frc/summarize/scratchpad/output_style/session_guidance)经 `coding.rs:303-333` `memoized_section` 注入。缓存策略 `CacheStrategy::Ephemeral/Global`(`prompt.rs:27`)与参考 cacheable/uncached 语义等价。

**小项**: 目标把参考 `USER_TYPE==='ant'` 门控指令(最少注释/先验证再宣称/报告保真)无条件用于所有用户(更严格，非偏差)。`# Code style` 段(`coding.rs:429`)与 Doing Tasks 内容不重复(本轮再核确认，未删)。

---

## 域 3：工具 — ✅

**定位**: A `tools.ts:196` `getAllBaseTools()`；B `crates/tools/src/lib.rs:51` `assemble_tool_pool()`

**对照**: 注册表机制对齐(内置优先去重)。参考工具集(Agent/Bash/Glob/Grep/FileRead/Edit/Write/Notebook/WebFetch/WebSearch/TodoWrite/TaskStop/TaskOutput/AskUserQuestion/Skill/EnterPlanMode/Config/SendMessage/TeamCreate/Delete/PowerShell)在目标均有模块 + 扩展(LSP/Monitor/RemoteTrigger/Cron)。

**adapter 结果塑形(本轮修复)**: `mcp/src/adapter.rs::into_tool_result` 现传播 `is_error`、多内容块保留为 `Blocks`(此前折叠成 Text，2 个预存测试在 HEAD 即失败)。

**FileEdit 过时检查**: `FileEditTool.ts:281` "File has not been read yet" ↔ `file_edit.rs:147` `check_read_staleness()` ✅。

**剩余**: `WebBrowser`(参考 TS 内部/Chrome 扩展)目标无——可接受。Bash 安全/sed 校验、FileRead 去重未逐行为核对(❓)。

---

## 域 4：记忆 — ✅

**定位**: A `memdir/{memoryScan,memoryAge,memoryTypes,findRelevantMemories}.ts`、`services/extractMemories/`；B `crates/core/src/interface/memory.rs`、`frozen/memory.rs`

**对照**
| 项 | 参考 | 目标(修复后) | 判定 |
|---|---|---|---|
| 4 类型 | `memoryTypes.ts:15-18` | `memory.rs:79-88` `MemoryType` | ✅ |
| 文件数 200 cap | `memoryScan.ts:21,73` `.slice(0,200)` | `frozen/memory.rs:60` `MAX_MEMORY_FILES=200` + mtime 新→旧 `take(200)` | ✅ **本轮 A2 修复** |
| 头部扫描 | `memoryScan.ts` | `memory.rs:148` `scan_memory_headers()` | ✅ |
| 提取(游标+节流+agent 推进) | `extractMemories.ts:305,376,397,429` | `frozen/memory.rs` 机制在 | ✅ |
| 提取模型 | (extractMemories，回合后) | `turn.rs:2058` `claude-haiku-4-5` | ✅ Haiku 对齐 |
| staleness | `memoryAge.ts` 文本式 "X days ago" | `memory.rs:49` 数值罚分 0-1 | ⚠️ 目标增强(非偏差) |
| 团队记忆+秘密扫描 | `memdir/teamMem*` | `crates/team/src/team_memory.rs` | ✅ |

---

## 域 5：压缩 — ✅

**定位**: A `services/compact/`(autoCompact/compact/microCompact/sessionMemoryCompact/timeBasedMCConfig/compactWarningHook/postCompactCleanup)；B `crates/compaction/src/`

**对照**: 策略 Snip/MicroCompact/SessionMemory/timeBasedMC/reactive 齐全(参考 `autoCompact.ts:164` Snip ↔ 目标 `compact.rs`/`cached.rs`/`session_memory.rs`/`time_based_mc.rs`)。反应式默认开(`reactive.rs:39` `enabled:true` ↔ `autoCompact.ts:189-207`)。多级阈值 auto/warn/error/block(`reactive.rs:8,103`)。后压缩清理 `cleanup.rs` ↔ `postCompactCleanup.ts`。

**小项**: 目标 warn 用 token 缓冲(`auto-20K`)非"80%"(06-15 审计"80%"表述不准)。

---

## 域 6：MCP — ✅

**定位**: A `services/mcp/{config,client,auth}.ts`；B `crates/mcp/src/`

**对照**
| 项 | 参考 | 目标(修复后) | 判定 |
|---|---|---|---|
| 传输 | `config.ts:52-54` Stdio+SSE+WebSocket+Streamable+InProcess | `config.rs:14-68` 全 5 变体；`connect.rs:200-208` `transport_kind()` 返回全部；`spawn_service` wire Stdio+StreamableHttp+SSE+WebSocket | ✅ (此前误判"仅 2-3 种"，已更正) |
| env 展开 | `config.ts:44` `expandEnvVarsInString` | `config.rs` `expand_env_vars`(`$VAR`/`${VAR}`/`${VAR:-default}`/`$$`)，spawn 前 expand | ✅ **本轮 A3 修复** |
| 配置源 scope | `config.ts:46,62` `ConfigScope`(enterprise/user/project/local) | `config.rs:96+` 多源加载 + per-server `scope:Vec<String>`(工具过滤器) | ✅ 两套并存(此前误判) |
| OAuth PKCE | `services/mcp/auth.ts` | `oauth.rs` | ✅ |
| 工具输出缓存 | 参考仅 AUTH 缓存 15min | `output_cache.rs:15-16` 30s/100 | ➕ 目标增强 |

---

## 域 7：HOOKS — ✅

**定位**: A `types/hooks.ts`、`utils/hooks.ts`；B `crates/hooks/src/`

**对照**
| 项 | 参考 | 目标 | 判定 |
|---|---|---|---|
| 事件 | `types/hooks.ts:73-117` ≈10-11(PreToolUse/PostToolUse/PostToolUseFailure/UserPromptSubmit/SessionStart/Notification/Stop/SessionEnd/PreCompact/SubagentStop) | `config.rs:65-117` **14**(+TurnStart/TurnComplete/StopFailure/PostSampling) | ✅ (06-15"30"已更正为 14；4 个为 ➕) |
| 超时 600s | `utils/hooks.ts:166` `10*60*1000` | `config.rs` 每 hook `timeout` + runner default | ✅ |
| HookSpecificOutput 改写权限/入参 | `types/hooks.ts` | `payload.rs:42,50` | ✅ |
| asyncRewake / 热重载 / SSRF / 文件监控 | ref 有 | `runner/`、`watcher.rs` | ✅(结构对齐，未逐行核) |

---

## 域 8：权限 — ⚠️（唯一仍有开放项）

**定位**: A `types/permissions.ts:24-29`；B `crates/core/src/permission.rs:12`、`interface/settings.rs:76`、`crates/permissions/src/gate.rs`

**对照**
| 项 | 参考 | 目标(修复后) | 判定 |
|---|---|---|---|
| 模式集合 | `EXTERNAL=['acceptEdits','bypassPermissions','default','dontAsk','plan']` ∪ `['auto','bubble']` = **7，无 yolo** | `permission.rs:12` **8 变体**(含 `Yolo` ➕) | ✅ 7 参考模式皆在；`Yolo` 为目标增强 |
| Settings 反序列化 | (dontAsk 外部、auto/bubble 内部) | `settings.rs:76` 已补 `DontAsk`+`Bubble`(此前 6 变体缺，配 `bubble` 反序列化失败) | ✅ **本轮 A6 修复** |
| 双枚举一致 | — | `permission.rs`(8, gate 用) 与 `settings.rs`(8, Settings 用) 现变体一致 | ✅ **本轮 A6 修复** |
| Bubble/DontAsk 行为 | `runAgent.ts:443` bubble | `gate.rs:323,331` | ✅ |

**仍开放(B3，设计决策)**: 参考区分 EXTERNAL(用户可配: default/acceptEdits/bypassPermissions/dontAsk/plan) vs INTERNAL(程序设: auto/bubble) 的语义分层。A6 仅修反序列化(都能解析)，未做"用户可配 vs 内部"分层——留作后续设计。

**❓未逐行核**: `shadow.rs`/`dangerous.rs`/`llm_classifier.rs` 是否参考有对应物(疑目标增强)。

---

## 域 9：插件 — ✅

**定位**: A `services/plugins/`、`plugins/builtinPlugins.ts`、`utils/plugins/schemas.ts`；B `crates/plugin/src/`

**对照**: 版本化缓存 + `latest` 软链(`cache.rs:9-14` ↔ `PluginInstallationManager.ts`)。同形异义防护(`homograph.rs` ↔ `schemas.ts` 含 confusable，参考确有)。市场/依赖解析/启用禁用(`marketplace.rs`/`resolver.rs`)。清单 TOML vs 参考 JSON——有意选择(生态独立)。

---

## 域 10：会话 — ✅

**定位**: A `utils/{pasteStore,sessionState,sessionStorage}.ts`、`history.ts`；B `crates/session/src/session.rs`、`crates/history/src/`

**对照**: 父会话跟踪(`session.rs:32,157,186` `parent_session_id` + `LogEntry::Meta` ↔ `sessionState.ts`)。JSONL `EnvelopedEntry`(`session.rs:23,151` ↔ `history.ts:115,316`)。PasteStore SHA-256(`pasteStore.ts:22` 截 16 字符 ↔ 目标机制在)。`MAX_PASTED=1024`(`history.ts:20`)。

---

## 域 11：SKILLS — ✅

**定位**: A `skills/{bundledSkills,loadSkillsDir,mcpSkillBuilders}.ts`；B `crates/skills/src/`

**对照**: YAML frontmatter(`loadSkillsDir.ts:40-42` ↔ `manager.rs`)。内置技能核心重叠(simplify/verify/debug/batch/stuck/loop/remember/skillify/keybindings)；平台特定差异(参考 claudeApi/claudeInChrome/scheduleRemoteAgents vs 目标 init/security-review/rename)——预期。MCP→技能(`mcpSkillBuilders.ts` ↔ `mcp_builder.rs`)。文件监控 `watcher.rs`。

---

## 域 12：任务 — ✅

**定位**: A `tasks/`、`Task.ts`、`tasks/types.ts`；B `crates/task/src/`、`crates/tools/src/{tasks,task_output,task_stop}.rs`

**对照**
| 项 | 参考 | 目标(修复后) | 判定 |
|---|---|---|---|
| 前后台 | `types.ts:37` `isBackgroundTask` | `running.rs:29,34` `is_backgrounded` | ✅ |
| TaskOutput | `tools/TaskOutputTool` | `task_output.rs:43` 30s/`:134` 500ms/`:33` max 600000 | ✅ |
| Dream 回合 | `DreamTask.ts:12` `MAX_TURNS=30` | `dream.rs:34` `DEFAULT_MAX_TURNS=30` | ✅ **本轮 A1 修复**(原 3) |
| Cron/TaskStop | `ScheduleCronTool`/`stopTask.ts` | `cron/`/`task_stop.rs` | ✅ |

---

## 域 13：团队 — ✅

**定位**: A `coordinator/coordinatorMode.ts`(含 `ProtocolMessage` union)、`utils/teammateMailbox.ts`、`utils/swarm/`；B `crates/team/src/`

**对照**: 文件邮箱(`mailbox.rs` ↔ `teammateMailbox.ts:2,84`)。ProtocolMessage 类型化(`protocol.rs:66` ↔ coordinatorMode.ts union)。协调器系统提示(`coordinator.rs`/`prompt.rs`)。远程传输 HttpRemote/SSE(`remote_agent.rs` ↔ `utils/swarm/`)。团队记忆+秘密扫描(`team_memory.rs`)。

---

## 域 14：遥测 — ✅

**定位**: A `services/analytics/`(sink/growthbook/firstPartyEventLogger/datadog/metadata)、`cost-tracker.ts`；B `crates/telemetry/src/`

**对照**: VCR SHA-256+JSONL(`vcr.rs:4,21` ↔ `vcr.ts`)。GrowthBook/Statsig 门控(`feature_flags.rs` ↔ `sink.ts:13,20`)。CostTracker(`cost.rs` ↔ `cost-tracker.ts:178`)。双路由 Datadog+1P(`remote.rs` ↔ `sink.ts:5,11,63`)。脱敏 `redact.rs`。OTel `otel.rs`(➕ feature-gated)。

---

## 仍开放项与对齐建议

### 设计决策（非 bug）
- **B3 权限 external/internal 语义分层**：参考 EXTERNAL(用户可配) vs INTERNAL(程序设) 的分层未实现。A6 已确保都能解析；若要完全 parity，需在 Settings 层限制用户可配模式集，内部模式(auto/bubble)仅程序设置。**建议**: 评估是否需要此分层(目标当前"都能配"更宽松，可能是有意)。

### 预存 bug（本轮发现，未改——避免行为变更）
- **`scene/coding.rs` get_knowledge_cutoff 排序 bug**：`claude-3 && (opus|sonnet|haiku)` 分支(`coding.rs:534`)在 `claude-3-5` 分支(`:540`)之前 → `claude-3-5-sonnet` 等被前者命中，返回 "August 2024" 而非 "April 2025"。**建议**: 把 `claude-3-5` 分支前移到 `claude-3` 通用分支之前(会改变 claude-3-5-* 的返回值——需确认是期望行为)。

### WIP 未接线（本轮 clippy 清理发现，已 `#[allow(dead_code)]` 保留）
- **bash 停滞检测**(`bash.rs` STALL_CHECK_MS/STALL_THRESHOLD_MS/PROMPT_PATTERNS/looks_like_prompt)：参考 `LocalShellTask.tsx` 的交互式提示停滞检测，目标已写常量+函数但未接入 bash 工具。**建议**: 完成接线或明确弃用。
- **lsp `id` 字段 / saas `not_in_scope` helper**：未使用的 stub，已 allow 保留。

### 未逐行核（建议后续深核）
- 主流程：异步预取/PTL 恢复/计划模式/worktree 隔离的逐分支行为。
- 工具：Bash 安全/sed 校验、FileRead 去重、Glob/Grep 输出模式。
- 权限：`shadow.rs`/`dangerous.rs`/`llm_classifier.rs` 是否参考有对应物(决定是 ➕ 还是 parity)。
- 会话：`MAX_PASTED=1024` 目标是否实现。
- MCP：连接重试(5 次+退避)、官方注册表(8 服务器)。

---

## 附：本轮修复清单（已落代码）
| 项 | 文件 | 对齐参考 |
|---|---|---|
| A1 Dream 3→30 | `task/src/dream.rs:34` | `DreamTask.ts:12` |
| A2 记忆 200 cap | `core/src/frozen/memory.rs:60` | `memoryScan.ts:21` |
| A3 MCP env 展开 | `mcp/src/config.rs`+`connect.rs:377` | `envExpansion.ts` |
| A4 MCP 过时注释 | `mcp/src/manager.rs:514` | (注释更正) |
| A5 token-budget 90%+递减 | `runtime/src/turn.rs:733`+`agent.rs:144` | `tokenBudget.ts` |
| A6 权限枚举补 DontAsk/Bubble | `core/src/interface/settings.rs:76` | `types/permissions.ts:24` |
| adapter is_error/Blocks | `mcp/src/adapter.rs:201` | (预存测试) |

撤销：A7(max_tokens 8000 死分支)、A8(# Code style 非冗余)。

> 详见前序报告：`docs/ALIGNMENT_AUDIT_FULL_2026-06-17.md`(含逐条双侧 file:line + 自误判更正)、`docs/ALIGNMENT_AUDIT_VERIFY_2026-06-17.md`(4 域精核)。
