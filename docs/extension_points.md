# Extension points

Everything in this engine that a host, a plugin or a script can plug into,
what it costs, and who is allowed to use it.

Start with the question you actually have:

- **"I want to replace how the engine does X."** You want a **contract** —
  implement a trait, hand it to `runtime::agent::Builder`.
- **"I want to add something to what the engine already does."** You want a
  **registration** — contribute a named, ordered, revocable thing.
- **"I want to see or change something in flight."** You want an
  **interception** — sit in the path of something the engine is doing.

All three are Rust: implement the trait, hand it to `runtime::agent::Builder`.
To extend a build you are not compiling, use a carrier — a script
(`extending_quickjs.md`) or a WebAssembly plugin (`extending_wasm.md`). Each
reaches a subset of what is listed here, and each of those documents says which.
For how the engine is put together, see `ARCHITECTURE.md`.

## How to read the table

`Config` / `Script` / `Plugin` are the three provenances the engine
distinguishes, and the axis is not privilege but authorship: did the operator
write this, or download it. A script in the operator's own project can do what
they could have done by hand, because it is them. An installed plugin adds
freely and does more only if it declared the capability at install, where the
person installing it was shown the declaration.

`closed` means the point is a Rust trait wired at build time, so there is
nothing for a script or a plugin to register through — the embedding program
reaches it, nobody else.

**Frequency is a constraint, not trivia.** A point that fires once a session
can afford a subprocess. One that fires per streamed chunk cannot afford
anything, which is why those are closed to scripts and plugins outright.

<!-- BEGIN GENERATED TABLE — see base::interface::catalog::render_markdown -->

| Point | Kind | When | May change | Frequency | Config | Script | Plugin |
|---|---|---|---|---|---|---|---|
| `tool.registry` | contract | session build, and any time after | which tools exist and what they are | per session (10⁰) | closed | closed | closed |
| `tool.around` | interception | around dispatch, outside permission and hooks | the cancellation signal, the outcome; never the input | per tool call (10¹) | full | full | declared capability |
| `tool.result` | interception | after every hook, immediately before the model sees it | the result text and its images | per tool call (10¹) | full | full | declared capability |
| `prompt.block` | registration | prompt assembly, once per turn | adds only | per turn (10⁰–10¹) | full | full | add only |
| `prompt.context` | registration | prompt assembly, once per turn | adds only | per turn (10⁰–10¹) | full | full | add only |
| `prompt.variable` | registration | prompt assembly, after blocks are merged | its own placeholder, nothing else | per turn (10⁰–10¹) | full | full | add only |
| `prompt.assembler` | contract | prompt assembly, in place of the engine's own | order, cache boundaries, merge strategy — the whole result | per model call (10⁰–10¹) | closed | closed | closed |
| `prompt.assemble` | interception | prompt assembly, last | block content, order and membership | per turn (10⁰–10¹) | full | full | declared capability |
| `event.sink` | contract | every emission, on the sink's own task | nothing — observation only | per streamed chunk (10³–10⁴) | closed | closed | closed |
| `health.check` | registration | whenever something asks for a health report | nothing — a check reports, it does not repair | per process (10⁰) | closed | closed | closed |
| `elicitation.ask` | contract | whenever a decision needs a human | the answer | per turn (10⁰–10¹) | closed | closed | closed |
| `permission.check` | contract | before every tool call | permit, deny, or ask | per tool call (10¹) | closed | closed | closed |
| `scene` | contract | session build | everything about how an agent presents itself | per session (10⁰) | closed | closed | closed |
| `model` | contract | every model request | the whole exchange | per model call (10⁰–10¹) | closed | closed | closed |
| `model.factory` | registration | startup, when provider config is read | which protocols can be configured | per process (10⁰) | closed | closed | closed |
| `model.request` | interception | immediately before each model call | everything in the request | per model call (10⁰–10¹) | full | full | declared capability |
| `model.message` | interception | after the stream carrying it finishes | the message content | per model call (10⁰–10¹) | full | full | declared capability |
| `credentials` | contract | startup, when provider config is read | the credential | per process (10⁰) | closed | closed | closed |
| `config.source` | contract | process start, before anything is built | which layers exist and what JSON is in them; never the merge | per process (10⁰) | closed | closed | closed |
| `token.count` | contract | every budget check | the number compaction triggers on | per turn (10⁰–10¹) | closed | closed | closed |
| `history.store` | contract | every append, and on resume | how and where the log persists | per turn (10⁰–10¹) | closed | closed | closed |
| `history.query` | contract | whenever something asks for sessions rather than for one session | which sessions come back, in what order, and how they are matched | per session (10⁰) | closed | closed | closed |
| `history.blob` | contract | every append carrying an image or a large payload, and on load | where large content is kept and how it is addressed | per turn (10⁰–10¹) | closed | closed | closed |
| `history.projection` | contract | every time a transcript is read: resume, fork, search, paging | which entries become messages, and what they say | per session (10⁰) | closed | closed | closed |
| `history.extension_entry` | registration | any time; ordered with everything else | adds only; the kernel never reads the payload | per turn (10⁰–10¹) | full | full | add only |
| `memory.storage` | contract | recall, and whenever a memory is written | how and where memories persist | per turn (10⁰–10¹) | closed | closed | closed |
| `memory.retriever` | contract | once per user message, in the background | the recalled set | per turn (10⁰–10¹) | closed | closed | closed |
| `memory.retrieval_hook` | interception | around retrieval | the query and the recalled names | per turn (10⁰–10¹) | full | full | declared capability |
| `history.append_observer` | interception | after each append succeeds | nothing — read-only in the types, not by agreement | per turn (10⁰–10¹) | full | full | add only |
| `skill.source` | contract | at session build, and whenever an MCP server connects | which skills exist, and the text each one expands to | per session (10⁰) | closed | closed | closed |
| `instruction.source` | contract | at session build | the AGENTS.md text injected once per session | per session (10⁰) | closed | closed | closed |
| `rules.source` | contract | while the system prompt is assembled | which rule documents the model is told exist | per model call (10⁰–10¹) | closed | closed | closed |
| `turn.policy` | contract | before each model call, and after each one returns | whether the loop takes another step, and the reported stop reason | per model call (10⁰–10¹) | closed | closed | closed |
| `model.recovery` | contract | on an error, and on a response cut off at the output limit | whether to switch model, compact and retry, raise the limit, or fail | per model call (10⁰–10¹) | closed | closed | closed |
| `model.backoff` | contract | inside the client, below the model contract, per failed attempt | the delay before a retry, and whether there is one | per model call (10⁰–10¹) | closed | closed | closed |
| `budget` | contract | after each model call, and before each request is assembled | whether the turn continues, what it is told, the compaction ceiling | per model call (10⁰–10¹) | closed | closed | closed |
| `environment` | contract | whenever an answer is written down rather than measured | log timestamps, entry ids, the date the prompt carries | per turn (10⁰–10¹) | closed | closed | closed |
| `exec.process` | contract | every command a tool starts | which machine the work happens on | per tool call (10¹) | closed | closed | closed |
| `exec.filesystem` | contract | every read, write or stat a tool makes | which filesystem the tools see | per tool call (10¹) | closed | closed | closed |
| `exec.network` | contract | each outbound request; the egress policy binds the ones the model chose | where requests go, whether they go, what answers | per tool call (10¹) | closed | closed | closed |
| `exec.sandbox` | contract | before a command is started | the command that actually runs, and what it may touch | per tool call (10¹) | closed | closed | closed |
| `compaction` | contract | once a turn: two aging passes, a predictive one, then the threshold | the message history, and whether it is rewritten at all | per turn (10⁰–10¹) | closed | closed | closed |
| `script.carrier` | contract | wherever the carrier is bound; governed by a per-turn quota | whatever the bound point allows, under the script's own provenance | per turn (10⁰–10¹) | full | full | declared capability |
| `hooks` | interception | thirty named moments; see the hook event list | varies by event: block, rewrite input, end the turn | per tool call (10¹) | full | full | declared capability |
<!-- END GENERATED TABLE -->

