# Extending AttaCore with QuickJS scripts

A script is a `.js` file plus a line of configuration. The engine reads the
file when a session is built, runs the function you name at the extension
point you name, and hands the result back to whatever part of the engine was
asking. No build step, no subprocess, no network.

That is the whole tier: something between "recompile the engine" and "spawn a
process" for the small jobs — rewrite one line of a prompt, tag a tool result,
answer a tool call from a cache, narrow a recall query. The interpreter is
QuickJS, embedded in the daemon, and a call costs microseconds.

`docs/extension_points.md` is the catalog of everything in the engine that can
be extended, by anyone, in any way. This document covers the nine points a
script can be bound to today, and only those.

## Is the carrier in this build?

The script carrier is the `scripts` feature of the `daemon` crate, and it is
**on by default**:

```sh
cargo build -p daemon                                             # QuickJS
cargo build -p daemon --no-default-features --features plugin-compile
cargo build -p daemon --no-default-features                       # neither
```

A build carries **one** extension carrier or none. `scripts` and `plugins` are
mutually exclusive and the combination fails to compile — a plugin build needs
`--no-default-features`, because without it cargo unions the default `scripts`
back in and the guard in `daemon/src/lib.rs` refuses.

Without the feature, no JavaScript engine is linked in at all, and a `scripts`
section in `settings.json` is refused loudly:

```
this build carries no script engine, so a `scripts` section cannot be honored;
rebuild with the `scripts` feature or remove the section
```

## From zero

**1. Write the script.** Anywhere inside the project; `.atta/scripts/` is the
convention. This one is `tests/fixtures/script_project/.atta/scripts/house_style.js`:

```js
function onAssemble(blocks) {
  return blocks.concat([
    {
      name: "team.house_style",
      content:
        "House rule, absolute: end every reply with the line " +
        "'-- checked by the house style script'. No exceptions.",
    },
  ]);
}
```

**2. Bind it** in the project's `.atta/settings.json`:

```json
{
  "scripts": [
    {
      "path": ".atta/scripts/house_style.js",
      "point": "prompt.assemble",
      "entry": "onAssemble"
    }
  ]
}
```

**3. Start the daemon** with that project as its working directory — relative
script paths resolve against it. When a session is created it logs

```
bound scripts to extension points  scripts=1
```

and from then on every reply in that session ends with the house-style line.
`tests/cases/010.script_carrier.test` is that exact setup, asked of a real
model.

Two things to know about step 3. The file is read **when a session is built**,
so editing the script changes nothing until a new session starts. And a
binding that cannot be honored — an unreadable file, a point that does not
exist, a point scripts cannot use — drops the **whole** set: the daemon logs

```
a script binding is invalid; no scripts were bound
```

and the session runs with no scripts at all rather than with half of them.

## The binding

| Field | Meaning |
|---|---|
| `path` | The script file. Absolute, or relative to the daemon's working directory — the same root that decides the script's authority. |
| `point` | Which extension point, by its catalog id. One of the nine below. |
| `entry` | The function in the file to call. |
| `timeout_ms` | Wall clock for one call. Default 100. |
| `calls_per_turn` | How many calls this binding may make in one turn. Default 1000. |

One file can be bound more than once — several points, or the same point with
different entry points. Each line is its own binding with its own budget.
Where two scripts share a point they are installed in the order the bindings
appear, which at `tool.around` means the first one is the outermost ring.

The `Script` column of the catalog in `docs/extension_points.md` says what
each point's *contract* allows a script to do. The nine below are what the
carrier has an adapter for. They are not the same list, and a binding to a
point that is open in principle but has no adapter is refused by name:

```
`history.append_observer` exists but scripts cannot be bound to it; today that
is prompt.assemble, prompt.block, prompt.context, prompt.variable, tool.around,
tool.result, memory.retrieval_hook, model.request, model.message
```

## How a script is called

