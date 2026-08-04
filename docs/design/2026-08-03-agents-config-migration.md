# AGENTS.md + .agents/ 配置模式迁移与跨工具导入 架构设计

**日期：** 2026-08-03（同日经过三轮讨论修订，见文末"变更记录"）
**基于需求：** 用户在对话中给出的设计结论与后续两轮修正。本文档不重新论证目录命名选择，聚焦于：现状如何、要改成什么样、怎么分阶段落地。

> 记号约定：标注 **[现状]** 的内容均已通过阅读源码确认；标注 **[建议]** 的内容是设计方案，尚未实现。本文档是**决策过程记录**，如果只想查最终目录规范，直接看 `docs/CONFIG_LAYOUT.md`（不含决策取舍推理，只有定论）。

---

## 0. 背景澄清：两层"配置"不要混在一起

AttaCore 仓库里实际存在 **两套不相关的 `.claude` / 技能目录**，设计和实现时必须分清楚：

| 层 | 路径 | 作用 | 是否本文档范围 |
|---|---|---|---|
| **Meta 层**（开发 AttaCore 本身用） | `.claude/skills/atta-*/SKILL.md`（本仓库根目录下） | 给 Claude Code 用的开发工作流 skill，服务于"如何开发 AttaCore" | **不涉及**，保持原状 |
| **产品运行时层**（AttaCore 引擎加载给最终用户用） | `crates/core/src/frozen/{memory.rs,skill.rs}`、`crates/core/src/paths.rs`、`crates/skills/` | AttaCore 作为 agent 引擎，运行在任意宿主项目里时，如何加载该项目的指令文件/技能 | **本文档主题** |

本文档只讨论第二层：AttaCore 引擎运行时对**宿主项目**（用户拿 AttaCore 构建的产品去跑的那个项目，可能是任何代码库）读取指令与技能的目录约定，以及 AttaCore 自身**用户级**私有状态该怎么放。

---

## 1. 现状 [已确认]

### 1.1 指令文件（instruction file）

- 实现位置：`crates/core/src/frozen/memory.rs::collect_memory_files_with()`
- 行为：从 `cwd` 向上walk到文件系统根，**每一层**都检查两个候选文件：`<dir>/AGENTS.md`、`<dir>/.atta/ATTA.md`。两个都存在时两个都读入。
- 顺序：远到近（repo root 在前，cwd 子目录在后）追加到 `memory_blocks`。
- 总长上限：`MAX_CLAUDE_MD_CHARS = 20_000` 字符，超出从最远的一段开始截断。
- **用户级文件当前未接入这条 walk-up 逻辑**——`~/.atta/ATTA.md` 并没有在 `collect_memory_files_with` 里被显式加入候选列表。这是现状的一个不一致点，见 §6 开放问题。

### 1.2 CLAUDE.md → ATTA.md 自动迁移 [勘误]

- 实现位置：同文件 `maybe_migrate_claude_to_atta()`
- 函数体行为（如果被调用）：从 cwd 向上收集每层的 `CLAUDE.md`；用 `.atta/code/migration.json` 记录 mtime 做增量检测；检测到变化后**不询问用户**，直接把内容用 HTML 注释 marker 包裹写入同目录 `ATTA.md`，marker 之后的用户手写内容保留。
- **勘误（实施阶段发现）**：全仓库 grep 确认这个函数**没有任何调用点**，只有定义和 `pub use` 重导出。上面这段"自动静默迁移"描述的是函数体本身写了什么逻辑，**不是当前正在发生的行为**——它从未被接线调用过。函数体已保留（未删除，未接线），作为未来 Phase 3 导入功能的基础。

### 1.3 Skills 加载

- **(a)** `crates/core/src/frozen/skill.rs::collect_skills()`：固定扫描 `~/.atta/code/skills/<name>/SKILL.md`（用户级）、`<cwd>/.atta/code/skills/<name>/SKILL.md`（项目级）、`~/.atta/code/plugins/<plugin>/skills/`（插件级）。产出 `SkillEntry` 列表，用于 system prompt 索引和 `/<name>` slash 调用展开。
- **(b)** `crates/skills/src/manager.rs::SkillManager::discover_for_paths()`：按需触发，从一批文件路径向上找名为 `skills/`（不带点前缀）的目录。
- **内置 skills**：`crates/skills/src/bundled.rs` 硬编码 14 个，disk 同名优先覆盖。

### 1.4 路径体系

`crates/core/src/paths.rs::ConfigPaths`：

```
user_data_dir  = $HOME/.atta/code/     (可用 ATTA_DATA_DIR 覆盖)
local_data_dir = <cwd>/.atta/code/     (可用 ATTA_LOCAL_DATA_DIR 覆盖)
```

下辖：`skills/`、`memory/`、`mcp/`、`sessions/`、`vcr/`、`settings.json`；另有 `crates/plugin/src/cache.rs` 里的 `~/.atta/code/plugins/`。这是 AttaCore **运行时私有状态**的统一根，settings 分层顺序（内置默认 < 用户级 < 项目级 < CLI 参数）不变。

### 1.5 现状小结

| 项 | 现状 |
|---|---|
| 指令文件名 | `AGENTS.md`（已对齐通用约定） + `.atta/ATTA.md`（专有，双轨） |
| Skills 目录 | 用户级/项目级都在 `.atta/code/skills/`，**两级路径结构完全对称** |
| CLAUDE.md 兼容 | 实现了迁移逻辑但从未接线调用（见 §1.2 勘误），实际无效果 |
| Codex/Cursor 兼容 | 无 |
| 用户级私有状态命名空间 | 固定写死 `code` 这个字面量，没有"这是哪个产品/领域实例"的概念 |

---

## 2. 目标状态 [建议，已过三轮修正确认]

### 2.1 项目级目录树

**收紧后的规则：只有"其他 agent 工具真的认得"的东西才进 `.agents/`。** 目前这一条只有 `skills/` 满足（Codex 官方扫描 `.agents/skills/`）。`workflows/agents/rules/hooks` 都是 AttaCore 自己的概念，别的工具不认识，和 memory/mcp/sessions/vcr 一样属于 AttaCore 私有状态，**不进 `.agents/`，进 `.atta/`**。