The table above is generated from `base::interface::catalog`, and
`daemon/tests/extension_points_doc.rs` fails if this file drifts from it. Edit
the catalog, not the table.

---

## Contracts — replacing a subsystem

Every one of these is a trait you implement and hand to the builder. They are
`closed` to scripts and plugins because they are wired in Rust at build time.

### The tool set — `tool.registry`

```rust
let tools: Arc<dyn base::tool::ToolRegistry> = Arc::new(MyRegistry::new());
let (agent, events, input) = Builder::new().tools(tools).model(model).build()?;
```

`register` / `replace` / `remove` each return a `Disposer`; disposing takes the
tool back out and it stops being visible to the model.
`base::tool::LayeredToolRegistry` is a worked example — it wraps another
registry and hides tools from it without mutating it.

### The LLM backend — `model`, `model.factory`, `credentials`

`base::interface::model::Model` is one protocol. To make a protocol
*configurable* — reachable by writing `api_type` in `settings.json` — register
a `ModelFactory`:

```rust
let mut factories = model::factory::builtin_registry();
factories.register(Arc::new(MyProtocolFactory));
let router = daemon::model_router::build_task_router_with(
    &providers, "default", resolved, &factories,
)?;
```

Registering an existing `api_type` replaces it, which is how a host puts its
own client behind `anthropic` without inventing a name its users must write
down.

Credentials come from `CredentialSource`, never from `config.api_key` directly:

```rust
let factories = model::factory::builtin_registry()
    .with_credentials(Arc::new(MyVaultCredentials));
```

The value comes back as `Secret`, which has no `Display`, no `Serialize`, and a
`Debug` that prints `<redacted>`. Reading it takes `.expose()`.

### Where settings come from — `config.source`

`Settings::load` reads six files: `settings.json` and its gitignored
`settings.local.json` overlay in each of the global, scene and project tiers.
A deployment whose configuration lives in a config service, a ConfigMap or a
database implements `base::interface::config_source::ConfigSource` and calls
`Settings::load_from` instead:

