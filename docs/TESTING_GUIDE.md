# AttaCore 测试指导书

> 自包含操作手册——照着本文档就能跑测试、加用例、扩展 fixture，不需要依赖之前的对话上下文。
> 架构决策和"为什么这么设计"见 `docs/design/2026-08-05-test-architecture.md`；本文档只讲"怎么做"。

## 1. 一分钟总览

- 测试系统在 `tests/`，独立 binary `attacore-test`（`tests/runner/`），支持两种模式：
  - `--mode api`：进程内直接构建 Agent，不经过 daemon，白盒验证核心流程。
  - `--mode cli`：拉起真实 `attacored` 子进程，走 Unix socket JSON-RPC，黑盒验证 daemon。
- LLM 调用用 VCR 录制/回放（`ATTA_VCR_RECORD` / `ATTA_VCR_REPLAY`）：录一次，之后回放不花钱不用网。
- 唯一配置源是仓库根 `.env`（已 `.gitignore`，需要自己创建）。
- 有一个配置完整的模板项目 fixture（`tests/fixtures/template_project/`），覆盖 AGENTS/SKILLS/RULES/HOOKS/MCP；插件另有独立 fixture（`tests/fixtures/plugins/demo-plugin/`）。

## 2. 前置：配置 `.env`

在仓库根目录创建 `.env`（不要提交，已在 `.gitignore`）：

```sh
export ANTHROPIC_BASE_URL=https://your-anthropic-compatible-endpoint
export ANTHROPIC_AUTH_TOKEN=sk-...
export ANTHROPIC_MODEL=claude-sonnet-4-6           # 录制阶段用的正式模型
export ANTHROPIC_SMALL_FAST_MODEL=claude-haiku-4-5  # 仅 LLM 比对裁判用，跟主流程 VCR 无关
```

**`ANTHROPIC_MODEL` 一旦用来录制过 cassette，之后不要改**——VCR 的请求哈希把模型名编进去了（见
§7），换了名字所有 cassette 会 100% miss。如果确实要升级模型，就是"重新录制"的时机（§8）。

## 3. 目录地图

```
tests/
├── cases/*.test                     # 测试用例（输入 + 预期描述）
├── fixtures/
│   ├── template_project/            # 配置完整的模板项目：AGENTS/SKILLS/RULES/HOOKS/MCP
│   ├── mcp_toy_server/               # 真实可用的 stdio MCP 测试服务（独立 crate）
│   ├── plugins/demo-plugin/          # 插件源码 fixture（未打包，测试时现场 zip）
│   └── cassettes/                    # VCR 录制数据本地缓存——不提交 git，随时可删重录
├── rpc_client/                       # daemon JSON-RPC 类型化客户端（crate: rpc-client）
├── runner/
│   ├── src/
│   │   ├── main.rs                   # CLI 入口
│   │   ├── config.rs                 # .env 解析
│   │   ├── fixture.rs                # 模板项目拷贝 + MCP 占位符替换
│   │   ├── plugin_fixture.rs         # demo-plugin 打包
│   │   ├── api_runner.rs / cli_runner.rs
│   │   └── comparator.rs / reporter.rs / script.rs
│   └── tests/
│       ├── fixture_smoke.rs          # 结构性验证：配置文件真的能被解析器认出来
│       ├── mcp_toy_server_smoke.rs   # 端到端：真的能连上 MCP、真的能调工具（#[ignore]）
│       └── plugin_lifecycle_smoke.rs # 端到端：真的能装/查/卸插件（#[ignore]）
└── output/                           # 人读报告/日志，纯生成物，不提交

daemon/tests/
├── daemon_e2e.rs / rpc_smoke.rs      # daemon 协议级测试，不涉及真实 LLM 调用
```

## 4. 跑测试

### 4.1 一键脚本

```sh
./tests/run_api.sh 000.c_project    # API 模式：先录制，再回放验证
./tests/run_cli.sh 000.c_project    # CLI 模式：拉起 daemon，走 JSON-RPC
```

### 4.2 手动跑

```sh
# 录制（第一次跑某个用例，或 .env 里模型/prompt/工具集变了之后）
ATTA_VCR_RECORD=000.c_project cargo run -p test-runner -- \
  --mode api --case tests/cases/000.c_project.test

# 回放（日常回归，不花钱不用网）
ATTA_VCR_REPLAY=000.c_project cargo run -p test-runner -- \
  --mode api --case tests/cases/000.c_project.test

# 加 --compare 触发 LLM 语义比对（用 ANTHROPIC_SMALL_FAST_MODEL，会打一次真实网络请求）
ATTA_VCR_REPLAY=000.c_project cargo run -p test-runner -- \
  --mode api --case tests/cases/000.c_project.test --compare
```