同时，项目级的 `.atta/` **不再嵌套 `code/` 这一层**——一个项目在同一时刻通常只被一种 Atta 系产品使用，不存在"个人目录下多产品共用同一根目录"那种命名空间冲突的顾虑，直接扁平化更符合 `.codex/`、`.claude/`、`.cursor/` 这些对标工具项目级目录本身就是扁平的惯例。

```
repo/
├── AGENTS.md                 # 项目常驻指令入口（唯一权威指令文件）
├── .agents/                  # 外部事实标准专用，只放"别的 agent 工具认识"的东西
│   └── skills/
│       └── <name>/SKILL.md
└── .atta/                    # AttaCore 私有：运行时状态 + 自家扩展概念，扁平、无 code/ 嵌套
    ├── .imported.json         # 导入决策记录（见 §3.5，从 .agents/ 移到这里——它是私有 bookkeeping，不是跨工具标准）
    ├── settings.json
    ├── memory/
    ├── mcp/
    ├── sessions/
    ├── vcr/
    ├── workflows/             # AttaCore 扩展：命名工作流
    ├── agents/                # AttaCore 扩展：subagent 定义
    ├── rules/                 # AttaCore 扩展：长文档规则，由 AGENTS.md 引用
    └── hooks/                 # AttaCore 扩展：hook 脚本
```

### 2.2 用户级目录树 —— scope 参数化，不写死 `code`

**第三轮修正**：用户级 `.atta/` 下面那层目录名不应该硬编码成字面量 `"code"`。AttaCore 是一个通用 agent 引擎库，**本身不预设、不绑定任何具体 scope**——它不是"一个编码助手产品"，`code` 只是文档里用来举例的一个可能名字（假设某个基于 AttaCore 构建的产品选了这个名字），并不代表 AttaCore 自身有这个身份。应该由**调用方在每次打开项目/启动引擎时，显式告诉引擎自己的 scope 是什么**，引擎只提供"scope 参数化"的能力，不写死默认值到具体某个字符串里，也不允许省略这个参数（"开箱即用的 AGENT 库"意味着换一个 scope 名字就能服务另一个产品，不需要改引擎代码，但每次使用都必须先说清楚"我是谁"）。

```
$HOME/
├── AGENTS.md?                 # [现状遗留待定] 见开放问题1
└── .atta/
    └── <scope>/                # scope 由调用方在打开项目/构建引擎时**必须**显式指定，无默认值；下文的 "code" 仅为举例
        ├── settings.json
        ├── memory/
        ├── mcp/
        ├── sessions/
        ├── vcr/
        ├── plugins/
        ├── skills/             # ~/.atta/<scope>/skills/<name>/SKILL.md —— 不迁移到 .agents/，理由见 §2.1
        ├── workflows/
        ├── agents/
        ├── rules/
        └── hooks/
```

**重要澄清**：AttaCore 是一个通用 agent 库，本身**不预设、不内置任何 scope 值**——它不是"一个 scope=code 的编码助手产品"，而是一个可以被任何领域的产品复用的引擎。下文举例用的 `code` 只是一个**示例值**（假设有个基于 AttaCore 构建的编码助手，选了这个名字），不代表 AttaCore 自带这个身份。真正落地时，调用方**每次打开一个项目/启动引擎，都必须显式传入 scope**，没有隐式默认值可以省略这一步——如果调用方传的是 `code`，落地路径就是 `~/.atta/code/...`；传的是别的名字，就落在别的子目录，和引擎代码本身无关。

**为什么用户级需要 scope、项目级不需要**：用户主目录是一个人机器上所有 Atta 系产品共用的根，如果同一台机器同时装了基于 AttaCore 构建的多个不同产品（比如编码助手、运维助手），它们的私有状态必须用 scope 隔离，否则会互相覆盖。项目目录不存在这个问题——判定"一个项目通常只被一种产品实例使用"，直接扁平化即可（§2.1）。如果这个假设未来被打破（同一个仓库真的被两个不同 scope 的产品同时使用），后来者可以自己在 `<cwd>/.atta/<new-scope>/` 开一个子命名空间，原有扁平数据不受影响——这是一个可退路的不对称，不是双向对称设计，需要在实现时的注释里写清楚。

**引擎侧的 scope 注入建议**（不改代码，只给方向，供实现阶段参考）：
- `crates/core/src/paths.rs::ConfigPaths::from_env` 增加一个 `scope: &str` 参数：`user_default = dirs_home().join(".atta").join(scope)`；`local_default` 不再拼 scope，直接 `cwd.join(".atta")`。
- scope 值的来源：跟着现有 `Builder`（`crates/runtime/src/agent.rs`）的注入风格，给 `Builder` 加一个 `.scope("code")` 方法，或者放进 `EngineConfig`/一个新的最小 `EngineIdentity { scope: String }` 结构体里，由宿主产品在 `Builder::new()` 链路上显式设置；**不建议**在 `paths.rs` 里给 scope 一个隐式默认值（哪怕是 `"code"`），因为默认值一旦写进引擎代码，就又变成"写死"了，只是从 `ConfigPaths::from_env` 挪到了 `EngineConfig::default()` 而已，没有解决问题。应该让"没有显式设置 scope"这件事在类型层面就不可构造（例如 `scope` 是 `Builder::build()` 的必填项），逼着每个基于 AttaCore 的产品在集成时刻意想清楚自己的 scope 是什么。

### 2.3 外部事实标准 vs AttaCore 私有 对照表

| 目录/文件 | 性质 | 依据 |
|---|---|---|
| `AGENTS.md` | 外部事实标准 | Codex 官方约定，逐级向上叠加读取 |
| `.agents/skills/<name>/SKILL.md` | 外部事实标准 | Codex 官方扫描路径 |
| `.atta/workflows/` | AttaCore 私有扩展 | 无外部标准 |
| `.atta/agents/` | AttaCore 私有扩展 | 无外部标准；建议对齐 `crates/plugin::agent_registry` |
| `.atta/rules/` | AttaCore 私有扩展 | 无外部标准；被 `AGENTS.md` 显式引用，不自动加载 |
| `.atta/hooks/` | AttaCore 私有扩展 | 无外部标准；建议对齐 `crates/hooks::config` |
| `.atta/{memory,mcp,sessions,vcr,settings.json,plugins}` | AttaCore 私有运行时状态 | 现状既有，本次不改动语义，只改动路径形状（项目级去掉 `code/`，用户级 `code` 改为可配置 scope） |

