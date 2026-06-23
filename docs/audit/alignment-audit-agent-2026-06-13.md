# 对齐审计报告 — `agent` — 2026-06-13

## 比较范围
- **参考实现 (A)**: `/Users/xbits/Workspace/Atta/AttaCore/3rds/claude-code-main/src/` (TypeScript, ~250 文件)
- **目标实现 (B)**: `/Users/xbits/Workspace/Atta/AttaCore/crates/` (Rust, 17 crates)
- **涉及文件**: A 侧 ~60 个关键文件, B 侧 ~50 个关键文件

## 概要

| 维度 | 评级 | 说明 |
|------|------|------|
| 能力对齐 | ⚠️ | 核心能力已覆盖，但多项高级能力缺失或简化 |
| 行为对齐 | ⚠️ | 主流程一致，但边界行为、错误恢复、缓存策略有偏差 |
| 提示词对齐 | ⚠️ | 系统提示骨干一致，但技能/记忆/MCP 提示词段落大量缺失 |
| 流程对齐 | ⚠️ | 核心 loop 结构一致，但缺少 multi-agent coordinator、VCR 测试夹具等子流程 |

## 代码位置清单

### 参考实现 (A)
| 文件 | 子系统 | 行数 |
|------|--------|------|
| `query.ts` | 主 query loop | ~1800 |
| `QueryEngine.ts` | 入口/引擎 | ~1300 |
| `tools.ts` | 工具注册 | ~450 |
| `Tool.ts` | Tool 类型定义 | ~800 |
| `tools/BashTool/` | Bash 工具 | ~12 文件 |
| `tools/FileEditTool/` | 文件编辑 | ~5 文件 |
| `tools/AgentTool/` | 子 Agent 工具 | ~12 文件 |
| `tools/SkillTool/` | 技能调用 | ~1100 |
| `skills/loadSkillsDir.ts` | 技能加载 | ~1100 |
| `memdir/memdir.ts` | 记忆构建 | ~450 |
| `memdir/memoryTypes.ts` | 记忆类型提示词 | ~350 |
| `memdir/findRelevantMemories.ts` | LLM 记忆检索 | ~120 |
| `services/compact/compact.ts` | 全量压缩 | ~600 |
| `services/compact/autoCompact.ts` | 自动压缩 | ~200 |
| `services/compact/microCompact.ts` | 微压缩 | ~300 |
| `services/compact/prompt.ts` | 压缩提示词 | ~200 |
| `services/mcp/client.ts` | MCP 客户端 | ~300 |
| `services/mcp/types.ts` | MCP 类型 | ~260 |
| `services/mcp/useManageMCPConnections.ts` | MCP 连接管理 | ~400 |
| `services/mcp/config.ts` | MCP 配置 | ~200 |
| `services/analytics/index.ts` | 遥测入口 | ~150 |
| `services/vcr.ts` | VCR 测试夹具 | ~250 |
| `plugins/builtinPlugins.ts` | 内置插件 | ~160 |
| `services/plugins/pluginOperations.ts` | 插件操作 | ~600 |
| `state/AppStateStore.ts` | 全局状态 | ~500 |
| `services/api/claude.ts` | Anthropic API | ~650 |
| `coordinator/coordinatorMode.ts` | 多 Agent 协调 | ~370 |
| `hooks/` | React hooks | ~15 文件 |

### 目标实现 (B)
| 文件 | 子系统 | 行数 |
|------|--------|------|
| `crates/runtime/src/agent.rs` | Agent 引擎 | ~550 |
| `crates/runtime/src/turn.rs` | Turn 循环 | ~750 |
| `crates/runtime/src/streaming.rs` | 流处理 | ~300 |
| `crates/runtime/src/agent_tool.rs` | 子 Agent 工具 | ~500 |
| `crates/core/src/interface/prompt.rs` | 提示词组装 | ~120 |
| `crates/core/src/interface/memory.rs` | 持久记忆 | ~250 |
| `crates/core/src/memory.rs` | 记忆结构 | ~220 |
| `crates/core/src/tool.rs` | Tool trait | ~250 |
| `crates/core/src/permission.rs` | 权限类型 | ~200 |
| `crates/tools/src/lib.rs` | 工具注册 | ~40 |
| `crates/tools/src/bash.rs` | Bash 工具 | ~400 |
| `crates/tools/src/file_edit.rs` | 文件编辑 | ~200 |
| `crates/skills/src/manager.rs` | 技能管理 | ~110 |
| `crates/compaction/src/compact.rs` | 压缩 | ~480 |
| `crates/compaction/src/grouping.rs` | 消息分组 | ~100 |
| `crates/model/src/adapter.rs` | Anthropic 适配器 | ~200 |
| `crates/model/src/client.rs` | HTTP 客户端 | ~300 |
| `crates/model/src/stream.rs` | SSE 流解析 | ~150 |
| `crates/mcp/src/manager.rs` | MCP 管理 | ~200 |
| `crates/mcp/src/adapter.rs` | MCP 工具适配 | ~100 |
| `crates/mcp/src/config.rs` | MCP 配置 | ~50 |
| `crates/telemetry/src/handle.rs` | 遥测句柄 | ~100 |
| `crates/telemetry/src/vcr.rs` | VCR 包装 | ~300 |
| `crates/plugin/src/manifest.rs` | 插件清单 | ~260 |
| `crates/plugin/src/lib.rs` | 插件入口 | ~5 |
| `crates/team/src/coordinator.rs` | 多 Agent 协调 | ~100 |
| `crates/scene/src/scene/coding.rs` | 编码场景 | ~740 |
| `crates/session/src/session.rs` | 会话管理 | ~200 |
| `crates/permissions/src/gate.rs` | 权限门控 | ~600 |
| `crates/hooks/src/runner/mod.rs` | 钩子执行 | ~200 |
| `crates/history/src/store.rs` | 历史存储 | ~200 |

