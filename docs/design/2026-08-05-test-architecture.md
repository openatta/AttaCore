# 测试架构设计（2026-08-05）

> 本文档记录 AttaCore 集成测试系统的重构设计与理由。现状扫描结论、待确认问题的讨论过程不在此文档中保留，只记最终结论与依据。实施状态：见文末 §9。

## 0. 背景

现有 `tests/` 已经有一套可用的雏形（`.test` 用例格式、VCR record/replay、LLM 语义比对、API/CLI 双模式），但在配置来源、代码复用、目录语义三处存在结构性问题，导致"录制的回归数据实际提交不上去""daemon 侧测试和 tests/runner 重复维护两份 Agent 构建逻辑""cli_runner 里有一个从未被 daemon 认识的 RPC 方法调用"。本轮重构不是推倒重来，而是在保留现有 `.test`/VCR/双模式骨架的前提下，修掉这几处结构性问题，并补齐"模板项目"和"daemon RPC 客户端"两块缺失的基础设施。

## 1. 目标

1. 测试系统只认一份配置来源：仓库根 `.env`（已在 `.gitignore`）。
2. 区分"录制成本"（存在必要）与"回归成本"（应该趋近于零）：录制用正式模型；回归路径上唯一会
   实际调用 LLM 的地方——比对裁判——用 flash 档模型（**cassette miss 时的兜底穿透不能用 flash**，
   见 §10.1 的修正）。
3. daemon 相关的 LLM 全流程测试只维护一份实现（消掉 `tests/runner` 与 `daemon/tests` 的重复代码）。
4. daemon 的 JSON-RPC 有一个类型化客户端，而不是每处测试各自手写 JSON 拼接/解析（这直接导致了一个真实 bug，见 §2.3）。
5. 有一个配置完整（AGENT/SKILLS/HOOK/RULES/MCP/PLUGIN 全配好）的模板项目/fixture 集合，用例可以在其上验证"配置真的生效"，而不是只测裸 Agent。
6. VCR 录制数据是**本地/CI 专用缓存，不提交进仓库**（见 §10.2 的修正）——第一轮曾计划提交作为回归基线，讨论后改变了决定。

非目标：不改动 daemon/引擎本身的行为；不引入新的测试框架或 CI 系统；不追求覆盖所有 `.test` 边界场景（用例数量后续按需补）。

## 2. 现状问题清单（重构依据）

### 2.1 配置源三头对齐不一致
- `tests/runner`（`--config` 默认值、`run_api.sh`/`run_cli.sh`）读仓库根 `.deepseek`。
- `daemon/tests/vcr_agent_integration_test.rs::load_deepseek_config()` 读 **`~/.deepseek`**（家目录，路径与前者不同）。
- 新建的 `.env`（仓库根，格式相同、字段更全：区分 opus/sonnet/haiku/small_fast_model）没有被任何代码消费。
- 附带 bug：`run_api.sh` 从未 `source` 配置文件，`api_runner.rs`/`comparator.rs` 里直接读的 `ANTHROPIC_MODEL`/`ANTHROPIC_SMALL_FAST_MODEL` 环境变量在 api 模式下实际上永远取的是硬编码 fallback 值（`claude-sonnet-4-6`/`claude-haiku-4-5`），配置文件里填的模型名没有生效。

### 2.2 daemon 测试与 tests/runner 重复实现
`daemon/tests/vcr_agent_integration_test.rs` 里的 `build_model()`/`make_tools()`/VCR 接线，与 `tests/runner/src/api_runner.rs` 逻辑高度重复（复制粘贴关系），两处独立演进，容易漂移。

### 2.3 `cli_runner.rs` 里的 RPC 方法是错的
`cli_runner.rs` 在清理阶段调用 `"session.delete"`，但 `daemon/src/server.rs::dispatch()` 的方法表里根本没有这个方法（真正的方法是 `session.close`，见 `docs/DAEMON_RPC.md` §`session.*`）。因为调用结果没有被检查（`let _ = writer.write_all(...)`），这个 bug 一直没被发现——测试跑完后 session 其实没有被正常关闭。

### 2.4 VCR cassette 目录被 `.gitignore` 排除
`.gitignore` 排除整个 `tests/output/`，但 `tests/README.md`（重构前）要求"提交 `.test` + `output/{name}/` 下的录制数据"，且 `main.rs`/`cli_runner.rs` 把 cassette（`.jsonl`）和人读报告/遥测日志都写进同一个 `tests/output/{case}/{mode}/` 目录——这个目录整体被忽略，意味着**回归用的录制数据实际从未被提交**，"定期更新录制数据保持演进"这个工作流现状是不成立的。

### 2.5 遗留目录
`tests/fixtures/vcr/` 只剩一个标注为"旧版，已迁移"的 `convert.py`，功能已经在 `tests/scripts/convert.py`，是纯遗留物。

## 3. 配置管理设计

### 3.1 唯一配置源

仓库根 `.env`，`export KEY=VALUE` 格式（与现有 `.deepseek` 解析器兼容，无需换 parser，只换默认路径）。字段与用途：

| 变量 | 用途 | 使用场景 |
|---|---|---|
| `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` | 目标 Anthropic 兼容端点 | 所有真实 LLM 调用 |
| `ANTHROPIC_MODEL` | 主对话模型 | 驱动 Agent 主流程——录制**和**回放（含 miss 兜底）都必须用同一个值，见 §3.2 |
| `ANTHROPIC_SMALL_FAST_MODEL` | 快/廉价档模型（flash 档） | 仅 LLM 语义比对裁判（`comparator.rs`），与主流程 VCR 无关，见 §3.2 |
| 其余（`ANTHROPIC_DEFAULT_*_MODEL`/`CLAUDE_CODE_*`） | 预留，当前测试系统不消费 | — |

`.deepseek`（仓库根与 `~/.deepseek` 两处）整体废弃，全部改读 `.env`。

### 3.2 flash 档的适用范围（第二轮已修正，见 §10.1）

