//! Does a `scripts` section actually change what the model is sent?
//!
//! Every other test of the script carrier stops one step short: the unit
//! tests call an adapter directly, and the binding tests in `script-host`
//! check that a file on disk produces an adapter. Neither runs a session, so
//! neither would notice a point that binds correctly and is then never
//! consulted, or an adapter installed on a builder nobody reads.
//!
//! So this drives a real `Agent`, from real `Settings`, with the scripts read
//! off `tests/fixtures/scripts/`, and asserts on the requests that reached
//! `ScriptedModel`. Each fixture leaves a `SCRIPT-TRACE-` mark that nothing
//! else in the engine produces — a script that logged, or that made a change
//! the engine would have made anyway, would let this pass without running.
//!
//! Each case is run twice: once with the `scripts` section and once without.
//! The second run is the load-bearing one. Without it "the mark is in the
//! request" would also be satisfied by a mark that was in the request all
//! along.
//!
//! # Adding a point
//!
//! Add a fixture beside the others, add a row to [`cases`], and give it a
//! mark of its own. A point that has no adapter yet has no row: a case
//! asserting that an unbound point changes nothing passes for the wrong
//! reason.

use base::interface::model::Model;
use base::interface::scene::AgentScene;
use base::interface::settings::{PathSettings, ScriptBinding, Settings, ThinkingMode};
use base::interface::tool::{
    InMemoryToolRegistry, PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext,
    ToolResult,
};
use base::memory::{DurableMemory, MemoryStore, MemoryType};
use runtime::agent::{Builder, InputMessage};
use std::sync::Arc;
use test_runner::scripted_model::{Reply, ScriptedModel};
use tokio_util::sync::CancellationToken;

/// What the session needs beyond a model and a tool for the point to fire.
#[derive(PartialEq, Eq)]
enum Needs {
    Nothing,
    /// A memory store with something in it, and recall switched on. Applied
    /// whether or not the script is bound, so the unbound run differs in the
    /// script and nothing else.
    Recall,
}

/// One bindable point, its fixture, and the mark that fixture leaves.
struct ScriptCase {
    /// Catalog id of the point. Also what a failure names.
    point: &'static str,
    /// File under `tests/fixtures/scripts/`.
    script: &'static str,
    entry: &'static str,
    /// Must reach the model when the script is bound, and must not when it
    /// is not.
    trace: &'static str,
    /// Must reach the model in neither run. For a script whose job is partly
    /// to take something away: the mark above proves the script ran, this
    /// proves the removing half of it did.
    absent: Option<&'static str>,
    /// Bindings beyond the one under test, bound in both runs.
    ///
    /// A variable is only visible once some block references it, so proving
    /// one expanded takes a second script to put `{{name}}` somewhere. Bound
    /// in the unbound run too, so the two runs still differ in exactly the
    /// script being tested.
    also: &'static [(&'static str, &'static str, &'static str)],
    /// One entry per user message, sent in order. More than one when the
    /// mark only reaches the model as history — a script that rewrites an
    /// assistant message changes the *next* request, not the one in flight.
    turns: &'static [&'static str],
    replies: fn() -> Vec<Reply>,
    needs: Needs,
}

