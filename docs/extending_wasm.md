# Extending AttaCore with WebAssembly plugins

A plugin is a directory with a `plugin.toml` and, usually, one or more
WebAssembly components. It is installed from an archive, it runs in a sandbox
whose reach it had to write down before anyone installed it, and it can be
uninstalled again without touching the binary.

This is the expensive tier. If what you want is a few lines of the operator's
own code at one point in the turn, that is the QuickJS carrier
(`docs/extending_quickjs.md`) — it costs about a megabyte, where this costs
twenty, and it needs no packaging. Reach for a plugin when the thing you are
adding is a *package*: someone else's code, distributed, versioned, installed
and removed by name.

The overview of every extension point, and which of them a plugin may reach at
all, is `docs/extension_points.md`.

---

## Before anything else: the build carries one carrier

The plugin subsystem is **not** in the default build. `daemon`'s default
feature is `scripts` (QuickJS), and the two carriers are mutually exclusive —
`daemon/src/lib.rs` refuses a build carrying both with a `compile_error!`
rather than accepting it quietly, because cargo's feature unification makes
"both" somewhere you arrive by accident.

```bash
cargo build -p daemon                                            # QuickJS scripts (default)
cargo build -p daemon --no-default-features --features plugin-compile   # plugins, compiler in-process
cargo build -p daemon --no-default-features --features plugins          # plugins, no compiler linked
cargo build -p daemon --no-default-features                       # neither carrier
```

`--no-default-features` is required: `--features plugins` on its own unions
`scripts` back in and the guard rejects the build.

The second flag is about *where a component gets compiled*:

- **`plugin-compile`** links Cranelift into the daemon. Installing a plugin
  compiles its components in-process.
- **`plugins`** alone links no WebAssembly compiler at all. `Component::new`
  does not exist in that build, so the daemon can only load artifacts
  something else produced — it shells out to the `atta-plugin-compile` binary
  at install, found by searching upward from the running executable, and a
  cache miss at load is a refusal rather than a quiet recompile. Two thirds
  of the carrier's size is the compiler, so this is the build for a deployment
  that runs plugins but does not want a code generator in the serving process.

`tests/scripts/locked_build.sh` checks all of this against a real dependency
graph: that both carriers together fail, that the no-carrier build has no
plugin machinery in `cargo tree`, and that the `plugins`-without-`plugin-compile`
build links no Cranelift.

A running daemon answers the question about itself — `daemon.doctor` reports
`plugins.status` as `compiled-out`, `enabled` or `disabled-by-policy`, and the
`plugin.*` RPCs return `PLUGINS_DISABLED` (`-32016`) rather than an empty list
when the subsystem is not there.

---

## From zero: a plugin that works

The repository ships the component half of one at
`tests/fixtures/wasm_echo_plugin/` — small on purpose, and what
`crates/wasm-host/tests/component_roundtrip.rs` builds and provokes. Everything
below is that fixture with a manifest around it.

### 1. The world your component implements

The contract is one WIT world, `crates/wasm-host/wit/plugin.wit`, published as
`atta:plugin@0.1.0`:

```wit
world plugin {
  import host;                                    // log, progress, now-ms, http-request, secret, kv-get, kv-set
  export tools;                                   // list-tools, call-tool
  export events;                                  // on-event
  export init: func(config-json: string) -> result<_, string>;
}
```

`tools` and `events` are both required exports of the world. A component that
does not participate in events still exports `on-event` and answers `proceed`.

### 2. The component

A Rust guest, outside the workspace (it builds for `wasm32-wasip2`, and a
member that only compiles for another target breaks `cargo build --workspace`):

```toml
# Cargo.toml
[package]
name = "wasm-echo-plugin"
version = "0.1.0"
edition = "2021"

[workspace]            # deliberately its own workspace

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = "0.48"
```