CLI 模式同理，把 `--mode api` 换成 `--mode cli`，需要先 `cargo build -p daemon`（或让脚本自动构建）。

`run_api.sh`/`run_cli.sh` 都接受第二个位置参数指定轮次：`./tests/run_api.sh 000.c_project 002`。
轮次机制详见 §9。

### 4.3 挂载模板项目

任意一次运行加 `--fixture tests/fixtures/template_project`：

```sh
ATTA_VCR_REPLAY=001.hooks_demo cargo run -p test-runner -- \
  --mode api --case tests/cases/001.hooks_demo.test \
  --fixture tests/fixtures/template_project
```

运行器会把该目录拷贝到一个临时工作目录（不改动 fixture 本身），`Settings::load()` 真实读取拷贝出来
的 `.atta/settings.json`，hooks/mcp/agents/rules 全部生效——不是手搭的空配置。MCP server 的 `command`
占位符会在拷贝后自动替换成真实编译出来的 `mcp-toy-server` 路径（没编译过会自动 `cargo build`）。

不需要项目级配置的用例（裸 Agent 冒烟测试）省略 `--fixture` 即可，行为不变。

## 5. 验证配置面各自生效（不跑真实 LLM）

这些测试只验证"配置文件能被真实解析器/连接器认出来"，不涉及网络调用，跑得快，改了 fixture 之后
应该先跑这些确认没弄坏：

```sh
cargo test -p test-runner --test fixture_smoke
```

覆盖：`hooks_config` 能解析成 `hooks::HooksSettings`、`mcp_servers` 能解析成
`mcp::config::McpServerConfig`、`.atta/agents/*.md` 的 frontmatter 能被
`runtime::agent_tool::load_agent_types_from_dir` 认出来、hook 脚本有可执行权限、
`Settings::load()` 指向 fixture 时 `project_root()` 解析正确、MCP 占位符替换机制本身。

## 6. MCP 测试服务（`mcp_toy_server`）

`tests/fixtures/mcp_toy_server/` 是一个真实的、用 `rmcp` 写的最小 stdio MCP server（一个 `ping`
工具，回显 `toy: <message>`）。端到端验证（真实连接 + 真实调用工具，拉子进程较慢，默认 `#[ignore]`）：

```sh
cargo test -p test-runner --test mcp_toy_server_smoke -- --ignored --nocapture
```

**扩展它**：给 `tests/fixtures/mcp_toy_server/src/main.rs` 里的 `#[tool_router] impl ToyServer`
加一个新的 `#[tool(description = "...")] fn xxx(&self, Parameters(XxxRequest {..}): Parameters<XxxRequest>) -> String`
方法即可，参考已有的 `ping`。不要为了单个用例另起一个 server 二进制。

**为什么模板项目里 `mcp_servers.demo.command` 写的是 `{{MCP_TOY_SERVER_BIN}}`**：fixture 会被拷贝
到随机临时目录运行，没法在提交的文件里写死一个绝对路径。真正的替换逻辑在
`tests/runner/src/fixture.rs::resolve_mcp_toy_server_placeholder()`，`--fixture` 挂载时自动调用。

## 7. VCR 录制/回放的关键约束

`crates/telemetry/src/vcr.rs::hash_request()` 用 `system prompt + 工具名列表 + 模型名 +
消息内容` 算一个哈希，回放时按哈希查表。这意味着：

- **模型名是哈希的一部分**——录制和回放（含 cassette miss 时的兜底穿透）必须用同一个
  `ANTHROPIC_MODEL`，不能"回放用便宜模型省钱"，那样会导致所有 cassette 永久 miss。
- **工具的完整 schema 不在哈希里，只有工具名列表**——改了某个工具的参数/描述但没改名字，哈希
  不会变，回放会用旧 cassette 而不会提醒你需要重录。改工具 schema 后要记得手动 `ATTA_VCR_RECORD`。
- **prompt 里任何字面变化都会改哈希**（除了 `dehydrate()` 已经处理的 CWD/HOME/日期/`ls -l`
  时间戳等可移植性占位符）——包括模板项目的配置内容变化：改了 `.atta/settings.json`/
  `AGENTS.md`/skill 描述之类，会导致相关 cassette miss，需要重录。