---

## 详细发现

### 1. Agent Loop / 处理流程

#### ✅ 一致项

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| `query.ts:307` — `while(true)` 主循环 | `turn.rs:116` — `loop { }` | 无限循环直到 turn 完成 |
| `query.ts:659` — `deps.callModel()` | `turn.rs:165` — `self.model.stream()` | 模型调用 |
| `query.ts:1366` — `runTools()` / `StreamingToolExecutor` | `streaming.rs:42` — `execute_stream()` | 两阶段: consume stream → execute tools |
| `query.ts:401-454` — snip → microcompact → autocompact | `turn.rs:421` — `self.compact_if_needed()` | 每轮前检查压缩 |
| `query.ts:1580` — attachment messages | `turn.rs:96-108` — CLAUDE.md `<system-reminder>` | 上下文注入 |
| `query.ts:1705` — max turns | `turn.rs:89` — `max_api_calls_per_turn` | 轮次限制 |

#### ⚠️ 差异项

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `query.ts:301-304` — `pendingMemoryPrefetch` 异步预取 | B: ❌ 缺失 | 记忆预取在模型流传输时异步进行 | 偏差 |
| `query.ts:331-335` — `pendingSkillPrefetch` 技能发现预取 | B: ❌ 缺失 | 技能发现同样异步预取 | 偏差 |
| `query.ts:709-854` — 流式回退 + withheld errors (prompt-too-long, max-output-tokens) | `turn.rs:215-265` — 简化版错误恢复 | A 有 3 种 withheld error + structured output retry; B 有 max_tokens recovery (3 retry → 8000) + budget tracking | 偏差 |
| `query.ts:376-394` — `applyToolResultBudget()` per-message budget enforcement | B: ❌ 缺失 | A 在 microcompact 前对工具结果大小施加 per-message 预算 | 缺失 |
| `query.ts:1000` — post-sampling hooks | B: `hooks/runner/mod.rs` — PostToolUse hooks | A 的 post-sampling hook 在每次模型采样后触发，B 的 hook 系统以工具为中心 | 偏差 |
| `query.ts:1267` — `handleStopHooks()` | B: ❌ 缺失 | 停止钩子——在 turn 终止前的最终检查 | 缺失 |
| `query.ts:1308` — token budget continuations | B: `turn.rs:90` — `max_budget_usd` | A 可动态选择继续/停止; B 仅硬限制 | 偏差 |

#### ❌ 缺失项

| 参考 (A) | 缺失内容 | 影响 |
|-----------|---------|------|
| `query.ts:1621` — skill discovery injection | 每轮注入新发现的技能 | 新安装技能不会被自动发现 |
| `query.ts:1600` — memory prefetch consume | 异步记忆预取结果在每轮被消费 | 跨会话记忆不会自动浮出水面 |

---

### 2. 记忆 (Memory)

#### ✅ 一致项

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| `memdir/memdir.ts:34` — `MEMORY.md` 索引文件 | `memory.rs:84` — `INDEX_FILE = "MEMORY.md"` | 一致的索引机制 |
| `memdir/memdir.ts:35` — 200 行限制 | `memory.rs:84` — 同样使用索引文件 | 限制策略一致 |
| `memoryTypes.ts:14-18` — 4 种记忆类型 | `memory.rs:39-49` — 4 种 `MemoryType` (User/Feedback/Project/Reference) | 类型映射一致 |
| `memoryTypes.ts:183-195` — `WHAT_NOT_TO_SAVE_SECTION` | B: ❌ 缺失提示词段落 | 记忆该存/不该存的指引规则需对齐 |