### 2.4 `.atta/{agents,workflows,rules,hooks}` 与现有 crate 的关系 [建议]

**`.atta/rules/`** — 纯 Markdown 文档，**不被引擎自动加载**，由 `AGENTS.md` 手写引用（如 `- .atta/rules/architecture.md`），模型用 Read 工具按需读取。故意设计成惰性，避免规则文档不分场合塞进每次的 system prompt 预算。

**`.atta/skills/`（项目/用户各一份，注意不再叫 `.agents/skills` 除了项目级那一份真正对外的镜像）**——

> 这里需要澄清一个容易混淆的点：项目级技能的**唯一权威来源**是 `.agents/skills/`（因为要给 Codex 也能扫到）；`.atta/` 下**不再重复存一份项目级 skills**。用户级技能维持在 `.atta/<scope>/skills/`，因为用户级根本不存在 `.agents/`（§2.2）。也就是说"skills 该放哪"在项目级和用户级答案不同，这是刻意的非对称，需要在 `collect_skills()` 的代码注释里写清楚，避免以后有人"顺手统一"改错。

**`.atta/agents/`** — [建议] 对接 `crates/plugin::agent_registry` 作为其项目级/用户级非插件来源；格式建议为 `<name>.md`（YAML frontmatter + 描述）。与插件 manifest 提供的同名 agent 的优先级顺序未定，见开放问题。

**`.atta/hooks/`** — [建议] 只放钩子脚本本体，**不引入第二套 hook 配置协议**；`settings.json` 仍是唯一的"启用哪些钩子、匹配什么事件"配置源，`.atta/hooks/` 只是脚本存放规范。

---

## 3. 导入功能设计 [2026-08-03 第五轮：决策已定，进入实施]

### 3.0 本轮决策（用户拍板，覆盖之前"上层询问"的模糊表述）

1. **单选，不做多源合并导入**——即便同时检测到 2-3 个来源，也只能选其中一个执行导入，不支持"同时导入 CLAUDE.md 又导入 Cursor 规则"。理由：多源合并会在 `AGENTS.md` 追加顺序、marker 冲突上引入不必要的复杂度，单选足够覆盖"项目历史上用过某一个工具"的主流场景。
2. **检测粒度是进程级，不是 session 级**——一个 daemon 进程生命周期内只探测/询问一次，不随每个新 session 重复。
3. **回调需要超时**——超时后视为"和没注册回调一样"：不导入，且**不写永久性的 `skipped` marker**（超时是"没人来得及答"，不是"用户明确拒绝"，下次进程启动应该继续再问，而不是被永久静默）。
4. **手动触发命令复用现有机制，不新开 RPC 方法**——调查确认 daemon 当前只有 4 个 RPC 方法（`daemon.status`/`daemon.shutdown`/`session.list`/`session.run_turn`），且 `session.run_turn` 的 `message` 字段本来就会先过 `crates/runtime/src/commands.rs` 的斜杠命令解析（`/name args`），命中就直接执行、不过 LLM。但现有 `Command::Local` 的 handler 签名是**同步闭包**、**没有 `cwd` 访问权限**（`Box<dyn Fn(&SlashCommand) -> CommandResult>`），无法直接塞进异步文件 I/O 逻辑，改这个签名会牵连全部 5 个内置本地命令。**采用的复用方式**：不改 `commands.rs`，而是把"导入"实现成一个新的 **`Tool`**（和 `EnterWorktreeTool`/`TeamCreateTool` 同类——这些本来就是异步、有 `ToolContext.cwd` 访问权限的既有抽象），配一个新的 bundled skill（`/import`，和 `/init` 同款）作为触发入口。调用链：`session.run_turn` 的 `message: "/import"` → 斜杠命令展开成 skill prompt → 模型据此调用新 `Import` Tool（不传 `source` = 列出候选；传 `source` = 执行该来源导入）。**RPC 层、`SessionPool`、`commands.rs` 全部不用改一行**，完全靠已有的 skill→Tool 调用路径承载，符合"不用新接口"的要求。

### 3.1 目标（细化）

- **自动路径（进程级 + 回调）**：daemon（或任何嵌入 AttaCore 的宿主）在自己的进程启动点调用一个新的库函数 `maybe_detect_and_import(cwd, callback, timeout)`；`callback` 是 `Option<Arc<dyn ImportCallback>>`——**没有传（`None`）就完全不做检测、不做任何事**，直接满足"没注册回调则不导入，等后续使用者手工做"。
- **手动路径（`/import`）**：任何时候都可用，不受"进程级只问一次"的限制、不查 `.imported.json` 的"已经问过"状态——用户主动要求，就重新探测、重新执行，这是刻意设计成跟自动路径解耦的兜底通道。
- **`.agents/` 存在 → 两条路径都不提示**（自动路径直接跳过；手动路径仍然可以运行，但探测结果会显示"项目已是 AttaCore 格式，没有需要导入的旧配置"）。

### 3.2 检测规则（不变，路径不变）

判断"是否已是 AttaCore 项目"：存在 `.agents/` 目录（含 `skills/` 或为空）即视为已处理。

| 来源工具 | 探测路径 |
|---|---|
| Claude Code | `CLAUDE.md`、`.claude/skills/**/SKILL.md` |
| Codex | 已存在 `AGENTS.md` 但没有 `.agents/skills/` → 提示"补全 `.agents/skills/`"而非整体导入 |
| Cursor | `.cursorrules`、`.cursor/rules/*.mdc` |

### 3.3 触发时机与架构分层 [细化为 3 层]

