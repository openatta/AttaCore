# AttaCore vs Claude Code TS — 行为对齐核对报告（独立验证）

> 核对日期: 2026-06-17
> 参考实现 (A): `3rds/claude-code-main/` (TypeScript)
> 目标实现 (B): AttaCore (Rust, 17 crates + daemon)
> 方法: 对照 06-15 自报式审计，独立做带**双侧 file:line** 的行为级核对（4 个高影响域）
> 状态标记: ✅ 一致 | ⚠️ 等价/小偏差 | ❌ 行为偏差 | ➕ 目标增强(参考无) | ❓ 未验证

---

## 项目状态（截至 2026-06-17）

- **构建**: `cargo check --workspace --lib` 通过，仅警告（unused import / doc-comment），无错误。
- **未提交 WIP**: 自 06-14 提交 `40d9a2b0` 起，53 文件改动 / +7022 / -358。新增未跟踪源文件（`shadow.rs`、`dangerous.rs`、`llm_classifier.rs`、`session_memory.rs`、`agent_tool.rs`、`bash/safety.rs`、`bash/sed_validate.rs`、`task_output.rs`、`task_stop.rs`、`remote_trigger.rs` 等）正是 06-15 审计所描述的特性——即审计文档反映的是当前工作树状态。
- **既有审计性质**: `docs/ALIGNMENT_AUDIT_FINAL_2026-06-15.md` 全程标 ✅、**未引用任何参考侧代码位置**，属自报式清单，其"对齐"声明此前未被独立验证。本报告即针对其高影响域做 ground-truth 核对。

---

## 比较范围
- 参考实现 (A): `3rds/claude-code-main/src/`
- 目标实现 (B): `crates/`
- 核对域: 系统提示词 · 工具生态与关键工具行为 · 权限系统 · 主轮次循环

## 概要

| 维度 | 评级 | 说明 |
|------|------|------|
| 系统提示词 | ✅ | 20 section 齐全，含逐字短语匹配；Doing Tasks 忠实重组；动态 section 已注入。小瑕疵：# Code style 冗余、ant 门控指令未门控 |
| 工具生态 | ✅ | 注册表机制对齐；工具集覆盖参考集 + 扩展；FileEdit 过时检查行为一致。未逐工具全量核对 |
| 权限系统 | ⚠️ | 参考 7 模式全部存在；但 `Yolo` 被误标为"对齐"(实为目标增强)；存在两个不一致的 `PermissionMode` 枚举 |
| 主轮次循环 | ⚠️ | `max_tokens` 恢复**逐字对齐**(含恢复消息原文)；但 `token-budget` 续写停止逻辑**行为偏差**(100%+硬上限10 vs 参考 90%+收益递减无上限) |

> 总体：能力覆盖度与 06-15 审计的 ~92% 大致相符；但**行为精度被高估**——把目标侧自创行为(10 次上限、Yolo)当作参考对齐，并漏掉了枚举不一致。核心提示词/工具/`max_tokens` 恢复确属高质量对齐；token-budget 续写与权限枚举是两处实质行为差异。

---

## 域 1：系统提示词 — ✅

### 定位
- 参考组装入口: `3rds/.../src/constants/prompts.ts:444` `getSystemPrompt()`（静态 cacheable 段 + 动态 registry 段，prompts.ts:491-576）
- 目标组装入口: `crates/core/src/interface/prompt.rs:70` `assemble_prompt()` → 场景骨架 `crates/scene/src/scene/coding.rs:29` `build_system_prompt()`