```rust
let source = config_source::Chain(vec![
    Arc::new(config_source::FileTiers::new(global.clone(), scene.clone(), project.clone())),
    Arc::new(control_plane.fetch_layers()?),   // an `InMemoryLayers`
]);
let settings = Settings::load_from(&source, global, scene, project, "code", "opus");
```

A source decides only which layers exist, in what order, and what JSON is in
them. The merge is the same either way — `paths` stripped from every layer, an
overlay's `permission_rules` held apart, later layers merged recursively over
earlier ones — so moving configuration off the disk cannot change what a
configuration file means.

The directory arguments stay even for a source that reads nothing from them:
they are where the scene's *data* lives, which is not something a settings file
is ever allowed to decide.

`layers()` returns layers rather than a `Result`, because `Settings::load`
never fails. A source that cannot reach its store logs why and returns what it
has: losing a layer is bad, and refusing to start because a remote service is
slow is worse.

### Tool authorization — `permission.check`

`base::interface::permission::Permission` decides whether a tool call runs.
Returning `Prompt` puts the question to a human through
[`elicitation.ask`](#asking-a-person--elicitationask) rather than deciding.

```rust
Builder::new().permission(Arc::new(MyGate));
```

Two binding methods exist for handlers that consult state: `bind_tool_registry`
gives the handler the registry dispatch actually uses (a handler bound to a
different one cannot see tools registered later, and its unknown-tool branch
becomes a hole), and `bind_session_state` keeps it seeing mode changes made
mid-session. Both default to no-ops for handlers that need neither.

### How an agent presents itself — `scene`

`base::interface::scene::AgentScene` owns the system prompt skeleton, the tool
whitelist and the budgets:

```rust
Builder::new().scene(Arc::new(MyScene));
```

Four scenes ship, three of them written as a copy-from-here ladder: a full
reference, a compact one, and a minimal skeleton. A scene that names its own
prompt sections keeps those names under the `scene.` prefix — see the block
names below.

### Session storage — `history.store`

`history::store::HistoryStore`. Two implementations ship:
`JsonlHistoryStore` (files under a project directory) and
`InMemoryHistoryStore`. The contract's guarantees are in its doc comment;
`store::contract_tests` runs the same six properties against both, and is the
place to point a third backend at.

### Finding sessions — `history.query`

`HistoryStore::find_sessions` takes a `history::query::SessionQuery` — a
needle, a scope and a ceiling — and answers with summaries, newest first. The
recency listing and the text search are the same question with and without a
needle, which is why they are one method: a backend that can answer one
cheaply can answer both.

The default reads every session in range to answer, and `JsonlHistoryStore`
does better only by ordering on file mtime and narrowing by directory. A
backend with a real index overrides this one method and takes search over
whole; the guarantees it has to keep — newest first with a total order, at
most the limit, and never fewer matches than a case-insensitive substring
scan would find — are in the method's doc comment.

### Large content — `history.blob`

`history::blob::BlobStore`. A `User`, `Assistant` or `ToolResult` entry whose
content is over a kilobyte, or that carries an image at any size, is written
to the blob store and replaced in the JSONL by a `LogEntry::Blob` naming the
store and the id. A load with that store attached puts the original entry
back; everything above `HistoryStore` only ever sees hydrated entries.

```rust
JsonlHistoryStore::with_roots(cwd, roots).await?.with_blob_store(my_store)
```

Two implementations ship: `PasteStore` (content-addressed files under
`<base>/pastes/`, the default) and `InMemoryBlobStore`. An implementation must
be content-addressed, must answer `None` rather than an error for content it
does not have, and must keep its `name` stable — the name is written into the
log.

**An unresolvable reference is inert, never an error.** Uninstall the backend,
copy a log without its blobs, or let a cleanup run, and the session still
loads, forks and resumes with a gap where the content was. That is the same
rule `history.extension_entry` follows, for the same reason: refusing to open
a conversation because part of it is unreachable trades a degraded session for
no session.

### Memory — `memory.storage`, `memory.retriever`

Storage is where memories are; the retriever decides which ones a turn sees.
The shipped retriever asks the model, which costs a call:

```rust
Builder::new().memory_retriever(Arc::new(MyIndexRetriever))
```

`base::interface::memory_contracts::SubstringRetriever` costs nothing and is a
pure function of the store — what a test asserting recall wants.

### Asking a person — `elicitation.ask`

One trait for all three questions the engine asks a human: may this tool run,
what did you mean, shall I import this.

```rust
Builder::new().elicitation(Arc::new(MyDialogs))
```

With nothing registered every question is *declined with a reason*. Silence is
never consent.

### Where events go — `event.sink`

```rust
Builder::new().event_sink(sink)
```

Each sink gets its own queue and task, so a slow one loses its own events and
nobody else's time. It cannot pace a turn.

### Judging context size — `token.count`

`base::interface::token_counter::TokenCounter`. The default is a local
`cl100k_base` estimate that runs 5–15% high because Anthropic publishes no
tokenizer; a host that can be exact should be.

### Reaching the machine — `exec.process`, `exec.filesystem`, `exec.network`, `exec.sandbox`

Four contracts designed as one, because they are entangled: a sandbox
constrains a process, a process needs files and a network, and the network
policy has to reach inside the sandbox. `ARCHITECTURE.md` §5 has the
reasoning; this is the summary.

```rust
let mut ctx = /* … */;
ctx.exec = ExecProviders::in_process();   // switching is this call
```

Two provider sets ship. `ExecProviders::local()` is this machine and the
default everywhere. `ExecProviders::in_process()` is a memory tree, commands
decided in advance, a network that answers only what it was given, and a
sandbox that reports it constrains nothing — paired with a `FixedEnvironment`,
a whole session runs and replays without touching anything.

Three shapes are worth knowing before implementing a provider of your own.

**`Process` streams and `FileSystem` does not.** A long command's output has to
reach the user while it runs, so the handle yields chunks tagged with which
pipe they came from; a provider that returned everything at the end would
remove that silently. Files are whole values because every call site wants one
and tool results are capped long before a file could need chunking.

**Path safety is above the contract, canonicalization is inside it.** Whether a
path may be written is policy, and a provider deciding its own out-of-bounds
rules could cancel them. But a remote's symlink graph is the remote's, so the
order is: canonicalize through the provider, check the resolved path, write
through the provider.

**The egress policy asks where the *model* may reach.** A request carries who
chose its destination. `allowed_domains` binds `Origin::Agent` — a WebFetch
url, a Ping host — and not `Origin::Operator`, which is the model endpoint,
MCP servers, telemetry. Applying it to everything would cut the agent off from
its own model.

Operator traffic is built through the same contract, but a host cannot yet
replace the providers those clients use — the model, OAuth, telemetry and
registry clients each construct their own. So replacing this point governs
what the model can reach, and auditing or going offline *as a whole* is not
something it delivers today.

The sandbox has one invariant the engine enforces rather than requests: **a
policy that asked for constraint never silently becomes an unconstrained
run.** A backend reports `Full`, `Partial` or `None` and names what it could
not deliver; `sandbox.require_enforcement` decides whether anything short of
`Full` is refused. What to constrain and whether to constrain at all stay the
kernel's — a provider answers only *how*.

### Compaction — `compaction`

`compaction::compact::Compactor`. Both halves of the question: how a
conversation is shortened, and when.

A turn offers four opportunities and the contract answers each — two aging
passes that clear tool results by age, a predictive pass that fires before the
budget is reached, and the threshold that forces the transformation. Every one
has a default that is the engine's own arithmetic, so an implementor who only
cares about the transformation writes `compact` and nothing else.
`tool_result_budget` is the numbers-only version of the same idea: change how
much of a request accumulated tool results may hold without reimplementing
what is done about it.

```rust
Builder::new().compactor(Arc::new(OnlyAtThreshold(DefaultCompactor)))
```

`OnlyAtThreshold` wraps any compactor and declines everything the budget did
not force — the shape a deployment wants when it needs its transcript left
alone until a rewrite is genuinely unavoidable.

### When a turn has gone on long enough — `turn.policy`

`base::interface::turn_policy::TurnPolicy`. Two ceilings — model calls per
turn, structured-output retries — asked at the two points they are checked
today, because consolidating them would reorder the loop.

```rust
Builder::new().turn_policy(Arc::new(FirstOf(vec![engine_default, mine])))
```

Compose rather than replace: `FirstOf` keeps the engine's limits and adds
yours, and a stop is never overridden by a later policy.

Only judgements about *progress* live here. Cancellation is an instruction,
not an opinion; a `PostToolUse` or `Stop` hook ending a turn is already an
extension point's output, and letting a policy overrule one would invert the
trust order; and "the model asked for tools, so there is more to do" is the
loop's definition rather than a judgement about it.

### When a call goes wrong — `model.recovery`, `model.backoff`

`base::interface::recovery_policy::RecoveryPolicy` decides what a failure
*means for the turn*: switch to the fallback model, compact and retry, raise
the output limit, or fail. Classification stays with the engine — which errors
*are* an overload and which *are* a size refusal is a fact about the wire
protocol. `NeverRecover` is a real configuration rather than a stub: a
deployment billed per token would rather return a truncated answer than a
surprise 64K call.

`base::interface::backoff::BackoffPolicy` sits below the model contract, in
the clients, and answers the narrower question of whether a failed request is
sent again and how long to wait first. Both wire protocols go through the same
one. `retry-after` parsing stays in the Anthropic client, because that is a
protocol-specific input; what to do with it is the policy's.

### What a turn may spend — `budget`

`base::interface::budget_policy::BudgetPolicy`. Three judgements that were
constants: a cumulative token ceiling, an output-volume target with its
diminishing-returns rule, and the compaction threshold the scene returns.

The one that was missing entirely is a ceiling on request *size*. A threshold
is a trigger — crossing it starts a compaction, and if the compaction cannot
get far enough the request goes out anyway. `Capped` adds the ceiling, and a
turn still above it after compaction ends as `context_exceeded` rather than
sending something the deployment said must never be sent.

```rust
Builder::new().budget_policy(Arc::new(Capped {
    inner: EngineBudget::new(settings.execution.max_budget_tokens),
    context_hard_cap: 120_000,
}))
```

### What a log means to the model — `history.projection`

`history::transcript::TranscriptProjection`. Which entries become messages,
and what they say. It lives on the store rather than the engine because a log
and the rules for reading it travel together: resume, fork, search and paging
must all see the same conversation.

```rust
JsonlHistoryStore::with_roots(cwd, roots).await?
    .with_projection(Arc::new(ExtensionsAreVisible { namespaces: vec!["com.acme.deploy".into()] }))
```

`ExtensionsAreVisible` is the answer to the question `history.extension_entry`
raises and cannot settle: an extension's own entries becoming something the
model reads.

Saying yes carries one obligation. **Model-visible content must be
reconstructible from the log alone** — a projection that renders an entry by
asking the live extension what it means produces a conversation that cannot be
reopened once the extension is gone.
`transcript::model_visible_content_is_reconstructible` makes that checkable
rather than aspirational.

### Kept time and kept identifiers — `environment`

`base::interface::environment::Environment`. Wall-clock now, and fresh ids.

Only the answers that get *kept* come from here: a timestamp written into the
log, an id naming an entry, the date the prompt tells the model. There is
deliberately no monotonic clock on the contract — `Instant` cannot be stored
or transmitted, everything it is used for is measurement, and measurement is
the category this is not for.

```rust
let env = Arc::new(FixedEnvironment::epoch());
let store = JsonlHistoryStore::with_roots(cwd, roots).await?.with_environment(env.clone());
let agent = Builder::new()./* … */.history_store(store).environment(env).build()?;
```

Two places, because the log's half is written by the store and the
model-visible half by the agent. Configure both, or a replay differs in the
half you forgot.

The builder hands the same environment to the memory store it creates, so
durable memories age against it too — that is what decides which of them are
offered to the model. A store supplied with `Builder::memory_store` is
configured by whoever built it: `MemoryStore::new(user, local).with_environment(env)`.

### Where skills come from — `skill.source`

`base::interface::skill_provider::SkillProvider`. Three implementations ship
and cover the three places skills have always come from:
`skills::sources::SkillDirectory` (a directory tier), `BundledSkills` (the
built-ins compiled into the binary), and `McpSkills` (one connected server's
tools). A fourth is added, not substituted:

```rust
Builder::new().skill_provider(Arc::new(StaticSkills::new("company", entries)))
```

A source registered this way defers to what is already loaded. One that means
to replace a skill the engine ships returns `SkillPrecedence::Override`.

`SkillProvider::body` is why this is a source rather than a list: a skill
whose text lives in the source, not in a file, still expands when it is
invoked. `base::interface::skill_provider::StaticSkills` is the in-memory
implementation for a host that already holds both.

### Standing instructions and the rule index — `instruction.source`, `rules.source`

`base::interface::instruction_provider::InstructionProvider` decides what the
`AGENTS.md` / `CLAUDE.md` injection contains; `RuleProvider` decides which
rule documents the model is told exist. `InstructionFile` and `RuleDirectory`
are the filesystem implementations, `InlineInstructions` and `StaticRules` the
in-memory ones.

```rust
Builder::new().instruction_provider(Arc::new(InlineInstructions::new(
    "service://conventions",
    conventions_text,
)))
```

The three rule tiers — global, scene, project — are a `RuleProvider` each,
composed by `base::rules::default_rule_sources` and merged last-wins by
`discover_rules_from`, so a different tier list is a different composition
rather than a change to the discovery function.

---

## Registrations — contributing

### Prompt blocks — `prompt.block`, `prompt.context`, `prompt.variable`

```rust
use base::interface::prompt_registry::{orders, InMemoryPromptRegistry, RegisteredBlock};

let registry = InMemoryPromptRegistry::new();
let handle = registry.register_block(
    RegisteredBlock::system("mine.preamble", orders::SCENE + 50, "read this first"),
);
Builder::new().prompt_registry(registry.clone());
// later
handle.dispose();
```

Blocks sort by `order`, ascending, ties broken by registration order. The
kernel's stages sit at round hundreds (`orders::SKILLS_CATALOG` is 100,
`MEMORY_SESSION` 200, `RULES` 300, `MCP_INSTRUCTIONS` 400,
`CONFIG_PROMPT_APPEND` 500), so there are ninety-nine places between any two of
them and negative orders come before the scene.

