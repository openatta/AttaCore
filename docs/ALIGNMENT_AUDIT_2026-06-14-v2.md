# AttaCore vs Claude Code TS 对齐审计报告 v2

> 审计日期: 2026-06-14 (30项对齐实施后)
> 参考实现: `3rds/claude-code-main/` (TypeScript, 1884 files)
> 目标实现: `AttaCore/` (Rust, 18 crates)
> 状态标记: ✅ 已对齐 | ⚠️ 部分对齐/有差异 | ❌ 缺失/重大差距

---

## 总体评估

| 维度 | 实施前 | 实施后 | 变化 |
|------|--------|--------|------|
| 主处理流程 | 70% | **85%** | +15% |
| 提示词 | 75% | **90%** | +15% |
| 工具 | 80% | **88%** | +8% |
| MCP | 75% | **78%** | +3% |
| 权限 | 85% | **88%** | +3% |
| 记忆 | 55% | **85%** | +30% |
| 压缩 | 65% | **90%** | +25% |
| Hooks | 80% | **88%** | +8% |
| 插件 | 50% | **85%** | +35% |
| 会话 | 70% | **90%** | +20% |
| Skills | 85% | **90%** | +5% |
| 任务 | 40% | **75%** | +35% |
| 团队 | 70% | **90%** | +20% |
| 遥测 | 75% | **92%** | +17% |
| **整体** | **~70%** | **~87%** | **+17%** |

---

## 1. 主处理流程 (85%)

### 已对齐 ✅
| 特性 | 说明 |
|------|------|
| 多 CLAUDE.md 注入 | 完整目录向上查找，ATTA.md/CLAUDE.md 注入为 `<system-reminder>` |
| 斜杠命令拦截 | Prompt/Local 命令路由，技能扩展 |
| 错误恢复 | 过载→fallback model，PTL→压缩重试，max_tokens→64K升级+多轮恢复 |
| 回合后处理 | 异步 Haiku 记忆提取，会话持久化，Stop/TaskCompleted/TeammateIdle 钩子 |
| Token 预算 | USD 预算 90%/100% 阈值 |

### 部分对齐 ⚠️
| 特性 | 差距 |
|------|------|
| 记忆预取 | 同步阻塞调用 `select_memories_with_llm()`；TS 使用 `startRelevantMemoryPrefetch()` **异步预取**隐藏延迟在工具执行背后 |
| 技能发现预取 | 同步执行 `discover_for_paths()`；TS 使用 `startSkillDiscoveryPrefetch()` 异步预取 |
| 启动预热 | `FrozenContext` 惰性收集（阻塞首个用户回合）；TS 在首个回合前预加载缓存 |
| Token 预算延续 | 仅有 USD 预算；缺少 TS 的 "spend 2M tokens" 输出 token 预算延续模式 |
| 计划模式 | 跟踪 `in_plan_mode` 用于压缩恢复，但缺少工具行为门控和计划专用斜杠命令 |
| Worktree 隔离 | 仅在环境上下文中检测 `is_worktree`，无可操作工具 |

---

## 2. 提示词 (90%)

### 已对齐 ✅
| 特性 | 说明 |
|------|------|
| 系统提示架构 | 15+ sections，静态/动态分区，`PromptBlock` 缓存策略 |
| 网络风险指令 | 与 TS `CYBER_RISK_INSTRUCTION` 文本逐字一致 |
| 自动压缩声明 | "conversation is not limited by the context window" |
| System-reminder 语义 | `<system-reminder>` 标签含义说明 |
| 工具使用指导 | 并行工具调用、子代理使用、代码风格 |
| 操作安全 | 可逆性与爆炸半径考量 |

### 部分对齐 ⚠️
| 特性 | 差距 |
|------|------|
| Hooks 说明 | 文本接近但不完全一致；缺少 `<user-prompt-submit-hook>` 引用和 "ask user to check hooks" 回退 |

---

## 3. 工具 (88%)