- **录制阶段**（`ATTA_VCR_RECORD`）：用 `ANTHROPIC_MODEL`——录制的是"正式模型的真实行为"，必须真实，不能打折扣。
- **回放阶段**（`ATTA_VCR_REPLAY`，即日常回归）：Agent 主流程必须**仍然用 `ANTHROPIC_MODEL`**（包括
  cassette miss 时的 `fallback_on_miss` 穿透）——`crates/telemetry/src/vcr.rs::hash_request` 把
  `params.model` 编进了请求哈希，换成 flash 会导致回放对录制数据 100% miss（详见 §10.1，第一轮曾
  错误地计划"miss 时兜底用 flash"，未落地到代码就在复核时发现不可行）。
- 唯一合法用 flash 档的地方：LLM 语义比对裁判（`comparator.rs`）——它是独立于 VCR 哈希匹配的
  旁路调用，不受这个约束。

### 3.3 实现

新增 `tests/runner/src/config.rs`：

```rust
pub struct TestModelConfig {
    pub base_url: String,
    pub auth_token: String,
    pub model: String,        // ANTHROPIC_MODEL，record 用
    pub fast_model: String,   // ANTHROPIC_SMALL_FAST_MODEL，judge/fallback 用
}

/// 解析 .env（export KEY=VALUE 格式），并把每个键 set_var 进当前进程环境——
/// 这样 api_runner.rs / comparator.rs 里已有的 `std::env::var("ANTHROPIC_MODEL")`
/// 之类的读取，不管是 api 模式还是 cli 模式，都不需要外部 shell 先 `source` 一遍。
pub fn load_env_config(path: &Path) -> anyhow::Result<TestModelConfig>;

/// cli_runner 启动 daemon 子进程时注入的 env 列表（沿用现有实现，只换默认路径）。
pub fn parse_env_file(path: &Path) -> anyhow::Result<Vec<(String, String)>>;
```

`main.rs`：`--config` 默认值从 `.deepseek` 改为 `.env`；`run_api.sh`/`run_cli.sh` 里的 `CONFIG=".deepseek"` 同步改。

## 4. 双运行模式：消掉重复实现

保留现有的两种模式定位不变：

- **`--mode api`**（对应用户说的"B：类似 DAEMON 的测试程序，测 API 完整流程"）：进程内直接构建 `runtime::agent::Builder`，不经过 daemon/socket，白盒验证 Agent 核心流程。
- **`--mode cli`**（对应用户说的"A：基于 DAEMON 做测试"）：拉起真实 `attacored` 进程，走 Unix socket JSON-RPC，黑盒验证 daemon 对外的协议面。

`daemon/tests/vcr_agent_integration_test.rs` 删除——它是 `--mode api` 的重复实现，且用的还是废弃的 `~/.deepseek`。`daemon/tests/` 只保留不涉及真实 LLM 调用的协议级测试（`daemon_e2e.rs`、`rpc_smoke.rs`），这些测试用假 provider/mock 校验 RPC 语义（参数校验、落盘、脱敏等），跟"LLM 会不会正确回复"是两件事，不应该混在一起。

## 5. RPC 客户端设计

新增 crate `tests/rpc_client`，替换 `cli_runner.rs` 里手写的 JSON 拼接/逐行解析。覆盖 `docs/DAEMON_RPC.md` 列出的全部 13 个方法（而不是只有 `cli_runner.rs` 当前用到的 2 个），这样 `daemon/tests/daemon_e2e.rs`、`rpc_smoke.rs` 未来要补黑盒用例时也能复用，不用继续手写字符串。

```rust
pub struct DaemonRpcClient { /* UnixStream 读写半分 + 自增 id */ }

impl DaemonRpcClient {
    pub async fn connect(socket_path: &Path) -> anyhow::Result<Self>;

    // daemon.*
    pub async fn daemon_status(&mut self) -> anyhow::Result<Value>;
    pub async fn daemon_doctor(&mut self) -> anyhow::Result<Value>;
    pub async fn daemon_shutdown(&mut self) -> anyhow::Result<Value>;

    // session.*
    pub async fn session_list(&mut self) -> anyhow::Result<Value>;
    pub async fn session_close(&mut self, session_id: &str) -> anyhow::Result<Value>;
    /// 返回逐帧事件流（session.event StreamFrame）+ 最终 RpcResponse，
    /// 调用方按需收集 text_delta/tool_use/turn_complete。
    pub async fn session_run_turn(&mut self, params: RunTurnParams) -> anyhow::Result<TurnEvents>;

    // config.*
    pub async fn config_set_provider(&mut self, params: Value) -> anyhow::Result<Value>;
    pub async fn config_get_provider(&mut self, include_secrets: bool) -> anyhow::Result<Value>;
    pub async fn config_reload(&mut self) -> anyhow::Result<Value>;

    // mcp.*
    pub async fn mcp_status(&mut self) -> anyhow::Result<Value>;
    pub async fn mcp_add_server(&mut self, name: &str, config: Value) -> anyhow::Result<Value>;

    // import.*
    pub async fn import_list(&mut self) -> anyhow::Result<Value>;
    pub async fn import_run(&mut self, source: &str) -> anyhow::Result<Value>;

    // commands.* / plugin.*
    pub async fn commands_list(&mut self) -> anyhow::Result<Value>;
    pub async fn plugin_list(&mut self) -> anyhow::Result<Value>;
    pub async fn plugin_install(&mut self, params: Value) -> anyhow::Result<Value>;
    pub async fn plugin_uninstall(&mut self, params: Value) -> anyhow::Result<Value>;
    pub async fn plugin_set_enabled(&mut self, name: &str, enabled: bool) -> anyhow::Result<Value>;
}
```

修正 §2.3 的 bug：`session_close()` 对应真实存在的 `"session.close"` 方法，不再调用不存在的 `"session.delete"`。

`cli_runner.rs` 改为基于 `DaemonRpcClient::session_run_turn` + `session_close`，删除手写的 socket 读写/JSON 拼接逻辑。

## 6. 模板项目 fixture 设计

新增 `tests/fixtures/template_project/`，结构对齐 `docs/CONFIG_LAYOUT.md` 定义的项目级目录树（`.agents/skills/` 是外部标准区，`.atta/` 是私有扩展区）：

