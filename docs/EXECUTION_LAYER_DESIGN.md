# AttaCore 执行层设计

> 承接 `EXTENSIBILITY_DESIGN.md` §3.4（X1–X7）与 §4.5 的契约草案。
> 本文档是阶段 4 的实施蓝图：四个契约一次设计到位，然后全部内建工具改为通过它们工作。

## 0. 这一层是什么

前三个阶段开放的是**决策**——轮次怎么停、错误怎么恢复、上下文怎么压缩、日志怎么读。
它们的共同形状是契约小、默认实现即现有行为、改一处不牵动别处。

执行层不是这个形状。它开放的是**实现**：工具怎么真正碰到这台机器。
一个工具要跑命令、读写文件、发请求，今天这三件事都是它自己直接对操作系统做的。
把它们收到契约后面，宿主才谈得上"换一个地方执行"。

**四个契约必须一组设计**，因为它们互相咬合：沙箱约束的是进程，进程要文件与网络，
而网络策略又要能作用于沙箱里的进程。分开设计必然返工，返工的代价是全部内建工具改两遍。

### 0.1 边界：什么**不**进这四个契约

这是本设计最重要的一节。执行层很容易长成第二个内核——任何跟"跑东西"沾边的
东西都想往里塞。以下几类明确在外，各自已有归属：

| 不进执行层 | 归属 | 理由 |
|---|---|---|
| 重试、超时、缓存、指标 | `tool.around`（`ToolMiddleware`） | 这些是围绕**工具调用**的横切关注，跟在哪儿执行无关。provider 只负责"做这一次"，做不成就如实说 |
| provider 是否健康、是否降级 | `health.check`（`HealthCheck`） | 已有宿主可注入的上报口，不必另造 |
| 留存的时间戳与标识符 | `environment`（`Environment`） | 已有契约。provider 内部测量用的 `Instant` 不走它，这条规则不变 |
| 路径安全（Unicode 归一化、越界写） | `permissions::path_safety` | 是**策略**不是实现。见 §2.2 |
| 沙箱策略与是否施加 | 内核（`X6`，不开放） | provider 决定"如何约束"，内核决定"约束什么、要不要约束" |

### 0.2 与 `EXTENSIBILITY_DESIGN.md` §4.5 草案的两处偏离

草案列了五个 trait：`Subprocess` / `FileSystem` / `Shell` / `Terminal` / `SandboxBackend`。
本设计是 `Process` / `FileSystem` / `Network` / `Sandbox` 四个，外加延后的 `Terminal`。

1. **`Shell` 并入 `Process`。** 一条 shell 命令就是 `program = bash, args = ["-c", cmd]`
   的进程规格——`bash.rs` 今天已经是这么做的（`sandbox::wrap` 产出 `program + args`）。
   保留两个执行契约意味着同一件事有两条路，沙箱要挂两次，也就有两处可能挂漏。
   Shell 是 `Process` 之上的**构造器**，不是并列的执行面。
2. **补上 `Network`（X7）。** 草案漏了它，而它是四者里唯一一个**现在就有实质缺陷**的：
   见 §2.3。

`Terminal`（X4 / PTY）全工作区零实现，是纯新增能力而非迁移，**不阻塞第二个 provider**，
因此排在最后，也可以不做。

---

## 1. 现状实测（2026-08-31）

| 项 | 数 |
|---|---|
| 内建工具 | 37（`crates/tools`），另有 Agent / Team 侧若干 |
| 直连 `Command::new` | 47（全工作区），`crates/tools` 内 20 |
| `crates/tools` 内 `std::fs` / `tokio::fs` | 111 处，12 个文件 |
| 各建 `reqwest::Client` | 10 处，跨 8 个 crate |
| PTY | 0 |

**调用量大，但契约面小。** 111 处文件操作只用到 8 个不同的调用：

| 操作 | 处数 |
|---|---|
| `write` | 62 |
| `read_to_string` | 26 |
| `create_dir_all` | 10 |
| `metadata` | 3 |
| `canonicalize` | 3 |
| `read` | 2 |
| `read_dir` | 1 |
| `remove_dir_all` | 1 |

这个分布决定了迁移的性质：**契约要设计好，但迁移本身是机械的。**
风险不在改不完，在契约形状定错。

---

## 2. 四个契约

