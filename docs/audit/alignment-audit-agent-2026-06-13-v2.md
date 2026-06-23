# 对齐审计报告 — `agent` (再审计) — 2026-06-13

## 比较范围
- **参考实现 (A)**: `3rds/claude-code-main/src/` (TypeScript)
- **目标实现 (B)**: `AttaCore/crates/` (Rust)
- **上次审计**: 2026-06-13 (13 项 P0/P1/P2 修复前)
- **本次审计**: 修复后 re-audit

## 概要

| 维度 | 评级 | 说明 |
|------|------|------|
| 能力对齐 | ✅→⚠️ | 13 项已修复, 但仍有 ~15 项中轻度差距 |
| 行为对齐 | ✅→⚠️ | 主流程高度一致, 预取/缓存/通知等高级行为缺失 |
| 提示词对齐 | ✅ | 记忆/协调器/记忆选择提示词已逐字对齐 |
| 流程对齐 | ✅→⚠️ | Turn loop 结构对齐, 但缺少异步预取和令牌预算 |

## 代码位置清单

### 参考实现 (A)
| 文件 | 子系统 |
|------|--------|
| `query.ts` | 主 query loop |
| `Tool.ts` / `tools.ts` | 工具定义/注册 |
| `skills/loadSkillsDir.ts` / `bundledSkills.ts` | 技能加载 |
| `memdir/memdir.ts` / `memoryTypes.ts` / `findRelevantMemories.ts` | 记忆系统 |
| `services/compact/compact.ts` / `microCompact.ts` | 压缩 |
| `services/mcp/useManageMCPConnections.ts` | MCP 连接管理 |
| `services/vcr.ts` | VCR 夹具 |
| `coordinator/coordinatorMode.ts` | 协调器 |
| `plugins/builtinPlugins.ts` / `services/plugins/pluginOperations.ts` | 插件 |
| `state/AppStateStore.ts` / `assistant/sessionHistory.ts` | 会话/状态 |
| `services/analytics/growthbook.ts` | 特性开关 |

### 目标实现 (B)
| 文件 | 子系统 |
|------|--------|
| `crates/runtime/src/turn.rs` / `streaming.rs` | Turn loop |
| `crates/core/src/tool.rs` / `crates/tools/src/*` | 工具 |
| `crates/skills/src/manager.rs` / `crates/core/src/frozen/frontmatter.rs` | 技能 |
| `crates/core/src/interface/memory.rs` / `crates/core/src/memory.rs` | 记忆 |
| `crates/compaction/src/compact.rs` | 压缩 |
| `crates/mcp/src/manager.rs` | MCP |
| `crates/telemetry/src/vcr.rs` | VCR |
| `crates/team/src/prompt.rs` / `coordinator.rs` | 协调器 |
| `crates/plugin/src/*` (6 files) | 插件 |
| `crates/session/src/session.rs` | 会话 |
| `crates/core/src/features.rs` | 特性开关 |

## 详细发现

### ✅ 已确认一致的修复项 (13 项)

| # | 项目 | 判定 |
|---|------|------|
| 1 | 记忆提示词 XML 格式 | ✅ 逐字对齐 TYPES/WHAT_NOT_TO_SAVE/WHEN_TO_ACCESS/TRUSTING_RECALL |
| 2 | 内置技能注册 | ✅ `register_bundled()` + Builder 集成 |
| 3 | VCR 自动检测 | ✅ `CARGO_TEST_RUNNER` / `ATTA_VCR_AUTO_DETECT` |
| 4 | 技能前端解析 | ✅ 委托到 `frozen::frontmatter::parse_skill_file` (11/16 字段) |
| 5 | MCP 指数退避重连 | ✅ MAX_RETRIES=5, INITIAL=1s, MAX=30s, ±25% jitter |
| 6 | 工具结果预算 | ✅ `enforce_tool_result_budget()` 50KB/条, 500KB/总计 |
| 7 | 停止钩子 | ✅ `HookEvent::Stop` 在 turn 完成前调用 |
| 8 | 动态技能发现 | ✅ `discover_for_paths()` |
| 9 | 协调器系统提示词 | ✅ 6 段完整提示词 |
| 10 | MCP 通知监听 | ✅ `McpNotification` + `McpNotificationHandler` |
| 11 | LLM 记忆检索 | ✅ `select_memories_with_llm()` |
| 12 | SessionMemory 压缩 | ✅ `session_memory_extract()` |
| 13 | 插件市场 + 解析器 + 缓存 + CLI | ✅ 4 个新模块 |
| 14 | 特性开关 | ✅ `FeatureFlags` + 编译期 `cfg` |

---

### ⚠️ 新发现的差异 (修复后仍然存在)