```
tests/fixtures/template_project/
├── AGENTS.md                       # 指令入口，引用下面的 rules/skills
├── .agents/
│   └── skills/
│       └── code-review/SKILL.md    # 项目级 skill（外部工具也认得的标准位置）
└── .atta/
    ├── settings.json               # hooks + mcp_servers + permission_mode 全配好
    ├── agents/
    │   └── reviewer.md             # 自定义 subagent 类型（frontmatter: name/description/allowed_tools/model）
    ├── rules/
    │   └── testing.md              # 长文档规则，被 AGENTS.md 显式引用
    └── hooks/
        └── pre_bash_log.sh         # settings.json 里 PreToolUse 引用的 command hook
└── src/
    └── main.py                     # 一个最小的示例源码，供 Agent 实际操作（读/改/跑测试）
```

要点：
- `settings.json` 里 `hooks` 字段按 `crates/hooks/src/config.rs::HookConfig::Command` 格式配一条 `PreToolUse`（引用 `.atta/hooks/pre_bash_log.sh`，`if: "Bash"` 过滤），验证 hook 接线（`docs/CONFIG_LAYOUT.md` §12.1）真的生效。
- `mcp_servers` 指向 `tests/fixtures/mcp_toy_server`——一个真实可连接的 stdio MCP server（第一轮曾计划用 `InProcess` 假 server，复核时发现那条 transport 依赖的 `register_in_process_service()` 根本不存在，见 §10.3）。
- `.atta/agents/reviewer.md` 验证自定义 subagent 类型加载（`docs/CONFIG_LAYOUT.md` §12.2）。
- `.atta/rules/testing.md` 被 `AGENTS.md` 显式引用，验证 rules 发现机制（§12.3，只读文件名+首行,不自动展开全文）。
- 用例跑在这个模板项目的**拷贝**上（每次运行 `cp -r` 到临时目录），不直接修改 fixture 本身，保证可重复运行。

## 7. VCR 录制/回放工作流

### 7.1 目录职责拆分（修正 §2.4 的问题；提交与否第二轮又改了一次,见 §10.2）

```
tests/
├── fixtures/
│   ├── template_project/         # §6，随仓库提交
│   ├── mcp_toy_server/           # §10.3，随仓库提交（源码,不是二进制）
│   ├── plugins/demo-plugin/      # §10.4，随仓库提交（源码,不是打包后的 zip）
│   └── cassettes/{case}/{mode}/  # VCR 录制数据（.jsonl）——本地/CI 缓存，不提交，见 §10.2
└── output/                       # 纯生成物：人读 .md 转换、report.json/md、telemetry 日志
                                    # 继续被 .gitignore 排除，运行时可随时删除重建
```

`tests/output/` 与 `tests/fixtures/cassettes/` 现在其实是同一种性质（都是生成物、都不提交），
只是刻意分开放：前者是"这次跑测试产生的、下次跑会覆盖的东西"（报告/日志，看完就扔），后者是
"跨多次运行复用的缓存"（cassette，命中就不用真的调 LLM）——分开放是为了让 `.gitignore` 规则和
清理策略可以分别调整，不是因为两者提交状态不同（两者都已被 `.gitignore` 排除）。

`main.rs`/`api_runner.rs`/`cli_runner.rs` 里 `vcr_dir` 的计算从 `out_dir.join(scenario).join(mode)` 改为 `fixtures_dir.join("cassettes").join(scenario).join(mode)`；telemetry/报告仍然写到 `out_dir`。

### 7.2 更新周期

录制数据在以下情况需要重新录制（`ATTA_VCR_RECORD=<case>`，覆盖旧 cassette）：
- `.env` 里 `ANTHROPIC_MODEL` 升级到新模型版本；
- Scene 系统提示词（`crates/tools/src/prompts/...`）或工具 schema 有实质变化；
- 模板项目的配置（hooks/agents/rules/mcp）发生变化；
- `.test` 用例本身的输入/预期描述改动。

cassette 不进 git 之后（§10.2），"定期更新录制数据保持演进"变成每个开发者/CI 机器本地按需
`ATTA_VCR_RECORD` 刷新，不再有一份大家共享、PR 里能看 diff 的基线——这是本轮为了避免 §10.2
列的那些问题，主动放弃的能力，不是遗漏。

### 7.3 CI/日常回归策略

- 默认 `ATTA_VCR_REPLAY`：零 LLM 调用成本（除 §3.2 的 flash 档裁判）。
- cassette 缺失时的 `fallback_on_miss` 保留（对新增用例友好：本地先跑一次自动补录），穿透调用
  用的模型**必须和录制时一样**（`ANTHROPIC_MODEL`，不能是 flash，见 §10.1）——本地第一次跑某个
  用例、或者 `.env`/prompt/工具集变了导致哈希不匹配时会自动触发,不是 bug。
- 本仓库目前没有 CI 工作流（`.github/workflows/` 不存在）；VCR 回放当前定位是"本地快速迭代"的
  工具，不是"CI 上零成本跑通所有 LLM 回归"的工具——后者需要一套 cassette 的团队共享/缓存机制
  （比如 CI cache、对象存储），这是一个仓库当前范围之外的、有意留白的开放问题。

## 8. 目录结构 Before / After

**Before**（简化，只列关键项）：
```
tests/{README.md, run_api.sh, run_cli.sh, cases/, runner/, scripts/,
       fixtures/vcr/convert.py(遗留),
       output/{case}/{api,cli}/{case}.jsonl+md+report(实际未提交)}
daemon/tests/{daemon_e2e.rs, rpc_smoke.rs, vcr_agent_integration_test.rs(与 tests/runner 重复)}
根目录 .deepseek（示例/占位）, ~/.deepseek（个人，未纳管）
```