四者共用一个错误类型 `ExecError`，因为它们的失败模式是同一类：
目标不可达、被策略拒绝、目标本身报错。工具需要区分这三者——
"provider 连不上"和"文件不存在"对模型是完全不同的两句话。

```rust
pub enum ExecError {
    /// provider 自己不可用（远端不可达、后端未挂载）。
    Unavailable(String),
    /// 策略拒绝（沙箱、网络出口、路径越界）。
    Denied(String),
    /// 目标本身的错误（文件不存在、命令退出非零、HTTP 5xx）。
    Failed(String),
}
```

### 2.1 `Process`

```rust
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub stdin: Option<Vec<u8>>,
}

#[async_trait]
pub trait Process: Send + Sync {
    async fn spawn(
        &self,
        spec: ProcessSpec,
        cancel: CancellationToken,
    ) -> Result<Box<dyn ProcessHandle>, ExecError>;
}

pub trait ProcessHandle: Send {
    /// 增量输出。**必须是流式的**——见下。
    fn output(&mut self) -> BoxStream<'_, Result<OutputChunk, ExecError>>;
    /// 等待退出。
    fn wait(self: Box<Self>) -> BoxFuture<'static, Result<ExitStatus, ExecError>>;
}
```

**流式是硬要求，不是优化。** `bash.rs` 今天把 stdout / stderr 边读边推给
`ProgressSender`，用户在长命令跑的过程中就能看见输出。一个"跑完再一次性返回"
的 provider 会把这个能力静默拿掉——远端实现必须传增量，这是设计约束。

`timeout` 不在 `ProcessSpec` 里：那是 `tool.around` 的事（§0.1）。
provider 收到 `cancel` 就要停，怎么判断该停是上面的判断。

### 2.2 `FileSystem`

```rust
#[async_trait]
pub trait FileSystem: Send + Sync {
    async fn read(&self, path: &Path) -> Result<Vec<u8>, ExecError>;
    async fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), ExecError>;
    async fn create_dir_all(&self, path: &Path) -> Result<(), ExecError>;
    async fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>, ExecError>;
    async fn remove_dir_all(&self, path: &Path) -> Result<(), ExecError>;
    async fn metadata(&self, path: &Path) -> Result<Metadata, ExecError>;
    async fn canonicalize(&self, path: &Path) -> Result<PathBuf, ExecError>;

    /// 26 处调用点要的是这个；默认实现 = `read` + UTF-8 校验。
    async fn read_to_string(&self, path: &Path) -> Result<String, ExecError> { /* … */ }
}
```

**整值读写，不流式。** 跟 `Process` 相反，理由是全部 111 个现有调用点都是整值的，
而且工具结果本来就有预算上限（P3-7 的 `ToolResultBudget`，单条 50 KB）。
为一个没有消费者的能力设计流式接口，会让每个实现都得实现它。

**路径安全留在契约之上。** `permissions::path_safety::check_write` 是**策略**：
它回答"这个路径允不允许写"，跟文件在哪台机器上无关，也不该由 provider 来决定
——一个 provider 能决定自己的越界规则，就等于能取消它。

但有一处必须穿过契约：**符号链接由 provider 解析**。远端的符号链接图是远端的，
本地 `canonicalize` 一个远端路径没有意义。所以顺序是：

```
provider.canonicalize(path)  →  path_safety::check_write(canonical, policy)  →  provider.write(…)
```

`check_write_resolve_symlinks` 现在自己做解析，迁移时要改成接收已解析的路径。
这是 X2 迁移里唯一一处不是机械替换的地方。

### 2.3 `Network`

四个契约里唯一一个**修的是现存缺陷**而不是铺路的。

`settings.sandbox.allowed_domains` 是一条配置里写着、用户合理认为生效的网络策略。
实测：它只作用于 Bash 沙箱。`WebFetch` 与 `Ping` 的源文件里对它**零引用**，
各自建自己的 `reqwest::Client`。更容易误导的是 `WebSearch` 有一个**同名但无关**的
`allowed_domains`——那是过滤搜索结果的工具入参——让这条策略看起来是覆盖的。