#### ⚠️ 差异项

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `memoryTypes.ts:37-177` — 详细类型描述 (XML 格式，含 `<when_to_save>` `<how_to_use>` `<examples>`) | B: ❌ 完全缺失 | A 为每种记忆类型提供 ~40 行 XML 描述; B 只有 Rust enum | ❌ 严重偏差 |
| `memoryTypes.ts:197-204` — `TRUSTING_RECALL_SECTION` | B: ❌ 缺失 | "信任记忆但验证"指引 | ❌ 缺失 |
| `memoryTypes.ts:206-220` — `WHEN_TO_ACCESS_SECTION` | B: ❌ 缺失 | 何时检索记忆的指引 | ❌ 缺失 |
| `findRelevantMemories.ts:39` — Sonnet 驱动记忆检索 | B: ❌ 缺失 | A 使用 LLM 选择最多 5 条相关记忆; B 只有 substring 搜索 (`memory.rs:167`) | ❌ 严重偏差 |
| `memdir/memdir.ts:57-100` — `truncateEntrypointContent()` 双限制 (行+字节) | B: ❌ 缺失 | A 有 200 行 + 25KB 字节双层截断 | ⚠️ 偏差 |
| `memdir/memdir.ts:419-450` — 多模式记忆加载 (auto-only / team+auto / KAIROS) | B: `memory.rs:92-119` — `load_all()` 仅 local override user | A 支持 team memory sync + auto memory + KAIROS daily log 三种模式 | ❌ 偏差 |
| `memoryTypes.ts:33-36` — `TYPES_SECTION_COMBINED` (private + team scope) | B: ❌ 缺失 | 团队记忆的 scope 语义 | ❌ 缺失 |

---

### 3. 工具 (Tools)

#### ✅ 一致项

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| `Tool.ts:362` — `Tool` type with `call/validateInput/checkPermissions/prompt/isReadOnly/isConcurrencySafe` | `tool.rs:33-156` — `Tool` trait | 接口高度一致 |
| `tools.ts:193` — `getAllBaseTools()` | `tools/src/lib.rs` — 各工具模块 | 核心工具集已覆盖 |
| `BashTool/` — shell 执行 + 安全 | `bash.rs` + `bash/sandbox.rs` | Bash 工具存在 |
| `FileReadTool/` | `file_read.rs` | ✅ |
| `FileWriteTool/` | `file_write.rs` | ✅ |
| `FileEditTool/` — 外科手术式 diff | `file_edit.rs` | ✅ |
| `GlobTool/` / `GrepTool/` | `glob.rs` / `grep.rs` | ✅ |
| `WebFetchTool/` / `WebSearchTool/` | `web_fetch.rs` / `web_search.rs` | ✅ |
| `AgentTool/` — 子 agent 启动 | `agent_tool.rs` | ✅ |
| `TaskCreate/Get/Update/List` | `tasks.rs` | ✅ |
| `CronCreate/Delete/List` | `cron/create.rs` `delete.rs` `list.rs` | ✅ |
| `SkillTool` — 技能调用 | `skill_tool.rs` | ✅ |

#### ⚠️ 差异项

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `AgentTool/runAgent.ts` — 子 agent fork + resume | `agent_tool.rs` | A 支持 agent memory snapshot + resume; B 仅简单 fork | ⚠️ |
| `AgentTool/builtInAgents.ts` — 内置 agent 类型 (general-purpose/plan/explore/verification/claude-code-guide) | B: code-reviewer/Explore/Plan/general-purpose 但缺少 claude-code-guide | B 的 agent 类型数对齐 | ⚠️ |
| `SkillTool/SkillTool.ts:122` — `executeForkedSkill()` 隔离子 agent 中运行技能 | B: `skill_tool.rs` | B 直接内联展开技能文本; A 在隔离 agent 中 fork 执行 | ❌ 行为偏差 |
| `SkillTool/SkillTool.ts:969` — `executeRemoteSkill()` 远程技能 | B: ❌ 缺失 | A 支持从 AKI/GCS 缓存加载远程规范技能 | ❌ 缺失 |
| `ToolSearchTool/` — 动态工具发现 | `tool_search.rs` | ✅ 存在但可能不如 A 完善 |
| `LSPTool/` — LSP 集成 | B: ❌ 缺失 | A 有完整的 LSP 工具 (go-to-definition, references, hover 等); B 无对应 | ❌ 缺失 |
| `BriefTool/` — 摘要检索 | B: ❌ 缺失 | A 的紧凑信息检索工具 | ❌ 缺失 |
| `REPLTool/` | B: ❌ 缺失 | REPL 模式工具 | ❌ 缺失 |
| `ConfigTool/` | B: ❌ 缺失 | 运行时配置更改工具 | ❌ 缺失 |
| `StructuredOutputTool/` | `structured_output.rs` | ✅ 存在 | ✅ |
| `Tool.ts:158` — `ToolUseContext` 大型上下文 (queryTracking, contentReplacementState, toolPermissionContext...) | B: `tool.rs` — 简化版上下文 | A 的 ToolUseContext 包含 ~40 字段; B 的版本相对简化 | ⚠️ |