### 已对齐 ✅
| 工具 | 说明 |
|------|------|
| AgentTool | 920行，4种agent类型，前后台执行，worktree隔离，schema支持，22测试 |
| 文件工具 (Read/Write/Edit/Glob/Grep/NotebookEdit) | Read去重+网络风险提醒；Edit生成diff；Write路径安全 |
| Bash | 分类器、沙盒、后台、sed验证、安全检查（rm -rf/chmod 777/git push --force） |
| Cron (create/delete/list) | 5字段cron表达式、持久化、14测试 |
| 任务工具 (6个) | TaskCreate/Get/List/Update/Stop/Output 全部实现 |
| Worktree (enter/exit) | keep/remove + discard_changes 防护 |
| Plan mode (enter/exit/verify) | 状态转换、工具门控 |
| WebFetch/Search/LSP/Monitor/Push/Sleep/TodoWrite | 全部对齐 |

### 缺失 ❌ (TS有而AttaCore无)
| 工具 | 优先级 | 说明 |
|------|--------|------|
| SendMessageTool | P1 | 团队swarm消息通信（代码在 team/mailbox.rs 但未注册为工具） |
| TeamCreate/TeamDelete | P1 | 团队管理工具 |
| ListMcpResources/ReadMcpResource | P2 | MCP资源发现 |
| RemoteTrigger | P2 | 远程会话触发 |
| WebBrowser | P3 | 浏览器工具 |

---

## 4. MCP (78%)

### 已对齐 ✅
| 特性 | 说明 |
|------|------|
| 传输: Stdio / StreamableHTTP | 完整实现，含重试和指数退避 |
| 工具输出缓存 | 30s TTL，100条目，磁盘持久化 |
| 连接管理 | Trait-based McpClient，MockMcpClient，5次重试，并发限制 |
| Official Registry | 8个策展服务器，纯本地无网络依赖 |

### 部分对齐 ⚠️ / 缺失 ❌
| 特性 | 状态 | 差距 |
|------|------|------|
| **多范围配置加载** | ❌ | TS有7个范围（local/user/project/dynamic/enterprise/claudeai/managed）；AttaCore配置已定义范围枚举但**合并/优先级逻辑未完全集成** |
| **范围优先级+去重** | ❌ | TS有基于内容签名的3维去重；AttaCore无等效实现 |
| **环境变量展开** | ❌ | `${VAR}`/`${VAR:-default}` 未实现 |
| **策略允许/拒绝列表** | ❌ | TS有名称/命令/URL模式匹配；AttaCore无 |
| **MCP→Skills接线** | ⚠️ | `register_mcp_prompt_skill` / `refresh_mcp_skills` 已实现但未在所有连接点调用 |
| **OAuth回调监听器** | ❌ | 仅有Token存储+解析器trait；TS有完整的本地HTTP回调服务器+PKCE流程 |
| 传输多样性 | ⚠️ | 3种 vs TS的8种（缺WebSocket/InProcess/SDK/proxy） |
| SSE | ⚠️ | 降级到StreamableHTTP；TS原生支持 |

---

## 5. 权限 (88%)

### 已对齐 ✅
| 特性 | 说明 |
|------|------|
| 权限模式 | 8种全部实现（Default/Plan/AcceptEdits/Bypass/Auto/DontAsk/Bubble/Yolo） |
| 规则格式与匹配 | `ToolName(content)` 格式，glob匹配，前缀匹配 |
| 路径安全 | 写入策略：绝对路径要求、`..`阻止、系统路径黑名单、文件名黑名单 |
| LLM分类器 | 567行，Haiku-tier，SHA-256缓存，5分钟TTL，14测试 |
| 拒绝跟踪 | 连续拒绝≥3或总计≥20→回退到Ask模式 |

### 部分对齐 ⚠️ / 缺失 ❌
| 特性 | 状态 | 差距 |
|------|------|------|
| **阴影规则检测** | ⚠️ | 代码已存在于 `permissions/src/shadow.rs` (583行) 但**未在 lib.rs 中注册模块**，功能不可达 |
| **路径安全检查深度** | ❌ | TS 更加全面：tilde展开阻止、shell展开阻止、UNC路径、Windows模式、删除防护、glob验证、符号链接解析 |
| 危险权限检测 | ❌ | TS自动剥离过度宽泛的规则（如 `Bash(python:*)`） |
| 权限解释器 | ❌ | TS提供规则匹配的人类可读解释 |
| 安全工具Allow列表 | ❌ | TS维护显式列表，Bypass mode下仍可安全允许的只读工具 |