- **prompt 拼装本身必须是确定性的**——曾经真实踩过一次：skills 列表用 `HashMap` 存,直接
  `.values().collect()` 渲染进系统提示词,同一组技能每次进程启动顺序都不一样,导致连第一轮都会
  随机 miss（已修,见 §11 和 `docs/design/2026-08-05-test-architecture.md` §14）。加新的、会被
  写进 prompt 的动态内容时,想一下"这个集合的遍历顺序是不是稳定的"。

## 8. 何时需要重新录制

`ATTA_VCR_RECORD=<case>` 触发条件：

- `.env` 里 `ANTHROPIC_MODEL` 升级
- Scene 系统提示词或工具 schema 有实质变化
- 模板项目的配置（hooks/agents/rules/mcp/skills）发生变化
- `.test` 用例本身的输入/预期描述改动

cassette 不提交 git（见 §9），所以"重新录制"是本地/每台机器按需做的事，不是走 PR 提交一份新基线。
想保留旧数据对照时，重新录制前用 `--round`/第二个脚本参数换一个新轮次名（见 §9），不要在同一轮
里覆盖。

## 9. VCR cassette 按轮存储——重新录制不会污染旧数据

cassette 路径带一层 `{round}`：`tests/fixtures/cassettes/{scenario}/{mode}/{round}/{scenario}.jsonl`。
`round` 解析优先级（`tests/runner/src/config.rs::resolve_vcr_round`）：

1. `--round <name>` CLI 参数（`run_api.sh`/`run_cli.sh` 的第二个位置参数会转成这个）
2. `ATTA_VCR_ROUND` 环境变量
3. 都没给：今天的 UTC 日期（`YYYY-MM-DD`）——同一天内多次录制自然归到同一轮，不用每次想名字；
   过了这天再录，默认就是新的一轮

**这解决的问题**：同一个 scenario 换模型/改 prompt 后重新录制，如果和旧数据写进同一个文件，
旧的过时条目会一直堆在文件里（VCR 内部按 hash 做 upsert，不会主动清理），不同批次的录制混在一起，
没法区分"这份数据是哪次录的、还能不能信"。按轮分目录之后，每一轮物理上是独立目录，旧轮次原样
留在磁盘上，互不覆盖——想对比"改动前后 Agent 行为差异"，直接翻旧轮次目录里的 `.jsonl`/转换出来
的 `.md` 就行,不需要 git history。

**回放要用哪一轮**：默认解析规则（今天日期）意味着"过了今天"之后，不显式传 `--round`/
`ATTA_VCR_ROUND` 的裸 `ATTA_VCR_REPLAY` 会指向*今天*这个新轮次，而不是自动找最近的旧轮次——如果
只想回放已经录好的某一轮，要显式传 `--round <那一轮的名字>`。这是有意的简单化：不做"跨轮次自动
搜索最近可用录制"的模糊匹配，轮次之间是完全独立、互不感知的。

## 10. VCR cassette 为什么不提交 git

`tests/fixtures/cassettes/` 在 `.gitignore` 里。原因：cassette 是"prompt 原文 + 完整工具集 +
精确模型名 + 逐 token 模型输出"的合集，任何 prompt/模型微调都会让大段内容失效重录，提交它在 git
history 里留下的是不可读的重录 diff，而不是有意义的行为记录；它本质上是可随时用
`ATTA_VCR_RECORD` 重新生成的构建缓存，不是需要人工维护的源——按轮分目录（§9）已经在本地层面
解决了"新旧数据互不影响"的诉求，不需要靠 git 来保留历史版本。

**实际影响**：现在没有"团队共享一份录制基线"的机制——每台机器第一次跑某个用例都要么本地录制
（需要真实 API key），要么走 `fallback_on_miss`（同样需要真实网络，只是自动触发不用手动敲
`ATTA_VCR_RECORD`）。这是本仓库目前没有 CI（`.github/workflows/` 不存在）情况下的合理取舍；如果
以后要接 CI 并且要求"零 LLM 调用跑通全部回归"，需要另外设计 cassette 的团队级缓存/同步机制
（比如 CI cache、对象存储，可以复用按轮存储的目录结构作为缓存 key 的一部分），当前不在范围内。

## 11. 诊断 VCR cassette miss

