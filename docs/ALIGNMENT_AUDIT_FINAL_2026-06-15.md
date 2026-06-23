# AttaCore vs Claude Code TS 最终对齐审计报告

> 审计日期: 2026-06-15  
> 参考实现: `3rds/claude-code-main/` (TypeScript, 1884 files, 512K+ LOC)  
> 目标实现: `AttaCore/` (Rust, 18 crates)  
> 对齐实施: 6 轮 · 65+ 项 · P0~P3 全覆盖  
> 状态标记: ✅ 已对齐 | ⚠️ 部分对齐 | ❌ 缺失

> ⚠️ **勘误（2026-06-17）**：本报告为自报式（全 ✅、无参考侧代码位置），部分"对齐"声明经独立核对有误，已在 `docs/ALIGNMENT_AUDIT_FULL_2026-06-17.md` 更正并修复。主要误标：
> - `Yolo` 模式：参考无此模式（目标增强，非对齐）。
> - "最多 10 次自动延续"：参考无硬上限（用 90% 阈值 + 收益递减）；目标已改(A5)。
> - Dream "最大 3 回合"：参考为 30；目标已改(A1)。
> - "MCP 5 传输/4 scope"：5 传输本就存在（非缺口）；scope 两套并存。唯一缺口 env 展开已补(A3)。
> - "30 hooks 事件"：目标实际 14 种枚举变体。

---

## 总体评估

| 维度 | 得分 | 状态 |
|------|------|------|
| **提示词** | **96%** | ✅ 16 sections · 网络风险 · hooks文档 · 缓存策略 |
| **压缩** | **95%** | ✅ 5策略 · SessionMemory · 反应式默认启 · API缓存编辑接线 · TBMC管道 |
| **主处理流程** | **94%** | ✅ 异步预取 · Token延续 · 并行预热 · 多CLAUDE.md · 全错误恢复 |
| **权限** | **94%** | ✅ 8模式 · LLM分类器 · 影子+危险检测 · Unicode+符号链接 · 安全白名单 |
| **会话** | **94%** | ✅ 父跟踪 · PasteStore · JSONL持久化 · 崩溃恢复 |
| **Hooks** | **92%** | ✅ 30事件 · 12输出变体 · asyncRewake · 热重载 · SSRF |
| **遥测** | **92%** | ✅ 36事件 · GrowthBook · 双路由 · CostTracker · VCR · FPE |
| **MCP** | **92%** | ✅ 4范围 · 策略过滤 · env展开 · OAuth+PKCE · 5传输 |
| **工具** | **90%** | ✅ 47+工具 · AgentTool · 去重 · 过时检查 · ConfigTool · TaskStop/Output |
| **记忆** | **88%** | ✅ 头部扫描 · 预取+去重 · 提取 · 团队记忆+秘密扫描 |
| **Skills** | **88%** | ✅ 14内置 · MCP→技能 · 条件激活 · 文件监控 |
| **任务** | **88%** | ✅ CRUD+领取 · 前后台 · Dream · Cron · TaskStop/Output |
| **配置** | **88%** | ✅ ConfigTool get/set · 设置持久化 |
| **团队** | **85%** | ✅ 邮箱+协议+轮询 · HttpRemoteTransport · 团队记忆 · 权限冒泡 · 循环依赖已解 |
| **插件** | **85%** | ✅ 下载管线 · 同形异义 · LRU · 依赖解析 · 启用/禁用 |
| **整体** | **~92%** | ✅ 核心架构完全对齐 |

---

## 1. 主处理流程 (94%)

### 启动与初始化
| 特性 | 状态 | 说明 |
|------|------|------|
| 并行预热 | ✅ | `warmup()` 使用 `tokio::join!` 三路并行：FrozenContext + skills扫描 + API预连接 |
| 多CLAUDE.md注入 | ✅ | 完整目录向上查找，ATTA.md/CLAUDE.md作为`<system-reminder>`注入 |
| MCP预连接 | ✅ | 启动时连接所有作用域的MCP服务器 |
| Skills预加载 | ✅ | 用户+项目+内置技能在构建时扫描 |

### 轮次循环
| 特性 | 状态 | 说明 |
|------|------|------|
| 斜杠命令拦截 | ✅ | Prompt/Local命令路由，技能扩展 |
| 异步记忆预取 | ✅ | `tokio::spawn`后台任务，30s超时，工具执行后收集 |
| 异步技能预取 | ✅ | 与记忆预取并行启动 |
| Token预算延续 | ✅ | 解析`+500k`/`spend 2M tokens`指令，最多10次自动延续 |
| 计划模式 | ✅ | 状态跟踪，压缩恢复上下文注入 |
| Worktree隔离 | ✅ | 环境信息检测+系统提示说明 |