**After**（含第二轮追加的 MCP/plugin fixture，见 §10）：
```
tests/
├── README.md                        # 短 stub，@ 引用 docs/TESTING_GUIDE.md（§10.5）
├── run_api.sh / run_cli.sh          # CONFIG=.env
├── cases/*.test
├── fixtures/
│   ├── template_project/            # §6
│   ├── mcp_toy_server/              # §10.3，新增：真实 stdio MCP server 源码（独立 crate）
│   ├── plugins/demo-plugin/         # §10.4，新增：插件源码（未打包，测试时现场 zip）
│   └── cassettes/{case}/{mode}/     # 本地/CI 缓存，不提交，见 §10.2
├── rpc_client/                      # crate，§5
├── runner/
│   ├── Cargo.toml                   # 同时是 [lib] test_runner + [[bin]] attacore-test（§10.6）
│   ├── src/{lib,main,config,fixture,plugin_fixture,api_runner,cli_runner,comparator,reporter,script}.rs
│   └── tests/{fixture_smoke,mcp_toy_server_smoke,plugin_lifecycle_smoke}.rs
├── scripts/convert.py
└── output/                          # 纯生成物，继续 gitignore

daemon/tests/{daemon_e2e.rs, rpc_smoke.rs}   # vcr_agent_integration_test.rs 已删除

docs/TESTING_GUIDE.md                # §10.5，新增：自包含操作手册
根目录 .env                          # 唯一配置源，已 gitignore
```

`tests/fixtures/vcr/`（遗留 `convert.py`）整体删除。

## 9. 实施步骤与状态

第一轮（1-10）+ 第二轮（11-17,见 §10）均已实施并验证（`cargo build --workspace` 通过；相关
测试——`fixture_smoke`、`mcp_toy_server_smoke --ignored`、`plugin_lifecycle_smoke --ignored`、
`daemon_e2e`/`rpc_smoke` 离线部分——全部跑过）。

1. 本文档。
2. 配置源统一（`tests/runner/src/config.rs` + `main.rs`/`run_*.sh` 改用 `.env`）。
3. 新建 `tests/rpc_client` crate，纳入 workspace。
4. `cli_runner.rs` 改用 `rpc_client`，修正 `session.delete` → `session.close`。
5. 新建 `tests/fixtures/template_project/`，含 `--fixture` 挂载机制（`api_runner.rs`/`cli_runner.rs`/`fixture.rs`）。
6. cassette 目录从 `tests/output/` 迁到 `tests/fixtures/cassettes/`。
7. 删除 `daemon/tests/vcr_agent_integration_test.rs`。
8. 删除 `tests/fixtures/vcr/` 遗留目录。
9. 更新 `tests/README.md`（第二轮改为 @ 引用 stub，见 §10.5）。
10. `cargo build --workspace` + 离线单测验证。
11. 修正 flash 兜底设计错误（§10.1）。
12. `tests/fixtures/cassettes/` 加入 `.gitignore`（§10.2）。
13. 新增 `tests/fixtures/mcp_toy_server`——真实 stdio MCP server（§10.3）。
14. 新增 `tests/fixtures/plugins/demo-plugin` + `plugin_fixture.rs` + 端到端生命周期测试（§10.4）。
15. 新增 `docs/TESTING_GUIDE.md`，`tests/README.md` 改为 `@` 引用（§10.5）。
16. `test-runner` 拆出 `[lib]` 面，供集成测试直接调用 `fixture`/`plugin_fixture`（§10.6）。
17. 最终 `cargo build --workspace` + 全部测试验证。

## 10. 第二轮：修正 + 补完 MCP/Plugin harness（同日）

用户复核第一轮结果后指出四个问题：flash 兜底的设计要重新核实是否真的可行、VCR 数据不该进 git、
模板项目的 MCP 需要一个真正能连上的服务而不是预期失败的占位、完全没有 plugin 配置面。逐一处理：

### 10.1 flash 兜底穿透实际上不可行——已改为只保留裁判用 flash

复核 `crates/telemetry/src/vcr.rs::hash_request()` 发现：VCR 的请求哈希把 `params.model`
字符串本身编进了哈希输入（连同 system prompt、工具名列表、消息内容）。这意味着**回放时用来
匹配 cassette 的 key 本身就含精确模型名**——如果 miss 时的兜底穿透换成 flash 模型，不会是
"省一点钱"，而是从此以后这个 turn 用 flash 名字算出来的哈希永远对不上用 `ANTHROPIC_MODEL`
录的 cassette，等于让 replay 对这个 turn 永久失效。第一轮设计文档里"cassette miss 时的兜底
调用统一改用 `ANTHROPIC_SMALL_FAST_MODEL`"这条**从未真正写进代码**（`api_runner.rs` 里主 Agent
流程一直用的是 `ANTHROPIC_MODEL`），只是文档和日志文案这么写了——复核时发现文档描述和哈希机制
矛盾，及时改正,没有产生需要回滚的代码。

修正后的结论：flash 档只用在真正独立于 VCR 哈希匹配的旁路调用——LLM 比对裁判
（`comparator.rs`，已经是这么实现的，本来就没问题）。`tests/runner/src/config.rs`、
`main.rs` 的日志文案、`tests/README.md`/本文档 §3.2 已同步改正措辞。

### 10.2 VCR cassette 不提交进 git

第一轮把 cassette 从 `tests/output/`（gitignore）搬到 `tests/fixtures/cassettes/`（不 gitignore），
理由是"让 PR 能看到 Agent 行为变化的 diff"。用户复核后否决了这个方向,理由：

- cassette 是"prompt 原文 + 完整工具集 + 精确模型名 + 逐 token 的模型输出"的合集,任何一处提示词
  微调、模型升级都会让整份 `.jsonl` 大量失效并重新生成——不是那种"改一行、diff 一行"的可读文件,
  提交它在 git history 里留下的是一堆几乎不可 review 的重录 diff,不是有意义的行为变更记录。
- 这类数据本质上是**可再生的构建缓存**,不是源代码或需要人工维护的基线——`ATTA_VCR_RECORD` 随时能
  重新生成,不需要靠 git 历史保存。

处理：`tests/fixtures/cassettes/` 加入根 `.gitignore`；`tests/fixtures/{template_project,
mcp_toy_server,plugins}` 不受影响,继续提交（这些是源码/配置,不是模型输出）。副作用（有意留白,
见 §7.3）：现在没有"团队共享同一份录制基线"的机制,回归实际上退化为"本地/单机的加速缓存"，跨
机器/CI 复用录制数据需要另外的机制（不在本轮范围）。