#### ❌ 缺失工具

| 参考 (A) | 影响 |
|-----------|------|
| `LSPTool` — 语言服务器协议集成 | 无法在编辑器中获取定义/引用/悬停 |
| `BriefTool` — 紧凑信息检索 | 无法通过专用工具获取摘要 |
| `REPLTool` — REPL 模式 | 缺失交互式 REPL |
| `ConfigTool` — 运行时配置 | 无法在不重启的情况下更改设置 |

---

### 4. 技能 (Skills)

#### ✅ 一致项

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| 技能从目录加载 .md 文件 | `skills/manager.rs:47` — `load_dir()` | ✅ |
| 前端 YAML 解析 (name, description) | `skills/manager.rs:94-102` — `parse_skill_file()` | ⚠️ 极其简单 |

#### ❌ 严重偏差

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `loadSkillsDir.ts:185` — `parseSkillFrontmatterFields()` 解析 15+ 字段 (name/description/allowed-tools/arguments/hooks/model/effort/shell/...) | `manager.rs:94` — 仅 2 字段 (name/description) | A 解析完整的技能前端; B 仅解析 description 行 | ❌ 严重偏差 |
| `loadSkillsDir.ts:407` — SKILL.md 子目录格式 | B: ❌ 缺失 | A 支持 `skill-name/SKILL.md` 子目录 + 引用文件 | ❌ 缺失 |
| `loadSkillsDir.ts:566` — 旧版平铺 .md 文件 | B: `manager.rs:47` — 仅平铺格式 | B 只支持平铺 .md | ⚠️ |
| `loadSkillsDir.ts:861` — `discoverSkillDirsForPaths()` 动态技能发现 | B: ❌ 缺失 | A 从文件路径向上遍历目录树动态发现相关技能 | ❌ 缺失 |
| `loadSkillsDir.ts:997` — `activateConditionalSkillsForPaths()` 条件技能 | B: ❌ 缺失 | A 支持基于路径过滤的条件技能激活 | ❌ 缺失 |
| `bundledSkills.ts:131` — `extractBundledSkillFiles()` 安全文件提取 (O_NOFOLLOW/O_EXCL/0o600) | B: ❌ 缺失 | A 安全地将打包的技能引用文件提取到临时目录 | ❌ 缺失 |
| `bundled/index.ts` — 内置技能注册 (commit/review/test 等) | B: ❌ 缺失 | A 有预编译的内置技能 | ❌ 缺失 |
| `mcpSkillBuilders.ts` — MCP 技能适配器 | B: ❌ 缺失 | A 中的 MCP 服务器可以提供技能; B 没有 MCP→技能 桥接 | ❌ 缺失 |

---

### 5. 压缩 (Compaction)

#### ✅ 一致项

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| 多策略优先级 (Snip → MicroCompact → FullCompact) | `compact.rs:17-29` — `CompactStrategy` 枚举 | ✅ 策略映射一致 |
| `autoCompact.ts:160` — `shouldAutoCompact()` 阈值检测 | `turn.rs:421` — `compact_if_needed()` | ✅ |
| `compact.ts:530` — `buildPostCompactMessages()` 恢复上下文 | `compact.rs:458-471` — `PostCompactContext` | ✅ |

#### ⚠️ 差异项

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `microCompact.ts:42-50` — `COMPACTABLE_TOOLS` 白名单 (仅清理特定工具) | `compact.rs:97-140` — 清理所有旧工具结果 | A 仅清理读/搜索/获取类工具; B 清理所有 | ⚠️ 语义不同 |
| `microCompact.ts:36` — `TIME_BASED_MC_CLEARED_MESSAGE` 基于时间的缓存清除 | B: ❌ 缺失 | A 支持基于时间的微压缩 (CACHED_MICROCOMPACT) | ❌ 缺失 |
| `compact.ts:145` — `stripImagesFromMessages()` 移除图片 | B: ❌ 缺失 | A 在压缩前从 payload 中移除图片以节省 tokens | ❌ 缺失 |
| `compact/grouping.ts` — API 轮次分组 | `grouping.rs` — 同样存在 | ✅ |
| `compact/prompt.ts` — 压缩 agent 专用提示词 | B: ❌ 缺失 | B 的 `LlmCompactor` 内联提示词; A 有独立模块 | ⚠️ |
| `sessionMemoryCompact.ts` — 会话记忆压缩 | B: ❌ 缺失 (在 `compact.rs:28` 定义为策略但未实现) | 在压缩过程中提取持久记忆 | ❌ 缺失 |