Your file is evaluated, then the `entry` function is called with one argument:
the point's input, already parsed from JSON. What you return is serialized
back to JSON. Returning nothing is the same as returning `null`.

Declare the entry as a plain top-level function — `function onAssemble(input)`.
There are no modules and no `export`.

Every call gets a **fresh runtime**. Nothing survives between calls: no
globals you set, no cache you built, nothing another session could see. If you
need state, it has to be in the input.

A script can reach **nothing** outside itself. There is no `require`, no
`process`, no `fetch`, no `XMLHttpRequest`, no filesystem and no network.
`Date` works. Input in, output out.

## The nine points

Each section says when the point fires, what your function receives, and what
it may return. Every fixture quoted here lives in `tests/fixtures/scripts/`
and is exercised end to end by `tests/runner/tests/script_carrier.rs`.

### `prompt.assemble` — the whole system prompt, last

Once per turn, after every block has been assembled. Receives the blocks in
order, returns the list it wants:

```json
[{ "name": "scene.skeleton", "content": "…" }, { "name": "rules", "content": "…" }]
```

The returned list is **diffed** against what you were given, name by name:

- a name that was not there is **added**, as a new block at the end;
- a name whose content changed is a **modification**;
- a name you were given and did not return is a **removal**;
- a block handed back unchanged is not an edit at all, so returning the whole
  list to change one line costs you nothing.

Blocks without a name are ignored in both directions. Reordering is not
expressible here — position comes from the block's registered order, not from
where it sits in your array.