`register_context` is the same thing with the text computed at assembly time;
returning `None` contributes no block at all, which is how a contribution stays
out of the sessions it has nothing to say about. `register_variable` makes
`{{name}}` expand everywhere — an unregistered placeholder, or one whose
provider declines, is left exactly as written.

### The kernel's block names

These are a **published contract**. An extension positioning itself relative to
one is relying on the name to keep meaning what it means.

| Name | What it is |
|---|---|
| `scene.skeleton` | the scene's prompt, when the scene names no sections |
| `scene.<section>` | a scene that names its own sections keeps them, prefixed |
| `skills.catalog` | the inventory of available skills |
| `memory.session` | how to use the file-based memory system |
| `rules` | the discovery index of `.atta/rules/` |
| `mcp.instructions` | instructions from connected MCP servers |
| `config.prompt_append` | `settings.prompt_append` |
| `config.prompt_override` | `settings.prompt_override`, which replaces everything |

Every block also carries an `origin` — `Kernel`, `Config`, `Plugin(name)` or
`Script(path)` — which is what the assembly hook's authority rules read.

### Your own state in the session log — `history.extension_entry`

```rust
store.append(session, LogEntry::Extension {
    ns: "com.example.myplugin".into(),
    event: "checkpoint".into(),
    payload: serde_json::json!({ "step": 3 }),
}).await?;
```