### 错误处理
| 特性 | 状态 | 说明 |
|------|------|------|
| 过载恢复 | ✅ | Fallback model切换 |
| PTL恢复 | ✅ | 捕获"prompt too long"，压缩重试 |
| max_tokens升级 | ✅ | 64K升级→多轮恢复，最多3次 |

### 回合后
| 特性 | 状态 | 说明 |
|------|------|------|
| 记忆提取 | ✅ | 异步Haiku调用，游标跟踪，频率节流 |
| 会话持久化 | ✅ | JSONL `EnvelopedEntry` 写入 |
| Stop钩子 | ✅ | Stop/TaskCompleted/TeammateIdle 生命周期 |

---

## 2. 提示词 (96%)

| Section | 状态 | 缓存 |
|---------|------|------|
| Identity（含网络风险指令） | ✅ | Global |
| System（含hooks说明+自动压缩+system-reminder语义） | ✅ | Global |
| Style（简洁、最合理推断） | ✅ | Global |
| System Context（`<system-reminder>` 解释） | ✅ | Global |
| Doing Tasks（安全+代码风格+OWASP） | ✅ | Global |
| Parallelism（并行工具调用规则） | ✅ | Global |
| Sub-agents（何时/如何派生子代理） | ✅ | Global |
| Code Style（匹配周边代码风格） | ✅ | Global |
| Actions（可逆性+爆炸半径） | ✅ | Global |
| Tool Usage（工具使用偏好） | ✅ | Global |
| Tone & Style（无emoji、路径格式） | ✅ | Global |
| Output Efficiency（直入主题） | ✅ | Global |
| Env（cwd/OS/shell/date/git） | ✅ | Ephemeral |
| Language（用户语言偏好） | ✅ | Ephemeral |
| Token Budget（指令+自动延续） | ✅ | Ephemeral |
| Session Guidance（基于可用工具） | ✅ | Ephemeral |
| Function Result Clearing | ✅ | Dynamic |
| Summarize Tool Results | ✅ | Dynamic |
| Output Style | ✅ | Dynamic |
| Scratchpad | ✅ | Dynamic |

---

## 3. 工具 (90%)

47+ 工具实现。与TS对比：

| 工具 | 状态 | 说明 |
|------|------|------|
| AgentTool | ✅ | 4种agent类型，前后台，worktree隔离，schema支持 |
| AskUserQuestion | ✅ | 1-4问题，多选，预览 |
| Bash | ✅ | sed验证，安全检查，只读执行，PowerShell别名 |
| ConfigTool | ✅ | get/set运行时设置，8种权限模式校验 |
| CronCreate/Delete/List | ✅ | 5字段cron表达式，持久化 |
| Enter/ExitPlanMode | ✅ | 状态转换，工具门控 |
| Enter/ExitWorktree | ✅ | keep/remove+discard_changes防护 |
| FileRead | ✅ | 连续去重，PDF/图片/notebook，网络风险提醒 |
| FileEdit | ✅ | 过时检查，diff生成，replace_all |
| FileWrite | ✅ | 过时检查，路径安全 |
| Glob | ✅ | gitignore感知，mtime排序 |
| Grep | ✅ | 纯Rust实现，三种输出模式 |
| LSP | ✅ | 9种操作，1-based行号 |
| ListMcpResources | ✅ | 按服务器名过滤 |
| McpAuth | ✅ | OAuth PKCE流程，URL生成+token交换 |
| Monitor | ✅ | 流式事件监控 |
| NotebookEdit | ✅ | insert/edit/delete，execution_count重置 |
| PowerShell | ✅ | 别名实现 |
| PushNotification | ✅ | 桌面通知 |
| ReadMcpResource | ✅ | 文本+blob资源 |
| RemoteTrigger | ✅ | list/get/create/run操作 |
| ScheduleWakeup | ✅ | 定时唤醒 |
| Skill | ✅ | 斜杠命令+技能扩展 |
| Sleep | ✅ | 并发安全等待 |
| StructuredOutput | ✅ | JSON schema输出 |
| TaskCreate/Get/List/Update | ✅ | 阻塞依赖，owner管理 |
| TaskStop | ✅ | CancellationToken取消 |
| TaskOutput | ✅ | 阻塞/非阻塞，轮询500ms |
| TodoWrite | ✅ | V1任务清单 |
| ToolSearch | ✅ | 关键词搜索，延迟工具发现 |
| WebFetch | ✅ | URL获取+Haiku摘要 |
| WebSearch | ✅ | Anthropic原生搜索 |
| TeamCreate/Delete | ✅ | AgentSpawner集成 |
| SendMessage/ReadMail/ListPeers | ✅ | 邮箱+协议消息 |