---

### 6. MCP (模型上下文协议)

#### ✅ 一致项

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| Stdio / SSE / HTTP 传输 | `mcp/config.rs:10-41` — `Stdio` / `StreamableHttp` / `Sse` | ✅ |
| 工具名前缀 `mcp__<server>__<tool>` | `mcp/adapter.rs` | ✅ |
| MCP 工具包装为 Tool trait | `mcp/adapter.rs` | ✅ |
| MCP 资源列表/读取工具 | `mcp/tools.rs` — `ListMcpResourcesTool` / `ReadMcpResourceTool` | ✅ |

#### ⚠️ 差异项

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `types.ts:124` — 6 种服务器配置类型 (stdio, SSE, SSE-IDE, HTTP, WebSocket, SDK, claudeai-proxy) | `config.rs:10-41` — 3 种 (Stdio, StreamableHttp, Sse) | 缺少 SSE-IDE, WebSocket, SDK, claudeai-proxy | ❌ 偏差 |
| `types.ts:221` — `MCPServerConnection` 5 种状态 (Connected/Failed/NeedsAuth/Pending/Disabled) | B: 简化为连接或失败 | 缺少 NeedsAuth 和 Pending 状态 | ⚠️ 偏差 |
| `useManageMCPConnections.ts` — 指数退避重连 (MAX=5, INITIAL=1s, MAX=30s) | B: ❌ 缺失 (失败后直接放弃) | 严重偏差——将导致生产环境 MCP 不稳定 | ❌ 严重偏差 |
| `useManageMCPConnections.ts` — 监听 MCP 通知 (tool_list_changed 等) | B: ❌ 缺失 | 动态工具注册变更无法传播 | ❌ 缺失 |
| `mcp/auth.ts` — OAuth 认证 | B: `auth/` — 独立 crate | ✅ |
| `mcp/channelPermissions.ts` — 通道权限中继 | B: ❌ 缺失 | Telegram/iMessage 等通道权限中继 | ❌ 缺失 |
| `mcp/elicitationHandler.ts` — URL 引出处理 | B: ❌ 缺失 | MCP 工具可以发起用户 URL 引出 | ❌ 缺失 |

---

### 7. 遥测 (Telemetry)

#### ✅ 一致项

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| 事件日志 → mpsc 队列 | `telemetry/handle.rs:11-13` — `TelemetryHandle` (mpsc sender) | ✅ |
| 非阻塞记录 (通道满时静默丢弃) | `telemetry/handle.rs:95` — "通道满时静默丢弃" | ✅ |
| 会话 + 轮次 + 时间戳 metadata | `telemetry/events/mod.rs:37-54` — UUID v7 + session_id + turn_no | ✅ |

#### ⚠️ 差异项

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `analytics/index.ts:45` — `stripProtoFields()` PII 过滤 | B: ❌ 缺失 | A 有专门的 PII 标记类型 `AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS` | ❌ 缺失 |
| `analytics/sink.ts` — Datadog + 1P event logging | B: ❌ 缺失 (无远程后端实现) | B 定义了 40+ 事件载荷但缺少远程后端 | ❌ 偏差 |
| `analytics/growthbook.ts` — GrowthBook feature flag 集成 | B: ❌ 缺失 | 功能开关系统——在 A 中广泛使用 (树摇、条件功能) | ❌ 缺失 |

---

### 8. VCR (测试夹具录制/回放)

#### ✅ 一致项

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| SHA 哈希请求匹配 | `vcr.rs:111-120` — SHA-256 前 16 十六进制字符 | ✅ 比 A 更强 (SHA-256 vs SHA-1) |
| Dehydrate/Hydrate (路径替换) | `vcr.rs:120` — "Messages are dehydrated before hashing" | ✅ |
| CI 保护 (缺少夹具 → 硬错误) | `vcr.rs:101-103` — `is_ci()` | ✅ |
| 环境变量: `VCR_RECORD` / `VCR_REPLAY` | `vcr.rs:90-98` — `ATTA_VCR_RECORD` / `ATTA_VCR_REPLAY` | ✅ (前缀不同) |
| JSONL 存储 | `vcr.rs` — JSONL 格式 | ✅ |
| Turn-level 分组 | `vcr.rs:35` — `current_turn_id: Option<String>` | B 添加了 A 中不存在的按 turn 分组 |

#### ⚠️ 差异项

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `vcr.ts:23-33` — `shouldUseVCR()` 自动检测 (NODE_ENV=test 或 USER_TYPE=ant+FORCE_VCR) | `vcr.rs:90-98` — 仅环境变量 | A 在测试模式下自动启用; B 需要显式环境变量 | ⚠️ |
| `vcr.ts:39-80` — `withFixture<T>()` 通用夹具包装器 | B: VCR 包装在 Model trait 级别 | A 可以在任何数据类型上使用 VCR; B 限制在 Model::stream() | ⚠️ |
| `vcr.ts:54` — `CLAUDE_CODE_TEST_FIXTURES_ROOT` 可配置夹具根目录 | `vcr.rs:31-33` — 固定于 user_vcr_dir/local_vcr_dir | 不同的配置策略 | ⚠️ |