### 逐 section 核对
| Section | 参考 | 目标 | 判定 | 说明 |
|---|---|---|---|---|
| Identity | `prompts.ts:175` | `coding.rs:343` `identity_block` | ✅ | 安全测试指引 + URL 猜测警告匹配；品牌 Claude Code→AttaCode(预期) |
| System | `prompts.ts:186` | `coding.rs:358` `SYSTEM_INFO_BLOCK` | ✅ | hooks/system-reminder/自动压缩/提示注入标记一致 |
| Style | — | `coding.rs:369` `STYLE_BLOCK` | ✅ | |
| System context | `prompts.ts:131` | `coding.rs:379` `SYSTEM_CONTEXT_BLOCK` | ✅ | |
| Doing tasks | `prompts.ts:199-253` | `coding.rs:385` `DOING_TASKS_BLOCK` | ✅ | 参考 `prependBullets` 同为项目符号；目标重组为加粗分类，覆盖相同指令(范围纪律/OWASP/过度工程/最少注释/向后兼容/先验证再宣称/报告保真/`/issue`) |
| Parallelism | `prompts.ts`(短语) | `coding.rs:404` `PARALLELISM_BLOCK` | ✅ | "Parallelism is your superpower" **逐字匹配** |
| Sub-agents | `prompts.ts:316` | `coding.rs:415` `SUB_AGENTS_BLOCK` | ✅ | |
| Code style | (并入 Doing Tasks `codeStyleSubitems`) | `coding.rs:429` `CODE_STYLE_BLOCK` | ⚠️ | 目标单列 `# Code style` 段，参考把同内容折叠进 Doing Tasks——冗余但内容已覆盖 |
| Actions | `prompts.ts:255` | `coding.rs:435` `ACTIONS_BLOCK` | ✅ | 可逆性/爆炸半径一致 |
| Using tools | `prompts.ts:269` | `coding.rs:449` `TOOL_USAGE_BLOCK` | ✅ | |
| Tone & style | `prompts.ts:430` | `coding.rs:460` `TONE_STYLE_BLOCK` | ✅ | `file_path:line_number`、无 emoji 一致 |
| 动态: frc/summarize/scratchpad/output_style/session_guidance/memory/language/env/mcp | `prompts.ts:491-555` | `coding.rs:303-333` `memoized_section` | ✅ | 全部以动态段注入；缓存策略 `CacheStrategy::Ephemeral/Global`(`prompt.rs:27`) 与参考 cacheable/uncached 语义等价 |

### 小偏差
- 目标把参考中 `USER_TYPE==='ant'` 门控的指令（最少注释、先验证再宣称、报告保真、`/issue` 引导）**无条件应用于所有用户**。语义更严格，非偏差但行为不同。
- 参考 `ant_model_override`(prompts.ts:496)、`token_budget` 段文本(prompts.ts:548)、`numeric_length_anchors` 等 ant/feature 门控段，目标未确认存在（Atta 非 Ant，多数可忽略）。

**小结**: 缺失 0 / 偏差 1(冗余) / 一致 19。**总判定 ✅**。

---

## 域 2：工具生态与关键工具行为 — ✅

### 定位
- 参考注册: `3rds/.../src/tools.ts:196` `getAllBaseTools()`
- 目标注册: `crates/tools/src/lib.rs:51` `assemble_tool_pool()`（注释标注 TS parity: `assembleToolPool()`）

### 工具清单
参考 `getAllBaseTools` 集合：`AgentTool, TaskOutputTool, BashTool, GlobTool, GrepTool, ExitPlanMode, FileReadTool, FileEditTool, FileWriteTool, NotebookEditTool, WebFetchTool, TodoWriteTool, WebSearchTool, TaskStopTool, AskUserQuestionTool, SkillTool, EnterPlanModeTool, ConfigTool, SendMessageTool, TeamCreate/Delete, PowerShellTool`。

目标模块覆盖上述全部 + 扩展（`lsp, monitor, remote_trigger, cron, schedule_wakeup, sleep, structured_output, tool_search, push_notification, worktree*`；`agent_tool` 在 `crates/runtime/`）。`assemble_tool_pool` 内置优先去重（BTreeMap，builtin 后插覆盖 mcp）与参考一致。

- ➕ 目标增强: LSP / Monitor / RemoteTrigger / Cron 等（参考或无或不同）
- ❌ 参考有目标无: `WebBrowser`（参考 TS 内部工具，依赖 Chrome 扩展；06-15 审计已承认）— 可接受

### 关键工具行为核对

**FileEditTool — 过时检查（read-before-edit）**
| 行为点 | 参考 | 目标 | 判定 |
|---|---|---|---|
| 编辑前必须先 Read | `FileEditTool.ts:281` "File has not been read yet. Read it first before writing to it." | `crates/tools/src/file_edit.rs:147` `check_read_staleness()` + `:139` "file must have been read" | ✅ 行为一致 |
| 读时间戳失效 | `FileEditTool.ts:519` 更新读时间戳 | `file_edit.rs:750` 预填读缓存使过时检查通过 | ✅ 等价 |

**BashTool**：目标 `crates/tools/src/bash/{safety.rs, sed_validate.rs, sandbox.rs}` 结构与参考 BashTool 的校验/安全职责对应；本次未逐分支核对（❓）。