一次 miss 本身不会报错——`fallback_on_miss`（默认开）会静默打一次真实网络请求兜底，唯一症状是
"这次跑得比预期久"。两个工具专门为诊断这个而存在：

**看日志**：`RUST_LOG=telemetry=debug` 会打印每一次 hit/miss，miss 时带上 `hash`（没匹配到什么）
和 `available_entries`（cassette 是不是压根没加载——`available_entries=0` 说明 round/scenario
传错了，不是内容不一致）。

**`ATTA_VCR_STRICT=1`**：一 miss 就立刻报错退出，不再静默打真实网络。诊断时优先用这个而不是干等——
不然每次试错都要再等一轮真实请求：

```sh
ATTA_VCR_STRICT=1 ATTA_VCR_REPLAY=000.c_project RUST_LOG=telemetry=debug \
  cargo run -p test-runner -- --mode api --case tests/cases/000.c_project.test --round 2026-08-06
```

如果**连第一轮就 miss，且每次重跑 miss 掉的 hash 都不一样**（不是稳定复现同一个 hash），说明有
什么东西在 prompt 里的遍历顺序是逐进程随机的（典型是某处直接遍历了 `HashMap`/`HashSet` 而没排序）
——不是 `dehydrate()` 覆盖面不够的问题，`dehydrate()` 只能处理"内容本身合理地不同"（时间戳、
路径），处理不了"同样的内容，顺序随机"。真实案例见
`docs/design/2026-08-05-test-architecture.md` §14（skills 列表的 `HashMap` 顺序）。

如果是**中间某一轮开始持续 miss**，大概率是 §7 提到的那类不可移植内容（`ls -l` 时间戳之类）没被
`dehydrate()` 覆盖到——把可疑内容加进 `crates/telemetry/src/vcr.rs::dehydrate()`/`hydrate()`，
参考已有的 `RE_LS_TIMESTAMP` 写法。

## 12. Plugin 测试（`demo-plugin`）

`tests/fixtures/plugins/demo-plugin/` 是提交进仓库的插件源码（`plugin.toml` + 一个
`/toy-greet` slash command + 一个 `SessionStart` hook），**没有**提交打包后的 zip——测试时用
`tests/runner/src/plugin_fixture.rs::package_demo_plugin()` 现场打包、算 sha256。

端到端生命周期验证（真实拉起 `attacored` 子进程，`ATTA_CONFIG_HOME` 指向临时目录、不碰真实
`~/.atta`，走真实 `plugin.install`/`plugin.list`/`plugin.uninstall` RPC）：

```sh
cargo test -p test-runner --test plugin_lifecycle_smoke -- --ignored --nocapture
```

**扩展它**：往 `tests/fixtures/plugins/demo-plugin/plugin.toml` 加字段（`skills.include`/
`mcp.servers`/`agents` 等，完整 schema 见 `crates/plugin/src/manifest.rs::PluginManifest`），
对应的资源文件放同一目录下，`package_demo_plugin()` 会把整个目录打包，不需要改打包逻辑本身。
需要多个不同的插件 fixture（比如测试插件之间冲突）时，在 `tests/fixtures/plugins/` 下新建
兄弟目录，仿照 `demo-plugin` 的结构，`plugin_fixture.rs` 加一个对应的 `package_xxx()`。

## 13. 编写新测试用例

`.test` 文件格式：

```text
# 用例说明（第一个 >>>> 之前的内容，前置条件/预期行为等）

>>>>>>>>>>>>>>>>
[第 1 轮输入 — 中文]
<<<<<<<<<<<<<<<<
[第 1 轮预期输出描述 — 自然语言，给 LLM 裁判用，不是精确字符串匹配]

>>>>>>>>>>>>>>>>
[第 2 轮输入]
<<<<<<<<<<<<<<<<
[第 2 轮预期输出描述]
```

规则：系统提示词不翻译；用户输入用中文；预期输出用自然语言描述（语义比对，不是精确匹配）；每轮
独立比对；工具 schema 的 description 保持英文。

步骤：

1. 在 `tests/cases/` 下新建 `{编号}.{name}.test`（如 `001.my_feature.test`）
2. 需要验证 hooks/mcp/agents/rules/skills 配置面时，运行时加
   `--fixture tests/fixtures/template_project`