---

### 9. 插件 (Plugins)

#### ❌ 严重偏差

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `plugins/builtinPlugins.ts:28` — `registerBuiltinPlugin()` 内置插件注册 | `plugin/manifest.rs` — `PluginManifest` 结构 | B 有清单加载但无内置插件注册表 | ❌ 严重偏差 |
| `services/plugins/pluginOperations.ts:600` — 完整的插件操作 (install/uninstall/enable/disable/update) | B: ❌ 缺失 | B 只有清单加载; A 有完整的 marketplace 集成 + 依赖图 + 版本缓存 | ❌ 严重偏差 |
| `services/plugins/PluginInstallationManager.ts:60` — 后台插件安装管理器 | B: ❌ 缺失 | 声明式 vs 实际安装的协调 | ❌ 缺失 |
| `services/plugins/pluginCliCommands.ts` — CLI 插件管理命令 | B: ❌ 缺失 | `plugin install/uninstall/enable/disable/list` CLI | ❌ 缺失 |

---

### 10. 会话 (Session)

#### ✅ 一致项

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| 消息列表管理 (push/pop/resume) | `session/session.rs:13-20` — `SessionManager` | ✅ |
| Session ID (UUID 格式) | `session/session.rs` | ✅ |
| turn 计数 | `session/session.rs` — `turn_count` | ✅ |

#### ⚠️ 差异项

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `state/AppStateStore.ts:89` — 全局应用状态 (50+ 字段) | B: 分布在多个 crate (Agent, SessionState, EngineConfig) | A 有集中式不可变状态树; B 是分散式可变状态 | ⚠️ |
| `assistant/sessionHistory.ts:73-81` — SDK 模式会话历史 (分页 + cursor) | B: ❌ 缺失 | A 支持从 API 拉取历史事件用于 SDK/headless 会话恢复 | ❌ 缺失 |
| `state/onChangeAppState.ts` — 状态变更副作用 | B: ❌ 缺失 | 当状态变更时推送到 bridge/remote 的机制 | ❌ 缺失 |

---

### 11. 后端模型 (Backend Model)

#### ✅ 一致项

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| Anthropic Messages API 流式传输 | `model/adapter.rs:37` — `stream()` | ✅ |
| SSE 事件解析 (content_block_start/delta/stop) | `model/stream.rs:10-38` — `StreamEvent` | ✅ |
| 指数退避重试 | `model/client.rs:212-259` — 6 步退避 + 25% 抖动 | ✅ |
| API Key / OAuth Token 认证 | `model/client.rs:182-191` — `AuthMode` enum | ✅ |
| Prompt caching (cache_control) | `model/` — 通过 `CacheStrategy` 支持 | ✅ |
| 模型回退 (overloaded → fallback) | `turn.rs:215-225` — overloaded recovery | ✅ |

#### ⚠️ 差异项

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `services/api/errors.ts` — `PROMPT_TOO_LONG_ERROR_MESSAGE` + `categorizeRetryableAPIError` | B: 简化的错误处理 | A 区分 prompt-too-long vs 其他可重试错误; B 的区分度较低 | ⚠️ |
| `services/api/withRetry.ts` — `FallbackTriggeredError` | B: `turn.rs` — 内联的 overloaded 回退 | A 有正式的 FallbackTriggeredError 类型用于遥测 | ⚠️ |
| `services/api/claude.ts:358` — `getCacheControl()` 1h TTL | B: `prompt.rs:26-30` — `CacheStrategy::Ephemeral/Global` | ⚠️ 语义接近，但实现方式不同 |
| `services/api/claude.ts:633` — `assistantMessageToMessageParam()` | B: 内联于 `model/adapter.rs` | ✅ 功能存在 |

---

### 12. 多 Agent / 协调 (Coordinator/Team)

#### ⚠️ 差异项

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `coordinator/coordinatorMode.ts:36` — `isCoordinatorMode()` 环境变量检测 | `team/coordinator.rs:20-27` — `Coordinator` trait | A 通过 env var 激活; B 通过 trait 抽象 | ⚠️ |
| `coordinator/coordinatorMode.ts:111-369` — 300+ 行协调器系统提示词 | `team/coordinator.rs:40-90` — `DefaultCoordinator` 50 行 | A 有完整的协调器角色 + worker 工具 + 阶段式工作流 (Research/Synthesis/Implementation/Verification) + 并发规则; B 有简化的阶段式编排 | ❌ 严重偏差 |
| 邮箱系统 (SendMessage/ReadMail/ListPeers) | `team/mailbox.rs` — ✅ 存在 | ✅ 映射一致 |
| `tools/shared/spawnMultiAgent.ts` — 群体生成 | `team/coordinator.rs` — 基本多 agent 支持 | A 支持群体并行生成 + 收件箱; B 有基本的多 agent | ⚠️ |