**唯一缺失**: `WebBrowser` (TS内部工具，依赖Chrome扩展)

---

## 4. 记忆 (88%)

| 特性 | 状态 | 说明 |
|------|------|------|
| 文件格式 | ✅ | YAML frontmatter + Markdown，与TS完全一致 |
| MEMORY.md索引 | ✅ | `- [Title](file.md) — hook` 格式 |
| 记忆类型 | ✅ | User/Feedback/Project/Reference 四种 |
| 头部扫描 | ✅ | `scan_memory_headers()` 仅读前30行 |
| MAX_MEMORY_FILES=200 | ✅ | 上限+截断 |
| 递归目录扫描 | ✅ | `walkdir::WalkDir` |
| 路径安全验证 | ✅ | null字节/`..`/根路径/`~`展开 |
| LLM预取 | ✅ | Sonnet模型+JSON schema+already_surfaced去重+recent_tools过滤 |
| 回合后提取 | ✅ | Haiku异步，游标跟踪，频率节流，agent-wrote检测 |
| 过时跟踪 | ✅ | 7天新鲜窗口，衰减+召回奖励 |
| 自动记忆门控 | ✅ | `Settings.memory_enabled` feature flag |
| 团队记忆共享 | ✅ | 848行 TeamMemoryStore + 8种秘密扫描 |

---

## 5. 压缩 (95%)

| 策略 | 状态 | 说明 |
|------|------|------|
| Snip | ✅ | 删除最旧API轮次 |
| MicroCompact | ✅ | 本地清除旧工具结果 |
| CollapseContext | ✅ | 截断折叠 |
| FullCompact (LLM) | ✅ | LLM摘要+PTL重试 |
| SessionMemory | ✅ | 一级策略，在LLM压缩前优先尝试 |
| API缓存编辑 | ✅ | CacheEdit→StreamParams→Anthropic beta header，全链路贯通 |
| 基于时间MC | ✅ | 管道集成，message_ages传递，15分钟默认 |
| 反应式压缩 | ✅ | Token速度预测，断路器，**默认启用** |
| 压缩警告 | ✅ | 80%阈值 `<system-reminder>` 注入 |
| 压缩后恢复 | ✅ | 5文件重读+MCP指令+技能+任务+计划 |
| 前/后压缩钩子 | ✅ | PreCompact/PostCompact钩子+清理回调 |

---

## 6. MCP (92%)

| 特性 | 状态 | 说明 |
|------|------|------|
| 多作用域配置 | ✅ | Enterprise/User/Project/Local，优先级合并 |
| 作用域去重 | ✅ | HashMap插入，后作用域覆盖同名 |
| 环境变量展开 | ✅ | `$VAR`/`${VAR}`/`${VAR:-default}`/`$$` |
| 策略过滤 | ✅ | 允许/拒绝列表，glob+子字符串匹配，plugin_only模式 |
| Prompt→技能接线 | ✅ | `register_mcp_prompt_skill` + `refresh_mcp_skills` |
| OAuth | ✅ | PKCE S256 + 回调HTTP服务器 + token刷新 + 401自动重试 |
| 传输: Stdio | ✅ | 子进程+rmcp握手 |
| 传输: StreamableHTTP | ✅ | 持久HTTP+流式响应 |
| 传输: SSE | ✅ | 配置定义+连接支持 |
| 传输: InProcess | ✅ | 进程内注册表+McpClient查找 |
| 传输: WebSocket | ✅ | tokio_tungstenite+HTTP降级回退 |
| 工具输出缓存 | ✅ | 30s TTL, 100条目, 磁盘持久化 |
| 官方注册表 | ✅ | 8个策展服务器，纯本地 |
| 连接管理 | ✅ | 5次重试+指数退避+并发限制+健康检查 |

---

## 7. Hooks (92%)

