//! Behavior net for the turn loop.
//!
//! Each case drives a real `Agent` with a scripted model
//! (`test_runner::scripted_model`) and snapshots two things: the sequence of
//! `AgentEvent`s the host observes, and the session log the run leaves behind.
//! Both are normalized — ids, timestamps and durations are replaced with
//! placeholders — so the only thing a diff can be about is what the engine
//! *decided*.
//!
//! # Reading a failure
//!
//! A failing case prints the expected and actual traces side by side. If the
//! change was intended, re-run with `ATTA_UPDATE_GOLDEN=1` to rewrite the
//! golden file, **and read the diff before committing it** — a regression net
//! whose goldens get regenerated reflexively is a slower way of having none.
//!
//! # Why scripted rather than replayed from a cassette
//!
//! See `test_runner::scripted_model`'s module doc: cassettes are gitignored by
//! design (they bake in the exact prompt, so they go stale on every prompt
//! change), and they contain no error outcomes at all, so the recovery paths
//! this net most needs to cover are not reachable that way.

use base::event::AgentEvent;
use base::id::Id;
use base::interface::model::Model;
use base::interface::scene::AgentScene;
use base::interface::settings::{PathSettings, PermissionMode, Settings, ThinkingMode};
use base::interface::tool::{InMemoryToolRegistry, Tool};
use history::store::HistoryStore as _;
use runtime::agent::{Builder, InputMessage};
use std::sync::Arc;
use test_runner::scripted_model::{Reply, ScriptedModel};
use tokio_util::sync::CancellationToken;

mod support;
use support::{fake_tools, normalize_events, normalize_log, BudgetScene, GoldenFile};

/// How a case is configured. Defaults are the boring ones; a case overrides
/// only what it is about.
struct Case {
    name: &'static str,
    turns: Vec<&'static str>,
    replies: Vec<Reply>,
    /// Compaction threshold in tokens. `0` disables it, which is the default
    /// everywhere else in this file.
    compact_threshold: usize,
    permission_mode: PermissionMode,
    fallback_model: Option<String>,
    /// How the harness answers a `PermissionPrompt`. `None` means the case
    /// does not expect one, and getting one is a failure rather than a hang.
    approve_prompts: Option<bool>,
    /// Raw `settings.hooks_config`, for cases about hooks.
    hooks_config: Option<serde_json::Value>,
}

impl Case {
    fn new(name: &'static str, turns: Vec<&'static str>, replies: Vec<Reply>) -> Self {
        Self {
            name,
            turns,
            replies,
            compact_threshold: 0,
            permission_mode: PermissionMode::BypassPermissions,
            fallback_model: None,
            approve_prompts: None,
            hooks_config: None,
        }
    }
}