#### 技能 (Skills)

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `loadSkillsDir.ts:877` — `.claude/skills` | `manager.rs:182` — `.skills` / `skills` | **目录名不匹配** — B 永远不会找到 A 路径下的技能 | ❌ P0 |
| `loadSkillsDir.ts:876` — 遍历上限为 CWD | `manager.rs:206` — 遍历到文件系统根 | 无 CWD 上限 | ⚠️ P1 |
| `loadSkillsDir.ts:185-264` — 16 个 frontmatter 字段 | `frontmatter.rs:85-167` — 11 个字段 | 缺失: hooks, agent, displayName, effort, shell | ⚠️ P1 |
| `bundledSkills.ts:131` — 安全文件提取 (O_NOFOLLOW) | B: ❌ 缺失 | 内置技能不能携带引用文件 | ⚠️ P2 |
| `loadSkillsDir.ts:997` — `activateConditionalSkillsForPaths()` | B: ❌ 未接入 SkillManager | 条件技能激活缺失 | ⚠️ P2 |

#### 回合循环 (Turn Loop)

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `query.ts:301-304` — 记忆预取 (异步启动) | B: ❌ 缺失 | 记忆在模型流传输时异步预取 | ⚠️ P1 |
| `query.ts:331-335` — 技能发现预取 (异步启动) | B: ❌ 缺失 | 技能发现同样异步预取 | ⚠️ P1 |
| `query.ts:1308-1355` — 令牌预算继续 | `turn.rs:298-317` — 美元预算 | A 可按 token 预算动态继续/停止; B 仅有美元硬限制 | ⚠️ P1 |
| `query.ts:440-447` — 上下文折叠 (CONTEXT_COLLAPSE) | `compact.rs` — ❌ 缺失策略 | A 有 git-log 重放的独立策略 | ⚠️ P2 |
| `query.ts:1580-1590` — 排队命令/附件注入 | B: ❌ 缺失 | 通知和提示不会在每轮被注入 | ⚠️ P2 |

#### 压缩 (Compaction)

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `compact.ts:145-200` — `stripImagesFromMessages()` | B: ❌ 缺失 | 压缩前不移除图片 | ⚠️ P1 |
| `microCompact.ts:267-270` — 基于时间的微压缩缓存 | B: ❌ 缺失 | CACHED_MICROCOMPACT 功能门控 | ⚠️ P2 |
| `compact.rs:534` — SessionMemory 硬编码时间戳 | `"2026-06-13T00:00:00Z"` | 使用静态字符串而非实际时间 | ⚠️ P2 |

#### 工具 (Tools)

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `tools.ts:193-251` — ~45 个工具 | B: ~35 个工具 | 缺失: WebBrowser, LSP, TerminalCapture, Snip, Monitor, PushNotification, Workflow 等 | ⚠️ P1 |
| `Tool.ts:429-472` — 10+ 个内省方法 | B: tool.rs 缺失 | isSearchOrReadCommand, isOpenWorld, requiresUserInteraction, alwaysLoad, maxResultSizeChars 等 | ⚠️ P1 |
| `tools.ts:345-367` — `assembleToolPool()` | B: ❌ 缺失 | 无内置+MCP 工具去重组合 | ⚠️ P1 |
| B: `impls.rs` vs 专用模块 | 工具名重复 ("Read"/"Edit" 等) | 注册冲突风险 | ⚠️ P2 |

#### MCP

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `useManageMCPConnections.ts` — 断连重试 | `manager.rs` — 仅初始连接重试 | A 在传输关闭时重试; B 仅在 `connect_all` 期间重试 | ⚠️ P1 |
| `useManageMCPConnections.ts:507-571` — 通道通知 | B: ❌ 缺失 | Chat/GitHub 通道通知 | ⚠️ P2 |
| `manager.rs:62` — `McpServerState` `#[allow(dead_code)]` | 已定义但未使用 | 重连状态未反馈到 server_states | ⚠️ P2 |

#### VCR

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `vcr.ts:39-86` — `withFixture<T>()` 通用夹具 | B: ❌ 缺失 | A 可对任何数据类型用 VCR; B 只限 Model 级别 | ⚠️ P2 |

#### 记忆 (Memory)

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `memdir.ts:288` — MEMORY.md 入口内容注入 | `build_memory_prompt()` — 仅指令文本 | B 不读取/截断/追加 MEMORY.md 内容到提示词 | ⚠️ P1 |
| `findRelevantMemories.ts:99` — Sonnet 模型 | `memory.rs:561` — Haiku 硬编码 | 记忆选择应使用更强模型 | ⚠️ P1 |
| `findRelevantMemories.ts:23` — 工具感知过滤 | `memory.rs` — 缺失 | 提示词中无"最近使用的工具"上下文 | ⚠️ P2 |
| `findRelevantMemories.ts:47` — `alreadySurfaced` 去重 | B: ❌ 缺失 | 已浮现的记忆不会被重新选择 | ⚠️ P2 |
| `memdir.ts:419-450` — KAIROS/team 多模式 | B: ❌ 缺失 | 仅单用户模式 | ⚠️ P2 |