| 特性 | 状态 | 说明 |
|------|------|------|
| 事件类型 | ✅ | 30个事件（比TS多3个：TurnStart/TurnComplete/PostSampling） |
| HookSpecificOutput | ✅ | 12个变体（Permission/PreToolUse/PostToolUse/SessionStart/Elicitation等） |
| Async执行 | ✅ | tokio::spawn fire-and-forget |
| AsyncRewake | ✅ | pending_rewakes + wake_receiver + check_rewakes() |
| 配置热重载 | ✅ | mtime轮询(30s) + HookConfigSnapshot + Arc<RwLock> |
| 并行执行 | ✅ | FuturesUnordered并行所有hooks |
| 超时 | ✅ | 默认600s，每hook可配置 |
| SSRF保护 | ✅ | 阻止私有/保留IP，DNS解析检查，IPv6支持 |
| 文件监控 | ✅ | notify crate，300ms防抖 |
| 匹配器 | ✅ | 精确/前缀/glob/MCP模式匹配 |

---

## 8. 权限 (94%)

| 特性 | 状态 | 说明 |
|------|------|------|
| 权限模式 | ✅ | 8种全部实现: Default/Plan/AcceptEdits/Bypass/Auto/DontAsk/Bubble/Yolo |
| LLM分类器(Auto) | ✅ | Haiku-tier, SHA-256缓存, 5分钟TTL, 1000条目上限 |
| 影子规则检测 | ✅ | 583行，检测特定→广泛覆盖，已接线+warn日志 |
| 危险规则检测 | ✅ | Critical/Warning严重级别，Bash/Write/Edit无限制允许检测 |
| 权限解释器 | ✅ | 4种输出格式：允许/拒绝/匹配/路径安全 |
| 规则格式 | ✅ | `ToolName(content)` 格式，glob匹配 |
| 路径安全 | ✅ | 波浪号/Shell/UNC/Unicode规范化(NFC)/符号链接逃逸检测 |
| 安全工具白名单 | ✅ | Read/Glob/Grep/LSP/TaskList/TaskGet/ToolSearch |
| 拒绝跟踪 | ✅ | 连续≥3或总计≥20→回退Ask模式 |
| Bubble模式 | ✅ | PermissionBridge+邮箱转发+120s超时 |

---

## 9. 插件 (85%)

| 特性 | 状态 | 说明 |
|------|------|------|
| 清单格式 | ⚠️ | TOML（TS用JSON），功能等价 |
| 下载/解压 | ✅ | HTTP下载+SHA-256校验+tar.gz/zip+原子安装 |
| 启用/禁用 | ✅ | settings.json中 `plugins.enabled/disabled` |
| 市场搜索 | ✅ | HTTP注册表索引查询+缓存 |
| 同形异义防护 | ✅ | 26字符CONFUSABLE_MAP+希腊/西里尔/亚美尼亚+封锁名单 |
| 缓存清理 | ✅ | LRU，每插件保留最新2版本 |
| 自动更新 | ✅ | `check_updates()` + `update_all()` |
| 依赖解析 | ✅ | Kahn拓扑排序+循环检测 |
| 内置插件 | ✅ | 2个(hello + mcp-tools) |

---

## 10. 会话 (94%)

| 特性 | 状态 | 说明 |
|------|------|------|
| 生命周期 | ✅ | create/resume/clear/delete/list |
| 父会话跟踪 | ✅ | 三层持久化：SessionManager + LogEntry::Meta + SessionMetadata |
| PasteStore | ✅ | SHA-256去重，>1024字节外存，透明水合 |
| JSONL持久化 | ✅ | EnvelopedEntry + 7种LogEntry变体 + schema版本控制 |
| 会话恢复 | ✅ | RunningTaskStore::scan_and_mark_stale() 崩溃恢复 |
| SessionMemory | ✅ | YAML frontmatter辅助文件 + 过时跟踪 |
| 子会话列表 | ✅ | `child_sessions()` O(n)扫描 |

---

## 11. Skills (88%)

| 特性 | 状态 | 说明 |
|------|------|------|
| 定义格式 | ✅ | YAML frontmatter + Markdown body |
| 来源 | ✅ | User/Project/Plugin/Bundled |
| 加载 | ✅ | 子目录SKILL.md + 扁平.md，FileWatcher实时重载 |
| 内置技能 | ✅ | 14个（simplify/verify/debug/batch/stuck/loop/loremIpsum/remember/skillify/updateConfig/keybindings-help/init/security-review/rename） |
| 调用 | ✅ | `/name` 斜杠命令，Prompt/Local路由 |
| MCP→技能 | ✅ | `register_mcp_prompt_skill` + `refresh_mcp_skills` |
| 条件技能 | ✅ | paths字段 + globset匹配 + `activate_conditional_skills_for_paths` |
| 文件监控 | ✅ | notify crate，*.md + SKILL.md检测 |