/// Run one case and return its normalized trace.
async fn trace(case: Case) -> String {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::create_dir_all(root.join("workdir")).expect("workdir");

    let model_arc = ScriptedModel::new(case.replies);
    let model: Arc<dyn Model> = model_arc.clone();

    let mut settings = Settings::defaults_for("golden-model");
    settings.model.model_name = "golden-model".into();
    settings.model.max_tokens = 1024;
    settings.model.thinking_mode = ThinkingMode::Off;
    settings.model.fallback_model = case.fallback_model.clone();
    settings.paths = PathSettings {
        user_data_dir: root.join("user"),
        global_data_dir: root.join("global"),
        local_data_dir: root.join("local"),
        scope: "code".into(),
    };
    settings.memory_enabled = false;
    settings.permission_mode = case.permission_mode;
    settings.hooks_config = case.hooks_config.clone();
    let settings = Arc::new(settings);

    let registry = Arc::new(InMemoryToolRegistry::new());
    for t in fake_tools() {
        registry.register(t as Arc<dyn Tool>);
    }

    // A history store is what makes the session log observable at all — the
    // default `Builder` leaves it `None` and writes nothing. It also forces a
    // parseable session id, hence the fixed `Id` rather than a readable label.
    let store = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            &root.join("workdir"),
            history::path::HistoryRoots::under(&root.join("global")),
        )
        .await
        .expect("history store"),
    );
    let session_id = Id::new();

    // Without this the Builder falls back to its `AllowAll` default, which
    // permits everything regardless of the tool's own `check_permissions` and
    // regardless of `settings.permission_mode` — so a case about approval
    // would pass while proving nothing. The first draft of this file did
    // exactly that: the approve and deny goldens came out byte-identical.
    let permission: Arc<dyn base::interface::permission::Permission> =
        Arc::new(permissions::rule_set_permission::RuleSetPermission::from_settings(
            &settings,
            case.permission_mode.into(),
            registry.clone(),
            [],
        ));

    let scene: Arc<dyn AgentScene> = Arc::new(BudgetScene::new(case.compact_threshold));
    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .tools(registry)
        .permission(permission)
        .settings(settings)
        .session_id(session_id.to_string())
        .history_store(store.clone())
        .skip_warmup(true)
        .build()
        .expect("agent builds");

    let cancel = CancellationToken::new();
    let run_cancel = cancel.clone();
    let join = tokio::spawn(async move { agent.run(run_cancel).await });

    let mut events: Vec<AgentEvent> = Vec::new();
    for (i, input) in case.turns.iter().enumerate() {
        input_tx
            .send(InputMessage::User {
                content: (*input).to_string(),
                attachments: vec![],
                turn_id: format!("t{i}"),
            })
            .expect("send turn");

        // Bounded so a loop that never terminates fails as a timeout with the
        // trace so far, instead of hanging the suite.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let ev = tokio::time::timeout_at(deadline, event_rx.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "case `{}` turn {i}: no TurnComplete within 30s. Events so far:\n{}",
                        case.name,
                        normalize_events(&events)
                    )
                });
            let Some(ev) = ev else {
                panic!("case `{}` turn {i}: event channel closed early", case.name)
            };
            if let AgentEvent::PermissionPrompt { prompt_id, .. } = &ev {
                let Some(approve) = case.approve_prompts else {
                    panic!(
                        "case `{}` got an unexpected PermissionPrompt. Set \
                         `approve_prompts` if that is the point of the case.",
                        case.name
                    )
                };
                let decision = if approve {
                    runtime::agent::PermissionDecision::Permit
                } else {
                    runtime::agent::PermissionDecision::Deny {
                        reason: "denied by the behavior net".into(),
                    }
                };
                input_tx
                    .send(InputMessage::PermissionResponse {
                        prompt_id: prompt_id.clone(),
                        decision,
                    })
                    .expect("send permission response");
            }
            // `Error` ends a turn as surely as `TurnComplete` does — the
            // engine emits it and goes back to waiting for input. Treating
            // only `TurnComplete` as terminal makes any case that ends in an
            // error hang for the full timeout instead of recording what
            // happened.
            let done = matches!(
                ev,
                AgentEvent::TurnComplete { .. } | AgentEvent::Error { .. }
            );
            events.push(ev);
            if done {
                break;
            }
        }
    }

    cancel.cancel();
    drop(input_tx);
    let _ = join.await;

    let leftover = model_arc.unconsumed();
    let entries = store
        .load(base::session::SessionId(session_id))
        .await
        .unwrap_or_default();

    let mut out = String::new();
    out.push_str("== events ==\n");
    out.push_str(&normalize_events(&events));
    out.push_str("\n== session log ==\n");
    out.push_str(&normalize_log(&entries));
    out.push_str("\n== model calls ==\n");
    for (m, max) in model_arc.calls() {
        out.push_str(&format!("{m} max_tokens={max}\n"));
    }
    out.push_str(&format!("unconsumed_replies={leftover}\n"));
    out
}

async fn check(case: Case) {
    let name = case.name;
    let actual = trace(case).await;
    GoldenFile::new(name).assert_eq(&actual);
}

// ── the cases ───────────────────────────────────────────────────────────

/// Plain conversation: one call, text out, done. The floor of the net — if
/// this moves, everything moved.
#[tokio::test]
async fn plain_conversation() {
    check(Case::new(
        "plain_conversation",
        vec!["hello"],
        vec![Reply::TextChunks(&["Hel", "lo", " there"])],
    ))
    .await;
}

/// One tool call, its result, then a closing message.
#[tokio::test]
async fn single_tool_call() {
    check(Case::new(
        "single_tool_call",
        vec!["read the note"],
        vec![
            Reply::Tool {
                id: "c1",
                name: "GoldenEcho",
                input: serde_json::json!({"say": "note contents"}),
            },
            Reply::Text("The note says: note contents"),
        ],
    ))
    .await;
}

/// Two concurrency-safe tools in one assistant message. What the trace pins is
/// that both run and both results come back before the next model call — the
/// dispatch batching decision.
#[tokio::test]
async fn parallel_tool_calls() {
    check(Case::new(
        "parallel_tool_calls",
        vec!["do both"],
        vec![
            Reply::Tools(vec![
                ("c1", "GoldenEcho", serde_json::json!({"say": "first"})),
                ("c2", "GoldenEcho", serde_json::json!({"say": "second"})),
            ]),
            Reply::Text("both done"),
        ],
    ))
    .await;
}

/// A tool that fails. The loop must carry the error to the model as a result
/// rather than aborting the turn.
#[tokio::test]
async fn tool_error_is_reported_to_the_model() {
    check(Case::new(
        "tool_error",
        vec!["break it"],
        vec![
            Reply::Tool {
                id: "c1",
                name: "GoldenBoom",
                input: serde_json::json!({}),
            },
            Reply::Text("that failed"),
        ],
    ))
    .await;
}

/// Overload on the first call, success on the retry. With a fallback model
/// configured, the retry must go to it — visible only in the recorded call
/// list, since the response is the script's either way.
#[tokio::test]
async fn overload_falls_back_to_the_second_model() {
    let mut case = Case::new(
        "overload_fallback",
        vec!["hi"],
        vec![
            Reply::Fail(base::interface::model::ModelError::Overloaded),
            Reply::Text("recovered"),
        ],
    );
    case.fallback_model = Some("golden-fallback".into());
    check(case).await;
}