---

### 13. 上下文构建 (Context/Scene)

#### ✅ 一致项

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| `context.ts:155` — `getUserContext()` CLAUDE.md | `scene/coding.rs:65-74` — `build_system_reminder()` | ✅ |
| `context.ts:116` — `getSystemContext()` git status | `scene/coding.rs:65-74` — git_status 在 system_reminder 中 | ✅ |
| 拼装顺序: scene skeleton → skills → memory → MCP → user append | `prompt.rs:60-117` — `assemble_prompt()` | ✅ 一致 |

#### ⚠️ 差异项

| 参考 (A) | 目标 (B) | 差异说明 | 判定 |
|-----------|-----------|---------|------|
| `scene/coding.rs:29-33` — 系统提示词渲染 (15+ 段落) | B: `scene/coding.rs:735` 行 | A 的系统提示词通过多个分散文件构建; B 集中在单一的 coding.rs 中 | ⚠️ |
| `context.ts:122-130` — `getSystemContext()` 包含 git status 摘要 | B: `scene/coding.rs:67-69` — 仅 git_status 可选字段 | A 在系统上下文中额外包含日期/时间/平台信息 | ⚠️ |

---

### 14. Hooks (钩子系统)

#### ✅ 一致项

| 参考 (A) | 目标 (B) | 确认点 |
|-----------|-----------|--------|
| 多种钩子事件类型 | `hooks/config.rs:61-110` — 28 个 `HookEvent` 变体 | ✅ (可能比 A 更多) |
| Command/Prompt/Http/Agent 钩子类型 | `hooks/config.rs:12-56` — `HookConfig` | ✅ |
| 并行钩子执行 | `hooks/runner/mod.rs` — `FuturesUnordered` | ✅ |
| 钩子输入/输出 JSON (HookInput/HookResponse) | `hooks/payload.rs` | ✅ |

---

## 提示词差异逐条

| 参考 (A): 关键文本摘要 | 目标 (B): 对应文本 | 位置 | 判定 |
|---|---|---|---|
| `memoryTypes.ts:37-177` — 4 种记忆类型的完整 XML 描述 (每种 ~30-40 行含 `<when_to_save>` `<how_to_use>` `<examples>` `<body_structure>`) | B: 无对应提示词文本 | A:`memoryTypes.ts:37-177` B:❌ | ❌ 严重缺失 |
| `memoryTypes.ts:183-195` — `WHAT_NOT_TO_SAVE_SECTION` ("Code patterns...Git history...Debugging...CLAUDE.md...Ephemeral") | B: 无对应提示词文本 | A:`memoryTypes.ts:183-195` B:❌ | ❌ 缺失 |
| `memoryTypes.ts:197-204` — `TRUSTING_RECALL_SECTION` ("Recall is proactive...verify the memory against current state") | B: 无对应提示词文本 | A:`memoryTypes.ts:197-204` B:❌ | ❌ 缺失 |
| `memoryTypes.ts:206-220` — `WHEN_TO_ACCESS_SECTION` | B: 无对应提示词文本 | A:`memoryTypes.ts:206-220` B:❌ | ❌ 缺失 |
| `findRelevantMemories.ts:18-34` — `SELECT_MEMORIES_SYSTEM_PROMPT` (Sonnet 分类器提示词，~15 行) | B: 无 LLM 记忆检索 | A:`findRelevantMemories.ts:18-34` B:❌ | ❌ 缺失 |
| `coordinator/coordinatorMode.ts:111-369` — 300+ 行协调器系统提示词 | B: 无对应提示词 | A:`coordinatorMode.ts:111-369` B:❌ | ❌ 缺失 |
| `compact/prompt.ts` — 压缩 agent 系统提示词 | B: 内联在 `LlmCompactor` 中 | A:`compact/prompt.ts` B:`compact.rs:238-452` | ⚠️ 功能等价但未作为独立提示词模块 |

---

## 建议

### P0 阻塞 (影响模型行为或稳定性)

1. **添加记忆类型提示词段落** — 从 A 复制 `TYPES_SECTION_INDIVIDUAL` / `WHAT_NOT_TO_SAVE_SECTION` / `TRUSTING_RECALL_SECTION` / `WHEN_TO_ACCESS_SECTION` 到 `memory.rs` 的 `build_memory_prompt()`。没有这些提示词，模型会错误地保存派生信息 (代码模式/git 历史) 而非真正有价值的记忆。