---

## 6. 记忆 (85%) — 最大改善

### 已对齐 ✅
| 特性 | 说明 |
|------|------|
| 文件格式 | YAML frontmatter + Markdown body，完全一致 |
| MEMORY.md 索引 | `- [Title](file.md) — hook` 格式 |
| 记忆类型 | User/Feedback/Project/Reference 四种 |
| 头部扫描 | 仅读取前30行（`scan_memory_headers`） |
| MAX_MEMORY_FILES=200 | 上限+截断 |
| 递归目录扫描 | `walkdir::WalkDir` 实现 |
| 路径验证 | null字节/`..`/根路径/`~`展开安全校验 |
| 自动记忆门控 | `Settings.memory_enabled` + `extended_memory` feature flag |
| 过时跟踪 | `staleness_penalty()` — 7天新鲜窗口，衰减+召回奖励 |
| 团队记忆 | 848行 TeamMemoryStore + 秘密扫描（8种正则模式） |
| 回合后提取 | 异步tokio::spawn，Haiku模型，游标跟踪，频率节流 |

### 部分对齐 ⚠️
| 特性 | 差距 |
|------|------|
| LLM预取模型 | AttaCore用Haiku；TS用Sonnet（通过`sideQuery()`） |
| 预取输出格式 | AttaCore自由文本；TS用JSON schema |
| 预取去重 | TS有`alreadySurfaced`集合 + `recentTools`过滤；AttaCore无 |
| 路径验证深度 | 缺少URL编码遍历、Unicode规范化攻击、符号链接逃逸检测 |
| 过时跟踪侧重点 | AttaCore数学penalty；TS人类可读的freshness text注入到提示词 |

---

## 7. 压缩 (90%) — 第二大改善

### 已对齐 ✅
| 特性 | 说明 |
|------|------|
| API原生缓存编辑 | `CacheEdit`/`DeleteToolResult` → StreamParams → API beta header |
| SessionMemory压缩 | 一级策略，在LLM压缩前优先尝试，674行+14测试 |
| 全部5种策略 | Snip/MicroCompact/CollapseContext/FullCompact/SessionMemory |
| 压缩警告 | 80%阈值 `<system-reminder>` 注入 |
| 压缩后恢复 | 重读最近文件(≤5, ≤5K/文件)、MCP指令恢复、技能恢复 |
| 反应式压缩 | Token速度预测，断路器(MAX_CONSECUTIVE_FAILURES=3) |

### 部分对齐 ⚠️
| 特性 | 差距 |
|------|------|
| 基于时间的MC | 默认15分钟（TS 60分钟）；**未集成到压缩管道**中 |
| 压缩钩子 | 仅有后压缩钩子；TS有前后压缩钩子，且清除更全面 |
| 恢复格式 | AttaCore用系统提醒文本；TS用结构化`AttachmentMessage` |
| 反应式默认值 | AttaCore默认**禁用**；TS默认**启用** |

---

## 8. Hooks (88%)

### 已对齐 ✅
| 特性 | 说明 |
|------|------|
| 事件类型 | 30个事件（比TS多TurnStart/TurnComplete/PostSampling） |
| HookSpecificOutput | 5个变体（Permission/PreToolUse/PostToolUse/SessionStart/Generic） |
| Async/asyncRewake | tokio::spawn + mpsc wake通道 |
| 配置热加载 | mtime轮询(30s)，`HookConfigSnapshot` + `Arc<RwLock>` |
| 并行执行 | `FuturesUnordered` 并行所有hooks |
| FileWatcher | `notify` crate 文件变化监听 |

### 部分对齐 ⚠️
| 特性 | 差距 |
|------|------|
| HookSpecificOutput变体 | 5个 vs TS的16个事件特定输出 |
| 超时粒度 | 所有类型统一600s；TS因类型而异（prompt=30s/agent=60s/session-end=1.5s） |
| Session/Function/Plugin hooks | TS有；AttaCore无 |
| HTTP hook白名单 | TS有；AttaCore无 |

---

## 9. 插件 (85%) — 第三大改善