```rust
// src/lib.rs
wit_bindgen::generate!({ path: "…/crates/wasm-host/wit", world: "plugin" });

use exports::atta::plugin::tools::{Guest as ToolsGuest, ToolDef, ToolOutput};
use exports::atta::plugin::events::{Guest as EventsGuest, HookDecision};

struct Component;

impl ToolsGuest for Component {
    fn list_tools() -> Vec<ToolDef> {
        vec![ToolDef {
            name: "echo".into(),
            description: "Echo the `text` argument back".into(),   // ships every turn
            doc: Some("Returns `text` verbatim, plus a structured copy.".into()), // fetched on demand
            input_schema: r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#.into(),
            read_only: true,
            concurrency_safe: true,
        }]
    }

    fn call_tool(_name: String, input_json: String, call_id: String) -> ToolOutput {
        atta::plugin::host::progress(&call_id, "echoing");   // streamed to the user
        ToolOutput { content: input_json, structured: None, is_error: false }
    }
}

impl EventsGuest for Component {
    fn on_event(_event: String, _payload_json: String) -> HookDecision {
        HookDecision::Proceed
    }
}

impl Guest for Component {
    fn init(_config_json: String) -> Result<(), String> { Ok(()) }
}

export!(Component);
```

Build it:

```bash
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/wasm_echo_plugin.wasm
```

### 3. The manifest

```toml
# plugin.toml, at the root of the plugin directory
[plugin]
name = "echo-plugin"
version = "1.0.0"
api_version = "0.1"
description = "Echoes things back"

[[wasm]]
component = "echo.wasm"
```

`api_version` has no default. A manifest that omits it fails to parse, and one
naming a version this build does not implement is refused with the supported
set in the message — the WIT world, the capability semantics and the event
whitelist move together, so there is no partial contract to fall back to.

### 4. Package and install

A plugin archive is a plain zip of the plugin directory, with `plugin.toml` at
the archive root. Nothing about the layout is special; `tests/runner/src/plugin_fixture.rs`
is the whole packaging step.

```bash
cd my-plugin && zip -r ../my-plugin.zip . && shasum -a 256 ../my-plugin.zip
```

Install over the daemon's JSON-RPC socket:

```json
{"jsonrpc":"2.0","id":1,"method":"plugin.install",
 "params":{"name":"echo-plugin","version":"1.0.0",
           "download_url":"file:///abs/path/my-plugin.zip",
           "checksum":"<sha256 hex>",
           "scope":"global"}}
```

- `download_url` accepts `https://`/`http://` and `file://`. A **checksum is
  required for network sources** and refused-before-fetching if absent;
  `file://` is a local sideload path and may omit it.
- `scope` is `"global"` (default) or `"scene"`, picking which tier's cache the
  plugin lands in.
- Install extracts, then **compiles every declared component immediately**. A
  component that cannot be compiled fails the install and the plugin is removed
  again — discovering that while the user is standing there beats discovering
  it mid-session.
- Zip entries with `..` or absolute paths are skipped, not extracted.