> 未对全部 47+ 工具逐行为核对；注册表机制 + FileEdit 深核确认移植忠实。**总判定 ✅（注册表与抽样行为对齐；逐工具全量审计未做）**。

---

## 域 3：权限系统 — ⚠️

### 参考侧定位（ground truth）
- 模式定义: `3rds/.../src/types/permissions.ts:24-29`
  - `EXTERNAL_PERMISSION_MODES = ['acceptEdits','bypassPermissions','default','dontAsk','plan']`
  - `InternalPermissionMode = ExternalPermissionMode | 'auto' | 'bubble'`
  - **参考共 7 模式：default / acceptEdits / bypassPermissions / dontAsk / plan / auto / bubble。无 `yolo`。**

### 目标侧定位
- 活跃枚举: `crates/core/src/permission.rs:12` `enum PermissionMode` — 8 变体: `Default, Plan, AcceptEdits, BypassPermissions, Auto, DontAsk, Bubble, Yolo`
- 重复枚举: `crates/core/src/interface/settings.rs:76` `enum PermissionMode` — **仅 6 变体: `Default, AcceptEdits, BypassPermissions, Plan, Auto, Yolo`（缺 `DontAsk`, `Bubble`）**
- 闸门: `crates/permissions/src/gate.rs`（`use base::permission::PermissionRule`，即用 8 变体枚举）

### 10 项声称逐条验证
| # | 声称 | 参考 | 目标 | 判定 | 说明 |
|---|---|---|---|---|---|
| 1 | 8 模式全对齐 | `types/permissions.ts:24` (7 模式) | `permission.rs:12` (8) | ⚠️ | 7 参考模式皆在；`Yolo` 参考无 → ➕ 目标增强，**06-15 审计误标为"对齐"** |
| 2 | LLM 分类器(Auto) | `auto` 内部模式存在；具体实现未核 | `llm_classifier.rs`(570 行) | ❓ | 参考 `auto` 是否真用 LLM 分类器未独立确认 |
| 3 | 影子规则检测 | 未在参考定位到 | `shadow.rs`(582 行) | ❓ | 可能目标增强；未确认参考有对应物 |
| 4 | 危险规则检测 | 未在参考定位到 | `dangerous.rs`(517 行) | ❓ | 同上 |
| 5 | 规则格式 `ToolName(content)` | `gate.rs` 注释自称 TS parity | `ruleset.rs` | ❓ | 未逐字核对 |
| 6 | 路径安全 Unicode/符号链接 | 参考有 normalizePath/symlink | `path_safety.rs`(712 行) | ❓ | 未逐项核对 |
| 7 | Bubble 模式 | `runAgent.ts:443` `'bubble'` | `gate.rs:331` `PermissionMode::Bubble` | ✅ | 参考确有 bubble（子代理用）；目标已实现 |
| 8 | DontAsk | `types/permissions.ts:24` | `gate.rs:323` `DontAsk=>Deny` | ✅ | 参考确有；目标已实现 |

### ❗ 实质问题：两个不一致的 `PermissionMode` 枚举
- 活跃枚举 `permission.rs:12`（8 变体，gate.rs 使用）与 `settings.rs:76`（6 变体，缺 `DontAsk`/`Bubble`）**变体集合不一致**。
- 若配置反序列化走 `settings.rs` 枚举，`dontAsk`/`bubble` 模式将**无法从配置解析**。这是真实的不一致/坏味道，06-15 审计未提及。

**小结**: 一致 2 / 未验证 4 / 偏差 1(Yolo 误标) + 1 枚举不一致。**总判定 ⚠️**。

---

## 域 4：主轮次循环 — ⚠️

### 定位
- 参考主循环: `3rds/.../src/query.ts`（68KB；`QueryEngine.ts`）
- 目标主循环: `crates/runtime/src/turn.rs`（2088 行）、`agent.rs`