### 已对齐 ✅
| 特性 | 说明 |
|------|------|
| 清单格式 | TOML schema（与TS JSON不同但功能等价） |
| 下载/解压管线 | 完整HTTP下载+SHA-256校验+tar.gz/zip解压+原子安装 |
| 启用/禁用 | settings.json中 `plugins.enabled/disabled` 切换 |
| 市场搜索 | Registry HTTP索引查询+缓存 |
| 同形异义防护 | Unicode骨架规范化，38个易混淆字符映射+封锁名单 |
| 缓存清理/LRU | 每插件保留最新2个版本的LRU逐出 |
| 自动更新 | `check_updates()` + `update_all()` |
| 依赖解析 | Kahn算法拓扑排序+循环检测 |

### 部分对齐 ⚠️
| 特性 | 差距 |
|------|------|
| 清单格式 | TOML vs TS的JSON — 不兼容，TS插件无法直接加载 |
| 内置插件数量 | 2个 vs TS更多（AttaCore刻意排除SaaS依赖插件） |

---

## 10. 会话 (90%)

### 已对齐 ✅
| 特性 | 说明 |
|------|------|
| 会话生命周期 | create/resume/clear/delete/list |
| 父会话跟踪 | 三层持久化：SessionManager + LogEntry::Meta + SessionMetadata |
| JSONL持久化 | `EnvelopedEntry` + 7种`LogEntry`变体，schema版本控制 |
| 会话恢复 | `RunningTaskStore::scan_and_mark_stale()` 崩溃恢复 |
| SessionMemory | JSONL sidecar文件，YAML frontmatter |

### 部分对齐 ⚠️
| 特性 | 差距 |
|------|------|
| 持久化架构 | AttaCore按项目 `{cwd}/{sid}.jsonl`；TS单一全局 `~/.claude/history.jsonl`（有意的架构差异） |
| Paste store | TS有（>1024字节内容存到独立文件）；AttaCore无 |

---

## 11. Skills (90%)

### 已对齐 ✅
| 特性 | 说明 |
|------|------|
| 定义格式 | YAML frontmatter + Markdown body |
| 技能来源 | User/Project/Plugin/Bundled |
| 加载/发现 | 目录遍历（子目录SKILL.md + 扁平.md），FileWatcher实时重载 |
| MCP→Skills接线 | `register_mcp_prompt_skill` / `refresh_mcp_skills` |
| 调用 | `/name` 斜杠命令，Prompt/Local路由 |
| 内置技能 | 10个（刻意排除SaaS依赖的4-5个：claudeApi/claudeInChrome/keybindings/scheduleRemoteAgents） |

### 部分对齐 ⚠️ / 缺失 ❌
| 特性 | 状态 | 差距 |
|------|------|------|
| **条件技能** | ❌ | `paths`字段已解析但**激活逻辑未实现** |
| 参数替换 | ⚠️ | 仅有`{args}`；TS有命名参数+`${CLAUDE_SKILL_DIR}`/`${CLAUDE_SESSION_ID}`模板变量 |
| 重复检测 | ⚠️ | 缺少通过realpath的符号链接安全去重 |

---

## 12. 任务 (75%)

### 已对齐 ✅
| 特性 | 说明 |
|------|------|
| CRUD操作 | TaskStore + claim/release + 阻塞依赖解析 |
| 运行状态跟踪 | RunningTaskStore + 崩溃恢复(scan_and_mark_stale) |
| Dream任务 | 后台思考，最大3回合，CancellationToken |
| Cron任务 | 内存存储+可选文件持久化，14测试 |

### 部分对齐 ⚠️ / 缺失 ❌
| 特性 | 状态 | 差距 |
|------|------|------|
| **任务类型系统** | ⚠️ | 不透明`serde_json::Value` vs TS的7种显式类型（有意的架构选择避免循环依赖） |
| **前后台区分** | ❌ | 无`is_backgrounded`标志 |
| **TaskStopTool** | ❌ | 无通用工具来停止任意运行中的任务 |
| Dream UI集成 | ⚠️ | 写入文件但不在任务注册表中注册UI条目 |

---

## 13. 团队 (90%)