The response carries `success`, `message`, and a `disclosure` object — see
[Install-time disclosure](#install-time-disclosure).

The rest of the lifecycle: `plugin.list` (every installed plugin with its
enable state, including disabled ones), `plugin.enable` / `plugin.disable`
(persisted per tier in `enabled.json`; scene setting wins over global, and an
unset plugin is enabled), `plugin.uninstall` (optional `version`, omit to
remove all), `plugin.reload`.

### 5. See it work

A plugin's tools reach a model through a scene the plugin owns, so give it one:

```toml
# plugin.toml, continued
[scene.own]
name = "Echo"
prompt = "scene/prompt.md"
```

Reinstall, then create a session in it:

```json
{"jsonrpc":"2.0","id":2,"method":"session.create","params":{"scene":"plugin:echo-plugin"}}
```

`scene.list` shows `plugin:echo-plugin` as active, and the session's tool
registry now carries `plugin__echo-plugin__echo` — deferred, so the model finds
it through `ToolSearch` rather than in every request's tool array.

### 6. Where it lands

```
~/.atta/plugins/cache/<name>/<version>/                   # global tier
~/.atta/scenes/<scope>/plugins/cache/<name>/<version>/    # scene tier, overrides global by name
    plugin.toml
    echo.wasm
    config.json          # optional, user-supplied (see [plugin.config])
    .aot/<hash>.cwasm    # compiled artifact, content-addressed by component bytes
```

The scene tier overrides the global one for the same plugin name; among several
installed versions of one name, the highest semver wins. A manifest that fails
to load is skipped with a warning, and the other plugins still load.

---

## `plugin.toml`

Every section below is one the loader actually reads. Anything else in the file
is ignored silently.

```toml
[plugin]
name = "github-tools"          # required
version = "1.2.0"              # required
api_version = "0.1"            # required; must be one this build implements
description = "GitHub tools"   # reaches the model — disclosed at install
author = ""
homepage = ""

[plugin.config]
schema = "config.schema.json"  # JSON Schema, validated against config.json before init

# ── WebAssembly payloads ──
[[wasm]]
component = "gh.wasm"                     # relative to the plugin root
tools = ["diff"]                          # install-time visibility only
events = ["PreToolUse", "PostToolUse"]    # must be in the subscribable whitelist

[wasm.capabilities]
fs_read  = ["${workspace}/src"]
fs_write = ["${plugin}/scratch"]
net      = ["api.github.com"]
env      = ["GITHUB_TOKEN"]
max_memory_mb = 128            # default 64
timeout_ms    = 5000           # default 30000

# ── MCP payloads ──
[[mcp]]
name = "github"
kind = "native"                # `config` required for native
config = "mcp/github.json"

[[mcp]]
name = "pr-helper"
kind = "dsh"                   # `entry` required for dsh
entry = "dist/index.js"
env = ["GITHUB_TOKEN"]

# ── A scene the plugin owns ──
[scene.own]
name = "GitHub workflow"
description = "PR review and issue triage"
prompt = "scene/prompt.md"     # required; markdown system prompt
reminder = "scene/reminder.md" # optional per-turn reminder
tools = ["Read", "Grep"]
disallowed_tools = ["Bash"]
deferred_tools = []

[scene.own.budget]
compact_threshold = 120000     # default 150000
compact_keep_recent = 20       # default 20
max_api_calls_per_turn = 40    # default unbounded

# ── Agent types the plugin declares ──
[[agent]]
name = "pr-reviewer"
description = "Reviews PRs"    # reaches the model — disclosed
prompt = "agents/reviewer.md"  # reaches the model — disclosed
allowed_tools = ["Read"]
disallowed_tools = []
model = "claude-opus-5"
permission_mode = "plan"
effort = "high"
max_turns = 30
scene = "plugin:github-tools"  # run this agent's sub-agents in the plugin's scene
```

Two validation rules are worth knowing before you hit them:

- **At most one `[[wasm]]` component may declare `events`.** The host resolves
  an event to a plugin *by name*, so a second subscribing component would give
  one name two possible answers and one subscription would be silently ignored.
  Several components are fine as long as only one subscribes.
- `tools = [...]` under `[[wasm]]` is documentation for the installer. At
  runtime the component's own `list-tools` is the authority — that is what gets
  registered.

### The subscribable events

```
PreToolUse   PostToolUse   PostToolUseFailure
PermissionRequested   SessionStart   SessionEnd
```

A deliberate subset of the engine's thirty hook events. Two rules put an event
on it: it has to be low-frequency (each call builds a fresh WASM store), and
its payload has to be small enough to cross the sandbox boundary by value.
Naming anything else is a manifest error listing what is allowed.

A subscribed component answers one of three things, and the host narrows it to
what a downloaded package may say:

| answer | what happens |
|---|---|
| `proceed` | nothing |
| `block(reason)` | the event is refused, with the reason shown to the user |
| `add-context(text)` | an attributable note joins the conversation |

Each subscription is registered with the component's own `timeout_ms` as its
deadline — a hook runs inside a turn that is waiting on it, so it gets no more
time than it asked for. A hook whose component has not loaded is not registered
at all, so an event it claimed does not pay a dispatch for nothing.

There is deliberately no "rewrite the input" variant. The engine's own
`HookResponse` can carry `updated_input`, and the plugin path cannot reach it:
refusing a tool call and quietly changing what it does are different powers,
and only the first is one a downloaded package gets.

### User configuration

If `[plugin.config] schema` is set, `config.json` in the plugin's installed
directory is validated against that JSON Schema before the component is loaded,
and **all** violations are reported at once. A plugin with no `config.json` gets
`{}` — absent is not the same as wrong. The validated JSON is then handed to the
component's `init`, and the plugin gets the last word: an `init` that returns
`Err` means the plugin does not load. A schema that is itself malformed is an
error, not a silent "no validation".

---

## The capability model

**Nothing is granted by omission.** A component that declares no capabilities
can compute and no more: no files, no network, no environment. Whatever a plugin
wants has to be written down, and what is written down is what the installer
shows the user.

The table and every predicate over it live in `base::interface::capabilities`,
above the carrier and shared with every other one. A carrier converts its
manifest into the kernel's `CapabilityDeclaration` and asks; it does not answer.
`daemon/tests/carrier_invariants.rs` fails if a carrier grows its own
`allows_url` / `allows_env` / `allows_read` / `allows_write`.

### `fs_read`, `fs_write`

Paths **must** be anchored to `${workspace}` (the daemon's working directory) or
`${plugin}` (the plugin's installed directory). An unanchored or absolute path
is refused at load — a bare `/` in `fs_read` reads as a grant of the machine,
which is exactly the line a reviewer skims past. `..` anywhere in the expanded
path is refused too.

Expansion happens once, at load, so no call-time decision depends on state a
plugin could change between the check and the use. What survives becomes WASI
preopens and nothing else — there is no file API in the host interface, because
re-implementing path checks as host functions is how those checks get subtly
wrong.

Inside the component, a preopened directory appears under its **last path
segment**: `/Users/me/secret/project` is `/project` to the guest. A plugin has
no business learning where on the machine its workspace lives.

A capability naming a directory that does not exist fails the load with the path
in the message, rather than failing mysteriously the first time a file is
touched.

### `net`

Exact host matches, checked in `host.http-request`:

- case-insensitive on the host, port not part of the match
- **not** a suffix rule — `api.github.com` does not admit `evil.api.github.com.attacker.test`
- userinfo cannot disguise the host: `https://allowed.example@evil.example/` points at `evil.example` and is refused
- non-`http(s)` URLs are denied rather than parsed — `file:///etc/passwd` never reaches a resolver

A refusal names the *host*, never the URL: the message is returned to the guest,
which typically hands it straight back as a tool result, into the model's
context and the transcript.

### `env`

Exact, case-sensitive names, readable only through `host.secret`. `GITHUB_TOKEN`
does not admit `github_token` or `GITHUB_TOKEN_2`. An undeclared key returns
`none` and logs that the plugin asked.

### `max_memory_mb`, `timeout_ms`

Enforced by the store's resource limiter and by the call deadline. Defaults are
64 MB and 30 s.

### What the host lends back

`log`, `progress` (streamed to the user for the in-flight call), `now-ms`,
`http-request`, `secret`, `kv-get`, `kv-set`. The key-value namespace is
per-plugin, in the host's memory, and it exists because a component gets a
**fresh store on every call** and therefore has no memory of its own. The host
can see it, clear it, and drop it with the plugin — which is the point. It does
not survive a daemon restart or a plugin reload.

---

## Install-time disclosure

`plugin.install` returns a `disclosure` object, and an installer is expected to
put it in front of the person installing:

```json
{"plugin":"github-tools","version":"1.2.0",
 "capabilities":["read files under ${workspace}/src","make network requests to api.github.com"],
 "events":["PreToolUse"],
 "scene":"plugin:github-tools",
 "mcp_servers":["github (native)"],
 "model_visible":[{"origin":"plugin description","text":"GitHub tools"},
                  {"origin":"tool `diff` description","text":"…"},
                  {"origin":"tool `diff` guide","text":"…"},
                  {"origin":"agent `pr-reviewer` system prompt","text":"…"}],
 "inert":false}
```

The reason `model_visible` exists: the sandbox handles what a plugin *executes*
and does nothing about what it *says*. A tool description, an agent description
and a scene's system prompt all reach the model verbatim, and text reaching the
model is the one attack the isolation model cannot address. The only defence is
a person reading it, so the install response carries all of it, each piece
labelled with where it came from. Capability lines are shown as the manifest
wrote them, `${workspace}` and all.

Two limits are refusals, not warnings — a limit that only warns is one every
automated installer walks straight past:

- a one-line description over **500 characters** (a label that long is either a
  mistake or an attempt to smuggle instructions into a field a reviewer skims)
- a prompt or long tool guide over **40 000 characters**

Note that the tool text can only come from a *loaded component* — the manifest
does not contain it — so a disclosure produced without the WASM host simply
lists no tools. `inert: true` means the plugin declares no capabilities, no
events, no scene, no MCP servers and no model-visible text: nothing to scrutinise
beyond its name.

Disclosure is deliberately carrier-neutral. It is about what an extension *says*,
so `crates/plugin/src/disclosure.rs` names no runtime at all, and the invariant
test fails if it starts to.

---

## What a plugin can contribute

Everything installed plugins present to the engine goes through one trait,
`runtime::plugin_host::PluginHost`, held by the host as an `Option` — which is
what lets the whole subsystem compile out with no `#[cfg]` scattered through the
engine.

| contribution | how it reaches a session |
|---|---|
| **Tools** | Adapted from a component's `list-tools` into ordinary engine tools named `plugin__<plugin>__<tool>` |
| **MCP servers** | Merged into the daemon's configured server set, keyed `<plugin>-mcp-<name>` |
| **Hook subscriptions** | Registered into the session's `HookRunner`, backed by the component's `on-event` |
| **A scene** | Registered and activated as `plugin:<name>`; enterable with `session.create {"scene": "plugin:<name>"}` |
| **Agent types** | Discoverable through `Agent`'s `subagent_type` |

Two things about that table are worth stating plainly.

**Tools reach a session through the plugin's own scene.** `PluginScene` carries
its plugin's tools as the scene's `extra_tools`, and `Builder::build` registers
those into the session's registry — which is why owning a scene is how a plugin
puts its tools in front of a model. A session in a built-in scene does not pick
them up.

**A plugin shapes behaviour only in a scene it owns.** Rewriting the prompt of a
scene the user chose for other reasons is hijacking; owning a scene the user
explicitly enters is the plugin doing its job. So a plugin gets its own scene and
never anyone else's. A plugin scene's `max_api_calls_per_turn` cannot *raise* the
real ceiling either — the turn loop resolves the budget as `min(settings, scene)`,
so a scene may only ever tighten.

Some consequences of the tool adapter that surprise people:

- Plugin tools are **deferred** by default. A third-party schema shipped in every
  request's tool array is a fixed token cost for something the model usually
  ignores; the model fetches the long `doc` with `ToolSearch` when it wants the
  tool. `description` is the one-liner that ships every turn.
- `check_permissions` always answers **Ask**. A plugin asserting its own call is
  fine is not evidence of anything, and `read_only` / `concurrency_safe` from the
  component are used for scheduling only.
- The `plugin__<name>__<tool>` shape mirrors `mcp__<server>__<tool>`, but a
  permission rule has to name the **whole tool** — `plugin__github-tools__diff`.
  The prefix form that lets one rule cover a whole MCP server is special-cased to
  `mcp__` in `permissions::ruleset::matches_tool_name`, so `plugin__github-tools`
  on its own matches nothing today.
- A component shipping an input schema that will not parse still registers; it
  advertises an open object, because refusing would punish the user for the
  author's typo.

Plugins do **not** contribute skills or slash commands, and they cannot be bound
to the interception points a script can reach (`prompt.assemble`, `model.request`
and the rest). `docs/extension_points.md`'s `Plugin` column describes what each
*contract* opens; the WebAssembly carrier's actual surface is the five rows above.

---

## Boundaries

### The sandbox

One wasmtime `Engine` per process — it owns the compiler and the code cache, so
one per plugin would pay that repeatedly for no isolation benefit. **Isolation
comes from the store, not the engine**: a component gets a fresh `Store` for
every single call, and a store owns its linear memory, its tables and its
resource limits. A trap, a runaway loop and an allocation blow-up all end the
same way — the store is dropped, the call returns an error, the engine carries
on.

The deadline is enforced with epoch interruption, not fuel: "has this outlived
its budget" and "has the user pressed Ctrl-C" are wall-clock questions. A
dedicated OS thread advances the epoch every 10 ms — a timer task would not do,
because "every async worker is busy" is precisely the situation a runaway guest
creates. The epoch makes the guest yield, which turns an uninterruptible loop
into a future the host can drop, so a timeout and a cancellation are the same
operation rather than two mechanisms with two failure modes.

What that buys, as the round-trip test asserts against a real component: a
plugin whose tool loops forever is stopped at its declared timeout and the next
call to the same plugin succeeds; a trapping tool is an error result and the
next call succeeds; an undeclared host is unreachable from inside the guest and
the error says which capability would have allowed it.

How failures surface to the caller:

| what happened | what the model sees |
|---|---|
| timeout | an error *result* — this tool failed, not the turn |
| trap | an error result naming the plugin and tool |
| cancellation | the engine's own `Cancelled` error, never a result the model would read as an answer |

### The health breaker

Per-call isolation makes it *safe* to keep calling a broken plugin, not useful.
A component that traps on every invocation turns each of the model's attempts
into an error result and the model keeps trying. So after **3 consecutive
faults** (`wasm_host::health::FAULT_LIMIT`) the plugin is set aside: every tool
it exports refuses with a message naming the plugin, not just the tool that
failed.

Three rather than one, because a fault can come from an input the plugin
mishandles, and one bad argument should not cost the user the tool. A single
success resets the streak, so a plugin failing at its edges is not penalised.

**Only faults count.** A timeout or a cancellation says something about the work
or the user, not about the plugin's soundness — a plugin doing something
genuinely slow would otherwise disable itself for being used as intended.

Fault records live in a registry keyed by plugin name, so they outlive the
component instances — which are rebuilt on every install, uninstall, enable and
disable. In practice a set-aside plugin comes back at the next reload anyway:
reloading asks every component for its tool list again, and a `list-tools` that
answers is a success that clears the streak. So `plugin.reload` is the way to
give a plugin another chance after its author has shipped a fix.

The records are reported as the `plugins.breakers` health check (`degraded`,
with the per-plugin fault counts in `details`), so a decision the carrier was
otherwise making invisibly — the tool stops being offered, the model stops
asking, and whoever installed it gets no signal — is something an operator can
act on.

### Load

Plugins load at most 4 at a time, and the whole load phase has a 30-second
budget. Loading runs a plugin's `init`, so it is not merely I/O. The daemon
awaits this before it serves; a plugin still loading when the budget expires is
dropped with a warning, because starting without it is recoverable and never
starting is not. One plugin failing any step is dropped and the others still
load.

### The trust model

- Capabilities are **declaration ∩ user rules**, never a union. A declared
  capability is what the plugin *may ask for*; the user's permission rules still
  decide each call, and plugin tools default to `Ask`.
- An agent type a plugin declares carries `AgentTypeSource::Plugin`, which is
  what clamps its `permission_mode` / `max_turns` overrides — a definition that
  arrived over the network must not hand its sub-agent more than the session
  already had.
- `settings.json`'s `plugins` section is the deployment-level switch:
  `{"plugins": {"enabled": false}}` loads none, and a non-empty
  `{"plugins": {"allow": ["a", "b"]}}` is an allow-list — a name not on it is
  refused rather than merely unmentioned. A deployment that wants no plugins at
  all should prefer the build flag, which leaves nothing to switch back on.
- Plugin names are checked against a blocklist at install, and against homograph
  confusion with official names when a marketplace index is configured.
- AOT artifacts are keyed by the component's content hash alone, so a rebuilt
  plugin never reads back the previous build's machine code. Compatibility is not
  the key's job: `Component::deserialize` validates the artifact's own header and
  rejects one from a foreign toolchain, so the wrong artifact produces wasmtime's
  own message rather than a silent miss. In a build with the compiler, a
  rejection demotes to a recompile; in a runtime-only build it is where loading
  stops, which is the whole point of removing the compiler.
