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
| `prompt.assemble` | interception | prompt assembly, last | block content, order and membership | per turn (10⁰–10¹) | full | full | declared capability |
| `event.sink` | contract | every emission, on the sink's own task | nothing — observation only | per streamed chunk (10³–10⁴) | closed | closed | closed |
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
| `history.extension_entry` | registration | any time; ordered with everything else | adds only; the kernel never reads the payload | per turn (10⁰–10¹) | full | full | add only |
| `memory.storage` | contract | recall, and whenever a memory is written | how and where memories persist | per turn (10⁰–10¹) | closed | closed | closed |
| `memory.retriever` | contract | once per user message, in the background | the recalled set | per turn (10⁰–10¹) | closed | closed | closed |
| `memory.retrieval_hook` | interception | around retrieval | the query and the recalled names | per turn (10⁰–10¹) | full | full | declared capability |
| `history.append_observer` | interception | after each append succeeds | nothing — read-only in the types, not by agreement | per turn (10⁰–10¹) | full | full | add only |
| `compaction` | contract | when the budget threshold is crossed | the message history | per turn (10⁰–10¹) | closed | closed | closed |
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

### Compaction — `compaction`

`compaction::Compactor`. Not yet reached by this refactor: the trigger and
several strategy decisions still live in the turn loop rather than behind the
trait. That is Phase 3.

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

## Carrier invariants

Whatever loads an extension — WebAssembly today, a script engine next to it —
four things hold, and `daemon/tests/carrier_invariants.rs` fails if they stop
holding:

1. **One capability table, one authorization function.** Both live in
   `base::interface::capabilities`. A carrier converts its manifest into
   `CapabilityDeclaration` and asks; it does not answer.
2. **Carriers do not call each other.** They reach each other through host
   contracts, never a direct call across memory models.
3. **Every carrier is a compile-time feature, independently.** A build carries
   one, both or neither: `plugins` brings the WebAssembly tier, `scripts`
   brings QuickJS, and `cargo build -p daemon --no-default-features` links
   neither.
4. **Disclosure covers every carrier.** It is about what an extension *says*,
   so it names no carrier at all.

Nothing is granted by omission: an extension that declares no capabilities can
compute and no more.

---

## What is not open

Eleven things stay in the kernel, listed with their reasons in
`docs/EXTENSIBILITY_DESIGN.md` §5. The short version: the turn's skeleton, the
authorization point, append-only log semantics, the number of plugin
contribution points, and the resource and interrupt boundaries. A design that
needs one of them opened is a design to argue about, not a patch to write.

Two areas are open in principle and not yet reached:

- **Turn-loop decisions** — stop conditions, backoff, retry, when to compact —
  are still inside the loop rather than behind strategy contracts. Phase 3.
- **The execution layer** — process spawning, filesystem, network, sandbox —
  is still direct. The sandbox now reports honestly whether a policy is
  actually enforced (`sandbox.require_enforcement` refuses rather than running
  unconstrained), but the seams themselves are Phase 4.
