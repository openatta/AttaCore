# AttaCore 测试系统

四层，各自回答不同的问题：

| 位置 | 是什么 | 怎么跑 |
|---|---|---|
| `crates/*/src/**` 的 `mod tests` | 单元测试：一个函数/一个类型的行为 | `cargo test -p <crate>` |
| `daemon/tests/*.rs` | daemon 的 e2e：真的起一个 server，走真的 socket 收发 JSON-RPC | `cargo test -p daemon` |
| `tests/cases/*.test` + `tests/runner` | 行为测试：真的驱动 Agent 跑一个任务，录制/回放模型交互 | `tests/run_api.sh <用例号>` / `tests/run_cli.sh <用例号>` |
| `tests/scripts/*.sh` | 构建面的断言（无插件构建里没有插件依赖等） | `tests/scripts/locked_build.sh` |

## 几个约定

**daemon e2e 不打网络。** `session_lifecycle.rs` 用一个脚本化的
`AnthropicClient` 喂预设事件，所以工具循环、权限门、流式帧都是真的在跑，
只有模型那一端是假的。

**行为测试用录制回放。** `tests/fixtures/cassettes/<用例号>/` 是录好的模型响应；
回放模式（默认）不联网也不花钱。要重录用 `ATTA_RECORD=<用例>`，
需要真的 API key。

**测试进程不碰你的 `~/.atta`。** 每个测试自己拿 tempdir 当状态根；
`daemon/tests/home_is_never_discovered.rs` 保证没有哪个 crate 会自己去找 `$HOME`。
跑测试之后 `~/.atta` 里不该多出任何东西——如果多了，那是 bug。

**协议文档不许漂移。** `daemon/tests/protocol_doc_matches_dispatch.rs` 比对
`docs/daemon_rpc_protocol.md` §6 与 `dispatch` 的分支：文档里写了的必须能调用，
能调用的必须写进文档。

## fixture 项目

`tests/fixtures/template_project/` 是给行为测试用的模板项目，
有 `.atta/settings.json`（hooks + MCP）、`.agents/skills/`、`AGENTS.md`。
用例跑之前会整个拷贝到临时目录，改动不会污染这份原件。

## 磁盘

一次干净构建 **~13 GB**，这不是浪费：每个集成测试文件都是一个独立二进制，静态链接
整张依赖图（光 wasmtime + cranelift 的 rlib 就 507 MB），所以单个测试二进制约 190 MB，
而这样的二进制有几十个。strip 只能省 18 MB——macOS 上调试信息本来就不在二进制里，
那 190 MB 是实打实的机器码。

**变成浪费的是 cargo 从不回收。** 被取代的那一套会原地留着，和活着的那套一样久。

两条规矩：

| 时机 | 做什么 |
|---|---|
| **改完 workspace 版本号** | `cargo clean` |
| 每周 | `cargo sweep --time 7`（`cargo install cargo-sweep`） |

第一条是硬性的：版本号一变，23 个 crate 的指纹全变，整套重编而旧的一份不会被删——
这就是本仓库反复出现"几十 GB 突然爆掉"的直接原因。全量重建约 1 分 10 秒，比排查磁盘
满便宜得多。

`tests/scripts/disk_report.sh` 会报告当前占用、**7 天没被碰过的字节数**（即
`cargo sweep` 能回收的量），以及每一种"多出一套产物"的成因。