```rust
/// 这次出网是谁要求的。
pub enum Origin {
    /// 模型选的目标：WebFetch 的 url、Ping 的主机、Bash 里的 curl。
    Agent,
    /// 运维配的端点：模型 API、MCP server、遥测、插件市场、OAuth。
    Operator,
}

#[async_trait]
pub trait Network: Send + Sync {
    async fn send(&self, req: HttpRequest, origin: Origin) -> Result<HttpResponse, ExecError>;
    /// 响应头到了、body 还没读。默认实现 = `send` 之后当作一整块。
    async fn open(&self, req: HttpRequest, origin: Origin) -> Result<HttpStream, ExecError>;
    /// 预检，给不发请求也要判断的调用点（Ping、沙箱 profile 生成）。
    fn permits(&self, host: &str, origin: Origin) -> bool;
}
```

**`open` 是硬要求**，理由和 §2.1 的流式一样：Messages API 是一条挂着整个回答的
SSE 流，一个只会交出完整 body 的出口承载不了引擎发得最多的那个请求；而且退避策略
是在第一个 body 字节存在之前、按状态码和响应头分类的。

`HttpRequest` 上另有两个字段，都是**策略正确性**而非便利：

- `max_redirects` —— provider 自己跟重定向时**每一跳都要重新过策略**，否则一个
  被允许的主机可以把模型交给一个不被允许的主机，allowlist 只对链条的第一跳成立。
  `WebFetch` 要 0：下一个 URL 抓不抓是模型的决定，不是出口的。
- `max_bytes` —— 工具不再自建 client 之后，`WebFetch` 的 5 MB 上限没别处可放。

**`allowed_domains` 只作用于 `Origin::Agent`。** 这是本设计里最容易搞错的一条，
所以写在这里：把它作用到模型 API 上会让 agent 连自己的大脑都够不着。
运维配置的端点是运维选的，不是模型选的——**这条策略回答的是"模型能够到哪里"，
不是"这个进程能连出去哪里"。** 两者混为一谈正是今天这条策略名不副实的根源之一。

`Origin::Operator` 仍然走同一个契约，因为可审计、可限速、可离线这三件事对它同样成立
（一次离线运行要能把遥测和市场也断掉），只是不受 `allowed_domains` 约束。

> **兑现程度，如实记录（2026-08-31 复核）**：契约这一侧做完了——十一个客户端构造点
> 全部收敛，`Origin` 分流生效，工具侧的 provider 由宿主经 `ExecProviders` 注入。
> **但运维流量那一侧还缺注入点**：`HttpAnthropicClient` / `OAuth2Client` /
> `RegistryResolver` / 遥测各自的 `with_network` 在生产里**没有调用方**，它们各建一个
> `LocalNetwork::default()`。行为是对的（`Operator` 一律放行），但宿主换不掉它们，
> 所以"可审计 / 可限速 / 可离线"目前只对 `Origin::Agent` 成立。
> 补齐要把宿主的 `Network` 从 `Builder` 一路传到 `ModelFactory` 与各客户端构造器 ——
> 见 `PHASE_4_TASKS.md` §3 的 L-7。

**超时不进 `HttpRequest`。** 按 §0.1 它属于 `tool.around`，而且 provider 给流
加一个整体超时就等于把 SSE 回答拦腰截断。各调用点保留自己原有的超时。

**跨 crate 的代价要说清楚**：11 个客户端构造点分布在 9 个 crate 里，其中
`crates/model` 与 `crates/auth` 在依赖图上低于 `tools`。契约因此必须放
`crates/core`，且这两个 crate 会因此依赖它。这是 X7 无法回避的耦合。

**留在外面的一处**：MCP 的 streamable-HTTP 传输由 `rmcp` 自己持有，撬开它不属于
这一层的工作。

### 2.4 `Sandbox`

现有的 `sandbox::wrap` 已经是正确形状——它是个**纯函数**，把打算跑的命令
变换成实际要跑的命令，并如实报告策略兑现了多少。契约化基本是搬家：

```rust
pub trait Sandbox: Send + Sync {
    fn confine(&self, spec: ProcessSpec, policy: &SandboxPolicy) -> Confined;
}

pub struct Confined {
    pub spec: ProcessSpec,
    pub enforcement: Enforcement,
    /// 策略里这个后端兑现不了的部分，逐条列出。空 = 全部兑现。
    pub unmet: Vec<String>,
}

pub enum Enforcement { Full, Partial, None }
```