#### 插件 (Plugins)

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `pluginOperations.ts:1089` — 作用域系统 (user/project/local/managed) | B: ❌ 缺失 | CLI 命令无作用域概念 | ⚠️ P1 |
| `pluginOperations.ts` — 策略强制 | B: ❌ 缺失 | 无 `isPluginBlockedByPolicy` | ⚠️ P1 |
| `PluginInstallationManager.ts` — 后台协调 | B: ❌ 缺失 | 无声明式与实际安装的协调 | ⚠️ P1 |
| `builtinPlugins.ts` — 内置插件注册表 + isAvailable | B: ❌ 缺失 | 无内置插件概念 | ⚠️ P2 |
| `cli.rs:45` — "实际下载/提取为占位符" | 占位符 | 无实际下载管线 | ⚠️ P2 |

#### 特性开关 (Feature Flags)

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `growthbook.ts:1156` — 远程 eval + A/B 实验 + 用户定向 | `features.rs:144` — 6 个静态布尔字段 | A 是全功能远程平台; B 是简单的静态开关 | ⚠️ P2 (复杂度适当) |

#### 协调器 (Coordinator)

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `coordinatorMode.ts:80-109` — `getCoordinatorUserContext()` | B: ❌ 缺失 | 工人工具上下文未提供 | ⚠️ P1 |
| `coordinatorMode.ts:111-369` — ~260 行详细提示词 | `prompt.rs` — ~150 行精简版 | B 覆盖了所有 6 段但内容精简 | ⚠️ 等价 |

---

### ✅ 确认一致的关键项

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| `query.ts:307` while(true) | `turn.rs:116` loop {} | 主循环结构 |
| `query.ts:659` callModel | `turn.rs:142-156` model.stream | 模型调用 |
| `query.ts:1366` runTools | `streaming.rs:42` execute_stream | 两阶段流+工具 |
| `compact.ts` 多策略 | `compact.rs` 5 策略 | Snip/MicroCompact/CollapseContext/FullCompact/SessionMemory |
| `memoryTypes.ts` TYPES 等 | `memory.rs` 逐字对齐 | 所有 4 个提示词段 |
| `memdir/findRelevantMemories.ts` | `memory.rs` select_memories_with_llm | LLM 记忆检索 |
| `useManageMCPConnections.ts` 退避 | `manager.rs` connect_with_retry | 指数退避重连 |
| `vcr.ts` shouldUseVCR | `vcr.rs` env_config | 测试模式自动检测 |
| `coordinatorMode.ts` getCoordinatorSystemPrompt | `prompt.rs` build_coordinator_prompt | 协调器系统提示词 |
| `loadSkillsDir.ts` parseSkillFrontmatterFields | `frontmatter.rs` parse_skill_file | 前端解析 (pub 可见性) |
| `handleStopHooks` query.ts:1267 | `turn.rs:333-382` HookEvent::Stop | 停止钩子调度 |
| `applyToolResultBudget` query.ts:379 | `compact.rs:771` enforce_tool_result_budget | 工具结果预算 |

---

## 建议 (第二轮)

### P0 — 应尽快修复
1. **技能发现目录名** — `manager.rs:182` 将 `.skills`/`skills` 改为 `.claude/skills`（或同时支持两者）

### P1 — 重要功能差距
2. **内置技能接入** — 修复 `frozen/skill.rs:183` 的 TODO，让 `load_session_skills()` 实际注册内置技能
3. **记忆入口注入** — `build_memory_prompt()` 需要读取+截断+追加 MEMORY.md 内容
4. **回合循环预取** — 实现异步记忆预取 + 技能发现预取（`query.ts:301-335` 模式）
5. **令牌预算继续** — 在 turn loop 中添加基于 token 的预算检查
6. **MCP 断连重试** — 将重试逻辑从仅 `connect_all` 扩展到传输关闭事件
7. **压缩图片剥离** — 移植 `stripImagesFromMessages()` 到 `compact.rs`
8. **工具池组装** — 实现 `assembleToolPool()` 等效：内置+MCP 工具去重/排序
9. **插件作用域+策略** — 移植作用域系统和 `isPluginBlockedByPolicy`

### P2 — 改善
10. **缺失工具**: WebBrowser, LSP, TerminalCapture, Snip, Monitor, PushNotification, Workflow
11. **工具内省方法**: isSearchOrReadCommand, isOpenWorld, requiresUserInteraction, alwaysLoad, maxResultSizeChars
12. **SessionMemory 时间戳**: 修复硬编码时间戳为实际时间
13. **记忆选择改进**: Sonnet 而非 Haiku, 工具感知提示词, alreadySurfaced 去重
14. **前端字段补全**: hooks, agent, effort, shell (5/16 缺失)
15. **插件实际下载管线**: cli.rs 占位符 → 完整下载+提取
16. **McpServerState 使用**: 连接/重连时更新 server_states