1. **检测 + 转换执行**（纯函数，`crates/core/src/frozen/import.rs`）：`detect_import_sources(cwd) -> Vec<ImportSource>`、`execute_import(cwd, &ImportSource) -> ImportSummary`。不做任何 IO 之外的副作用，不知道"回调"或"Tool"的存在。
2. **自动触发 + 回调**（新 trait，不挂在 `Builder`/`Settings` 上）：`ImportCallback`（见 §3.7）是一个独立于 session 生命周期的接口，因为检测本身是"进程级"的，而 `Builder`/`Agent` 是"session 级"的——把回调塞进 `Builder` 在语义上是错的。宿主自己在进程入口调用 `maybe_detect_and_import(cwd, callback, timeout)`，与创建 `SessionPool`/第一个 session 无关。
3. **手动触发**（新 `Tool` + 新 bundled skill，见 §3.8）：完全走已有的 skill → Tool 调用链，不涉及 §3.3-2 的回调机制。

**v1 范围声明**：`daemon` 是无头（headless）JSON-RPC 服务，进程启动时刻没有任何客户端连接，没法同步弹一个 UI 对话框等答案。本轮实施中，**`daemon/src/main.rs` 调用 `maybe_detect_and_import` 时传 `None`**——即 daemon 这个产品在 v1 里不使用自动回调路径，效果上和"没接线"一样,用户完全依赖 §3.3-3 的 `/import` 手动路径。自动回调路径的库函数本身仍然完整实现、可测试，留给未来"进程内嵌、能同步弹 UI"的宿主（比如假设中的桌面 App）使用。**这是我做的一个范围假设，如果你希望 daemon 也要在 v1 具备某种自动提示能力（比如通过已有的 turn 事件流 `StreamFrame` 在第一个 session 连接时推一条通知），需要额外设计 daemon 怎么在"进程启动"和"第一个客户端连接"之间做取舍，这本身就是一块不小的新工作量，本轮先不做。**

### 3.4 字段映射规则 [更新目标路径]

| 来源 | 目标 | 转换方式 |
|---|---|---|
| `CLAUDE.md` | `AGENTS.md` | 内容直接迁移，marker 包裹策略同现状 `merge_atta_content()`，但改为用户确认后一次性执行 |
| `.claude/skills/<name>/SKILL.md` | `.agents/skills/<name>/SKILL.md` | 格式同构，直接复制文件 |
| 已存在的 `AGENTS.md`（Codex 风格） | 不转换，只在缺 `.agents/skills/` 时补目录 | — |
| `.cursorrules`（纯文本） | 追加到 `AGENTS.md` 末尾，marker 包裹 | 无 frontmatter，原样迁入 |
| `.cursor/rules/*.mdc` | **[更新]** `.atta/rules/<slug>.md`（不是 `.agents/rules/`，因为 rules 不是跨工具标准，见 §2.1 的收紧规则） | `alwaysApply: true` 的规则同时在 `AGENTS.md` 加一行引用；`globs` 字段仅作文档注释保留，不接自动匹配 |

### 3.5 一次性确认与状态记录 [单选后简化]

marker 文件：`<cwd>/.atta/.imported.json`——AttaCore 自己的私有决策记录，不是任何工具需要读的东西，不放进 `.agents/`。因为改成单选，`source` 字段从"可能多个"简化为"最多一个"：

```json
{
  "decided_at": "2026-08-03T12:00:00Z",
  "decision": "imported",
  "source": "claude_code",
  "sources_seen": ["claude_code", "cursor_mdc"],
  "skipped_reason": null
}
```

- `decision`：`imported`（已执行导入）/ `skipped`（用户明确选了"不导入"）/ 字段整体缺失或文件不存在（等同 `deferred`——包括超时的情况，见 §3.0 第3点：**超时不写 marker**，下次进程启动继续问）。
- `source`：仅当 `decision = imported` 时有值，取 `ImportSourceKind` 的字符串形式（`claude_code`/`codex`/`cursor`），供以后追溯"当初导入的是哪个"。
- `sources_seen`：即便单选，也记录当时检测到的**全部**候选（哪怕没选），供以后人工排查用。

### 3.6 与现状自动迁移的关系（不变）

废弃 `maybe_migrate_claude_to_atta()` 的自动调用；其 marker-包裹合并逻辑作为 `execute_import()` 里 ClaudeCode 分支合并 `AGENTS.md` 的实现参考（不是直接复用，因为目标文件名和 marker 前缀都要变）。

### 3.7 `ImportCallback` trait [新增设计，2026-08-03 第五轮]

```rust
#[async_trait]
pub trait ImportCallback: Send + Sync {
    /// 检测到候选来源时调用；宿主负责把 `sources` 呈现给人类（UI 弹窗/CLI 交互/
    /// 转发给远端客户端等），返回决策。
    async fn on_import_detected(&self, sources: &[ImportSource]) -> ImportDecision;
}

pub enum ImportDecision {
    /// 用户选定导入某一个来源（单选，见 §3.0 第1点）。
    Import(ImportSourceKind),
    /// 用户明确表示不导入——写 `skipped` marker，以后不再自动问。
    Skip,
    /// 未决（比如宿主选择"下次再问"）——不写 marker。
    Defer,
}
```

不挂在 `Builder`/`Settings` 上（理由见 §3.3）。调用方式：

```rust
pub async fn maybe_detect_and_import(
    cwd: &Path,
    callback: Option<&Arc<dyn ImportCallback>>,
    timeout: std::time::Duration,
) -> Option<ImportOutcome> {
    if already_decided(cwd).await { return None; }         // 含 .agents/ 存在判断
    let Some(cb) = callback else { return None; };          // 没注册回调 = 完全不做事
    let sources = detect_import_sources(cwd).await;
    if sources.is_empty() { return None; }
    match tokio::time::timeout(timeout, cb.on_import_detected(&sources)).await {
        Ok(ImportDecision::Import(kind)) => { /* 执行 execute_import + 写 imported marker */ }
        Ok(ImportDecision::Skip) => { /* 写 skipped marker */ }
        Ok(ImportDecision::Defer) | Err(_timeout) => { /* 什么都不写 */ }
    }
}
```

