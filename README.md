# AttaCore

> **AI Agent Orchestration Engine** — a Rust workspace delivering production-grade infrastructure for building AI coding assistants and intelligent agent runtimes.

---

AttaCore is **not** an end-user AI assistant. It is a **developer-facing agent engine** — the same class of infrastructure that powers Claude Code. It provides a behavior-aligned tool system, session management, permission control, context compaction, multi-agent coordination, MCP protocol support, and more. Build your own IDE plugin, desktop GUI, CLI tool, or server-side agent product on top of it.

## Why AttaCore

| Concern | What You Get |
|---|---|
| **Behavior Fidelity** | 40 built-in tools whose behavior is verified against Claude Code's TypeScript reference implementation — every function, every edge case, every compression strategy |
| **Context Is Hard** | Multi-strategy compaction (snip → micro-compact → collapse → LLM summarize), reactive triggers, circuit breakers, cache-aware edit generation — the system that keeps 200k+ token conversations coherent |
| **Concurrency** | v2 streaming tool executor pipelines safe-parallel tools while the model is still generating tokens — GPU-pipeline thinking applied to LLM tool calls |
| **Safety** | Three-tier permission model (allow/ask/deny), glob-based rule engine, Unicode-normalized path safety, sandboxed execution, LLM-assisted classification |
| **Multi-Agent** | First-class team coordination: Coordinator, Mailbox, shared memory — compose agents like microservices |
| **Multi-Provider Routing** | Declare several LLM providers in `settings.json` and route by task type — sub-agent spawns, compaction and memory extraction can each run on a cheaper/different model than the main conversation, resolved and validated at daemon startup. Two wire protocols implemented: Anthropic Messages and OpenAI Chat Completions |
| **Scene Customization** | Agent behavior — system prompt, tool whitelist, execution limits — lives behind the `AgentScene` trait, not hardcoded. Four built-in scenes, three of which double as a copy-from-here ladder: full reference, compact reference, minimal skeleton |
| **Observability** | 37 structured telemetry event types, OpenTelemetry export, LLM interaction recording and deterministic replay, cost tracking |
| **Sandboxed Plugins** | WebAssembly component-model plugins with capability declarations that default to nothing, install-time disclosure of every model-visible string, and a build that can drop the entire plugin subsystem from the dependency graph |
| **Embeddable** | Library mode (Rust API) or Daemon mode (JSON-RPC 2.0 over Unix socket / TCP, token-handshake authenticated) — same engine, your choice of integration surface |

## Architecture

AttaCore is a strictly-layered Rust workspace. Dependencies only flow upward — each layer builds on the one below it. No cycles. No shortcuts.

```
                          ┌──────────────────────────┐
                          │     Your Application      │
                          │  IDE · CLI · GUI · Server │
                          └──────────┬───────────────┘
                                     │
                          ┌──────────▼───────────────┐
                          │  L5  plugin-host         │  ← optional, compile-time
                          │  plugin-compiler         │     removable
                          ├──────────────────────────┤
                          │  L4  runtime             │
                          │  Agent loop · Builder    │
                          │  Streaming executor      │
                          │  Commands (/help, …)     │
                          ├──────────────────────────┤
                          │  L3  tools · skills      │
                          │  scene · team · task     │
                          │  40 built-in tools       │
                          │  Skill system · MCP      │
                          ├──────────────────────────┤
                          │  L2  model · history     │
                          │  permissions · mcp       │
                          │  compaction · session    │
                          ├──────────────────────────┤
                          │  L1  core · wasm-host    │
                          │  traits · types · ID     │
                          │  EngineConfig · Context  │
                          ├──────────────────────────┤
                          │  L0  auth · hooks        │
                          │  plugin · telemetry      │
                          └──────────────────────────┘
```