The kernel never parses the payload. An entry whose namespace nobody claims is
carried along and otherwise ignored, so uninstalling a plugin leaves its old
sessions loading, forking and resuming exactly as before.

### Whether things are working — `health.check`

```rust
struct QueueDepth(Arc<Metrics>);

impl HealthCheck for QueueDepth {
    fn name(&self) -> &str { "acme.queue" }
    fn check(&self) -> CheckResult {
        match self.0.depth() {
            d if d < 1000 => CheckResult::ok("queue drained"),
            d => CheckResult::degraded(format!("{d} queued"))
                .with_details(serde_json::json!({ "depth": d })),
        }
    }
}

let agent = Builder::new()./* … */.health_check(Arc::new(QueueDepth(metrics))).build()?;
let health = agent.health();          // take this before spawning the engine
let report = health.report();         // fresh answers, every time
```

The report carries every registered check's verdict and the worst of them.
The engine's own checks are registered by whoever wires it: `daemon` registers
the settings tiers, provider routing, hooks configuration and the plugin fault
records, and reports the lot under `daemon.doctor`'s `health` key.

Two rules the contract enforces rather than asks for. **A check reports and
never repairs** — there is no return value that reopens a circuit breaker,
reloads configuration or restarts anything, because a diagnostic that quietly
fixed things would describe what it just did rather than what it found. And
**`check()` is synchronous and expected to answer from state it already
holds**: a probe that blocks on the subsystem it is probing hangs exactly when
the answer matters most. A check that needs the network keeps a cached verdict
updated out of band and reports that.

