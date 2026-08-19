# AttaCore

> **AI Agent Orchestration Engine** — a Rust workspace delivering production-grade infrastructure for building AI coding assistants and intelligent agent runtimes.

[**中文版本**](docs/README.zh.md)

---

AttaCore is **not** an end-user AI assistant. It is a **developer-facing agent engine** — the same class of infrastructure that powers Claude Code. It provides a behavior-aligned tool system, session management, permission control, context compaction, multi-agent coordination, MCP protocol support, and more. Build your own IDE plugin, desktop GUI, CLI tool, or server-side agent product on top of it.

## Why AttaCore

| Concern | What You Get |
|---|---|
| **Behavior Fidelity** | 30+ tools aligned with Claude Code's TypeScript reference implementation — every function, every edge case, every compression strategy annotated with `TS parity:` source traces |
| **Context Is Hard** | Multi-strategy compaction (snip → micro-compact → collapse → LLM summarize), reactive triggers, circuit breakers, cache-aware edit generation — the system that keeps 200k+ token conversations coherent |
| **Concurrency** | v2 streaming tool executor pipelines safe-parallel tools while the model is still generating tokens — GPU-pipeline thinking applied to LLM tool calls |
| **Safety** | Three-tier permission model (allow/ask/deny), glob-based rule engine, Unicode-normalized path safety, sandboxed execution, LLM-assisted classification |
| **Multi-Agent** | First-class team coordination: Coordinator, Mailbox, shared memory — compose agents like microservices |
| **Multi-Provider Routing** | Declare several LLM providers in `settings.json` and route by task type — sub-agent spawns can run on a cheaper/different model than the main conversation, resolved and validated at daemon startup |
| **Scene Customization** | Agent behavior — system prompt, tool whitelist, execution limits — lives behind the `AgentScene` trait, not hardcoded. Three built-in scenes double as a copy-from-here ladder: full reference, compact reference, minimal skeleton |
| **Observability** | 40+ structured telemetry events, OpenTelemetry export, LLM interaction recording and deterministic replay, cost tracking |
| **Embeddable** | Library mode (Rust API) or Daemon mode (JSON-RPC 2.0 over Unix socket / TCP, token-handshake authenticated) — same engine, your choice of integration surface |

## Architecture

AttaCore is a 5-layer, strictly-layered Rust workspace. Dependencies only flow upward — each layer builds on the one below it. No cycles. No shortcuts.

```
                          ┌──────────────────────────┐
                          │     Your Application      │
                          │  IDE · CLI · GUI · Server │
                          └──────────┬───────────────┘
                                     │
                          ┌──────────▼───────────────┐
                          │  L4  runtime             │
                          │  Agent loop · Builder    │
                          │  Streaming · Dispatch    │
                          │  Commands (/help, …)     │
                          ├──────────────────────────┤
                          │  L3  tools · skills      │
                          │  scene · team · task     │
                          │  30+ built-in tools      │
                          │  Skill system · MCP      │
                          ├──────────────────────────┤
                          │  L2  model · history     │
                          │  permissions · mcp       │
                          │  compaction · session    │
                          ├──────────────────────────┤
                          │  L1  core                │
                          │  traits · types · ID     │
                          │  EngineConfig · Context  │
                          ├──────────────────────────┤
                          │  L0  auth · hooks        │
                          │  plugin · telemetry      │
                          └──────────────────────────┘
```

### The Layers

**L0 — Cross-Cutting Services** (zero internal deps)
`auth` (OAuth 2.0 PKCE), `hooks` (lifecycle callbacks — 11 event types, command/prompt/HTTP/agent hooks), `plugin` (marketplace + dependency resolution + version cache), `telemetry` (40+ structured events, OpenTelemetry export, LLM interaction recording and replay).

**L1 — Foundation** (`core` / `base` crate)
Shared types and traits for the entire system: `Model` (LLM backend abstraction), `AgentScene` (agent behavior), `Permission` (tool authorization), `Tool` (unified tool interface v7). Plus `Id` (BASE58 UUIDv4), `EngineConfig`, `SessionState`, `FrozenContext`, `ToolContext`, and the message/content block types.

**L2 — Infrastructure**
`model` — Anthropic Messages API adapter with streaming, tokenization, fallback routing. `history` — JSONL persistence with path sanitization and transcript chunking. `permissions` — glob-based rule engine with allow/deny/ask matching, path safety (Unicode NFC/NFD normalization), YOLO mode, LLM classifier. `mcp` — full MCP client: stdio / SSE / Streamable HTTP transports, tool adaptation, OAuth bearer tokens. `compaction` — multi-strategy context compression with reactive triggers and circuit breakers. `session` — in-memory session state and auto-naming.

**L3 — Domain Logic**
`tools` — 30+ built-in tools (Bash, Read, Write, Edit, Glob, Grep, LSP, WebFetch, WebSearch, CronCreate, TaskCreate, Skill, Agent, NotebookEdit, Monitor, PushNotification, …). `skills` — filesystem skill resolver + loader + watcher. `scene` — built-in scenes: Coding, Chat, Demo. `team` — multi-agent coordination: Coordinator, TeamTool, Mailbox. `task` — background task lifecycle: running, cron, store, delete.

