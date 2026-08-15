# 会话与场景不变量

本文记录几条**跨模块的约束**：它们由分散在多个 crate 的代码共同维持，任何一处单独看都不完整，而改错其中一处的后果在别处才会显现。

不属于本文的：某个函数怎么实现、某次改动为什么这么改。前者读代码，后者读 `git log`。

---

## 1. 一个进程可以跑多个场景

场景是无状态的——`CodingScene` / `ChatScene` / `ResearchScene` 都是零字段 unit struct，`Arc<dyn AgentScene>` 只是一组函数。所以同一进程里并存多个场景没有障碍：

- `--scene` 定 daemon 的默认场景，`--scenes chat,research` 追加激活
- `scene.activate` / `scene.deactivate` 运行期增删（`SessionPool::active_scenes`）
- `session.create {"scene": "chat"}` 指定该会话用哪个；未激活的场景返回 `SCENE_NOT_FOUND`
- 每个会话按**自己的场景**解析设置层，包括 `~/.atta/scenes/<scene>/` 下的 settings、skills、agents、plugins

最后一条是后补的。`settings_for_project` 曾按项目单键缓存、并从 pool 自身取 `scope`，于是哪个场景先建好条目，后面所有场景就都读它——`--scene coding` 启动的 daemon 上，一个 `chat` 会话会静默用 coding 的设置层。现在缓存键是 `(project, scene)`，`scope` 取会话自己的场景。

**唯一保留的进程级绑定**：`DaemonPaths::config_root()` 在启动时按首个场景算定。其它场景的目录由 `SessionPool::scene_root` 从 `global_root()/scenes/<id>` 推导；启动场景保留注入的 `config_root()`，因为测试用 `StaticDaemonPaths` 注入的是与该推导无关的临时目录。

### SCENE_MISMATCH

会话的 `Meta.scene` 记录它创建时所属的场景。当请求操作一个属于别的场景的会话时，`daemon/src/server.rs::scene_mismatch_response` 返回 `SCENE_MISMATCH`，不跨边界执行。

适用于 `session.close` / `.delete` / `.fork`——这些只需要是非判断。`session.resume` 不走这个共享辅助函数，因为它成功时还要额外报告 `scene_inferred`。

`Meta.scene` 缺失时（v1 时代的会话）记为 "unknown"，不导致整份文件解析失败；调用方从 resume/fork 请求本身推断场景。

**要点：** `SCENE_MISMATCH` 守的是"这个会话属于哪个场景"，不是"这个 daemon 只能跑一个场景"。多场景并存与它并不冲突——它恰恰是多场景下保证会话不串台的那道闸。

---

## 2. `project_root` 的三态

`daemon` RPC 里 `project_root` 有三种含义，靠 JSON 层面的"键缺失 / 键为 null / 键为字符串"区分，对应 `SessionPool::ProjectSelector`：

| 线格式 | 语义 | 变体 |
|---|---|---|
| 键省略 | 该 pool 的默认项目（`self.cwd`） | `Default` |
| `"project_root": null` | 真正的无项目会话（全局层） | `NoProject` |
| `"project_root": "<path>"` | 指定项目 | `Path` |

serde 的 `Option<Option<T>>` 能免费区分"键缺失"与"键存在"，但 `null` 与字符串的区分需要 wire 层（`server.rs`）显式检查——**在到达 `SessionPool` 之前**。

- `session.create` 对不存在的路径返回 `PROJECT_NOT_FOUND`
- `resume` 容忍项目被移走或删除

`NoProject` 下 `local_data_dir` 取 `global_data_dir` 本身，而非 `<project>/.atta`。

---

## 3. 锁的失效判定

daemon 实例锁（`daemon/src/discovery.rs`）与 team 目录锁（`crates/team/src/lock.rs`）共用同一套判定，实现在 `base::process_identity::decide_stale_lock`。

判定依据是 **pid + 进程启动时间**，不是文件 mtime，也不是超时：

| `ProcessLiveness` | 决策 | 理由 |
|---|---|---|
| `Alive` | `KeepExisting` | 持有者还在 |
| `Unknown` | `KeepExisting` | 无法确认时保守处理——宁可拒绝启动，不可双开 |
| `Reused` | `Reclaim` | pid 被复用（启动时间对不上），原持有者已死 |
| `Dead` | `Reclaim` | 进程不存在 |

**记录启动时间是为了防 pid 复用。** 只比 pid 会把一个碰巧复用了旧 pid 的无关进程误判为锁持有者，从而永久拒绝启动。

`Unknown` 走保守分支（平台无可靠探测手段、或启动时间从未采集到），会打 `warn!`。

---

## 4. Sidechain 会话