`timeout` 默认给一个合理值（比如 30 秒），不是本轮讨论的决策点，实现时按工程判断定，可后续调整。

### 3.8 手动 `/import` 命令的实现方式 [新增设计，2026-08-03 第五轮]

见 §3.0 第4点的结论。具体落地两个新增件（均为纯新增，不改任何既有分发代码）：

1. **`ImportTool`**（新 `base::tool::Tool` 实现，注册方式和 `EnterWorktreeTool`/`TeamCreateTool` 一致）：
   - 入参 `{ source?: string }`。
   - 不传 `source`：调用 `detect_import_sources(ctx.cwd)`，把结果格式化成文本列表返回（工具结果），模型据此向用户复述"检测到 X、Y，回复其一"。
   - 传 `source`：按 `ImportSourceKind` 匹配，调用 `execute_import()`，写 `imported`/对应 marker，返回执行摘要。
   - **手动路径不查 `.imported.json` 的"已经问过"状态**——用户主动调用就一定重新探测、允许重新执行（哪怕之前已经 `skipped` 过）。
2. **`import` bundled skill**（`crates/skills/src/bundled.rs`，格式同 `init`）：body 指导模型"先无参调用 `Import` Tool 列出候选，再带 `source` 参数调用一次执行"。这是唯一的用户入口——`/import`。

调用链：`session.run_turn { message: "/import" }` → `commands.rs` 斜杠解析命中 `import` → 该 skill 展开成 prompt → 模型调用 `Import` Tool（两次，一次列表一次执行）。RPC 层、`SessionPool`、`CommandRegistry` 均不改动。

---

## 4. 落地阶段划分 [建议，已按三轮修正更新]

**阶段 1 — 路径切换（破坏性，需要主版本号说明）[已实施，见 §8]**
- Skills：项目级 `.atta/code/skills/` → `.agents/skills/`；用户级路径结构不变，只是把原来写死的字面量 `code` 改造成必填的 scope 参数（`code` 仅为举例值，调用方传什么就落在什么子目录下）。
- 项目级私有状态扁平化：`<cwd>/.atta/code/{memory,mcp,sessions,vcr,settings.json,migration.json}` → `<cwd>/.atta/{memory,mcp,sessions,vcr,settings.json}`（`migration.json` 随 §3.6 的迁移逻辑改造一起处理或移除）。
- `ConfigPaths::from_env` 签名改为接收 `scope: &str`；`Builder`（或等价集成点）新增必填的 scope 设置。
- 以上两类路径改动都需要**旧路径 fallback 读取**：发现新路径不存在但旧路径（`.atta/code/...` 项目级 / 用户级字面量 `code`）存在数据时继续读，新路径优先，给存量用户一个平滑过渡窗口。
- 更新 README.md / docs/README.zh.md 目录文档。

**阶段 2 — 指令文件收口 [已实施，见 §8]**
- `collect_memory_files_with()` 停止读取 `.atta/ATTA.md`，只认 `AGENTS.md`。
- 是否补上用户级 `~/AGENTS.md` walk-up，见开放问题。

**阶段 3+4 合并 — 导入功能（三方一次做完）[已实施，见 §8.8]**
- 决策已定（§3.0），三方（Claude Code/Codex/Cursor）检测 + 单选执行一次做完，不再分两阶段——三方检测逻辑本来就类似，拆开反而增加两次改动的开销。
- 交付物：`crates/core/src/frozen/import.rs`（检测+执行+marker）、`ImportCallback` trait（§3.7）、`ImportTool` + `import` bundled skill（§3.8）、daemon 里调用 `maybe_detect_and_import(cwd, None, timeout)` 一次（v1 不接真回调,见 §3.3 的 v1 范围声明）。
- marker 落在 `<cwd>/.atta/.imported.json`。

**阶段 5 — 扩展目录落地**
- `.atta/agents/` 接入 `crates/plugin::agent_registry` 的项目级/用户级发现路径。
- `.atta/hooks/` 作为 hook 脚本存放规范。
- `.atta/workflows/`、`.atta/rules/` 作为纯文档约定，主要工作是 `AGENTS.md` 模板引用方式。

---

## 5. 最终版本状态示例

见 `docs/CONFIG_LAYOUT.md`（本次新增的稳定参考文档，不含决策推理，只给定论 + 示例，供日常查阅）。

---

## 6. 开放问题（需要用户决策）

1. **用户级 `~/AGENTS.md` 是否要接入 walk-up 逻辑？** 现状代码里用户级指令文件实际未被读取。新方案要不要补上 `~/AGENTS.md`（`$HOME` 下的兄弟文件，不放进 `.atta/`）作为最外层？**仍未实施**——阶段 1-2 落地时没有加这个。
2. **阶段 1 的旧路径 fallback 保留多久？** **已实施但未定弃用时间表**——项目级 skills 加了 `.atta/code/skills/` fallback（新路径同名覆盖旧路径），用户级和其余目录直接切换、无 fallback（见 §8）。
3. **`maybe_migrate_claude_to_atta` 是直接移除还是保留一个 `settings.json` 开关？** **已实施**——两者都不是：函数体保留但依然不接线调用（本来就没被调用），本轮只勘误了文档描述，没有新增开关。
4. **`.atta/agents/` 与 `crates/plugin` 插件系统同名 agent 的优先级规则未定**，需要在阶段 5 前定下来。**仍未实施**——本轮只做了 Phase 1-2（目录布局），`.atta/agents/` 本身尚未接入任何加载逻辑。
5. **scope 命名规则未定** → **第六轮彻底解决，不再是开放问题**：`--scope` 整体废弃，daemon 改用 `--scene`，值必须是 `resolve_scene()` 里枚举过的 `coding`/`chat`/`demo` 之一，不支持的值直接启动失败。不再需要"格式校验"，因为合法值本来就是一个封闭枚举，不是自由字符串。见 §9.1。

---

## 7. 变更记录