3. 录制：`ATTA_VCR_RECORD={name} cargo run -p test-runner -- --mode api --case tests/cases/{file}.test [--fixture ...]`
4. 生成可读日志：`python3 tests/scripts/convert.py tests/fixtures/cassettes/{name}/api/{name}.jsonl`
5. 检查日志（`tests/output/{name}/api/{name}.md`），确认 Agent 行为符合预期
6. 提交 `.test` 文件本身（cassette 不提交，见 §9）

## 14. 故障排查

| 现象 | 原因 | 处理 |
|---|---|---|
| `daemon socket not ready after 10s` | daemon 启动失败，多半是缺 `ANTHROPIC_AUTH_TOKEN`（daemon 启动时会校验，哪怕这次测试根本不打真实网络请求） | 确认 `.env` 或运行环境里有 `ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_API_KEY`；调试时把 `Stdio::null()` 换成继承父进程 stdout/stderr 看真实报错 |
| VCR 回放大量 miss / 报 "no fixture for hash" | prompt/工具集/模型名任一变化都会改哈希（见 §7） | 确认 `.env` 的 `ANTHROPIC_MODEL` 没变；如果是有意的 prompt/配置改动，重新 `ATTA_VCR_RECORD` |
| `mcp.status` 看不到 demo server / 连接失败 | `mcp-toy-server` 没编译，或 `--fixture` 没走到 `resolve_mcp_toy_server_placeholder` | 先手动 `cargo build -p mcp-toy-server` 确认能编译；确认用的是 `--fixture` 而不是直接指向原始 fixture 目录（后者不会替换占位符） |
| `plugin.install` 报 checksum 相关错误 | `file://` scheme 本来允许省略 checksum，但如果传了就会校验；`package_demo_plugin()` 打包内容和传给 RPC 的 checksum 要来自同一次打包结果 | 确认没有把上一次打包的 checksum 和这一次的 zip 配对 |
| 改了 `tests/fixtures/template_project/` 后 `fixture_smoke` 挂了 | 结构性测试就是为了在这种时候报警 | 看具体哪个 assert 失败，通常是 JSON 语法错误或字段名拼错；对照 `crates/hooks/src/config.rs`/`crates/mcp/src/config.rs` 的真实类型定义，不要靠猜 |
| 昨天录的 cassette，今天回放却全 miss | 默认轮次是当天 UTC 日期（§9），跨天之后裸 `ATTA_VCR_REPLAY` 会指向新的一天，找不到昨天那一轮 | 显式传 `--round <昨天的日期>`（或录制时用的那个轮次名） |
| 真实录制中途卡住不动很久、或报 API 400（`tool_use`/`tool_result` 配对相关的错误） | 已经修过三个真实 bug（网络层无超时保护、`AgentEvent::Error` 被测试框架吞掉、并发+顺序工具混用时消息分组错误），完整调查过程见 `docs/design/2026-08-05-test-architecture.md` §12 | 如果是全新的类似错误，先跑 `cargo test -p runtime --lib streaming` 确认没有回归；用 `RUST_LOG=trace` 重跑一次定位卡在哪一行，不要凭空猜 |
| 录制卡住不动，几分钟都没反应，CPU 占用是 0 | 大概率是当前进程连不上 `ANTHROPIC_BASE_URL`（比如在某些沙箱/受限网络环境里跑）——`api_runner.rs` 对整轮设了超时（默认 180s，见下），到点会主动取消并报清楚的错，而不是永远挂着；如果 180s 内还没超时说明确实在等 | 确认能从当前网络环境访问 `ANTHROPIC_BASE_URL`（比如 `curl` 一下）；需要更长/更短超时用 `ATTA_TEST_TURN_TIMEOUT_SECS=<秒数>` 覆盖默认的 180 |

## 15. 命令速查

```sh
# 结构性验证（快，不碰网络）
cargo test -p test-runner --test fixture_smoke

# MCP / Plugin 端到端（拉子进程，较慢）
cargo test -p test-runner --test mcp_toy_server_smoke -- --ignored --nocapture
cargo test -p test-runner --test plugin_lifecycle_smoke -- --ignored --nocapture

# daemon 协议级测试（不涉及真实 LLM）
cargo test -p daemon --test rpc_smoke
cargo test -p daemon --test daemon_e2e -- \
  --skip run_turn_without_session_id_creates_new \
  --skip run_turn_nonexistent_session_errors

# 完整用例（真实 LLM 调用，先录后放）
./tests/run_api.sh {case_name}
./tests/run_cli.sh {case_name}
```