/// `max_tokens` is a stop reason the loop treats as "not finished" — it
/// continues rather than ending the turn.
#[tokio::test]
async fn max_tokens_stop_reason() {
    check(Case::new(
        "max_tokens",
        vec!["write a lot"],
        vec![
            Reply::TextWithStop {
                text: "part one",
                stop_reason: "max_tokens",
            },
            Reply::Text(" and part two"),
        ],
    ))
    .await;
}

/// Two turns in one session. Pins that the second turn sees the first in its
/// history and that the log accumulates rather than restarting.
#[tokio::test]
async fn multi_turn_session() {
    check(Case::new(
        "multi_turn",
        vec!["first question", "second question"],
        vec![Reply::Text("first answer"), Reply::Text("second answer")],
    ))
    .await;
}

/// Compaction. The threshold is set absurdly low so the second turn crosses
/// it; what the trace pins is *when* the loop decides to compact and what it
/// leaves in the log.
#[tokio::test]
async fn compaction_triggers_and_is_logged() {
    let mut case = Case::new(
        "compaction",
        vec!["first", "second"],
        vec![
            Reply::Text("a fairly long first answer that fills the budget"),
            Reply::Text("second answer"),
        ],
    );
    case.compact_threshold = 10;
    check(case).await;
}

/// A tool whose `check_permissions` returns `Ask`, in a session that is not
/// bypassing permissions. The prompt must reach the host and the approval must
/// let the call proceed.
#[tokio::test]
async fn permission_prompt_approved() {
    let mut case = Case::new(
        "permission_approved",
        vec!["do the guarded thing"],
        vec![
            Reply::Tool {
                id: "c1",
                name: "GoldenAsk",
                input: serde_json::json!({}),
            },
            Reply::Text("done"),
        ],
    );
    case.permission_mode = PermissionMode::Default;
    case.approve_prompts = Some(true);
    check(case).await;
}

/// The same prompt, denied. The tool must not run, and the model must be told
/// — a denial that silently looks like success is the failure worth pinning.
#[tokio::test]
async fn permission_prompt_denied() {
    let mut case = Case::new(
        "permission_denied",
        vec!["do the guarded thing"],
        vec![
            Reply::Tool {
                id: "c1",
                name: "GoldenAsk",
                input: serde_json::json!({}),
            },
            Reply::Text("understood, skipping"),
        ],
    );
    case.permission_mode = PermissionMode::Default;
    case.approve_prompts = Some(false);
    check(case).await;
}

/// A `PreToolUse` hook that blocks the call. The tool must not run, and the
/// model must see a denial rather than a result.
///
/// This one shells out, because the only hook backend that can answer a
/// decision without a subprocess or a model call does not exist yet — that
/// missing in-process backend is the gap `P2-7` is about. It is a `printf` of
/// a fixed string, so the content is deterministic even though the mechanism
/// is heavier than the rest of this file.
#[tokio::test]
async fn pre_tool_use_hook_blocks_the_call() {
    let mut case = Case::new(
        "hook_blocks_tool",
        vec!["try the blocked thing"],
        vec![
            Reply::Tool {
                id: "c1",
                name: "GoldenEcho",
                input: serde_json::json!({"say": "should not run"}),
            },
            Reply::Text("the hook stopped me"),
        ],
    );
    case.hooks_config = Some(serde_json::json!({
        "PreToolUse": [{
            "type": "command",
            "command": "printf '{\"decision\":\"block\",\"message\":\"blocked by the behavior net\"}'"
        }]
    }));
    check(case).await;
}

/// Prompt-too-long recovery.
///
/// **This golden currently records the recovery failing.** The path compacts
/// with a threshold of `max(scene_threshold, 50_000)` and only retries if the
/// message count actually dropped; a two-message conversation cannot shrink,
/// so the turn ends in `Error` and the scripted retry is never consumed
/// (`unconsumed_replies=1` says so). That is the behavior today, recorded as
/// found rather than as wished for — if someone makes this path work, this
/// case fails and the diff is the proof.
#[tokio::test]
async fn prompt_too_long_compacts_and_retries() {
    check(Case::new(
        "prompt_too_long",
        vec!["build up some history", "now overflow"],
        vec![
            Reply::Text("a first answer long enough to be worth dropping later"),
            Reply::Fail(base::interface::model::ModelError::Internal(
                "prompt too long".into(),
            )),
            Reply::Text("recovered after compaction"),
        ],
    ))
    .await;
}

/// Sub-agent spawn. `Builder::build()` registers the `Agent` tool itself, so
/// the model can call it; the child runs on the same scripted model, which is
/// why the script carries the child's answer too.
#[tokio::test]
async fn subagent_spawn() {
    check(Case::new(
        "subagent_spawn",
        vec!["delegate this"],
        vec![
            Reply::Tool {
                id: "c1",
                name: "Agent",
                input: serde_json::json!({
                    "description": "do a thing",
                    "prompt": "do a thing",
                    "subagent_type": "general-purpose"
                }),
            },
            Reply::Text("child answer"),
            Reply::Text("parent wrap-up"),
        ],
    ))
    .await;
}