- **第一轮**：确立 `AGENTS.md` + `.agents/{skills,workflows,agents,rules,hooks}`（项目级）、`~/.agents/skills`（用户级）的初版方案；`.atta/code/` 仅保留运行时私有状态。
- **第二轮**：用户级取消 `~/.agents/`，全部收回 `~/.atta/code/`（`workflows/agents/rules/hooks` 作为 `skills/` 的兄弟目录挂在 `~/.atta/code/` 下）；确认"项目共享、用户私有"的边界原则。
- **第三轮（本次）**：
  - 收紧"进 `.agents/` 的标准"为"仅跨 agent 工具事实标准"——`workflows/agents/rules/hooks` 从 `.agents/` 移回 `.atta/`（项目级）。
  - 项目级 `.atta/` 去掉 `code/` 嵌套层，直接扁平（对齐 `.codex/`/`.claude/`/`.cursor/` 的扁平惯例）。
  - 用户级 `.atta/code/` 的 `code` 从硬编码字面量改为**由宿主产品实例声明的 scope 参数**，引擎不内置默认值，体现"通用 agent 库"定位。
  - 导入 marker 文件从 `.agents/.imported.json` 改到 `<cwd>/.atta/.imported.json`（它是私有 bookkeeping，不是跨工具标准，应遵守同一条收紧规则）。
- **第四轮（勘误）**：纠正一处表述瑕疵——AttaCore 本身是通用 agent 库，**不是**"scope=code 的产品"，`code` 全篇只是举例值；打开项目/启动引擎时 scope 是必填项，没有默认值。

---

## 8. 已实施状态（2026-08-03，Phase 1-2）

本节记录 Phase 1-2（目录布局迁移）的实际落地结果，供后续 Phase 3-5（跨工具导入功能）的实施者对照。**跨工具导入功能本身尚未实施**，仍是设计（§3）。

### 8.1 核心路径抽象

- `crates/core/src/paths.rs::ConfigPaths::from_env(cwd, scope)` — 新增 `scope: &str` 必填参数；`local_data_dir` 扁平化（去掉 `code/`）；新增 `user_workflows_dir/user_agents_dir/user_rules_dir/user_hooks_dir`；删除了 `local_skills_dir()`（项目级 skills 不再挂在 `.atta/` 下）。
- 自由函数 `atta_code_dir()` 改名为 `atta_scope_dir(scope: &str)`。

### 8.2 指令文件与 skills（`crates/core/src/frozen/`）

- `collect_memory_files_with()` 不再读取 `.atta/ATTA.md`，只认 `AGENTS.md`。
- `collect_skills()` / `load_session_skills()`：用户级仍是 `~/.atta/<scope>/skills/`；项目级权威路径改为 `<cwd>/.agents/skills/`，同时保留 `<cwd>/.atta/code/skills/` 的读取 fallback（新路径同名覆盖旧路径）。
- `FrozenContext::collect(cwd, scope)` / `collect_with_options(cwd, opts, scope)` 增加 `scope` 参数，透传到 memory/skills/output-style 三处。
- output-style：用户级 `~/.atta/<scope>/output-styles/`；项目级扁平化为 `<cwd>/.atta/output-styles/`。

### 8.3 daemon（真正的产品入口）[2026-08-03 第六轮更新：`--scope` 已废弃，见 §9]

- ~~新增 `--scope` CLI flag~~ ——**第六轮已整体替换为 `--scene`**，不再有独立的 scope 概念，见 §9.1。本节其余内容仍准确：
- 消除了此前 `daemon/src/main.rs`（两处）+ `daemon/src/config.rs::DefaultDaemonPaths`（一处）三份互相独立、容易失步的 `.atta/code` 拼接逻辑，统一改为调用 `ConfigPaths::from_env(cwd, scope)`。
- 项目级 settings.json 合并路径扁平化：`<project>/.atta/settings.json`（原 `.atta/code/settings.json`）。

### 8.4 项目级扁平化（`.atta/` 去掉 `code/`）

`crates/tools/src/worktree.rs`（`WORKTREES_SUBDIR`）、`crates/team/src/coordinator.rs`（team 目录）均已扁平化为 `.atta/worktrees/`、`.atta/teams/`。

### 8.5 意外发现的真实 bug（本次一并修复，不属于迁移范围但不修就会回归）

实施过程中发现 `crates/runtime/src/agent.rs::Builder::build()`（skill manager 初始化）和 `warmup()`（session 启动时重扫 skills）都独立地把项目级 skills 目录算成 `settings.paths.local_data_dir.join("skills")`——这两处如果不改，扁平化之后会指向一个不存在任何数据的 `.atta/skills/`，导致项目级 skills 彻底失效。已同步改为 `project_root().join(".agents").join("skills")`。（**第六轮更新**：最初这里还加了一份旧路径 `.atta/code/skills/` fallback，第六轮按"还没发布、不搞兼容包袱"的决定整体移除，见 §9.2——不再有任何 skills 旧路径读取。）

### 8.6 机械性一致性修复（Group E + 扩展发现）

以下文件的硬编码 `.atta/code` 字面量按同一规则（用户级加 `scope` 参数，项目级扁平化）同步修改，包含 Explore 阶段就发现的死代码，以及实施中额外发现的模型可见 prompt/文案：

- 死代码（0 真实调用点，仅为保持仓库一致而修改）：`crates/auth/src/store.rs`、`crates/mcp/src/oauth.rs`、`crates/mcp/src/output_cache.rs`、`crates/task/src/{store,dream,running}.rs`
- 模型可见 / 用户可见文案（**必须修，否则内容会误导模型或用户**）：`crates/tools/src/skill_tool.rs`、`crates/tools/src/worktree_tools.rs`、`crates/tools/src/prompts/coding/{skill,worktree_enter}.prompt.md`、`crates/team/src/tool.prompt.md`、`crates/skills/src/bundled.rs`（`init`/`remember`/`skillify`/`updateConfig` 几个内置 skill 的 prompt body，以及 `init` 的目标文件名从 `ATTA.md` 改成 `AGENTS.md`）
- 真实安全逻辑：`crates/tools/src/bash/sandbox.rs` 的 macOS sandbox-exec deny-write 规则——**第六轮已修复**用户级 settings.json 保护写死 `"code"` 的问题，见 §9.4（原文这里写"已知局限"，现已不成立）。