fn cases() -> Vec<ScriptCase> {
    vec![
        ScriptCase {
            point: "prompt.assemble",
            script: "prompt_assemble.js",
            entry: "onAssemble",
            trace: "SCRIPT-TRACE-ASSEMBLE",
            absent: None,
            also: &[],
            turns: &["hello"],
            replies: || vec![Reply::Text("hi")],
            needs: Needs::Nothing,
        },
        ScriptCase {
            point: "tool.result",
            script: "tool_result.js",
            entry: "onResult",
            // The tool's name is in the mark, so this also pins that the
            // script was told which call it was looking at.
            trace: "SCRIPT-TRACE-RESULT(ScriptEcho)",
            absent: None,
            also: &[],
            turns: &["use the tool"],
            replies: || {
                vec![
                    Reply::Tool {
                        id: "call-1",
                        name: "ScriptEcho",
                        input: serde_json::json!({"say": "anything"}),
                    },
                    Reply::Text("done"),
                ]
            },
            needs: Needs::Nothing,
        },
        ScriptCase {
            point: "memory.retrieval_hook",
            script: "memory_retrieval.js",
            entry: "onRetrieval",
            // The rewritten query is the only thing that matches either
            // memory, so a recall reaching the model at all is proof the
            // `before` half ran.
            trace: "SCRIPT-TRACE-RECALL",
            // Which the `after` half then dropped, though the rewritten
            // query found it too.
            absent: Some("secret-key-rotation"),
            also: &[],
            turns: &["when can I ship this?"],
            replies: || vec![Reply::Text("on tuesday")],
            needs: Needs::Recall,
        },
        ScriptCase {
            point: "prompt.block",
            script: "prompt_block.js",
            entry: "onBlock",
            trace: "SCRIPT-TRACE-BLOCK",
            absent: None,
            also: &[],
            turns: &["hello"],
            replies: || vec![Reply::Text("hi")],
            needs: Needs::Nothing,
        },
        ScriptCase {
            point: "prompt.context",
            script: "prompt_context.js",
            entry: "onContext",
            // The mark carries `cwd`, which the identity call cannot see —
            // so a block that was registered with static text at bind time
            // would arrive without it and this would fail.
            trace: "SCRIPT-TRACE-CONTEXT: working in",
            absent: None,
            also: &[],
            turns: &["hello"],
            replies: || vec![Reply::Text("hi")],
            needs: Needs::Nothing,
        },
        ScriptCase {
            point: "prompt.variable",
            script: "prompt_variable.js",
            entry: "onVariable",
            trace: "SCRIPT-TRACE-VARIABLE(",
            absent: None,
            // A variable nothing mentions expands nowhere, so a block that
            // mentions it comes along.
            also: &[("prompt_block_var.js", "prompt.block", "onBlock")],
            turns: &["hello"],
            replies: || vec![Reply::Text("hi")],
            needs: Needs::Nothing,
        },
        ScriptCase {
            point: "tool.around",
            script: "tool_around.js",
            entry: "onAround",
            // The tool's own answer never appears, because the ring answered
            // instead of dispatching — but `absent` means "in neither run",
            // and the unbound run is exactly where the tool does answer. So
            // the mark carries the proof on its own: nothing but this script
            // produces it, and the model still got a result.
            trace: "SCRIPT-TRACE-AROUND",
            absent: None,
            also: &[],
            turns: &["use the tool"],
            replies: || {
                vec![
                    Reply::Tool {
                        id: "call-1",
                        name: "ScriptEcho",
                        input: serde_json::json!({"say": "anything"}),
                    },
                    Reply::Text("done"),
                ]
            },
            needs: Needs::Nothing,
        },
        ScriptCase {
            point: "model.message",
            script: "model_message.js",
            entry: "onMessage",
            trace: "SCRIPT-TRACE-MESSAGE",
            absent: None,
            also: &[],
            // Two turns: the mark is put on the *first* turn's reply, and
            // only reaches the model as history on the second.
            turns: &["hello", "and again"],
            replies: || vec![Reply::Text("first answer"), Reply::Text("second answer")],
            needs: Needs::Nothing,
        },
    ]
}

/// Two memories the fixture's rewritten query finds and the user's own words
/// do not. One of them is the one the fixture then filters out.
fn seed_memories(store: &MemoryStore) {
    let memory = |name: &str, content: &str| DurableMemory {
        name: name.to_string(),
        description: "seeded by the script fixture test".into(),
        memory_type: MemoryType::Project,
        content: content.to_string(),
        source_session_id: String::new(),
        confidence: 0.9,
        last_seen: "2026-08-01T00:00:00Z".into(),
        recall_count: 0,
    };
    store
        .persist_batch(vec![
            memory(
                "deploy-window",
                "SCRIPT-TRACE-RECALL: deploys land on Tuesdays.",
            ),
            memory(
                "secret-key-rotation",
                "SCRIPT-TRACE-RECALL: rotate the signing key quarterly.",
            ),
        ])
        .expect("seed memories");
}

fn fixtures() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests/")
        .join("fixtures")
        .join("scripts")
}