### 10.3 `mcp_servers.demo`：从"预期失败的占位"换成真实可连接的 stdio server

第一轮的模板项目用 `McpServerConfig::InProcess { name: "template-project-demo-mcp" }`，README
写明"预期连接失败,用于验证配置链路本身"。复核 `crates/mcp/src/connect.rs`（`spawn_service` 的
`InProcess` 分支）发现：这条 transport 依赖的 `register_in_process_service()`
**在代码库里根本不存在**——不是"没注册所以连不上",是"这个功能从未被实现",`InProcess` 配置无论
指向什么名字,连接永远失败,和是否注册过毫无关系。第一轮"这是刻意设计成失败"的说法站不住脚。

修正：新增 `tests/fixtures/mcp_toy_server`（独立 crate,`rmcp` server 端 `#[tool_router]`/
`#[tool_handler]` 宏 + `stdio()` transport,一个 `ping` 工具,回显 `toy: <message>`）。模板项目
的 `mcp_servers.demo` 换成 `{"type": "stdio", "command": "{{MCP_TOY_SERVER_BIN}}"}`——`command`
是占位符而非绝对路径,因为 fixture 会被拷贝到随机临时目录运行,不能在提交的文件里写死路径;
`tests/runner/src/fixture.rs::resolve_mcp_toy_server_placeholder()` 在拷贝之后、启动
Agent/daemon 之前替换成真实编译产物路径（没编译过会先 `cargo build -p mcp-toy-server`）。
端到端验证：`tests/runner/tests/mcp_toy_server_smoke.rs`（真实 `McpManager::connect_all` 连接
+ `McpToolAdapter::call` 调 `mcp__demo__ping`,断言回显正确）。

### 10.4 Plugin harness：委托测试用插件 + 生命周期冒烟测试

之前完全没有 plugin 配置面。研究 `crates/plugin/src/manifest.rs` 确认 `plugin.toml`
（TOML,`[plugin] name/version` 必填,`skills.include`/`slash_commands`/`mcp.servers`/
`hooks.*`/`agents` 均可选)、以及 `plugin.install` RPC 的 `download_url` 支持 `file://<绝对路径>`
scheme（`checksum` 可省,是特意留的本地/sideload 路径,不需要真的托管一个下载地址）。

新增：
- `tests/fixtures/plugins/demo-plugin/`——提交**未打包的目录**（`plugin.toml` + 一个
  `/toy-greet` slash command + 一个 `SessionStart` hook 脚本),不提交 zip 二进制,保持可 diff/review。
- `tests/runner/src/plugin_fixture.rs::package_demo_plugin()`——测试时现场把这个目录打成 zip、
  算 sha256,返回 `(zip 路径, checksum)`,逻辑取自 `daemon/tests/daemon_e2e.rs` 里已有的
  `build_demo_plugin_zip` 同款打包方式（改成从磁盘目录读,不是内联字符串现造）。
- `tests/runner/tests/plugin_lifecycle_smoke.rs`——真实拉起 `attacored` 子进程（`ATTA_CONFIG_HOME`
  指向临时目录,不碰真实 `~/.atta`),用 `rpc_client::DaemonRpcClient` 走 `plugin.install` →
  `plugin.list`（断言出现）→`plugin.uninstall`,全部通过真实 RPC,不是 daemon crate 内部直接调用。

跑通确认过：安装/查询/卸载都成功,且卸载后确认 `~/.atta/plugins/` 里没有残留（隔离生效）。

### 10.5 `docs/TESTING_GUIDE.md`：面向"不依赖对话上下文"的操作手册

本文档（`docs/design/2026-08-05-test-architecture.md`）记录的是架构决策和理由,不是操作步骤——
不适合直接当"下次接手的人/AI 该怎么跑测试"的指南。新增 `docs/TESTING_GUIDE.md`,`tests/README.md`
收缩成一句话 + `@docs/TESTING_GUIDE.md` 引用,不维护两份内容。

### 10.6 `test-runner` 拆出 `[lib]` 面

为了让 `tests/runner/tests/*.rs` 里的集成测试（`fixture_smoke.rs` 等）能直接调用
`fixture::copy_dir_recursive`/`resolve_mcp_toy_server_placeholder`/`plugin_fixture::
package_demo_plugin` 而不是复制一遍逻辑,`tests/runner/Cargo.toml` 加了一个
`[lib] name = "test_runner"`（原来只有 `[[bin]] attacore-test`）,`main.rs` 从
`mod api_runner; mod cli_runner; ...` 改成 `use test_runner::{api_runner, cli_runner, ...};`。
纯内部结构调整,不影响 CLI 行为。

### 10.7 验证

`cargo build --workspace` 通过;`cargo test -p test-runner -- --include-ignored` 全部通过
（含 `mcp_toy_server_smoke`、`plugin_lifecycle_smoke` 两个真实拉子进程的端到端测试);
`cargo test -p daemon --test daemon_e2e -- --skip run_turn_without_session_id_creates_new
--skip run_turn_nonexistent_session_errors`（两个跳过项需要真实网络,与本轮无关,是既有的环境
限制）全部通过。

## 11. 第三轮：VCR 按轮存储 + 第一次真实录制（同日）

用户要求两件事：cassette 存储要"按轮"，重新录制之后新旧数据不能互相干扰；并且要真的跑一次录制，
产出第一轮基线数据，不只是纸面设计。

### 11.1 按轮存储设计

问题：VCR 的落盘逻辑（`crates/telemetry/src/vcr.rs::save_entry`）永远是 append-only,写进调用方
传入的 `local_vcr_dir/{scenario}.jsonl`。如果多次录制都指向同一个目录/文件,不同批次（换模型前/
换模型后,改 prompt 前/改 prompt 后）的录制条目会堆在同一个文件里,旧条目不会被清理,也没法区分
"这条数据是哪一轮录的"。