### 8.7 明确排除、留给未来的

- `crates/history/*`（`sessions_root`/`projects_root` 等）——daemon 目前 `history_store: None` 从未真正启用持久化，改了也无法验证，等真正接线那天一起处理（届时也该顺便修 `JsonlHistoryStore::sessions_root()` 忽略 `with_root()` 自定义根这个既有不一致）。
- `crates/runtime/src/agent_tool.rs` 里 `runtime::agent_tool::AgentTool`（区别于 `crates/tools::agent_tool::AgentTool`）的两处硬编码——生产代码里没有真实构造点。
- ~~`crates/runtime/src/turn.rs` 里 `FrozenContext::collect` 拿到错误的 cwd~~ ——**第六轮已修复**，见 §9.3。

### 8.8 导入功能（Phase 3+4）实施结果 [2026-08-03 第五轮]

按 §3.0 的四条决策落地，全部新增代码，未改动任何既有分发逻辑：

- **核心检测/执行**：`crates/core/src/frozen/import.rs`——`ImportSourceKind`/`ImportSource`（三方检测 `detect_import_sources`）、`execute_import`（单来源执行：ClaudeCode 合并 `AGENTS.md` + 拷贝 `.claude/skills/*` 到 `.agents/skills/*`（不覆盖同名项目 skill）；Codex 只补 `.agents/skills/` 空目录；Cursor 合并 `.cursorrules` 到 `AGENTS.md` + 把每个 `.mdc` 转成 `.atta/rules/<name>.md`，`alwaysApply: true` 的额外在 `AGENTS.md` 加引用行）、`.atta/.imported.json` marker 读写（`already_decided`/`mark_imported`/`mark_skipped`）。
- **回调**：`crates/core/src/interface/import_callback.rs`——`ImportCallback` trait（`on_import_detected(&[ImportSource]) -> ImportDecision`，`ImportDecision` = `Import(kind)`/`Skip`/`Defer`，单选）+ `maybe_detect_and_import(cwd, callback, timeout)`。**不挂在 `Builder`/`Settings` 上**——检测是进程级的，`Builder`/`Agent` 是 session 级的，语义上不该混在一起；宿主在自己的进程入口直接调用。超时或 `Defer` 都不写 marker（下次进程启动继续问），只有 `Skip` 才写 `skipped`。
- **手动命令**：新 `ImportTool`（`crates/tools/src/import_tool.rs`，注册在 `Builder::build()` 里，紧挨着 `TaskStopTool`/`TaskOutputTool`）+ 新 bundled skill `import`（`crates/skills/src/bundled.rs`，第 15 个内置 skill）。调用链完全复用既有 skill→Tool 路径（`session.run_turn` 的 `message: "/import"` → 斜杠命令展开 → 模型调用 `Import` Tool），**RPC 层、`SessionPool`、`commands.rs` 一行未改**。手动路径不查 `.imported.json`，每次都重新探测。
- **daemon 接线**：`daemon/src/main.rs` 在解析完 `cwd` 后 `tokio::spawn` 调用一次 `maybe_detect_and_import(cwd, None, 30s)`——按 §3.3 的 v1 范围声明，daemon 本身不注册回调（headless，启动时没有客户端可问），这行调用目前是快速 no-op，真正的用户入口是 `/import`。
- **测试**：`import.rs`/`import_tool.rs` 内联单测覆盖检测（单个/多个来源、Codex gap 的存在与否）、执行（三种来源、marker 写入、skill 目录不覆盖同名项目 skill）、marker 语义（`already_decided` 的三种真值来源）、`merge_marked_section` 的重复调用替换行为、手动命令忽略已 `skipped` 状态。

---

## 9. 第六轮：scope→scene、五项修复、精简去包袱 [2026-08-03，已实施]

代码装上 Rust 工具链后第一次真正编译+跑测试，用户以"以代码为准"复查了一遍整个配置系统，发现几个之前分析阶段没挑出来的真实问题。以下是这一轮的决策与实施结果。

### 9.1 `scope` 概念本身被推翻，改用已有的 `AgentScene`

**决策**：不存在独立的"scope"这个东西；daemon 只应该接受"代码实际支持的 scene"，不支持的输入应该让实例创建失败并返回错误，而不是接受任意字符串。

这个决策同时解决了简报里提的"`--scope` 无输入校验、存在路径穿越风险"——因为现在能到达路径拼接代码的值，只可能是 `daemon/src/main.rs::resolve_scene()` 里显式匹配过的 `"coding"`/`"chat"`/`"demo"` 三个字面量之一，不支持的值在 `resolve_scene()` 就直接 `anyhow::bail!` 返回错误、daemon 启动失败，根本走不到路径拼接那一步。这是"用类型/枚举天然堵死非法输入"，比"接受任意字符串再校验格式"更彻底。

**实施**：
- `daemon/src/main.rs`：`Cli.scope: String` 整个删除，改成 `Cli.scene: String`（`default_value = "coding"`）；新增 `resolve_scene(name) -> anyhow::Result<Arc<dyn AgentScene>>`，只认 `coding`/`chat`/`demo`，其余返回 `Err`（daemon 直接启动失败）。`let scope = scene.id().to_string();` 之后所有原来吃 `cli.scope` 的地方（`DefaultDaemonPaths::from_env`、`ConfigPaths::from_env`、`PathSettings.scope`）改吃这个派生出来的 `scope` 字符串。
- **引擎层（`base::paths`/`PathSettings`/`frozen` 模块）不改**——这些地方本来就只是通用的"给我一个 `scope: &str`，我不管你从哪来"的抽象，这次改动只影响 daemon 怎么产生这个字符串，符合"引擎不内置任何具体身份"的原则一直没变。
- 顺带发现并修复了一个衍生问题：daemon 原来同时构造 `scene_coding`/`scene_chat` 两个场景传给 `SessionPool`，但 `SessionPool::create()` 的所有真实调用点（`run_turn`/`resume_or_create`）**永远只用 `scene_coding`**——`scene_chat` 是从未被用来创建 session 的死字段，只在一处"是否需要 LLM 生成会话名"的判断里被硬编码查询（`self.scene_chat.auto_name_session()`），这本身就是个潜在 bug：不管 session 实际用的是哪个 scene，命名判断永远按 chat 场景的规则走。现在 `SessionPool` 简化成单一 `scene: Arc<dyn AgentScene>` 字段，此问题随之修复。