子代理（`Agent` 工具、Skill `context: fork`、team 成员）的 transcript 独立成一个会话，通过 `Meta` 里的 `parent_session_id` + `session_kind: Sidechain` 与父会话关联。

- 可经 `HistoryStore::child_sessions` 找到
- 默认不出现在 `session.list`
- 跑完后写 `LogEntry::SessionEnd { state }`，`state` 为 `Completed` / `Failed`

**被取消的 turn 刻意不写这个标记**：取消意味着被打断，不是到达了真正的终点，所以会话必须保持可 resume——和一个还在跑的会话一样。

对已写终止标记的 sidechain 发起 resume 会被拒为 `SIDECHAIN_TERMINAL`（协议细节见 `docs/daemon_rpc_protocol.md`）。

### 关联：`session.close` 的级联

关闭父会话会**级联删除**其全部 sidechain。父会话一旦关闭就没有任何路径能再触达它们，留着就是孤儿。

---

## 5. 磁盘布局

`~/.atta/` 下按**索引维度**分目录，不按产品名或场景分：

| 目录 | 索引键 | 内容 |
|---|---|---|
| `projects/<sanitized-cwd>/` | 项目 | `<session-id>.jsonl` transcript |
| `sessions/<session-id>/` | 会话 | `session_memory.md`、`metadata.json`、输入历史、`prompt_state.json` |
| `memory/` | 全局 | 跨会话记忆（项目级的在 `<项目>/.atta/memory/`） |

sidecar 之所以不能塞进 `projects/<cwd>/`，是因为 `session_sidecar_paths(session)` 只拿得到 session id——查找时没有项目信息可用。两者是并列的两个维度，不是两层嵌套。

历史包袱有两处，`history::migrate` 在 daemon 启动时一次性搬走，幂等且只移不删：

- transcript 曾写在 `sessions/<sanitized-cwd>/`，和 sidecar 挤在同一个根下，于是 `sessions/` 里同时躺着两种命名规则的目录
- `history::path::config_home()` 曾多一段写死的 `code`（产品名，不是场景名），导致这个 crate 眼里的数据根比 `ConfigPaths::global_data_dir` 深一层

区分两类目录靠**目录名能否解析成 `SessionId`**：sanitize 后的 cwd 必然以 `-` 开头（绝对路径的首字符是分隔符），解析不成 base58 id。

---

## 6. 委派深度

委派链由 `EngineConfig::max_agent_depth` 计数约束，而非靠从工具集里摘掉 `Agent`：

- `Builder::agent_depth` → `AgentTool::Inner::depth`
- `Inner::spawn_guard` 在**每一条** spawn 路径上检查（`run_sub_tagged` / `run_sub_inner` / `build_team_member_agent` / `resume_agent`）
- 到顶时返回普通工具错误，模型看得见，可以改为自己做

**过滤工具名不构成约束**，原因有二：`Builder::build()` 会为子会话新建注册表并把 `Agent` 注册进去，与父级传入的工具集无关；`RuntimeAgentSpawner`（Skill fork、team 成员）直接透传 `sub_tools()`，根本不经过 `AgentTool::resolve_tools`。

`CodingScene` 之外的场景若要改这个上限，改的是 `EngineConfig`，不是工具白名单。

---

## 7. 场景的工具面

场景对工具有三种控制，方向不同：

| 方法 | 作用 | 时机 |
|---|---|---|
| `tools()` | 白名单，与注册表取交集——**只能减** | 每次构建 tool defs |
| `extra_tools()` | 贡献注册表里没有的工具——**只能加** | `build()` 中，全部注册之后 |
| `deferred_tools()` | 哪些工具只报名字不带 schema | `build()` 中，最后一步 |

三条硬约束：

1. **`extra_tools()` 重名直接失败**，不覆盖。`InMemoryToolRegistry::get` 返回首个同名项，静默覆盖会让调用点拿到的和模型看到的不是同一个工具。检查必须跑在引擎态工具（`Agent`/`Skill`/`WebSearch`/`Cron*`/`Worktree*`）注册**之后**，否则场景能占用这些名字并赢得查找。
2. **`ToolSearch` 永不 defer**。它是取回 deferred 工具 schema 的唯一途径，藏起自己的 schema 会让其余全部 deferred 工具失联。
3. **`Tool::is_deferred()` 不是策略输入**。它是 `DeferredTool` 包装器设置的输出，`ToolSearchTool` 据此过滤。工具自己声明它没有任何裁剪 schema 的效果——包装器才有——只会让它对 `ToolSearch` 声称"我是 deferred"却照常发完整 schema。

`CodingScene` 用 `RESIDENT_TOOLS` / `ALL_DEFERRABLE_TOOLS` 两个常量表达策略，`every_registered_tool_has_a_deferral_policy` 测试保证新增工具不会漏进常驻集。