没有改动 `crates/telemetry`（生产 crate,VCR 机制本身是通用的,不应该为了这个仓库自己的测试习惯
塞进"轮次"这个概念）。改动完全在 `tests/runner` 调用侧：cassette 路径新增一层
`{scenario}/{mode}/{round}/`，`round` 就是传给 `VcrModel::new()` 的 `local_vcr_dir` 的最后一段
目录名——对 `telemetry` crate 来说,这仍然只是"一个目录里有个 `{scenario}.jsonl`",无感知。

`tests/runner/src/config.rs::resolve_vcr_round(explicit: Option<String>) -> String`，优先级：
`--round` CLI 参数 > `ATTA_VCR_ROUND` 环境变量 > 今天的 UTC 日期（`YYYY-MM-DD`，用已经在用的
`time` crate 的 RFC3339 格式化取前 10 位，不引入新依赖）。选日期做默认值而不是要求每次手动起名：
同一天内反复录制自然归到同一轮（不用纠结命名），过了这天再录默认就是新的一轮——这个默认行为
本身就是"定期演进、按需更新"最省心的落地方式。`main.rs`/`run_api.sh`/`run_cli.sh` 同步接入
`--round`/第二个位置参数。

**权衡记录**：回放不做"跨轮次自动找最近可用录制"的模糊搜索——裸 `ATTA_VCR_REPLAY` 用同一套
默认解析规则,如果录制是"昨天"、回放在"今天"跑,round 默认值会不一样,直接 miss。这是有意选择的
简单模型（轮次互相独立、互不感知），而不是引入"回退到上一个存在的轮次"这种隐式规则——后者会让
"到底回放的是哪一轮"变得不透明。想回放旧轮次必须显式 `--round`。

### 11.2 顺带修的 bug：`run_api.sh`/`run_cli.sh` 里 `convert.py` 指错路径

上一轮把 cassette 从 `tests/output/` 搬到 `tests/fixtures/cassettes/`（§10.2）之后，两个脚本
里生成可读日志那步（`python3 tests/scripts/convert.py tests/output/...`）没有跟着改，一直指向
一个不会再有 `.jsonl` 的旧路径——这一轮顺带修掉，改成算出这一轮实际落盘的
`tests/fixtures/cassettes/{case}/{mode}/{round}/{case}.jsonl`。

### 11.3 第一次真实录制

用 `.env` 里的真实凭证跑 `./tests/run_api.sh 000.c_project`（API 模式，两轮对话：建 C 项目四个
文件 + `make build`/`make run`），产出仓库第一份真实 VCR 录制数据，轮次是运行当天的 UTC 日期。
录制后紧跟着跑一次回放，确认同一轮次内录制的数据能被正确命中、不需要打真实网络请求。

### 11.4 验证

`cargo test -p test-runner --lib`（新增 `config::tests::round_resolution_priority`，覆盖三级
优先级）通过；`cargo build --workspace` 通过。真实录制第一次尝试实际上失败了——过程和根因见
§12，本节记录的只是按轮存储机制本身的验证，不是"录制成功"的记录（那是 §12 的事）。

## 12. 第四轮：真实录制过程中挖出的三个真实 bug（同日）

用户要求"跑一个真实测试并做 VCR，作为第一轮数据"。第一次尝试（`./tests/run_api.sh 000.c_project`）
在真实环境下卡住 21 分钟没有任何反应——这不是纸面设计问题，是三个独立的真实 bug 叠在一起，
只有跑真实流量才会暴露。逐一定位、修复、补回归测试的完整过程：

### 12.1 Bug 1：模型 HTTP 客户端的 SSE 流和首次请求都没有超时保护

`crates/model/src/client.rs`：`HttpAnthropicClient` 只有 TCP `connect_timeout`（30s），连上之后
如果代理/服务端吃掉请求或流中途静默卡死（不报错、不断连），无论是等首次响应
（`send_one` 内的 `.send().await`）还是等 SSE 流的下一个事件（`events.next().await`），都会永远
`pending`——`ClientBuilder::timeout()` 特意没用（会把整个响应体流一起框住，杀死正常的长流式
场景），但代价是完全没有兜底。

修复：新增 `STREAM_IDLE_TIMEOUT`（90s）常量，两处都套 `tokio::time::timeout`：
- `send_with_retry` 内包一层，超时转成 `AnthropicError::StreamInterrupted`（该 variant 已存在、
  已标记可重试，只是从未被这个场景使用过）。
- 新增 `next_with_idle_timeout()` 辅助函数包 `events.next()`，同样转成 `StreamInterrupted`。

回归测试（`crates/model/src/client.rs` 新增 2 个，用 `#[tokio::test(start_paused = true)]` +
`tokio::time::advance` 让 90s 等待在测试里瞬间流逝，不用真的等）：
`idle_timeout_fires_when_stream_goes_silent_after_first_event`、
`idle_timeout_does_not_fire_when_stream_ends_normally`。

### 12.2 Bug 2：`tests/runner/src/api_runner.rs` 静默吞掉 `AgentEvent::Error`

Bug 1 修好后重跑，还是卡到外部 watchdog 超时——但这次日志显示网络层其实已经在正常收发。定位
到 `crates/runtime/src/agent.rs::Agent::run()`：`process_turn()` 返回 `Err` 时,它会发一条
`AgentEvent::Error` 事件,然后回到 `input_rx.recv()` 继续等下一条输入——这对长期运行的对话式
session 是对的行为,但 `tests/runner/src/api_runner.rs` 的事件收集循环里,`AgentEvent::Error`
落进 `_ => {}` 兜底分支被直接丢弃,循环继续等一个不会再来的 `TurnComplete`。也就是说,网络层的
错误其实已经被正确地报出来了,只是没人接住。

修复：`collect` 闭包处理 `AgentEvent::Error` 分支,返回 `Err(...)` 而不是丢弃；外部超时的报错
文案同步改为"没等到任何事件"而不是猜测性的"很可能是没有网络"。

这个修复本身没有新增单测（纯 test-runner 内部的事件路由,不是产品代码逻辑）,但正是这个修复让
**下一个 bug**第一次变得可见——之前它一直被"安静地挂起"掩盖着。