**`Enforcement` 现在要长出 `Partial`。** 今天它刻意只有两值，代码里写着理由：
"在任何后端会报告它之前加这个区分，是一个没有生产者的区分"。阶段 4 之后生产者出现了——

- Linux 的 bwrap 没有域名级网络过滤，`Allowlist` 在那儿退化成整体断网。
  今天这是 `Full` + 一条 warn 日志，诚实的说法是 `Partial` + `unmet: ["domain allowlist"]`。
- 进程内 provider（§3）根本不约束真实子进程。

**硬约束不变，且是整个阶段 4 的第一条回归测试：受约束的策略绝不静默降级为无约束执行。**
`sandbox.require_enforcement` 已经在，`bash.rs` 已经在用；契约化之后这条**不许回退**。

---

## 3. 第二个 provider：进程内，不是容器

阶段 4 的验收是"存在至少两个执行层 provider，切换只改配置"。第二个是**进程内 provider**：

| 契约 | 本地实现 | 进程内实现 |
|---|---|---|
| `Process` | `tokio::process` | 脚本化：按 argv 匹配预置的输出与退出码 |
| `FileSystem` | `tokio::fs` | 内存树 |
| `Network` | `reqwest` | 离线：一律 `Denied`，或按 fixture 应答 |
| `Sandbox` | sandbox-exec / bwrap | 诚实的 `None`（它约束不了真实进程，因为它压根不跑真实进程） |

**它不是桩，是宿主真会用的配置**，跟 Phase 3 建立的模式一致
（`InMemoryLayers`、`InMemoryBlobStore`、`FixedEnvironment` 都是这个形状）。
真实用途是**工具层的确定性测试**：跟 `FixedEnvironment` 一合，
整条会话可以不碰机器地跑完并逐位重放——今天工具测试要真的写临时目录、真的 fork 进程，
`3bb7f23` 修掉的那批"测机器不测代码"的用例就是这么来的。

### 3.1 这个选择的代价，说在前面

进程内实现对契约形状的验证**弱于**远端实现。它不会逼着设计回答
"输出怎么分块传""符号链接谁解析""跨机时 `Partial` 还诚实吗"。

缓解办法是把这些问题**保留为设计约束**而不是实现约束——上面 §2.1 的流式硬要求、
§2.2 的符号链接顺序、§2.4 的 `Partial`，都是照着"如果这是远端会怎样"写的，
即使今天没有远端实现。这样以后真要接远端时不合身的概率降下来，
又不用今天就付造容器的钱。

**这仍然是一个降低了的风险，不是消除了的风险。** 如果将来要接容器或远端，
应当预期契约会有一轮修订。

---

## 4. 失败语义

**provider 不可达时，工具失败。绝不降级到本地。**

这条跟沙箱那条硬约束是同一条道理的两个面。一个配置成"在别处执行"的部署，
如果在别处连不上时悄悄在本地执行了,那么它的隔离承诺在最需要的时刻恰好不成立。
`ExecError::Unavailable` 原样报给模型,让它知道是环境问题不是它写错了。

三类错误对工具的意义：

| | 对模型说什么 | 谁该处理 |
|---|---|---|
| `Unavailable` | "执行环境不可用" | 宿主。`health.check` 上会同时看到 |
| `Denied` | "这个操作被策略拒绝" | 模型，它该换个做法 |
| `Failed` | 目标自己的错误原文 | 模型 |

---

## 5. 迁移顺序与不变量

1. **P4-1** 本地 provider：四个契约各一个本地实现，行为逐位不变，工具尚未迁移。
2. **P4-2** Bash / Shell 走 `Process` + `Sandbox`。第一条回归测试在这里落地。
3. **P4-3** 文件类工具走 `FileSystem`。唯一非机械的一处是 §2.2 的符号链接顺序。
4. **P4-4** 网络出口统一。修掉 §2.3 的现存缺陷。
5. **P4-6** 其余工具。
6. **P4-7** 进程内 provider + 配置切换。**这一单才是验收。**
7. **P4-5** PTY，纯新增，可做可不做。

全程不变量：

- 受约束的策略绝不静默降级为无约束执行
- provider 不可达绝不降级到本地
- 沙箱**策略**与是否施加仍是内核的，不开放
- 每一单的行为逐位不变（P4-4 除外——它按定义要改变行为，因为今天的行为是缺陷）