There is no `Err`. A check that cannot determine the answer has determined
something — it says `Degraded` and puts the reason in its summary, rather than
leaving every caller to decide for itself whether a failed check means
unhealthy.
### Prompt assembly itself — `prompt.assembler`

`base::interface::prompt_assembler::PromptAssembler`. The registrations above
open what goes into the prompt and `prompt.assemble` opens what happens to the
finished result; this opens the part in between — the order the stages are
placed in, how a contribution's order merges with the kernel's, where the cache
boundaries fall, whether two blocks stay two blocks.

```rust
Builder::new().prompt_assembler(Arc::new(MergedSystemPrompt::default()))
```

`DefaultAssembler` is the engine's own assembly and the default.
`MergedSystemPrompt` wraps any assembler and folds its system blocks into one,
which is what a deployment with more prompt contributors than the four
`cache_control` breakpoints a request allows wants: one cached prefix rather
than an arbitrary four.

An assembler receives an `AssemblyRequest` — the registry, the scene, the
settings, the memory store, the scene context, and the already-rendered skills
inventory and MCP instructions. It is a struct so that an implementation
reading two of them need not restate the other five.

---

## Interceptions — sitting in the path

### Around a tool call — `tool.around`

```rust
#[async_trait]
impl ToolMiddleware for Deadline {
    async fn around(&self, call: &ToolCall, exec: &mut ToolExec, next: NextDispatch<'_>)
        -> ToolOutcome
    {
        if call.name == "Bash" { exec.with_timeout(Duration::from_secs(30)); }
        next.run(call, exec).await
    }
}
Builder::new().tool_middleware(Arc::new(Deadline));
```

Narrow the signal (a timeout), answer without calling through (a cache), or
call through more than once (a retry). You cannot rewrite the call's arguments
— the call arrives behind a shared reference and `run` takes none. `with_timeout`
derives a *child* of the current token, so a wrapper can end a call earlier
than the turn would and never later.

Wrappers nest in registration order, first outermost.

### What a tool result may look like — `tool.result`

```rust
Builder::new().tool_result_transformer(Arc::new(RedactLiterals::new([api_key])));
```

Runs **last** — after every hook, immediately before the model reads it — which
is what makes a redacting transformer a guarantee rather than a suggestion.
`TruncateText` and `RedactLiterals` ship; neither is registered by default.

### The assembled prompt — `prompt.assemble`

```rust
impl AssemblyHook for Mine {
    fn on_assemble(&self, asm: &mut PromptAssembly, ctx: &ScenePromptContext<'_>)
        -> Result<(), String>
    {
        asm.insert_after("scene.skeleton", PromptBlock::system("…").named("mine.note"));
        asm.modify("skills.catalog", curated)?;   // needs authority
        Ok(())
    }
}
```