### 12.3 Bug 3（真正的根因）：重复 tool call 去重逻辑产生孤儿消息

Bug 1+2 修好后重跑，很快（几秒内）就报出一个真实的 API 400：

```
unexpected `messages.41.content.0: tool_use_id` found in `tool_result` blocks:
call_01_JmhZNg6TMxMxZwCi8R8Z8269. Each `tool_result` block must have a
corresponding `tool_use` block in the previous message.
```

定位到 `crates/runtime/src/streaming.rs`：模型在同一次响应里对同一个 `(name, input)` 发出了
重复的 `tool_use`（这本身是模型/上游 provider 的行为，不是我们能控制的），去重逻辑正确地跳过了
重复调用的**执行**，但只塞了一条合成的 `tool_result`，从未塞过对应的 `tool_use` 块（那个 push
在 dedup 检查**之后**，被 `continue` 跳过了）——session 历史里出现一条没有对应 `tool_use` 的
`tool_result`，API 直接拒绝。

修复：把 `ToolUse` 消息的记录挪到 dedup 检查**之前**，让重复调用也有它自己的 `tool_use` 块可以配对。

### 12.4 Bug 4：修 Bug 3 之后又暴露的第二个、方向相反的 pairing bug

Bug 3 修好、重新录制，turn 1（建 C 项目文件）完整录成功（真实 490KB cassette），但 turn 2
（构建并运行）又报了一个**方向相反**的 400：

```
messages.6: `tool_use` ids were found without `tool_result` blocks
immediately after: call_01_bMCLmYa22YZs9xK7vsTg1447. Each `tool_use` block
must have a corresponding `tool_result` block in the next message.
```

写了一个假设探测单测（先用测试复现，不再靠猜——见 `streaming.rs` 的
`mixed_concurrent_and_sequential_tool_calls_keep_strict_adjacency`）验证：一次响应里混了一个
并发安全工具（如 `Glob`）和一个非并发安全工具（如 `Bash`），当前实现会产出
`[ToolUse(Glob), ToolUse(Bash), ToolResult(Glob), ToolResult(Bash)]`——每个 `tool_use`/
`tool_result` 各自是独立的 session 消息，顺序上 `ToolUse(Bash)` 后面紧跟的是 `ToolResult(Glob)`
而不是它自己的结果，违反 API 的"tool_use 后面必须紧跟对应的 tool_result"这条硬约束。测试一次
跑就复现了，实锤。

根因：`execute_stream()` 里非并发安全工具会先"drain 当前并发批次的结果"再执行自己，但结果和
`tool_use` 各自单独 push 成一条 session 消息，drain 出来的结果和后面工具自己的 `tool_use`/
`tool_result` 交错，破坏了紧邻关系——这是**架构性**的，不是某个分支漏了一行代码。

修复（比前三个大）：重构 `execute_stream()`，不再"每个 tool_use/tool_result 各自一条 session
消息"，改成按批次分组——把一批（无论并发还是顺序执行）的 `ToolUse` 块攒进
`pending_use_blocks`，对应的 `ToolResult` 块攒进 `pending_result_blocks`，只在"批次边界"（要执行
一个非并发安全工具之前、或整个流结束时）才一次性 flush：一条 assistant 消息（含这批全部
`ToolUse` 块）紧跟一条 user 消息（含这批全部、且恰好匹配的 `ToolResult` 块）。并发执行的时机
完全不变（该并发还是并发），只是"什么时候写进 session"变了。

新增 4 个回归测试（`crates/runtime/src/streaming.rs`）：
- `duplicate_tool_call_still_gets_a_paired_tool_use_block`（Bug 3）
- `mixed_concurrent_and_sequential_tool_calls_keep_strict_adjacency`（Bug 4，复现用例）
- `all_concurrent_batch_flushes_together_at_stream_end`（全并发批次只在流结束时统一 flush）
- `two_sequential_tool_calls_produce_two_paired_batches`（纯顺序执行不受影响，两批各自独立配对）

以及一个共享断言辅助 `assert_adjacent_tool_pairing()`：遍历 session 消息，每条带 `ToolUse` 的
assistant 消息，断言紧跟着的下一条消息的 `ToolResult` id 集合精确匹配——这就是 API 实际校验的
那条规则，不是我们猜的。

### 12.5 四个 bug 修完之后：真实录制/回放全部跑通

`./tests/run_api.sh 000.c_project`（用 `.env` 里的真实 `deepseek-v4-pro[1m]` 凭证）：
- 录制：14 轮真实 API 往返，完整覆盖 `.test` 用例的两个用户回合（建 4 个 C 源文件 + 构建运行），
  无报错，cassette 236KB（`tests/fixtures/cassettes/000/api/2026-08-06/000.jsonl`）。
- 回放：`ATTA_VCR_REPLAY` 跑完整个用例无报错；耗时 2m10s（不是"零成本"的毫秒级）——2.17s 是
  实际 CPU 时间，其余是等待，说明至少有一部分请求命中了 `fallback_on_miss`（真实网络兜底）而
  不是纯 cassette 命中。这是已知、可接受的现状：`hash_request()` 把完整的 dehydrate 后消息内容
  纳入哈希，`dehydrate()` 目前覆盖的不可移植字段清单不是详尽的，仍有极小概率的字段（本轮未继续
  深挖是哪个字段）导致哈希不完全稳定。不影响本轮目标（拿到第一轮可用的真实录制数据、把三个
  会导致挂起/崩溃的真实 bug 修好），留作后续优化项。

### 12.6 验证

`cargo test -p runtime --lib`（71 个，含本轮新增 4 个）、`cargo test -p model`（37 个，含本轮
新增 2 个）全部通过；`cargo test --workspace --lib` 除 `team::remote_agent` 的 HTTP mock-server
测试外全部通过（该失败集合是既有的、和本仓库 sandbox 网络环境相关的已知问题，历次改动都复现
同一批失败，与本轮无关）；`cargo build --workspace` 通过。真实录制 + 回放如 §12.5 所述，两条路径
都已经用真实凭证跑通过,不是纸面验证。

## 13. 第五轮：把 §12.5 的遗留点也解决掉（同日）