`plugin-host` sits above `runtime` because it wires plugin contributions into the
engine's registries; that is why the whole plugin tier can be dropped without the
layers below it noticing. See [Plugin System](#plugin-system).

### The Layers

**L0 — Cross-Cutting Services** (zero internal deps)
`auth` (OAuth 2.0 PKCE), `hooks` (lifecycle callbacks — 30 event types; five hook backends: command / prompt / HTTP / agent / WASM), `plugin` (marketplace + dependency resolution + version cache + install-time disclosure), `telemetry` (37 structured event types, OpenTelemetry export, LLM interaction recording and replay).

**L1 — Foundation** (`core` / `base` crate, plus `wasm-host`)
Shared types and traits for the entire system: `Model` (LLM backend abstraction), `AgentScene` (agent behavior), `Permission` (tool authorization), `Tool` (unified tool interface v7). Plus `Id` (BASE58 UUIDv4), `EngineConfig`, `SessionState`, `FrozenContext`, `ToolContext`, and the message/content block types. `wasm-host` — the WebAssembly component runtime and capability resolver, depending only on `core` and `plugin`.

**L2 — Infrastructure**
`model` — two protocol adapters (Anthropic Messages, OpenAI Chat Completions) with streaming, tokenization, fallback routing. `history` — JSONL persistence with path sanitization and transcript chunking. `permissions` — glob-based rule engine with allow/deny/ask matching, path safety (Unicode NFC/NFD normalization), YOLO mode, LLM classifier. `mcp` — full MCP client: stdio / SSE / Streamable HTTP / WebSocket / in-process transports, tool adaptation, OAuth bearer tokens. `compaction` — multi-strategy context compression with reactive triggers and circuit breakers. `session` — in-memory session state and auto-naming.

**L3 — Domain Logic**
`tools` — 40 built-in tools (Bash, Read, Write, Edit, Glob, Grep, WebFetch, WebSearch, CronCreate, TaskCreate, Skill, NotebookEdit, Monitor, PushNotification, …). `skills` — skill resolver + loader + watcher over filesystem, bundled and MCP-derived sources. `scene` — built-in scenes: Coding, Chat, Demo, Research. `team` — multi-agent coordination: Coordinator, TeamCreate/List/Delete tools, Mailbox. `task` — background task lifecycle: running, cron, store, delete.

**L4 — Runtime**
`agent` — core Agent struct and Builder pattern. `turn` — the turn loop (~4000 lines excluding tests; the main loop function alone is ~1200), all orchestration logic. `streaming` — v2 streaming tool executor: `FuturesUnordered` batches of concurrency-safe tools dispatched during model generation, with sibling abort on error. `agent_tool` — sub-agent spawning and agent-type resolution. `commands` — slash command routing (/help, /skills, /clear, /compact, /cost, + custom).

**L5 — Plugins** (optional)
`plugin-host` — loads installed plugins and wires their five contribution points into the engine. `plugin-compiler` — ahead-of-time component compilation at install. Both vanish from the dependency graph in the locked build; see [Plugin System](#plugin-system).

## Core Capabilities

### Tool System (40 built-in tools, Claude Code behavior-aligned)

| Category | Tools |
|---|---|
| **Filesystem** | `Read`, `Write`, `Edit`, `Glob`, `Grep` |
| **Shell** | `Bash` (sandboxed, path-safe, timeout-controlled) |
| **Web** | `WebFetch`, `WebSearch` |
| **Task** | `TaskCreate`, `TaskList`, `TaskGet`, `TaskUpdate`, `TaskStop`, `TaskOutput` |
| **Planning** | `EnterPlanMode`, `ExitPlanMode`, `VerifyPlanExecution`, `TodoWrite` |
| **Scheduling** | `CronCreate`, `CronDelete`, `CronList`, `ScheduleWakeup` |
| **Workspace** | `EnterWorktree`, `ExitWorktree` (isolated git worktree per session) |
| **Editor** | `NotebookEdit` |
| **Interaction** | `PushNotification`, `Monitor`, `AskUserQuestion`, `StructuredOutput` |
| **Collaboration** | `Skill` (skill invocation), `Agent` (sub-agent spawning), `TeamCreate`, `TeamList`, `TeamDelete` |
| **Protocol** | `MCP` (server introspection), `ListMcpResources`, `ReadMcpResource`, plus one `mcp__<server>__<tool>` adapter per discovered MCP tool |
| **Meta** | `ToolSearch` (on-demand schema lookup for deferred tools) |
| **Diagnostics** | `Ping`, `Sleep` |

Every tool implements the unified `Tool` trait — consistent error handling, permission gating, and telemetry instrumentation.

Registration happens in two places, and the split is deliberate. `tools::register_builtin_tools` installs the 24 self-contained tools. The rest need per-session state, so `runtime::agent::Builder::build()` registers them into a registry it creates fresh for that session: `Skill`, `WebSearch`, `Cron*`, `*Worktree` and `TaskStop`/`TaskOutput` always; `Agent` only while under the sub-agent depth limit; the three MCP tools and the `mcp__*` adapters only when MCP clients are configured; `Team*` only when `execution.team_enabled` **and** the scene's `supports_team()` both say yes — a scene that does not do teamwork never shows the model tools it would then be refused.

That is what gets *registered*. What the model actually sees is narrower still: the scene's `tools()` whitelist and `disallowed_tools()` filter the registry, and `deferred_tools()` decides which survive as name-only entries.

**Deferred tools.** A scene can mark tools as deferred via `AgentScene::deferred_tools()`: they stay allowed and callable, but are advertised to the model by name and one-line description only. The model pulls the full JSON schema on demand with `ToolSearch`, so rarely-used tools stop costing schema tokens on every API call.

### Scene Customization

Agent behavior — system prompt, tool whitelist, token budget, execution limits — is defined by the [`AgentScene`](crates/core/src/interface/scene.rs) trait, not hardcoded into the engine. Four scenes ship built in; three of them double as a learning ladder, each at a different depth:

| Scene | Depth | Use it as… |
|---|---|---|
| `CodingScene` | Full reference (~850 lines) — cache-optimized, multi-section system prompt, behavior aligned with Claude Code | The production-depth example |
| `ChatScene` | Complete but compact — every method implemented, each with a "why this design" comment | The template to copy when writing your own scene |
| `DemoScene` | Minimal skeleton — only the 6 required trait methods; every optional extension point left at its default | The "what happens if I don't override this" example |
| `ResearchScene` | A working non-coding scene — different tool surface and budget, same trait | Evidence the trait is not coding-shaped |

Implement `AgentScene` yourself to ship a scene tailored to your product — a support-bot scene, a data-analysis scene, whatever your domain needs — then register it in a `SceneRegistry` and select it via `--scene` (daemon mode) or `Builder::scene(...)` (library mode). See [Customizing Behavior](#customizing-behavior) below for the full trait-injection picture.

**One process, several scenes.** `--scene` sets the daemon's default and `--scenes chat,research` activates more alongside it; `scene.activate` / `scene.deactivate` add and remove them at runtime, and `session.create {"scene": "chat"}` picks one per session. Each session resolves its own settings, skills, agents and plugins under `~/.atta/scenes/<id>/`, and a request that touches a session belonging to another scene is refused with `SCENE_MISMATCH` rather than executed across the boundary.

### Multi-Provider LLM & Task-Level Routing

Beyond a single hardcoded Anthropic client, `settings.json` can declare several providers and route requests to them by task type instead of one model for the whole engine:

```json
{
  "providers": {
    "anthropic": { "api_type": "anthropic", "api_key": "sk-ant-...", "default_model": "claude-sonnet-4-6" },
    "deepseek":  { "api_type": "anthropic", "base_url": "https://api.deepseek.com", "api_key": "sk-...",
                   "default_model": "deepseek-pro", "models": ["deepseek-pro", "deepseek-flash"] }
  },
  "default_provider": "anthropic",
  "task_models": { "subagent": "deepseek", "compact": "deepseek" }
}
```

- **Resolved and validated at startup** — an unknown provider, a missing `default_model`, or an unsupported `api_type` fails the daemon at boot with a clear error, not mid-session.
- **Both `api_type` values have a protocol implementation.** `anthropic` (also the default when the field is absent) builds an Anthropic Messages client; `openai_compatible` builds `model::OpenAICompatibleModel`, speaking `POST <base_url>/v1/chat/completions` — which is what reaches OpenAI, vLLM, Ollama and the many gateways that only expose the OpenAI shape. `openai_compatible` requires an explicit `base_url`. Any other value is still a hard startup error, not a silent fall back to Anthropic.
- **Four task keys are wired**: `main` (the conversation itself), `subagent` (`Agent` tool spawns), `compact` (LLM summarization) and `memory` (memory extraction). Anything with no `task_models` entry falls back to `default_provider`.
- **`team` has no key of its own**: team coordination is handed the model resolved for `main`, and members it spawns route as `subagent`. Give `team` its own model by routing `subagent`.
- Inspect resolved routing anytime via the `daemon.doctor` RPC; read/write provider config without hand-editing JSON via `config.getProvider`/`config.setProvider` — both hot-reload the router (no daemon restart), and `config.reload` picks up a hand-edited settings.json the same way. Already-running sessions catch up lazily, on their next turn.

The authoritative schema for the whole `settings.json` surface, `providers` included, is [`docs/schemas/settings.schema.json`](docs/schemas/settings.schema.json). *(A prose config reference for providers does not exist yet.)*

### Context Compaction

The hardest problem in LLM agents, solved in production:

```
Budget Warning (80%) → Reactive Trigger → Micro-Compact (cache-aware)
     ↓                                         ↓
  Circuit Breaker ← Collapse (full) ← LLM Summarize (cost-aware)
                                           ↓
                                   Post-Compact Recovery
                        (re-inject files, skills, plan state, task summaries)
```

- **Micro-compact**: removes stale tool results while preserving prompt cache
- **Collapse**: merges consecutive user/assistant blocks
- **LLM Summarize**: delegates to a cheaper model for aggressive compression
- **Reactive**: predicts budget exhaustion from token velocity, triggers preemptively
- **Circuit breaker**: detects compression loops, falls back to safe defaults
- **Cache-aware edits**: generates `cache_edits` to avoid Anthropic prompt cache invalidation

### Permission & Safety

```
RuleSet { allow: [Glob], ask: [Glob], deny: [Glob] }
        ↓
Path Safety (Unicode NFC/NFD normalization, system directory blocklist)
        ↓
LLM Classifier (optional: delegate ambiguous cases to a fast model)
        ↓
YOLO Mode (auto-approve for CI/automation)
```

Three-tier decisions: **Permit** / **AskUser** / **Deny**. Rules match by glob pattern with directory-aware semantics. Path safety normalizes Unicode to prevent homograph attacks and blocks writes to system directories.

### Multi-Agent Team

Spawn sub-agents as naturally as calling a function:

```
Coordinator → [Agent A] [Agent B] [Agent C]
     ↕            ↕         ↕         ↕
  Mailbox  ← messages →  Mailbox  ←→  Mailbox
     ↕
  Shared Memory (file-based, wikilink cross-references)
```

- **Agent spawning**: `Agent` tool with type selection, worktree isolation, background execution
- **Mailbox**: typed message passing between agents
- **Shared memory**: file-based persistent knowledge with YAML frontmatter, `[[wikilink]]` cross-references, staleness scoring, LLM-based extraction and relevance selection
- **Coordinator**: task decomposition and result synthesis

### MCP Integration

Full Model Context Protocol support across five transports:

| Transport | Status |
|---|---|
| **stdio** | subprocess lifecycle, auto-restart |
| **SSE** | long-lived HTTP streaming |
| **Streamable HTTP** | stateless request/response |
| **WebSocket** | persistent bidirectional connection |
| **in-process** | resolves a pre-registered `McpClient` from a process-local registry — no subprocess, no socket |

The in-process transport is the one that matters for embedders: register your own `McpClient` implementation with `register_in_process_service`, name it in `mcp_servers`, and its tools reach the model over the same path as any external server, with no IPC in between.

MCP tools are adapted to the native `Tool` trait and injected into the system prompt. MCP servers can also register as skills for user invocation. Per-server `scope` limits which tools, resources and prompts a server may expose.

**OAuth**: the disk-backed token store and the PKCE flow live in `mcp::oauth`, and a server config can name an `oauth_provider`. The token *source* is a seam — `McpOAuthResolver`, installed by the host with `set_oauth_resolver`. **No implementation ships in this workspace**: with no resolver installed, OAuth is skipped and the connection proceeds without a bearer token. Embedders that need it implement the trait against the `auth` crate.

### Telemetry & Recorder

37 structured event types covering the full agent lifecycle: turn start/complete, tool execution, API errors, permission decisions, compaction operations, memory snapshots, MCP connect/disconnect, session lifecycle, startup timing, model routing, hook execution, slash command usage.

**Recorder**: wrap any `Model` with `RecorderModel` to record every LLM call — the assembled system blocks, the full tool table, every message, and the response stream down to token boundaries — then replay it deterministically. Zero API cost for integration tests, and a recording is a self-contained directory you can hand to someone. Implementation: `crates/telemetry/src/recorder/`.

### Plugin System

Plugins are **WebAssembly components** (component model, WIT world `atta:plugin@0.1.0`), not dynamic libraries and not scripts. A component imports a small host interface and exports its tools:

```wit
world plugin {
  import host;      // log, progress, now-ms, http-request, secret, kv-get, kv-set
  export tools;     // list-tools, call-tool
  export events;    // on-event — optional
  export init: func(config-json: string) -> result<_, string>;
}
```

**Capabilities default to nothing.** A component that declares no capabilities can compute and nothing else — no files, no network, no environment:

```toml
[wasm.capabilities]
fs_read  = ["./data"]     # WASI preopens, read-only
fs_write = []             # default: none
net      = ["api.example.com"]
env      = ["EXAMPLE_TOKEN"]   # reachable through host `secret`, nothing else
max_memory_mb = 64
timeout_ms    = 30000
```

`host.http-request` is checked against `net`; `host.secret` against `env`. There is one capability table and one authorization function — not a second whitelist per call path.

**Install-time disclosure is not skippable.** Sandboxing governs what a plugin *executes*; it does nothing about what a plugin *says*, and text reaching the model is the one attack isolation cannot address. So every model-visible string — tool descriptions, agent descriptions, a scene's system prompt — is presented for review at install, under hard caps (500 chars per description, 40 000 per prompt). Over the cap **refuses the install**; it does not warn and continue.

**Five contribution points, and the number is the point.** A plugin may add tools, MCP servers, hook subscriptions, scenes, and agent types. That is the entire surface, and adding a sixth is a decision to argue for. Within it:

- **Hook subscriptions are a whitelist of 6 events** (`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionRequested`, `SessionStart`, `SessionEnd`), enforced when the manifest is parsed — not the full 30 available to local hooks. Plugins reach the lifecycle through the hook dispatcher that already exists, so they add no new call site to the turn loop.
- **Permission rules a plugin contributes carry `RuleSource::Plugin`**, the lowest of the eight priorities — below user settings and organization policy, always. Uninstalling withdraws them in one call.
- **A plugin gets its own scene, never anyone else's.** Rewriting the prompt of a scene the user picked for other reasons is hijacking; owning a scene the user explicitly enters is the plugin doing its job.
- **Agent types a plugin declares cannot widen their own permissions** — `AgentTypeSource::Plugin` clamps the `permission_mode` and `max_turns` overrides.

**Faults cost one call.** Per-call isolation means a trapping component fails that invocation rather than the process. A component that traps three times consecutively is set aside — timeouts and cancellations do not count, since those say something about the work, not the plugin.

**The whole subsystem compiles out.** Two feature levels, both verified by checking the dependency graph rather than trusting the flag:

```sh
cargo build -p daemon --no-default-features                   # no plugin crates, no wasmtime
cargo build -p daemon --no-default-features --features plugins # plugins, no WebAssembly compiler
tests/scripts/locked_build.sh                                 # asserts both, then runs the suite
```

Build the locked artifact for that one package — never `--workspace`, since cargo unifies features across the graph and any other member enabling `plugins` turns it back on. With the feature off, `PluginHost` is `None` and every call site is an `if let Some(..)` that does nothing; there is no `#[cfg]` scattered through the engine and no behavior to reason about beyond "there are no plugins".

Full guide: [`docs/extending_wasm.md`](docs/extending_wasm.md).

## Crate Map

| Layer | Crate | Responsibility | Key Exports |
|---|---|---|---|
| L0 | `auth` | OAuth 2.0 PKCE client | `OAuth2Client`, `TokenStore`, `PkceVerifier` |
| L0 | `hooks` | Lifecycle hook runner | `HookRunner`, `HookConfig` (5 backends), `HookEvent` (30 types) |
| L0 | `plugin` | Plugin marketplace, resolution, disclosure | `Plugin`, `PluginManifest`, `PluginResolver`, `DependencyGraph`, `Disclosure`, `Capabilities` |
| L0 | `telemetry` | Telemetry + Recorder | `TelemetryHandle`, `TelemetryRecorder`, `TelemetryEvent` + `EventPayload` (37 variants), `RecorderModel`, `FileRecorder` |
| L1 | `core` (base) | Shared types, traits, ID | `Model`, `AgentScene`, `Permission`, `Tool`, `Id`, `EngineConfig`, `FrozenContext` |
| L1 | `wasm-host` | WASM component runtime | `API_VERSION`, capability resolver, per-plugin health tracking |
| L2 | `model` | Anthropic + OpenAI adapters | `AnthropicModel`, `OpenAICompatibleModel`, `AnthropicClient`, `ModelEvent`, `Usage` |
| L2 | `history` | JSONL session persistence | `HistoryStore`, `JsonlHistoryStore`, `LogEntry`, `project_messages` |
| L2 | `permissions` | Permission engine | `RuleSet`, `PermissionGate`, `AutoClassifier`, `LlmClassifier`, `WritePolicy` + `check_write` |
| L2 | `mcp` | MCP protocol client | `McpManager`, `McpClient`, `McpToolAdapter`, `McpOutputCache`, `McpOAuthResolver` |
| L2 | `compaction` | Context compression | `Compactor`, `DefaultCompactor`, `SessionMemoryCompactor`, reactive/cached/time-based strategies |
| L2 | `session` | In-memory session state | `SessionManager`, `SessionSummary` |
| L3 | `tools` | 40 built-in tools | `BashTool`, `FileReadTool`, `FileWriteTool`, `WebFetchTool`, `SearchProvider`, … |
| L3 | `skills` | Skill loader + manager | `SkillManager`, `SkillWatcher`, `build_skills_from_mcp` |
| L3 | `scene` | Built-in agent scenes | `SceneRegistry`, `CodingScene`, `ChatScene`, `DemoScene`, `ResearchScene` |
| L3 | `team` | Multi-agent coordination | `Coordinator`, `TeamCreateTool`, `TeamRegistry`, `Mailbox` |
| L3 | `task` | Background task lifecycle | `TaskStore`, `RunningTaskStore`, `RunningTaskData`, `DreamTask` |
| L4 | `runtime` | Agent runtime + turn loop | `Agent`, `Builder`, `PluginHost`, `AgentTypeDefinition`, `CommandRegistry` |
| L5 | `plugin-host` | Loads plugins, wires contributions | `InstalledPlugins`, `PluginScene` |
| L5 | `plugin-compiler` | Ahead-of-time component compile | `atta-plugin-compile` binary |
| — | `daemon` | JSON-RPC 2.0 server | `DaemonServer`, `SessionPool` (LRU + idle eviction), `build_task_router`, `run_doctor` |
| — | `test-runner` | .test scenario runner | API runner, CLI runner, LLM comparator, reporter |

20 crates under `crates/`, plus `daemon/` and the test-runner members — 24 workspace members in total.

## Quick Start

### Prerequisites

- **Rust** 1.80+
- **Anthropic API Key** (or compatible endpoint)

### Build & Test

```sh
# Full workspace build
cargo build --workspace

# Run all tests
cargo test --workspace

# Single crate
cargo test -p tools

# Daemon tests
cargo test -p daemon
```

### Run the Daemon

```sh
export ANTHROPIC_API_KEY=sk-...
cargo run -p daemon
# Listens on $HOME/.atta/<scene>/daemon.sock (--scene defaults to "coding"; must be a registered scene — coding/chat/demo/research — or the daemon fails to start)
# Writes discovery lock file → clients auto-discover
```

### Run Integration Tests

```sh
# Prerequisite: .env file at repo root (gitignored) with API key — see tests/README.md
# API mode (direct Agent construction)
./tests/run_api.sh 000.c_project

# CLI mode (daemon → JSON-RPC)
./tests/run_cli.sh 000.c_project
```

## Usage Modes

### Daemon Mode (JSON-RPC 2.0)

For IDE plugins, multi-process architectures, remote clients. The engine runs as a standalone process communicating over Unix domain sockets or TCP.

```sh
# Start the daemon
export ANTHROPIC_API_KEY=sk-...
cargo run -p daemon --release

# Send a turn via socat
echo '{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"Write hello world in Rust"},"id":1}' \
  | socat - UNIX-CONNECT:$HOME/.atta/coding/daemon.sock  # default scene; use $HOME/.atta/<scene>/daemon.sock if you passed --scene
```

Daemon features:
- **Session pool** with configurable capacity, LRU eviction, and idle timeout
- **Discovery** via PID lock file + Unix socket — clients find the daemon automatically
- **Graceful shutdown** with in-flight turn completion
- **TCP mode** requires a `daemon.auth` handshake (constant-time token comparison) as the first message on every connection before any other method is dispatched — Unix sockets skip this and rely on filesystem permissions instead

### Library Mode (Embedded Rust API)

For desktop apps, custom CLIs, server-side agents. Direct control over every aspect of the engine.

```rust
use runtime::agent::Builder;
use scene::scene::coding::CodingScene;
use model::adapter::AnthropicModel;

// One Agent = one session
let (mut agent, event_rx, input_tx) = Builder::new()
    .scene(Arc::new(CodingScene))
    .model(model)
    .settings(settings)
    .session_id(session_id)
    .build()?;

// Run the event loop in background
tokio::spawn(async move { agent.run(cancel).await });

// Send messages, receive streaming events
input_tx.send(InputMessage::User {
    content: "Write a TCP echo server".into(),
    attachments: vec![],
    turn_id,
})?;

while let Some(event) = event_rx.recv().await {
    match event {
        AgentEvent::TextDelta { text, .. } => print!("{text}"),
        AgentEvent::TurnComplete { .. } => break,
        _ => {}
    }
}
```

`scene`, `model` and `settings` are required — `build()` returns `Err(EngineError::Internal("… required"))` when one is missing, so it is a startup check, not a compile-time one. Everything else gets a sensible default: `AllowAll` permissions, an in-memory tool registry, `DefaultCompactor`, and a hook runner with no hooks configured.

### Customizing Behavior

Inject your own implementations at build time:

```rust
Builder::new()
    .scene(my_scene)              // Arc<dyn AgentScene>  — system prompt, tool surface, budgets
    .model(my_model)              // Arc<dyn Model>       — any LLM backend
    .permission(my_permission)    // Arc<dyn Permission>  — your authorization logic
    .compactor(my_compactor)      // Arc<dyn Compactor>   — custom compaction strategy
    .history_store(my_store)      // Arc<dyn HistoryStore> — session persistence backend
    .plugin_host(my_plugin_host)  // Arc<dyn PluginHost>  — plugin contributions
    .task_router(my_router)       // Arc<TaskRouter>      — per-task model routing
    .tools(my_registry)           // Arc<InMemoryToolRegistry> — concrete type, see below
    .hooks(my_hook_runner)        // Arc<HookRunner>           — concrete type, see below
    .build()?;
```

Seven of these take a trait object, so an external crate can supply the implementation. Two do not, and it is worth knowing which:

- **`.tools(...)` takes `Arc<InMemoryToolRegistry>`, a concrete type.** A `ToolRegistry` trait exists in `base::tool`, but it carries only `all()` and `find()` — `register`/`replace` are inherent methods on the concrete type, and there is no `remove`. An alternative registry implementation cannot currently be substituted here.
- **`.hooks(...)` takes `Arc<HookRunner>`, a concrete struct, not a trait.** Customization happens through hook *configuration* (five backends: command, prompt, HTTP, agent, WASM) rather than by replacing the runner. The two executor traits the runner delegates to — `PromptHookExecutor` and `AgentHookExecutor` — are injectable; without them, `prompt` and `agent` hooks are skipped with a stated reason rather than failing silently.

Note also that `Permission::bind_tool_registry` takes `Arc<InMemoryToolRegistry>` for the same reason, which couples a custom `Permission` implementation to the concrete registry.

## Configuration

### Settings Layers (lowest to highest priority)

1. Built-in defaults
2. `$HOME/.atta/<scene>/settings.json` (or `.toml`) — `<scene>` is the daemon's `AgentScene` id (`coding`/`chat`/`demo`/`research`); set via `--scene` (defaults to `coding`)
3. `<project>/.atta/settings.json` (or `.toml`) — project-level state is flat, no scope segment
4. CLI arguments

```json
{
  "model": "claude-sonnet-4-6",
  "max_tokens": 4096,
  "permission": {
    "mode": "default",
    "default_mode": "require_user_permission",
    "yolo": false
  },
  "mcp_servers": {}
}
```

### Environment Variables

| Variable | Purpose |
|---|---|
| `ANTHROPIC_AUTH_TOKEN` | **Required** unless `ANTHROPIC_API_KEY` is set — the daemon checks this one first and fails at boot if neither is present |
| `ANTHROPIC_API_KEY` | Fallback for the above. Still required at boot even when `providers` are configured in `settings.json` |
| `ANTHROPIC_BASE_URL` | Custom API endpoint (proxies, compatible providers) |
| `ATTACORE_DAEMON_TOKEN` | TCP mode authentication token |
| `ATTA_CONFIG_HOME` | Config root directory (default: `$HOME/.atta/<scene>`) |
| `ATTA_RECORD` | Record mode: `ATTA_RECORD=<recording_name>` (with `ATTA_RECORDINGS_DIR`) |
| `ATTA_REPLAY` | Replay mode: `ATTA_REPLAY=<recording_name>` (with `ATTA_RECORDINGS_DIR`) |

## ID System

All externally-visible identifiers are **BASE58(UUID v4)** — 22 characters, URL-safe:

```
Ab12Cd34Ef56Gh78Ij90Kl   ← session_id / turn_id / agent_id / tool_call_id
```

Single source of truth: `core::id::Id::new()`. Direct UUID generation and manual BASE58 encoding outside this entry point is forbidden. The `Id` type is a `#[sqlx(transparent)]` newtype over `[u8; 16]`, mapping to `TEXT` in both Postgres and SQLite.

```rust
use base::id::Id;

let id = Id::new();            // Random allocation — the ONLY generation path
let id = Id::parse(s)?;        // Validate and decode external input (checks 16-byte length)
```

## Design Principles

1. **Library-first.** Every capability is exposed through Rust crates. The daemon is a reference application, not the product.
2. **Trait injection.** `Model`, `Permission`, `AgentScene`, `Compactor`, `HistoryStore`, `PluginHost` — core behaviors are traits you implement. The engine owns no policy.
3. **Tool alignment.** 40 built-in tools, behavior-verified against Claude Code's TypeScript implementation.
4. **Safe by default.** Three-tier permission model, Unicode-normalized path safety, sandboxed execution, capability-gated plugins — you opt into less safety, not more.
5. **Observable everywhere.** 37 structured telemetry event types. Recorded LLM calls replay deterministically. Cost tracking. OpenTelemetry export.

## Project Structure

```
AttaCore/
├── crates/           # 21 Rust crates (the engine)
├── daemon/           # JSON-RPC 2.0 daemon (reference application)
├── bridges/          # Out-of-process bridges (atta-dsh-bridge)
├── tests/            # Integration tests + test runner + fixtures
├── docs/             # Documentation
├── Cargo.toml        # Workspace root (25 members)
└── README.md         # You are here
```

## Documentation

| Document | Audience |
|---|---|
| [README.md](README.md) | **You are here** — project overview, architecture, quick start |
| [ARCHITECTURE.md](docs/ARCHITECTURE.md) | **The concepts and how they fit** — crates, a turn's skeleton, sessions and scenes, history, the execution layer, locks, disk layout |
| [daemon_rpc_protocol.md](docs/daemon_rpc_protocol.md) | JSON-RPC method reference — fields, types, error codes, TCP auth handshake. Start here to write a client |
| [extension_points.md](docs/extension_points.md) | **Every seam in the engine** — what you can replace, contribute to or intercept, what it costs, who is allowed. Start here to build on AttaCore |
| [extending_quickjs.md](docs/extending_quickjs.md) | Writing script extensions for the QuickJS carrier — bindable points, the API, examples |
| [extending_wasm.md](docs/extending_wasm.md) | Writing WebAssembly plugins — manifest, capabilities, contribution points, examples |
| [schemas/settings.schema.json](docs/schemas/settings.schema.json) | Generated JSON Schema for `settings.json` |

There is currently no prose API reference for **Library mode**; the entry points are `runtime::agent::Builder` and [extension_points.md](docs/extension_points.md), which lists every trait the builder accepts along with a minimal example for each.

## License

Apache-2.0