`push` and `insert_after` are always permitted; `modify`, `remove` and
`move_before` need authority. A hook registered as
`Authority::local(BlockOrigin::Script(path))` has all of it. One registered as
`Authority::plugin(name, caps)` has only what `caps` declared, and a refused
edit returns `Denied` and is counted on the assembly rather than failing
silently.

### The model request and the finished message — `model.request`, `model.message`

```rust
Builder::new().model_interceptor(Arc::new(Mine));
```

`on_request` sees messages, tools and parameters before anything leaves the
process. `on_message` sees a complete message after the stream carrying it
finishes.

**There is no per-chunk hook and there will not be one by default.** A turn
produces thousands of chunks; a callback there is called thousands of times and
the cost is invisible to whoever writes it. A hook that could rewrite chunks
could also produce a message that never existed as a coherent whole. If you
need to transform a stream as it arrives, ask for a declarative rule the engine
can execute natively.

### Around memory recall — `memory.retrieval_hook`

`before_retrieve` changes the question; `after_retrieve` changes the answer. A
deployment knows things about its own vocabulary the retriever does not, and
things about its own policy the retriever should not have to.

### Watching the log — `history.append_observer`

```rust
let store = Arc::new(ObservedHistoryStore::new(inner, vec![my_observer]));
```

Read-only in the types, not by agreement: the entry arrives behind a shared
reference and nothing is returned. Observers run after the append succeeded, so
a failed write is never observed.

`history::observers::AppendCounts` ships as one: a running tally of how many
entries, of which kind, have gone into each session. The question it answers
— how big has this session got — otherwise costs a read and a parse of the
whole log, per asking. Hold the same `Arc` the store holds, and call `forget`
when a session is done with, or the map grows for the life of the process.

### Lifecycle hooks — `hooks`

Thirty named events, five backends (command, prompt, HTTP, agent, wasm),
configured in `settings.json`. Some events parse and accept configuration but
nothing fires them yet; `hooks::UNWIRED_EVENTS` is the list, the engine warns
when you configure one, and `daemon/tests/hook_event_wiring.rs` keeps the list
honest in both directions.

---

## Running your own code at a point — `script.carrier`

`base::interface::script::ScriptEngine` is the cheap tier: a piece of the
operator's code, in this process, in microseconds, between "recompile the
engine" and "spawn a subprocess".

The engine is QuickJS, in `crates/script-host`, behind `daemon`'s `scripts`
feature. `base` holds only the contract — it has no internal dependencies and a
JavaScript runtime there would end up in every build of everything.

The summary below is enough to decide whether you want this. **`extending_quickjs.md`
is the guide** — one section per bindable point, with the input and return
shapes and a working example for each.

Bind a script by writing one in your project and naming it in `settings.json`:

```jsonc
// .atta/scripts/prompt.js
function onAssemble(blocks) {
  return blocks.map(b => {
    if (b.name === "skills.catalog") b.content = curate(b.content);
    return b;
  });
}
```

```jsonc
// settings.json
"scripts": [
  { "path": ".atta/scripts/prompt.js", "point": "prompt.assemble", "entry": "onAssemble" }
]
```

Optional `timeout_ms` and `calls_per_turn` narrow the budget. No recompilation
is involved; the file and the config line are the whole of it.

**Authority follows the file's location, not the configuration.** A script
inside the project root is the operator's own and may rewrite anything; one
anywhere else arrived from outside and may add, like any downloaded extension.
Nothing in the binding can change that, because a declaration is exactly what
an outside script would lie in.

A binding naming a point that does not exist, or one scripts may not be bound
to, is refused at startup and named — and one bad binding drops the whole set
rather than applying half of it. A script that silently never runs sends its
author looking for a bug in their JavaScript.

Today the bindable points are `prompt.assemble` and nothing else. The list is
short on purpose: every entry is a place where a script's cost is bounded and
its authority is defined, so adding one means answering both questions rather
than appending a string.

**What a script can reach: nothing.** No filesystem, no network, no host
bindings, and no state that survives a call — each call gets a fresh runtime,
so one session's script cannot leave anything for another's.

`ScriptCarrier` enforces the per-turn quota and the wall-clock budget from
outside the engine, because an engine that enforced its own limits could choose
not to. The engine additionally carries the deadline into QuickJS's interrupt
handler, which is what actually stops `while(true){}` — a timeout abandons a
future without stopping a busy loop.

---

## What a script can be bound to

The `script` column above says what the *contract* opens. The QuickJS carrier
has an adapter for nine of those, and a binding to one of the others is
refused at startup naming the ones that work — a script that quietly never
runs is worse than one that fails to load.

```jsonc
"scripts": [
  { "path": ".atta/scripts/prompt.js", "point": "prompt.assemble", "entry": "onAssemble" }
]
```