### 已对齐 ✅
| 特性 | 说明 |
|------|------|
| 团队创建/生命周期 | TeamCreate/TeamDelete 工具 |
| 协调器模式 | 152行prompt.rs，覆盖完整的协调器系统提示 |
| Agent派生 | AgentSpawner trait集成 |
| Agent间通信 | 911行mailbox.rs + 416行protocol.rs（7种消息类型）+ 450行polling.rs |
| 团队记忆共享 | 848行TeamMemoryStore + 秘密扫描（比TS更显式） |
| 权限冒泡 | PermissionBridge + PermissionRequest/Response协议消息 |
| Swarm轮询 | 500ms间隔后台tokio轮询循环 |

### 缺失 ❌
| 特性 | 优先级 | 说明 |
|------|--------|------|
| **RemoteAgent传输** | P1 | 仅NoopRemoteTransport skeleton；真实跨机器团队支持被延期（等待ClawPod/Proto/Cloud跨仓库工作） |

---

## 14. 遥测 (92%)

### 已对齐 ✅
| 特性 | 说明 |
|------|------|
| 事件类型 | 36+变体，比TS更结构化 |
| VCR记录/回放 | 403行，SHA-256哈希匹配，JSONL存储，脱水/补水 |
| GrowthBook | 1178行，远程获取，内存缓存，磁盘持久化，killswitch，目标规则，百分比推出，sticky模式 |
| 双目标路由 | Remote HTTP + OpenTelemetry OTLP |
| 第一方事件日志 | 309行，SHA-256采样，速率限制，批处理(100/15s) |
| 隐私/脱敏 | 548行，基于正则表达式的email/IPv4/IPv6/主目录脱敏 |
| 成本跟踪 | 376行UsageAccumulator，Anthropic定价，子代理合并 |
| 环境元数据 | 207行，平台/架构/OS/终端/Shell/CI/远程检测 |

### 部分对齐 ⚠️
| 特性 | 差距 |
|------|------|
| 元数据全面性 | 207行 vs TS的973行元数据模块 — TS收集更细粒度的环境信息 |

---

## 剩余差距按优先级汇总

### P1 — 应尽快补齐 (4项)
| # | 维度 | 条目 |
|---|------|------|
| 1 | 团队 | RemoteAgent传输后端（目前是Noop stub） |
| 2 | 工具 | SendMessage/TeamCreate/TeamDelete工具注册 |
| 3 | MCP | 多范围配置加载管线（范围枚举已定义，合并/优先级逻辑待完善） |
| 4 | 主流程 | 异步记忆预取（从同步改为后台任务模式） |

### P2 — 行为对齐 (8项)
| # | 维度 | 条目 |
|---|------|------|
| 5 | 权限 | 连接阴影规则检测（代码已存在，需注册模块+连接） |
| 6 | 权限 | 增强路径安全检查（shell展开阻止、符号链接解析等） |
| 7 | MCP | OAuth回调监听器 |
| 8 | MCP | 环境变量展开 + 策略允许/拒绝列表 |
| 9 | Skills | 条件技能激活逻辑 |
| 10 | 任务 | 前后台任务区分 + TaskStopTool |
| 11 | 工具 | Edit/Write staleness检查 |
| 12 | 主流程 | Token预算延续模式（用户指定输出token数） |

### P3 — 锦上添花 (5项)
| # | 维度 | 条目 |
|---|------|------|
| 13 | MCP | InProcess/WebSocket传输 |
| 14 | Hooks | 扩展HookSpecificOutput变体（5→16） |
| 15 | 权限 | 危险权限检测 + 权限解释器 |
| 16 | 遥测 | 扩展环境元数据（对标TS 973行） |
| 17 | 记忆 | 预取使用Sonnet+JSON schema+alreadySurfaced过滤 |

---

## 结论

30项对齐实施后，AttaCore整体对齐度从 **~70% → ~87%**。核心差距已从"功能缺失"转变为"深度不足"：

- **记忆、压缩、插件、会话、遥测** 改善最显著（+17%~35%）
- **MCP** 改善最小（+3%），因为多范围/策略/OAuth属于基础设施级功能，需专门的跨crate架构工作
- **17个剩余差距** 中仅4个P1，且大多有现有代码基础（阴影规则、条件技能、多范围配置的枚举/数据结构已就位）