2. **实施 MCP 指数退避重连** — 从 A (`useManageMCPConnections.ts`) 移植带抖动的指数退避到 `mcp/manager.rs`。当前"连接失败后直接放弃"的行为在生产环境中不可接受。

3. **添加 Sonnet 驱动记忆检索** — 从 A (`findRelevantMemories.ts`) 移植 LLM 记忆选择器。当前 substring 匹配 (`memory.rs:167`) 太粗糙——在 50+ 条记忆中，"logging" 匹配 20 条，"api" 匹配 40 条。

### P1 重要 (影响功能完整性)

4. **完善技能前端解析** — 扩展 `skills/manager.rs:94-102` 以解析至少: `allowed-tools`, `arguments`, `hooks`, `model`, `shell`。目前仅解析 `description` 行，意味着技能无法声明工具依赖或参数。

5. **添加动态技能发现** — 实现 `discoverSkillDirsForPaths()` 等效功能，使技能可以基于当前文件路径上下文被发现。这是 A 体验中的关键差异化功能。

6. **实施协调器系统提示词** — 为 `team/coordinator.rs` 构建完整的协调器提示词（300+ 行 A 等效），覆盖角色定义、阶段式工作流 (Research/Synthesis/Implementation/Verification)、并发规则和 worker 提示词编写指南。

7. **添加 MCP 通知监听** — 实施对 `tool_list_changed`、`resource_list_changed`、`prompt_list_changed` MCP 通知的监听以支持动态工具注册。

### P2 改善 (提升鲁棒性和覆盖范围)

8. **添加 LSP 工具** — 从 A 移植语言服务器协议集成 (`tools/LSPTool/`)。使模型能够在编辑器中获取定义、引用和悬停信息。

9. **实施 per-message 工具结果预算** — 移植 `applyToolResultBudget()` 逻辑以在 microcompact 前对大型工具结果施加大小限制。

10. **添加内置技能** — 注册内置技能 (commit/review/test/explain 等，如同 A 的 `skills/bundled/`)。

11. **完善插件系统** — 实施 marketplace 集成、依赖解析、后台安装协调器和 CLI 管理命令。

12. **添加停止钩子** — 实施 `handleStopHooks()` 等效功能以在 turn 终止前进行最终检查。

13. **添加 VCR 自动检测** — 在 VCR 模式中实施 `shouldUseVCR()` 等效功能以在测试运行器中自动启用。

14. **添加 GrowthBook/feature flag 系统** — 实施功能标志集成以进行渐进式发布和实验。

---

## 对齐审计完成 — `agent` (v3)

### 总评
| 维度 | 评级 |
|------|------|
| 能力 | ⚠️ — 核心覆盖，多项缺失 |
| 行为 | ⚠️ — 主流程一致，边界行为有偏差 |
| 提示词 | ⚠️ — 骨干一致，关键段落缺失 |
| 流程 | ⚠️ — loop 结构对齐，子流程和恢复路径不完整 |

### 关键发现
- ❌ 12 项能力缺失 (LSP/Brief/REPL/Config 工具, 动态技能发现, 远程技能, 会话历史 API, MCP 通知监听等)
- ❌ 8 项提示词段落缺失 (记忆类型 XML 描述, 保存/不保存规则, 记忆信任/访问时机, 协调器提示词等)
- ⚠️ 15 项行为偏差 (记忆检索方式, 技能执行方式, 微压缩白名单, 插件系统等)
- ✅ 22 项确认一致 (工具集核心, MCP 传输, Agent loop, 压缩策略, 记忆文件格式, VCR, 钩子系统, SSE 解析等)

### 涉及文件
- **A** (参考): `query.ts` `Tool.ts` `tools.ts` `loadSkillsDir.ts` `memdir/*.ts` `memoryTypes.ts` `compact/*.ts` `mcp/*.ts` `vcr.ts` `builtinPlugins.ts` `pluginOperations.ts` `coordinatorMode.ts` `claude.ts` 等 ~60 文件
- **B** (目标): `crates/runtime/src/{agent,turn,streaming}.rs` `crates/core/src/{tool,memory,prompt,permission}.rs` `crates/tools/src/{bash,file_edit,...}.rs` `crates/skills/src/manager.rs` `crates/compaction/src/compact.rs` `crates/mcp/src/{manager,adapter,config}.rs` `crates/telemetry/src/{handle,vcr}.rs` `crates/plugin/src/manifest.rs` `crates/team/src/coordinator.rs` `crates/scene/src/scene/coding.rs` `crates/model/src/{adapter,client,stream}.rs` `crates/permissions/src/gate.rs` `crates/hooks/src/runner/mod.rs` `crates/history/src/store.rs` 等 ~50 文件