Which of those you may do depends on where your file is; see
[Authority](#authority) below.

```js
// tests/fixtures/scripts/prompt_assemble.js
function onAssemble(blocks) {
  blocks.push({ name: "script.fixture.assemble", content: "SCRIPT-TRACE-ASSEMBLE" });
  return blocks;
}
```

A throw, a timeout, an exhausted quota or a return value that is not a list of
blocks leaves the prompt exactly as it was — a half-edited prompt is worse
than an unedited one, because nothing downstream can tell which it is looking
at.

### `prompt.block` — one fixed block

Called **once**, when the binding is bound, with `null`. It answers with the
block, text included:

```js
// tests/fixtures/scripts/prompt_block.js
function onBlock() {
  return {
    name: "team.conventions",
    order: 250,
    content: "SCRIPT-TRACE-BLOCK: keep answers short.",
  };
}
```

`name` is required. `order` is optional and defaults to 500, which puts the
block after everything the engine contributes; the kernel's own stages sit at
0 (`scene.*`), 100 (`skills.catalog`), 200 (`memory.session`), 300 (`rules`),
400 (`mcp.instructions`) and 500 (`config.prompt_append`). `content` must be a
string — text that depends on the session belongs at `prompt.context`.

A name that already belongs to a kernel block — anything starting `scene.`, or
`skills.catalog`, `memory.session`, `rules`, `mcp.instructions`,
`config.prompt_append`, `config.prompt_override` — is refused. Blocks are
addressed by name and the first match wins, so a contribution squatting on a
kernel name would quietly absorb edits meant for the real block.

If this call fails, or answers without a usable `name`, or gives an `order`
that is not a number, nothing is registered and a warning says so.

### `prompt.context` — a block computed every assembly

Two kinds of call. The first, at bind time, passes `null` and asks for the
block's identity — the same `{ name, order }` as above. Every call after that
passes the session context and expects the block's text:

```js
// tests/fixtures/scripts/prompt_context.js
function onContext(ctx) {
  if (ctx === null) {
    return { name: "project.status", order: 260 };
  }
  return "SCRIPT-TRACE-CONTEXT: working in " + ctx.cwd;
}
```

The context is where the session is running, and nothing that is already a
prompt block:

```json
{
  "cwd": "/home/u/proj", "os": "linux", "shell": "bash",
  "homeDir": "/home/u", "date": "2026-06-10", "modelName": "claude-opus-5",
  "isGit": true, "gitBranch": "main", "isWorktree": false,
  "language": null, "scratchpadDir": null, "availableTools": "Read,Bash"
}
```

The skills inventory, the MCP instructions, the session memory and the output
style are deliberately absent: each is already a block, so a script that wants
to read or rewrite one binds to `prompt.assemble`. They are also the four
largest strings around, and they would be serialized into a fresh interpreter
once per turn for a script that wanted `cwd`. `gitStatus` is absent for its
size, and because an uncommitted diff is the one thing here a script that
arrived from outside the project has no business reading.

Anything but a string — including `null` — contributes no block that turn,
which is also what a failed or timed-out call contributes.

### `prompt.variable` — what `{{name}}` expands to

Same two-call shape. The identity call names the variable; `order` and
`content` mean nothing here. Later calls get the same context object as
`prompt.context` and answer with the value:

```js
// tests/fixtures/scripts/prompt_variable.js
function onVariable(ctx) {
  if (ctx === null) {
    return { name: "script_trace_var" };
  }
  return "SCRIPT-TRACE-VARIABLE(" + ctx.os + ")";
}
```

**Only a string is a value.** `null`, a number, an object, a throw, a timeout
and an exhausted quota all leave `{{script_trace_var}}` in the prompt exactly
as written: an unresolved variable is a bug to see, not a hole to hide. A
script that does want the placeholder to disappear returns `""`.

A variable nobody mentions expands nowhere — something has to put
`{{script_trace_var}}` in a block first.

### `tool.around` — one decision before a tool call

Once per tool call, before dispatch and outside the permission gate.

```json
{ "tool": "Read", "input": { "file_path": "/etc/hosts" } }
```

Return one of these, or anything else to mean "carry on":

```json
{ "action": "deny", "reason": "reads outside the project are not allowed" }
{ "action": "respond", "text": "(cached)" }
{ "action": "proceed", "timeoutMs": 2000 }
```

- **deny** — the call is not dispatched and the tool reports `reason` as its
  error. A `deny` with no `reason` has nothing to tell the model, so the call
  proceeds instead.
- **respond** — the call is not dispatched and `text` is the result the model
  sees. Without a `text` string it proceeds, because answering with an empty
  result would hand the model a tool that silently produced nothing.
- **proceed** — dispatch normally. `timeoutMs`, on this or on any decision
  that dispatches, stops the call after that long.

```js
// tests/fixtures/scripts/tool_around.js
function onAround(call) {
  if (call.tool === "ScriptEcho") {
    return { action: "respond", text: "SCRIPT-TRACE-AROUND: answered without dispatch" };
  }
  return null;
}
```

You **cannot** rewrite the arguments: `Read(a.txt)` cannot become
`Read(~/.ssh/id_rsa)` here. You cannot lengthen a deadline either — `timeoutMs`
installs a child of the turn's signal, so it can fire earlier than the session
would have given up but never later. And you do not see the outcome; the
script runs once, before dispatch. What a result looks like is `tool.result`'s
job.

Because this ring sits outside the permission gate, a `deny` refuses a call
before the user is asked anything, and a `respond` answers without the gate
being consulted. Neither executes anything, but both decide what the model is
told a tool did.

### `tool.result` — what one result looks like

Once per tool result, after every hook, immediately before the model sees it.

```json
{ "tool": "ScriptEcho", "input": { "say": "anything" }, "text": "…", "isError": false }
```

Return a string and it becomes the result text. Return anything else — a
number, an object, nothing — and the result is untouched, which is also what a
script with a bug does.

```js
// tests/fixtures/scripts/tool_result.js
function onResult(result) {
  return "SCRIPT-TRACE-RESULT(" + result.tool + ") " + result.text;
}
```

Text is the whole vocabulary here. A script cannot drop or replace the images
riding along with a result, and cannot change whether the result is an error.

### `memory.retrieval_hook` — both ends of recall

The point has two halves and a binding names one function, so the phase
travels in the input. Called once with `"phase": "before"`, once with
`"phase": "after"`:

```json
{
  "phase": "before",
  "query": "what did we decide about deploys",
  "limit": 5,
  "alreadySurfaced": ["deploy-window"],
  "recentTools": ["Bash", "Read"],
  "modelName": "claude-opus-5",
  "sessionId": "01J…"
}
```

The `"after"` call carries the same fields plus `names`, the list the
retriever produced, best first.

For `"before"`, return an object: `query` (a string) becomes the question,
`limit` (a positive integer) becomes the ceiling, both optional. For
`"after"`, return an array of strings — the names to keep, in the order the
model should see them.

```js
// tests/fixtures/scripts/memory_retrieval.js
function onRetrieval(recall) {
  if (recall.phase === "before") {
    return { query: "SCRIPT-TRACE-RECALL" };
  }
  return recall.names.filter(function (name) {
    return name.indexOf("secret-") !== 0;
  });
}
```

Anything else in either phase leaves recall as it was, and a request is never
half-moved: an answer with a good `query` and a nonsensical `limit` changes
neither. A name you invent is dropped downstream rather than being an error.

### `model.request` — the knobs, just before sending

Once per model call. What you get is the small part of the request:

```json
{
  "model": "claude-opus-5",
  "maxTokens": 8192,
  "thinkingMode": "auto",
  "fallbackModel": null,
  "tools": ["Read", "Write", "Bash"],
  "promptBlocks": ["scene.skeleton", "rules"],
  "messageCount": 24
}
```

**Not the conversation, and not the tool schemas.** Those are tens of
kilobytes on an ordinary turn, serialized and parsed and thrown away on every
model call, and message content has two cheaper points of its own —
`prompt.assemble` on the way out, `model.message` on the way back.

Return an object with any subset of the keys; anything absent is left alone.

```json
{ "maxTokens": 2048, "model": "claude-haiku-5", "thinkingMode": "off", "tools": ["Read"] }
```

- `model`, `maxTokens`, `thinkingMode`, `fallbackModel` replace the request's.
  `fallbackModel: null` clears it; leaving the key out keeps it.
  `thinkingMode` is `"auto"`, `"off"`, `"on"` or `{ "on_budget": 4096 }`.
  `maxTokens: 0` and an empty `model` are refused, because a request built
  from them cannot be sent and the script's bug would surface as a provider
  error.
- `tools` keeps the tools it names and drops the rest, in the order they
  already had. A name the request does not offer is ignored — a script cannot
  conjure a tool definition, so this narrows and never widens. `[]` is a
  legitimate answer.

```js
// tests/fixtures/scripts/model_request.js
function onRequest(req) {
  return { tools: req.tools.filter(function (t) { return t !== "ScriptEcho"; }) };
}
```

The whole object is read before any of it is applied, so a field of the wrong
type leaves the request entirely unchanged rather than partly changed.

### `model.message` — a finished message, before it is recorded

On whole messages, never on stream deltas: once for an assistant turn's text,
once for its batch of tool calls. A few times per model call, not thousands.

```json
{ "role": "assistant", "text": ["Here is what I found."], "toolUses": ["Read"] }
```

`text` holds the message's text blocks in order. `toolUses` names the tools
the message asked for, without their arguments — a single `Write` call carries
a whole file. A message with no text does not call the script at all, since
text is the only thing it could change.

Return an array of strings **exactly as long** as the `text` you were given;
each entry replaces the block at the same position, and `""` empties one.

```js
// tests/fixtures/scripts/model_message.js
function onMessage(msg) {
  return msg.text.map(function (t) {
    return "SCRIPT-TRACE-MESSAGE " + t;
  });
}
```

A different length says nothing about which block the extra or missing entry
belongs to, so it is discarded like every other shape. The message is then
recorded exactly as the model produced it.

Out of reach here: **thinking blocks and their signatures**, which must be
echoed back to the provider verbatim or the next call is rejected; **images**;
and **tool-use blocks**, whose arguments are what the engine is about to
dispatch.

Note that rewriting a message changes what the model sees on the *next*
request, not the one in flight.

## Budgets

**Time.** One call gets `timeout_ms`, 100 by default. That is enforced twice:
QuickJS carries the deadline in an interrupt handler that fires between
bytecode instructions, so even `while (true) {}` stops, and the carrier puts
its own timeout around the asynchronous path as a second line.

**Calls.** `calls_per_turn`, 1000 by default, counted per binding and reset at
the start of every user turn. A point that fires per tool call cannot turn a
pathological turn into a pathological bill.

**Memory.** Each runtime is capped at 16 MB.

Exceeding any of them fails *that call* and nothing else. The turn continues
with whatever the script would have contributed simply missing.

## Failure

The rule everywhere is that **a script that fails changes nothing.** A throw,
a timeout, an exhausted quota, a return value the point cannot act on — all of
them leave the point exactly as the adapter found it, and log a warning naming
the script. Each adapter computes its change fully before applying any of it,
so there is no half-applied state to reason about.

Two failures happen earlier than that:

- At **bind** time, if the file cannot be read or the point is wrong, no
  scripts are bound at all and the daemon logs an error.
- At **registration** time, `prompt.block`, `prompt.context` and
  `prompt.variable` make their identity call while the binding is being bound.
  A script that throws there, or that has no function by that name, registers
  nothing — a block with no name is not addressable and a block at no
  particular order is not placeable.

## Authority

What a script may do is decided by **where its file is**, not by anything it
or its binding declares — a declaration is exactly what an outside script
would lie in. The check is on the resolved, canonicalized path:

- **Inside the project root** — the operator wrote it, so it may do anything
  they could have done by editing the prompt by hand.
- **Anywhere else**, including a script that arrived with a downloaded
  plugin — it may **add**, and nothing more. Modifying, removing and
  reordering all require a capability it has not declared.

This only bites at `prompt.assemble`, the one point with something to
restrict. A denied edit does not cancel the permitted ones: the rest of the
pass still applies, and the refusal is reported and counted rather than
silently dropped, so "this script does nothing" can be told apart from "this
script is being stopped from doing something".

```
plugin '/opt/pkg/x.js' may not modify the prompt block 'rules': it did not
declare that capability at install
```

A script from outside the project is recorded as a plugin origin, which is
why it is named that way in the message: the axis is provenance, not
packaging.

The other eight points do not branch on provenance, because none of them has a
reduced mode to branch into — there is no "may narrow but not widen" version
of a recall filter the way prompt assembly has an add-only version of an edit.

## What scripts are not allowed to bind to

Four points in the catalog are open to scripts by contract and have no adapter
on purpose:

- **`history.append_observer`** fires once per log entry. That is the
  frequency band closed to scripts deliberately: the cost of a callback there
  is invisible to whoever writes one.
- **`history.extension_entry`** is a write capability, not a hook. A script
  would need an API to emit an entry, not a callback to receive one.
- **`hooks`** is its own subsystem with its own process model.
- **`script.carrier`** is the carrier itself.

Everything else in the catalog is marked `closed` for scripts — those are Rust
traits wired at build time, with nothing for a script to register through.

## Where to look next

- `tests/fixtures/scripts/` — one runnable fixture per point, each leaving a
  mark nothing else in the engine produces.
- `tests/runner/tests/script_carrier.rs` — drives a real session for every
  point, twice: once with the script bound and once without.
- `tests/fixtures/script_project/` — a complete project whose settings bind a
  script, used by `tests/cases/010.script_carrier.test`.
- `crates/core/src/interface/script_adapters/` — one adapter per point. The
  JSON contract each one implements is documented on the adapter itself, which
  is the version that cannot drift.
- `docs/extension_points.md` — every point in the engine, and who may use it.