/// Run one case and return every request that reached the model, as text.
async fn requests(case: &ScriptCase, bind_scripts: bool) -> Vec<String> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    let model_arc = ScriptedModel::new((case.replies)());
    let model: Arc<dyn Model> = model_arc.clone();

    let mut settings = Settings::defaults_for("script-model");
    settings.model.model_name = "script-model".into();
    settings.model.max_tokens = 1024;
    settings.model.thinking_mode = ThinkingMode::Off;
    settings.paths = PathSettings {
        user_data_dir: root.join("user"),
        global_data_dir: root.join("global"),
        local_data_dir: root.join("local"),
        scope: "code".into(),
    };
    settings.memory_enabled = case.needs == Needs::Recall;
    let binding = |path: &str, point: &str, entry: &str| ScriptBinding {
        path: path.into(),
        point: point.into(),
        entry: entry.into(),
        timeout_ms: None,
        calls_per_turn: None,
    };
    settings.scripts = case
        .also
        .iter()
        .map(|(p, pt, e)| binding(p, pt, e))
        .collect();
    if bind_scripts {
        settings.scripts.push(binding(case.script, case.point, case.entry));
    }
    let settings = Arc::new(settings);

    let registry = Arc::new(InMemoryToolRegistry::new());
    registry.register(Arc::new(ScriptEcho) as Arc<dyn Tool>);

    let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::chat::ChatScene);
    let mut builder = Builder::new()
        .scene(scene)
        .model(model)
        .tools(registry)
        .settings(settings.clone())
        .skip_warmup(true);

    if case.needs == Needs::Recall {
        let store = Arc::new(MemoryStore::new(
            root.join("mem-user"),
            root.join("mem-local"),
        ));
        seed_memories(&store);
        builder = builder.memory_store(store);
        // The shipped retriever asks the model which memories are relevant,
        // and a scripted model has no answer to give. The substring one is
        // the other shipped implementation and exists for exactly this: it
        // makes recall a pure function of the query, which is what lets a
        // rewritten query be the thing under test.
        builder = builder.memory_retriever(Arc::new(
            base::interface::memory_contracts::SubstringRetriever,
        ));
    }

    // The same call the daemon makes. Installing adapters is `Builder`'s job
    // precisely so this test cannot bind a point the session would not.
    if !settings.scripts.is_empty() {
        let engine: Arc<dyn base::interface::script::ScriptEngine> =
            Arc::new(script_host::QuickJsEngine::new());
        let bound = script_host::bindings::bind(engine, &settings.scripts, &fixtures())
            .unwrap_or_else(|e| panic!("case `{}`: {e}", case.point));
        builder = builder.bound_scripts(bound);
    }

    let (mut agent, mut event_rx, input_tx) = builder.build().expect("agent builds");

    let cancel = CancellationToken::new();
    let run_cancel = cancel.clone();
    let join = tokio::spawn(async move { agent.run(run_cancel).await });

    for (i, turn) in case.turns.iter().enumerate() {
        input_tx
            .send(InputMessage::User {
                content: turn.to_string(),
                attachments: vec![],
                turn_id: format!("t{i}"),
            })
            .expect("send turn");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let ev = tokio::time::timeout_at(deadline, event_rx.recv())
                .await
                .unwrap_or_else(|_| panic!("case `{}`: the turn never finished", case.point))
                .unwrap_or_else(|| panic!("case `{}`: event channel closed early", case.point));
            if matches!(
                ev,
                base::event::AgentEvent::TurnComplete { .. }
                    | base::event::AgentEvent::Error { .. }
            ) {
                break;
            }
        }
    }

    cancel.cancel();
    drop(input_tx);
    let _ = join.await;

    model_arc.request_texts()
}

fn holds(requests: &[String], needle: &str) -> bool {
    requests.iter().any(|r| r.contains(needle))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bound_script_leaves_its_mark_on_what_the_model_receives() {
    for case in cases() {
        let sent = requests(&case, true).await;
        assert!(
            !sent.is_empty(),
            "case `{}`: the model was never called",
            case.point
        );
        assert!(
            holds(&sent, case.trace),
            "case `{}`: `{}` never reached the model. Requests:\n{}",
            case.point,
            case.trace,
            sent.join("\n---\n")
        );
        if let Some(absent) = case.absent {
            assert!(
                !holds(&sent, absent),
                "case `{}`: `{absent}` reached the model, so the script's \
                 removing half did not run. Requests:\n{}",
                case.point,
                sent.join("\n---\n")
            );
        }
    }
}

/// The same sessions with the `scripts` section taken out. Everything else —
/// the tools, the memories, the model's answers — is identical, so a mark
/// that survives came from somewhere other than the script.
#[tokio::test(flavor = "multi_thread")]
async fn with_no_scripts_section_no_mark_reaches_the_model() {
    for case in cases() {
        let sent = requests(&case, false).await;
        assert!(
            !sent.is_empty(),
            "case `{}`: the model was never called",
            case.point
        );
        assert!(
            !holds(&sent, case.trace),
            "case `{}`: `{}` reached the model with no script bound, so the \
             case proves nothing. Requests:\n{}",
            case.point,
            case.trace,
            sent.join("\n---\n")
        );
        if let Some(absent) = case.absent {
            assert!(!holds(&sent, absent), "case `{}`: `{absent}`", case.point);
        }
    }
}

/// Echoes its argument, so a `tool.result` script has a result to rewrite.
#[derive(Debug)]
struct ScriptEcho;

#[async_trait::async_trait]
impl Tool for ScriptEcho {
    fn name(&self) -> &str {
        "ScriptEcho"
    }
    fn description(&self) -> &str {
        "Echo the `say` argument back."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"say": {"type": "string"}},
            "required": ["say"]
        })
    }
    fn is_read_only(&self, _: &serde_json::Value) -> bool {
        true
    }
    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &ToolContext,
    ) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        self.description().to_string()
    }
    async fn call(
        &self,
        input: serde_json::Value,
        _ctx: ToolContext,
        _p: ProgressSender,
    ) -> Result<ToolResult, base::error::ToolError> {
        let say = input.get("say").and_then(|v| v.as_str()).unwrap_or("");
        Ok(ToolResult::text(format!("echo: {say}")))
    }
}