用户看完 §12.5 的分析简报后要求"真正解决问题"，不满足于"记录下来留作后续"。加了两样东西，
第二样直接带出了真正的根因（§14）。

### 13.1 VCR hit/miss 日志

`crates/telemetry/src/vcr.rs::VcrModel::stream()` 的 Replay 分支：命中打 `debug`，未命中打
`warn`（带 `scenario`/`hash`/`turn_id`/`available_entries`/`fallback_on_miss`）。以前唯一的症状
是"这次跑得比预期久"——现在一眼能看出是不是真的在打真实网络请求、cassette 是不是压根没加载到
（`available_entries=0` 会立刻排除"round/scenario 传错了"这类问题）。

### 13.2 `ATTA_VCR_STRICT`：诊断用的严格模式

新增环境变量，设置后 `fallback_on_miss` 强制为 `false`——一 miss 立刻报错退出（带上没匹配到的
hash），不再静默打真实网络请求。两处要改：`VcrModel::env_config()`（当调用方传 `None` 走默认解析
时）和 `tests/runner/src/api_runner.rs`（因为它总是显式构造一个 `Some(VcrConfig{...})`，从不落到
`env_config()` 的默认解析路径——第一次实现只改了前者，实测发现 `ATTA_VCR_STRICT=1` 完全不生效，
两处都要改这一课学到了）。

这个模式让后续的整个诊断过程完全免费——不用再猜、不用再花真实调用去试错,一次 `ATTA_VCR_STRICT=1`
回放就能立刻看到卡在哪个 hash、卡了几个。

### 13.3 `dehydrate()` 补 `ls -l` 时间戳

VCR 只 mock 模型的响应,不 mock 工具执行——每次回放,`Bash`/`Write` 等工具都真实重跑一遍。
`ls -la` 这类命令的 stdout 带真实修改时间戳,录制和回放是两个不同的真实时刻,时间戳必然不同,
一旦被写进 session 历史,从那一轮起哈希就对不上了(而且是级联的——错一轮之后模型会生成全新的
tool_use id,后面全部跟着错,不是"漏一轮"那么轻,详见 §14 最终定位到的真正主因)。

`dehydrate()`/`hydrate()` 各加一条：`RE_LS_TIMESTAMP` 正则匹配 `Aug  5 22:59`（近期文件,时:分
形式）和 `Aug  5  2025`（超过约 6 个月的文件,年份形式）两种 BSD/GNU `ls -l` 默认日期格式,统一替换
成 `[LS_TIMESTAMP]`。新增单测验证的是真正要紧的性质——两次不同时间戳的 `ls -la` 输出必须
dehydrate 成同一个字符串（`dehydrate_normalizes_ls_l_timestamps_so_repeated_runs_hash_the_same`、
`dehydrate_normalizes_ls_l_year_form_too`），而不是简单测正则能不能匹配。

## 14. 真正的根因：skills 列表的 `HashMap` 迭代顺序不稳定

用 §13.2 的 `ATTA_VCR_STRICT` 模式一测就发现：连**第一轮**（不涉及任何工具执行历史,和 §13.3 的
ls -l 时间戳问题完全无关）都会 miss,而且每次进程重启,miss 掉的 hash 都不一样——说明有什么东西
是**每次进程启动都随机**的,不是"record 和 replay 两次运行不同"这么简单。

免费定位（不花真实调用）：直接对比两次录制（round `2026-08-06` 和 `2026-08-06-v2`）已经落盘的
`request.system_text`（`VcrEntry` 本来就存了录制时用于哈希的、dehydrate 之后的完整 system
prompt，白拿）。`diff` 出来：两边的 "## Available Skills" 小节包含**完全相同的一组技能**,但
**每次顺序都不一样**。

根因：`crates/skills/src/manager.rs::SkillManager::list()` 直接
`self.skills.read().unwrap().values().cloned().collect()`——底层是 `HashMap<String, SkillInfo>`,
Rust 标准库的 `HashMap` 出于抗 DoS 考虑,每个进程启动时用随机种子重新计算哈希,迭代顺序**逐进程
随机**,不是"未定义但至少稳定"。`crates/runtime/src/turn.rs::build_skills_text()`（构建真实系统
提示词里 "## Available Skills" 那一段的函数）直接把这个乱序列表渲染进文本——同一组技能,每次
进程启动,提示词字节都不一样。

这不是一个"只影响测试"的 bug：大多数 LLM API 按精确前缀匹配做 prompt caching,系统提示词每次
启动都换字节,生产环境里也会导致 system prompt 的 cache 命中率降到接近零,是真实的成本/延迟问题,
VCR 只是把它以"哈希对不上"的形式最先暴露出来。

修复：`SkillManager::list()` 按 `name` 排序后再返回，三个消费方（`build_skills_text` 真实系统
提示词、`SkillTool` 按名字查找、`/skills` 命令列表）全部因为在同一个源头修复而受益，不用各自
改。新增回归测试 `list_is_sorted_by_name_regardless_of_registration_order`：故意乱序注册几个
技能,断言 `list()` 输出按字母序——直接测"排序"这个契约本身,而不是试图（不可靠地）在单次
`cargo test` 里复现随机顺序。

### 14.1 最终验证

重新录制（round `2026-08-06`，覆盖掉之前哈希不稳定的旧录制）+ `ATTA_VCR_STRICT=1` 回放：
**23/23 条全部命中,总耗时从 2m10s 降到约 150ms**（无 WARN 输出，纯 cassette 命中，零真实网络
调用）。§12.5 记录的"遗留点"到此彻底解决，不再是"已知可接受的现状"。

### 14.2 验证

`cargo test -p telemetry --lib`（111 个，含本轮新增 3 个 dehydrate 测试）、
`cargo test -p skills --lib`（20 个，含本轮新增 1 个排序测试）全部通过；`cargo build --workspace`
通过；`cargo test --workspace --lib` 除既有的 `team::remote_agent` 失败集合外全部通过。真实录制
+ 严格模式回放如 §14.1 所述，23/23 命中，已用真实凭证验证，不是推测。