**L4 — Runtime**
`agent` — core Agent struct and Builder pattern. `turn` — the turn loop (~2200 lines), all orchestration logic. `streaming` — v2 streaming tool executor (pipeline safe-parallel tools during model generation). `dispatch` — `FuturesUnordered` + `Semaphore` controlled concurrent tool dispatch with sibling abort. `commands` — slash command routing (/help, /skills, /clear, /compact, /cost, + custom).

## Core Capabilities

### Tool System (30+ tools, Claude Code behavior-aligned)

| Category | Tools |
|---|---|
| **Filesystem** | `Read`, `Write`, `Edit`, `Glob`, `Grep` |
| **Shell** | `Bash` (sandboxed, path-safe, timeout-controlled) |
| **Web** | `WebFetch`, `WebSearch` |
| **Task** | `TaskCreate`, `TaskList`, `TaskGet`, `TaskUpdate`, `TaskStop` |
| **Planning** | `EnterPlanMode`, `ExitPlanMode` |
| **Scheduling** | `CronCreate`, `CronDelete`, `CronList` |
| **Workspace** | `EnterWorktree`, `ExitWorktree` (isolated git worktree per session) |
| **Editor** | `LSP` (9 operations: go-to-def, find-refs, hover, document-symbol, workspace-symbol, go-to-impl, call-hierarchy, incoming/outgoing-calls), `NotebookEdit` |
| **Notification** | `PushNotification`, `Monitor`, `ScheduleWakeup` |
| **Collaboration** | `Skill` (skill invocation), `Agent` (sub-agent spawning) |
| **Meta** | `ToolSearch` (on-demand schema lookup for deferred tools) |
| **Protocol** | Full MCP support (stdio / SSE / Streamable HTTP) |

Every tool implements the unified `Tool` trait — consistent error handling, permission gating, and telemetry instrumentation.

**Deferred tools.** A scene can mark tools as deferred via `AgentScene::deferred_tools()`: they stay allowed and callable, but are advertised to the model by name and one-line description only. The model pulls the full JSON schema on demand with `ToolSearch`, so rarely-used tools stop costing schema tokens on every API call.

### Scene Customization

Agent behavior — system prompt, tool whitelist, token budget, execution limits — is defined by the [`AgentScene`](crates/core/src/interface/scene.rs) trait, not hardcoded into the engine. Three built-in scenes double as a learning ladder, each at a different depth:

| Scene | Depth | Use it as… |
|---|---|---|
| `CodingScene` | Full reference (~850 lines) — cache-optimized, multi-section system prompt, behavior aligned with Claude Code | The production-depth example |
| `ChatScene` | Complete but compact — every method implemented, each with a "why this design" comment | The template to copy when writing your own scene |
| `DemoScene` | Minimal skeleton — only the 6 required trait methods; every optional extension point left at its default | The "what happens if I don't override this" example |