### 9.2 精简去包袱：移除 skills 的旧路径 fallback

**决策**：不考虑旧路径 fallback，不考虑兼容还没发布过的东西——没有正式发布，不需要背兼容包袱，越精简越好。

**实施**：`crates/core/src/frozen/skill.rs::collect_skills()`/`load_session_skills()`、`crates/runtime/src/agent.rs`（`Builder::build()` 和 `warmup()`）里读取 `.atta/code/skills/` 旧路径并做"新路径同名覆盖"的 fallback 逻辑全部删除——项目级 skills 现在**只**认 `.agents/skills/`，没有第二条路径。对应的两个"旧路径覆盖测试"替换成一条"确认旧路径不再被扫描"的测试。

这条决定只适用于**已经在这次会话里新引入的 fallback**（skills 一项）；至于"项目级私有状态本来就没有 fallback"（memory/mcp/sessions/settings.json）不算新引入的不一致，维持原样。

### 9.3 修复：`FrozenContext::collect` 拿错 cwd + `.atta/scratchpad` 双重嵌套

之前当作"和本次迁移正交的既有 bug"记录、没有修——这轮明确要求修复：

- 新增 `PathSettings::project_root()`：`local_data_dir.parent()`（`local_data_dir` 本身是 `<project_root>/.atta`，取其 parent 就是项目根；没有 parent 时退化为 `local_data_dir` 自身兜底）。
- `crates/runtime/src/turn.rs` 里 `if self.frozen.is_none()` 分支、`crates/runtime/src/agent.rs::warmup()` 都从 `settings.paths.local_data_dir.clone()` 改成 `settings.paths.project_root()`——`FrozenContext::collect`（进而 `AGENTS.md`/git 状态扫描）现在真正扫的是项目根，不是 `.atta/` 目录本身。
- `turn.rs` 里 `scratchpad_dir` 的拼接从 `local_data_dir.join(".atta/scratchpad")`（产生 `.atta/.atta/scratchpad` 双重嵌套）改成 `local_data_dir.join("scratchpad")`（`local_data_dir` 已经是 `.atta/` 本身，不需要再拼一次）。

### 9.4 修复：sandbox.rs 用户级 settings.json 保护改为覆盖所有已知 scene

之前受限于"`ToolContext` 没有 scope/scene 字段，不知道当前 session 用的是哪个 scope"，只能写死保护默认值 `"code"`。scope→scene 之后，可选值收窄成一个**小的、代码里枚举死的封闭集合**（`coding`/`chat`/`demo`），不再需要知道"当前是哪个"——直接把这个封闭集合里的每一个都保护一遍即可。

`crates/tools/src/bash/sandbox.rs` 新增 `const KNOWN_SCENES: &[&str] = &["coding", "chat", "demo"];`（注释注明需要和 `daemon::main::resolve_scene`、`crates/scene/src/scene/*.rs` 保持同步），user-level 的 `settings.json` deny-write 规则从"写死一条 `code`"改成"遍历 `KNOWN_SCENES` 各生成一条"。

### 9.5 修复：daemon `settings.json` 的 `mcp_servers` 不再被丢弃（`permission` 部分保留现状）

**`mcp_servers`**：`daemon/src/main.rs` 里 `daemon_config.mcp_servers`（已经从 settings.json 解析出来）之前直接被丢弃，`Settings.mcp_servers` 永远硬编码 `Vec::new()`。现在改成把 `HashMap<String, McpServerConfig>` 转成 `Vec<serde_json::Value>`（每个对象里塞一个 `name` 字段）写入 `Settings.mcp_servers`。

**必须说明的局限**：这**只是让配置值不再被扔掉**，不代表 MCP server 现在真的会被连接——调查发现 `Builder::build()` 只有在调用方显式构造 `McpManager` 并通过 `.mcp_manager(...)` 传入时才会真正连接 MCP server，而 `SessionPool::create()` 从来没这么做过。真正"让 settings.json 配置的 MCP server 生效"需要在 daemon 启动时把这份配置转成 `McpManager` 再传进去，这是一块单独的、更大的功能（daemon 目前完全没有 MCP 连接能力），不在这次修复范围内。

**`permission_rules`/`permission_mode`**：现状保持不变——daemon 固定用 `AllowAllPermission` + 强制 `BypassPermissions`，`SettingsFile` 里也从来没有解析 `permission` 相关字段的逻辑。代码里的注释写着"IDE plugins manage their own sandbox"，这看起来是有意为之的产品选择（daemon 面向的是自己管沙盒的 IDE 插件客户端），不是遗漏，所以这轮没有动它——如果这个假设不对，需要专门再讨论一次，工作量会比"重新接一下已解析的值"大得多（要从零建 `permission` 的 settings.json 解析）。

### 9.6 补充测试

`crates/core/src/interface/import_callback.rs` 新增 7 个测试，用一个可配置延迟/决策的 `MockCallback` 覆盖 `maybe_detect_and_import` 的全部分支：未注册回调、无可导入来源时不触发回调、已决定过的项目不触发回调、`Import`/`Skip`/`Defer` 三种决策、超时（行为等同 `Defer`，不写 marker）。`crates/tools/src/bash/sandbox.rs` 新增 1 个测试验证 `KNOWN_SCENES` 里每个 scene 都被沙盒保护。

### 9.7 验证

`cargo build --workspace` + `cargo test --workspace --no-fail-fast` 全绿（除了两组确认与本次改动无关的既有环境问题：`team::remote_agent` 17 个 + `tools::web_fetch`/`ping` 3 个，均为本地网络请求在这台机器的沙盒环境下返回异常响应，`git diff` 确认这些文件本轮未改动，失败数量与改动前完全一致）。