### ✅ `max_tokens` 恢复 — 逐字对齐
| 行为点 | 参考 | 目标 | 判定 |
|---|---|---|---|
| 首次升级到 64K | `query.ts:1199` `ESCALATED_MAX_TOKENS`(`utils/context.ts:25 = 64_000`) | `turn.rs:1358,1365` `ESCALATED_64K=64000`，首次 `recovery==1` 升级 | ✅ |
| 恢复次数上限 3 | `query.ts:164` `MAX_OUTPUT_TOKENS_RECOVERY_LIMIT = 3` | `turn.rs:1356` `MAX_TOKENS_RECOVERY_LIMIT: u32 = 3` | ✅ |
| 恢复消息原文 | `query.ts:1226` "Output token limit hit. Resume directly — no apology, no recap of what you were doing. Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces." | `turn.rs:1376-1380` **逐字相同** | ✅ |
| 门控 | `query.ts:1199` 受 `tengu_otk_slot_v1` feature flag 门控 | `turn.rs:1365` **未门控**（无条件升级） | ⚠️ |
| 后续重试 max_tokens | `query.ts:1235` `maxOutputTokensOverride: undefined`（回模型默认） | `turn.rs:1370` 设地板 `8000` | ⚠️ |

### ❌ `token-budget` 续写停止逻辑 — 行为偏差
| 维度 | 参考 (`query/tokenBudget.ts`) | 目标 (`turn.rs:733-779`) | 判定 |
|---|---|---|---|
| 继续条件 | `turnTokens < budget * 0.9`（**90%** 阈值，tokenBudget.ts:35） | `accumulated_output_tokens < target`（**100%**） | ❌ |
| 硬上限 | **无**；靠 90% + 收益递减自然停止 | `token_budget_continuation_count < 10`（**硬上限 10**，turn.rs:739,767） | ❌ |
| 收益递减早停 | `isDiminishing = continuationCount>=3 && delta<500`（tokenBudget.ts:16-19） | **无此启发式** | ❌ |
| nudge 消息 | `getBudgetContinuationMessage(pct, turnTokens, budget)` | "Continue working. Used X/Y output tokens (Z remaining)"（turn.rs:744） | ⚠️ |

> 06-15 审计将"最多 10 次自动延续"作为**参考对齐**陈述——**不正确**。参考无硬上限，靠 90% 阈值 + 收益递减停止；目标的 10 次硬上限与 100% 停止阈值是**目标侧自创行为**。差异使目标在长预算场景下会比参考更早停止（10 次）或更晚停止（100% vs 90%），取决于场景。

**小结**: max_tokens 恢复 ✅(含原文) / token-budget 续写 ❌(3 处偏差)。**总判定 ⚠️**。

---

## 建议

- **P1（行为偏差，应修）**
  - `token-budget` 续写：把停止阈值从 100% 改为 `budget*0.9`，去掉硬上限 10，加入收益递减启发式（`continuationCount>=3 && delta<500` 停止），对齐 `query/tokenBudget.ts`。或若有意保留 10 上限，应在审计中标注为"目标侧加严"而非"对齐"。
  - 权限枚举不一致：统一 `PermissionMode` 为单一枚举（以 `permission.rs` 8 变体为准），删除或修正 `settings.rs:76` 的 6 变体副本，确保配置能解析 `dontAsk`/`bubble`。
- **P2（表述修正）**
  - 06-15 审计把 `Yolo`、"`最多10次`续写"标为参考对齐——应在文档中更正为"目标侧增强/加严"。
- **P3（补验证）**
  - `shadow.rs` / `dangerous.rs` / `llm_classifier.rs` 的行为是否在参考有对应物，需独立确认（本次未核）；若参考无，应标 ➕。
  - `max_tokens` 升级的 `tengu_otk_slot_v1` feature 门控是否需要引入。
  - 工具域逐工具行为核对（Bash 安全/sed 校验、FileRead 去重等）尚未做。
- **安全观察（本审计范围外但已发现）**：`turn.rs:1431` `dangerously_disable_sandbox: true` 在 `execute_tool_inner` 中硬编码——Bash 工具的沙箱被默认关闭，与参考的沙箱执行语义可能不符，建议单独评估。

---

## 附：核对涉及文件
- 参考 (A): `constants/prompts.ts` · `tools.ts` · `query.ts` · `query/tokenBudget.ts` · `utils/context.ts` · `types/permissions.ts` · `tools/FileEditTool/FileEditTool.ts`
- 目标 (B): `crates/core/src/interface/{prompt,settings}.rs` · `crates/core/src/permission.rs` · `crates/scene/src/scene/coding.rs` · `crates/tools/src/{lib,file_edit}.rs` · `crates/permissions/src/{gate,shadow,dangerous,llm_classifier,path_safety,ruleset}.rs` · `crates/runtime/src/turn.rs`