| point | when it runs | what a script may do |
|---|---|---|
| `prompt.assemble` | after the prompt is assembled | rewrite blocks, subject to its authority |
| `prompt.block` | at assembly | add a named block |
| `prompt.context` | at assembly, per turn | add a block whose text it computes |
| `prompt.variable` | wherever `{{name}}` appears | supply the value |
| `tool.around` | before dispatch | refuse, answer instead, or shorten the clock |
| `tool.result` | before the model sees a result | rewrite its text |
| `memory.retrieval_hook` | both ends of recall | change the query, filter what came back |
| `model.request` | per model call | change model, ceiling, thinking mode; narrow the tool list |
| `model.message` | per completed message | rewrite text blocks |

Each adapter's own documentation carries the JSON contract — what the script
is handed and what it may return. That is the only documentation a script
author has, so it lives beside the code that has to keep it true.

### The four that stay closed, and why

**`history.append_observer`** fires once per log entry. That is the frequency
band the catalog closes to scripts deliberately: the cost of a callback there
is invisible to whoever writes one, and it is per *entry*, not per turn.

**`history.extension_entry`** is a write capability, not a hook. A script
needs an API to *emit* an entry; a callback that receives them is a different
thing wearing the same name.

**`hooks`** is its own subsystem with its own process model. A script engine
bound there would be a second way to do what that subsystem already does.

**`script.carrier`** is the carrier.

### What every adapter guarantees

**A script that fails changes nothing.** Times out, throws, exhausts its
quota, returns the wrong shape — all one outcome, and it is the outcome of
having done nothing. A point half-changed by a script that died mid-pass is
worse than an unchanged one, because nothing downstream can tell which it is
looking at. "Returned nonsense" and "has a bug" get the same harmless answer
on purpose.

**A script never widens its own authority.** Provenance travels with the
carrier: a script the operator wrote may rewrite, one that arrived with a
downloaded plugin may add. An adapter reads that and does not decide it.

**A script never gets more of the engine than the point needs.** A model
request is handed its knobs and not its messages; a prompt contribution is
handed the environment and not the other blocks; a message is handed its text
and not its thinking signatures. Each exclusion is argued where it is made,
because the tempting version of every one of these is "pass the whole struct".

**A quota is per turn, and a turn says so.** Carriers are reset at the top of
each turn; without that the budget would be per session, and a script bound to
a per-tool-call point would go quiet partway through a long one.

## Carrier invariants

Whatever loads an extension — WebAssembly today, a script engine next to it —
four things hold, and `daemon/tests/carrier_invariants.rs` fails if they stop
holding:

1. **One capability table, one authorization function.** Both live in
   `base::interface::capabilities`. A carrier converts its manifest into
   `CapabilityDeclaration` and asks; it does not answer.
2. **Carriers do not call each other.** They reach each other through host
   contracts, never a direct call across memory models.
3. **Every carrier is a compile-time feature, and a build carries at most
   one.** `scripts` brings QuickJS and is the default; `plugins` brings the
   WebAssembly tier and needs `--no-default-features` to get it, because
   asking for both is refused by a `compile_error!` rather than quietly
   accepted. `cargo build -p daemon --no-default-features` links neither.

   Exclusive because carrying both is twice the attack surface for a
   capability nobody asked for twice, and because cargo's feature unification
   makes "both" somewhere you arrive by accident rather than by choice —
   `--features plugins` without `--no-default-features` is enough to do it.
4. **Disclosure covers every carrier.** It is about what an extension *says*,
   so it names no carrier at all.

Nothing is granted by omission: an extension that declares no capabilities can
compute and no more.

**`extending_wasm.md`** is the guide for the WebAssembly carrier — the world a
component implements, `plugin.toml`, the capability model, install-time
disclosure, and what a plugin can contribute.

---

## What is not open

Eleven things stay in the kernel. Making any of them replaceable would hand
away the property it exists to guarantee.

| # | Kernel-only | Why it cannot open |
|---|---|---|
| 1 | Capability authorization and module resolution | The function that decides whether `import 'host:fs'` resolves. Replaceable means unauthorized |
| 2 | Permission rule evaluation order | The order of the eight stages *is* the security property — a tool's own Allow must come after deny rules and path checks |
| 3 | Resource limits and interrupts | The only way to stop a runaway extension |
| 4 | Scheduling and quota accounting | Replaceable accounting is bypassable quota |
| 5 | Whether a sandbox policy is applied | The backend is swappable; the decision to apply it is not |
| 6 | Append-only log semantics and their invariant checks | "Model-visible means logged" is held by runtime assertions; replaceable assertions mean no principle |
| 7 | The turn skeleton's step order | The order carries every invariant. The *decisions* at each step are open — see §2 of `ARCHITECTURE.md` |
| 8 | Install-time disclosure | A restriction that only warns is one every auto-installer steps straight over |
| 9 | Permission rule source precedence | Plugin rules must always rank below user settings and org policy |
| 10 | Scene composition and inheritance | The combination surface grows exponentially, and it contradicts the one-scene-per-session replay invariant |
| 11 | The number of plugin contribution points | The plugin subsystem can be compiled out; that depends on the contribution points staying countable |

The first six are not only safety requirements — they are what distinguishes
this kernel from a general-purpose framework. A design that needs one of them
opened is a design to argue about, not a patch to write.