Implement `AgentScene` yourself to ship a scene tailored to your product — a support-bot scene, a data-analysis scene, whatever your domain needs — then select it via `--scene` (daemon mode) or `Builder::scene(...)` (library mode). See [Customizing Behavior](#customizing-behavior) below for the full trait-injection picture.

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
  "task_models": { "subagent": "deepseek" }
}
```

- **Resolved and validated at startup** — an unknown provider, a missing `default_model`, or an unsupported `api_type` fails the daemon at boot with a clear error, not mid-session.
- **Wired**: the `Agent` tool's sub-agent spawns route through the resolved provider/model (`TaskRouter`) instead of always inheriting the parent session's model.
- **Not yet wired**: the main conversation model and `team` coordination still use the single env-var-configured client; `api_type: openai_compatible` is accepted by the config schema but has no protocol implementation yet — declaring one fails fast at startup with a descriptive error instead of silently falling back to Anthropic.
- Inspect resolved routing anytime via the `daemon.doctor` RPC; read/write provider config without hand-editing JSON via `config.getProvider`/`config.setProvider` — both hot-reload the router (no daemon restart), and `config.reload` picks up a hand-edited settings.json the same way. Already-running sessions catch up lazily, on their next turn.

See [`docs/LLM_PROVIDERS.md`](docs/LLM_PROVIDERS.md) for the full config reference and exact implementation status.

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

Full Model Context Protocol support across all three transports:

| Transport | Status |
|---|---|
| **stdio** | subprocess lifecycle, auto-restart |
| **SSE** | long-lived HTTP streaming |
| **Streamable HTTP** | stateless request/response |

MCP tools are adapted to the native `Tool` trait and injected into the system prompt. MCP servers can also register as skills for user invocation. OAuth 2.0 bearer token exchange supported.

### Telemetry & Recorder

40+ structured event types covering the full agent lifecycle: turn start/complete, tool execution, API errors, permission decisions, compaction operations, memory snapshots, MCP connect/disconnect, session lifecycle, startup timing, model routing, hook execution, slash command usage.

**Recorder**: wrap any `Model` with `RecorderModel` to record every LLM call — the assembled system blocks, the full tool table, every message, and the response stream down to token boundaries — then replay it deterministically. Zero API cost for integration tests, and a recording is a self-contained directory you can hand to someone.

## Crate Map

| Layer | Crate | Responsibility | Key Exports |
|---|---|---|---|
| L0 | `auth` | OAuth 2.0 PKCE client | `OAuth2Client`, `TokenStore`, `PkceVerifier` |
| L0 | `hooks` | Lifecycle hook runner | `HookRunner`, `HookConfig`, `HookEvent` (11 types) |
| L0 | `plugin` | Plugin marketplace + resolution | `Plugin`, `PluginManifest`, `DependencyResolver` |
| L0 | `telemetry` | Telemetry + Recorder | `TelemetryHandle`, `TelemetryEvent` (40+), `RecorderModel`, `FileRecorder` |
| L1 | `core` (base) | Shared types, traits, ID | `Model`, `AgentScene`, `Permission`, `Tool`, `Id`, `EngineConfig`, `FrozenContext` |
| L2 | `model` | Anthropic API adapter | `AnthropicModel`, `AnthropicClient`, `ModelEvent`, `Usage` |
| L2 | `history` | JSONL session persistence | `HistoryStore`, `TranscriptEntry` |
| L2 | `permissions` | Permission engine | `RuleSet`, `Gate`, `LLMClassifier`, `PathSafety` |
| L2 | `mcp` | MCP protocol client | `McpManager`, `McpClient`, `ToolAdapter`, `OutputCache` |
| L2 | `compaction` | Context compression | `Compactor`, `DefaultCompactor`, reactive/cached/time-based strategies |
| L2 | `session` | In-memory session state | `SessionManager`, `SessionSummary` |
| L3 | `tools` | 30+ built-in tools | `BashTool`, `FileReadTool`, `FileWriteTool`, `LspTool`, `WebFetchTool`, … |
| L3 | `skills` | Skill loader + manager | `SkillManager`, `SkillWatcher`, `McpBuilder` |
| L3 | `scene` | Built-in agent scenes | `CodingScene`, `ChatScene`, `DemoScene` |
| L3 | `team` | Multi-agent coordination | `Coordinator`, `TeamTool`, `Mailbox` |
| L3 | `task` | Background task lifecycle | `TaskManager`, `TaskStore`, `RunningTask`, `CronTask` |
| L4 | `runtime` | Agent runtime + turn loop | `Agent`, `Builder`, `TurnOutcome`, `StreamResult`, `CommandRegistry` |
| — | `daemon` | JSON-RPC 2.0 server | `DaemonServer`, `SessionPool` (LRU + idle eviction) |
| — | `test-runner` | .test scenario runner | API runner, CLI runner, LLM comparator, reporter |

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

The `Builder` enforces compile-time required fields (`scene`, `model`, `settings`) and applies sensible defaults for everything else — `AllowAll` permissions, in-memory tool registry, default compactor, noop hooks.

### Customizing Behavior

Inject your own implementations through traits:

```rust
Builder::new()
    .scene(my_scene)              // impl AgentScene — controls system prompt, behavior
    .model(my_model)              // impl Model — any LLM backend
    .permission(my_permission)    // impl Permission — your authorization logic
    .tool_registry(my_tools)      // impl ToolRegistry — custom tool set
    .hook_runner(my_hooks)        // impl HookRunner — lifecycle callbacks
    .compactor(my_compactor)      // impl Compactor — custom compaction strategy
    .build()?;
```

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
| `ANTHROPIC_API_KEY` | **Required.** API key for the model provider |
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
2. **Trait injection.** `Model`, `Permission`, `AgentScene` — core behaviors are traits you implement. The engine owns no policy.
3. **Tool alignment.** 30+ tools, behavior-verified against Claude Code's TypeScript implementation. Systematic `TS parity:` annotations throughout.
4. **Safe by default.** Three-tier permission model, Unicode-normalized path safety, sandboxed execution — you opt into less safety, not more.
5. **Observable everywhere.** 40+ structured telemetry events. Recorded LLM calls replay deterministically. Cost tracking. OpenTelemetry export.

## Project Structure

```
AttaCore/
├── crates/           # 17 Rust crates (the engine)
├── daemon/           # JSON-RPC 2.0 daemon (reference application)
├── tests/            # Integration tests + test runner + fixtures
├── docs/             # Documentation
├── Cargo.toml        # Workspace root (19 members)
└── README.md         # You are here
```

## Documentation

| Document | Audience |
|---|---|
| [README.md](README.md) | **You are here** — project overview, architecture, quick start |
| [README.zh.md](docs/README.zh.md) | 中文版本 — 同样的内容，面向中文开发者 |
| [DEV_GUIDE.md](docs/DEV_GUIDE.md) | Full API reference for Daemon and Library modes |
| [LLM_PROVIDERS.md](docs/LLM_PROVIDERS.md) | Multi-provider LLM config reference — schema, examples, exact wiring status |
| [DAEMON_RPC.md](docs/DAEMON_RPC.md) | Complete JSON-RPC method reference — params, errors, TCP auth handshake |

## License

Apache-2.0