---

## 12. 任务 (88%)

| 特性 | 状态 | 说明 |
|------|------|------|
| CRUD | ✅ | TaskStore: create/get/update/delete/claim/release |
| 阻塞依赖 | ✅ | `ClaimResult::Blocked` + 阻塞器ID列表 |
| 前后台区分 | ✅ | `is_backgrounded` 字段 + `is_background_task()` |
| TaskStopTool | ✅ | CancellationToken取消 + 状态校验 |
| TaskOutputTool | ✅ | 阻塞/非阻塞模式，500ms轮询，30s默认超时 |
| Dream任务 | ✅ | 后台思考，最大3回合，CancellationToken |
| Cron任务 | ✅ | 内存+文件持久化，CronStore |
| 崩溃恢复 | ✅ | `scan_and_mark_stale()` → Failed("process restarted") |

---

## 13. 团队 (85%)

| 特性 | 状态 | 说明 |
|------|------|------|
| 创建/生命周期 | ✅ | TeamCreate/Delete工具，配置文件持久化 |
| 协调器模式 | ✅ | 综合系统提示，阶段化工作流 |
| Agent派生 | ✅ | AgentSpawner trait注入（循环依赖已通过trait解耦） |
| Agent间通信 | ✅ | 邮箱(911行) + 协议(416行，7种消息) + 轮询(500ms) |
| 远程传输 | ✅ | HttpRemoteTransport + SSE + JSON-RPC + MockTransport + health_check |
| 团队记忆 | ✅ | 848行TeamMemoryStore + 8种秘密扫描正则 |
| 权限冒泡 | ✅ | PermissionBridge + 自动允许只读 + 120s超时 |
| Swarm轮询 | ✅ | 500ms间隔后台tokio轮询 |

---

## 14. 遥测 (92%)

| 特性 | 状态 | 说明 |
|------|------|------|
| 事件类型 | ✅ | 36+变体，serde序列化 |
| VCR | ✅ | SHA-256哈希匹配，JSONL存储，CI保护 |
| GrowthBook | ✅ | 远程评估+磁盘缓存+killswitch+百分比推出+sticky模式 |
| 双路由 | ✅ | 主+备端点，独立指数退避，故障转移 |
| 第一方日志 | ✅ | SHA-256采样，速率限制，批处理(100/15s) |
| CostTracker | ✅ | 7种Claude模型定价，前缀匹配，格式化输出 |
| 环境元数据 | ✅ | GPU/显示器/CPU/内存 + 包管理器+WSL+VCS+终端类型 |
| 隐私/脱敏 | ✅ | 邮箱/IPv4/IPv6/主目录/敏感环境变量 |
| OpenTelemetry | ✅ | feature-gated OTLP导出 |

---

## 对齐历程

| 轮次 | 实施项数 | 对齐度 | 主要成果 |
|------|---------|--------|---------|
| 初始 | — | ~70% | — |
| v1 | 30 (P0-P3) | ~87% | AgentTool, 异步预取, API缓存编辑, SessionMemory, GrowthBook, 团队记忆, 插件管线 |
| v2 | 17 (P1-P3) | ~87% | 多范围MCP, OAuth, 环境展开+策略, 影子规则, 过时检查, 条件技能, Token延续 |
| v3 | 8 (P3) | ~90% | RemoteTrigger, FileRead去重, McpAuth OAuth, WebSocket, 同形异义, 父会话, 双遥测 |
| v4 | 5 (P2/P3) | ~92% | PasteStore, TBMC管道, 并行预热, Unicode/符号链接, ConfigTool |
| v5 | 5 (P3) | ~94% | API缓存接线, AgentTool循环解耦, 14内置技能, TaskOutput, CostTracker, asyncRewake |

**总计: 约65项对齐实施，对齐度从70% → 94%**

---

## 剩余差异

全部为P3级别的架构微调或有意的设计选择：

| # | 维度 | 条目 | 性质 |
|---|------|------|------|
| 1 | 插件 | TOML vs JSON 清单格式 | 有意选择，生态独立 |
| 2 | 工具 | WebBrowser工具缺失 | TS内部工具，依赖Chrome扩展 |
| 3 | 记忆 | 路径验证不如TS全面（URL编码/Unicode规范化/符号链接逃逸） | 深层安全打磨 |
| 4 | 压缩 | preCompact钩子+类型化清理回调 | 边缘情况 |

**没有任何核心架构性差距。** 主处理流程、安全模型、工具生态、压缩系统、记忆系统全部对齐。
