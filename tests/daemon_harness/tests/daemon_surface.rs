//! What a client gets when it talks to a daemon.
//!
//! Each case here exists because a piece of the harness would be useless if
//! it were wrong, and asserting that piece once keeps every later scenario
//! from having to. They are also the first tests in this workspace that reach
//! the model over HTTP: everything else fakes it at the `AnthropicClient`
//! seam, which skips request serialization, the SSE decoder and the retry
//! loop — three things a daemon depends on.
//!
//! # Two modes
//!
//! Every case is written against a [`Mode`] and run twice. In this process it
//! is fast and runs by default; against a spawned `attacored` it is
//! `#[ignore]`d, like every other case here that starts a subprocess, and CI
//! runs it in the ignored step.
//!
//! The parity is the claim being tested. A case that passes in one mode and
//! not the other has found something real: the spawned daemon resolves its
//! own settings, its own paths and its own model endpoint from a command line
//! and an environment, and every one of those is a step the in-process build
//! is handed rather than takes.

use daemon_harness::{Block, Daemon, DaemonOptions, Mode, ProviderStub, Reply, World};
use serde_json::json;
use std::time::Duration;

/// The events a session has written so far, waited for rather than read once.
///
/// Telemetry leaves the engine on a channel and is written by another task,
/// so a turn's last events land some time after the RPC that produced them
/// has already answered. Reading the file the moment a turn returns is a race
/// that only loses under load — which is to say, in CI.
async fn telemetry_until(
    path: &std::path::Path,
    enough: impl Fn(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let events: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if enough(&events) || std::time::Instant::now() > deadline {
            return events;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn kinds(events: &[serde_json::Value]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| e.get("type").and_then(|t| t.as_str()))
        .collect()
}

/// A daemon, a stub, and a project — the three every case starts with.
async fn stage(
    name: &str,
    mode: Mode,
) -> anyhow::Result<(World, ProviderStub, Daemon, std::path::PathBuf)> {
    let world = World::new()?;
    let project = world.project(name)?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).mode(mode),
    )
    .await?;
    Ok((world, provider, daemon, project))
}

// ── The cases, each written once and run in both modes ───────────────────

async fn a_turn_crosses_http_and_comes_back(mode: Mode) -> anyhow::Result<()> {
    let (_world, provider, daemon, _project) = stage("plain", mode).await?;
    provider.script([Reply::text("STUB-ANSWERED-THE-TURN")]);

    let mut client = daemon.connect().await?;
    let session = client.session_create(json!({})).await?;
    let turn = client
        .session_run_turn(&session, "say something", "t1", None)
        .await?;

    assert!(
        turn.text.contains("STUB-ANSWERED-THE-TURN"),
        "the scripted answer did not reach the client: {turn:?}"
    );
    assert!(turn.turn_complete, "turn never completed: {turn:?}");
    assert_eq!(provider.call_count(), 1, "expected exactly one model call");
    daemon.stop().await;
    Ok(())
}

async fn the_builtin_tools_reach_the_model(mode: Mode) -> anyhow::Result<()> {
    let (_world, provider, daemon, _project) = stage("tools", mode).await?;
    provider.script([Reply::text("nothing to do")]);

    let mut client = daemon.connect().await?;
    let session = client.session_create(json!({})).await?;
    client
        .session_run_turn(&session, "hello", "t1", None)
        .await?;

    let names = provider.calls()[0].tool_names();
    for expected in ["Bash", "Read", "Write", "Edit"] {
        assert!(
            names.iter().any(|n| n == expected),
            "`{expected}` never reached the model; the pool registered {names:?}"
        );
    }
    daemon.stop().await;
    Ok(())
}

/// The whole loop: the model asks for a tool, the daemon runs it in the
/// project, and the next request carries what it returned.
async fn a_tool_call_runs_in_the_project(mode: Mode) -> anyhow::Result<()> {
    let (_world, provider, daemon, project) = stage("tool-loop", mode).await?;
    let target = project.join("note.txt");
    provider.script([
        Reply::Blocks(vec![Block::tool(
            "call-1",
            "Write",
            json!({ "file_path": target.to_string_lossy(), "content": "written by the tool\n" }),
        )]),
        Reply::text("TOOL-LOOP-FINISHED"),
    ]);

    let mut client = daemon.connect().await?;
    let session = client.session_create(json!({})).await?;
    let turn = client
        .session_run_turn(&session, "write the note", "t1", None)
        .await?;

    assert_eq!(
        std::fs::read_to_string(&target)?,
        "written by the tool\n",
        "the tool did not run against the project directory"
    );
    assert!(
        turn.tool_uses.iter().any(|(name, _)| name == "Write"),
        "no Write in the frames the client saw: {turn:?}"
    );
    assert_eq!(
        provider.call_count(),
        2,
        "a tool result should cost a second call"
    );
    assert!(
        provider.calls()[1].message_count() > provider.calls()[0].message_count(),
        "the second request did not carry the first one's exchange"
    );
    daemon.stop().await;
    Ok(())
}

/// One daemon, two projects, one of them carrying a settings file. The
/// unmarked project is the control: without it, a mark found in the prompt
/// could be one that was always there.
async fn a_projects_settings_reach_only_its_own_prompt(mode: Mode) -> anyhow::Result<()> {
    let world = World::new()?;
    let marked = world.project("marked")?;
    let plain = world.project("plain")?;
    world.write_project_settings("marked", &json!({ "prompt_append": "SETTINGS-MARK-XYZZY" }))?;

    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(plain.clone()).mode(mode),
    )
    .await?;
    provider.script([Reply::text("one"), Reply::text("two")]);

    let mut client = daemon.connect().await?;
    let marked_session = client
        .session_create(json!({ "project_root": marked.to_string_lossy() }))
        .await?;
    client
        .session_run_turn(&marked_session, "hello", "t1", None)
        .await?;
    let plain_session = client
        .session_create(json!({ "project_root": plain.to_string_lossy() }))
        .await?;
    client
        .session_run_turn(&plain_session, "hello", "t2", None)
        .await?;

    let calls = provider.calls();
    assert!(
        calls[0].system_text().contains("SETTINGS-MARK-XYZZY"),
        "the marked project's settings never reached its prompt"
    );
    assert!(
        !calls[1].system_text().contains("SETTINGS-MARK-XYZZY"),
        "one project's settings leaked into another's session"
    );
    daemon.stop().await;
    Ok(())
}

async fn a_session_writes_the_telemetry_it_was_asked_for(mode: Mode) -> anyhow::Result<()> {
    let (world, provider, daemon, _project) = stage("telemetry", mode).await?;
    provider.script([Reply::text("recorded")]);
    let output = world.telemetry_file("session.jsonl");

    let mut client = daemon.connect().await?;
    let session = client
        .session_create(json!({
            "options": { "telemetry": { "output": output.to_string_lossy() } }
        }))
        .await?;
    client
        .session_run_turn(&session, "hello", "t1", None)
        .await?;

    let events = telemetry_until(&output, |events| kinds(events).contains(&"turn_complete")).await;
    let written = kinds(&events);
    for expected in [
        "session_start",
        "turn_start",
        "api_request",
        "turn_complete",
    ] {
        assert!(
            written.contains(&expected),
            "no `{expected}` in the telemetry a turn wrote: {written:?}"
        );
    }

    daemon.stop().await;
    Ok(())
}

async fn a_session_reports_itself_and_its_transcript(mode: Mode) -> anyhow::Result<()> {
    let (_world, provider, daemon, _project) = stage("detail", mode).await?;
    provider.script([Reply::text("remembered")]);

    let mut client = daemon.connect().await?;
    let session = client.session_create(json!({})).await?;
    client
        .session_run_turn(&session, "hello there", "t1", None)
        .await?;

    let detail = client.session_get(&session).await?;
    let detail = detail.result.expect("session.get returned no result");
    assert_eq!(
        detail.get("turn_state").and_then(|v| v.as_str()),
        Some("idle"),
        "a finished turn should leave the session idle: {detail}"
    );

    let history = client
        .call("session.history", json!({ "session_id": session }))
        .await?;
    let text = history.result.unwrap_or_default().to_string();
    assert!(
        text.contains("hello there") && text.contains("remembered"),
        "the transcript is missing one side of the exchange: {text}"
    );
    daemon.stop().await;
    Ok(())
}

// ── In this process ──────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_crosses_http_and_comes_back_here() -> anyhow::Result<()> {
    a_turn_crosses_http_and_comes_back(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
async fn the_builtin_tools_reach_the_model_here() -> anyhow::Result<()> {
    the_builtin_tools_reach_the_model(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_call_runs_in_the_project_here() -> anyhow::Result<()> {
    a_tool_call_runs_in_the_project(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_projects_settings_reach_only_its_own_prompt_here() -> anyhow::Result<()> {
    a_projects_settings_reach_only_its_own_prompt(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_writes_the_telemetry_it_was_asked_for_here() -> anyhow::Result<()> {
    a_session_writes_the_telemetry_it_was_asked_for(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_reports_itself_and_its_transcript_here() -> anyhow::Result<()> {
    a_session_reports_itself_and_its_transcript(Mode::InProcess).await
}

// ── Against a spawned daemon ─────────────────────────────────────────────

/// A named case, already started. Boxed because the six have different
/// concrete future types and this list holds them together.
type Case = (
    &'static str,
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>>>>,
);

/// One test rather than six, because each of these pays for a process start
/// and a settings load; the case that failed is named in the panic.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn every_case_holds_against_a_spawned_daemon() {
    let cases: Vec<Case> = vec![
        (
            "a turn crosses http and comes back",
            Box::pin(a_turn_crosses_http_and_comes_back(Mode::Spawned)),
        ),
        (
            "the builtin tools reach the model",
            Box::pin(the_builtin_tools_reach_the_model(Mode::Spawned)),
        ),
        (
            "a tool call runs in the project",
            Box::pin(a_tool_call_runs_in_the_project(Mode::Spawned)),
        ),
        (
            "a project's settings reach only its own prompt",
            Box::pin(a_projects_settings_reach_only_its_own_prompt(Mode::Spawned)),
        ),
        (
            "a session writes the telemetry it was asked for",
            Box::pin(a_session_writes_the_telemetry_it_was_asked_for(
                Mode::Spawned,
            )),
        ),
        (
            "a session reports itself and its transcript",
            Box::pin(a_session_reports_itself_and_its_transcript(Mode::Spawned)),
        ),
    ];

    for (name, case) in cases {
        if let Err(e) = case.await {
            panic!("`{name}` fails against a spawned daemon but passes in process: {e:?}");
        }
    }
}

// ── Only a separate process can answer these ─────────────────────────────

/// A daemon announces itself while it runs and takes the announcement back
/// when it stops. A stale entry is worse than none: a client that finds one
/// connects to a socket nobody is listening on.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn a_daemon_publishes_a_discovery_entry_for_as_long_as_it_runs() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("discovery")?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project).mode(Mode::Spawned),
    )
    .await?;

    let entry = world.instances_dir().join("harness.json");
    let published = std::fs::read_to_string(&entry)
        .map_err(|e| anyhow::anyhow!("no discovery entry at {}: {e}", entry.display()))?;
    let published: serde_json::Value = serde_json::from_str(&published)?;
    assert_eq!(
        published.get("socket").and_then(|v| v.as_str()),
        Some(world.socket().to_string_lossy().as_ref()),
        "the entry names a socket this daemon is not on: {published}"
    );
    assert_eq!(
        published.get("protocol_version").and_then(|v| v.as_u64()),
        Some(2),
        "discovery entry is missing the protocol version: {published}"
    );

    daemon.stop().await;
    assert!(
        !entry.exists(),
        "the discovery entry outlived the daemon that wrote it"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn daemon_shutdown_ends_the_process() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("shutdown")?;
    let provider = ProviderStub::start().await?;
    let mut daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project).mode(Mode::Spawned),
    )
    .await?;

    let mut client = daemon.connect().await?;
    client.daemon_shutdown().await?;

    let status = daemon
        .wait_for_exit(Duration::from_secs(10))
        .await
        .ok_or_else(|| anyhow::anyhow!("daemon was still running ten seconds after shutdown"))?;
    assert!(
        status.success(),
        "a requested shutdown should be a clean exit, got {status}"
    );
    Ok(())
}

/// The daemon promises a network listener fails at startup without a token,
/// rather than binding and then refusing everyone. Only a process can be
/// observed failing to start.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn a_network_listener_without_a_token_refuses_to_start() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("tokenless")?;
    let provider = ProviderStub::start().await?;

    let (status, stderr) = Daemon::spawn_and_wait(
        &world,
        &provider,
        DaemonOptions::new(project)
            .mode(Mode::Spawned)
            .arg("--listen")
            .arg("127.0.0.1:0"),
        Duration::from_secs(20),
    )
    .await?;

    assert!(
        !status.success(),
        "a tokenless TCP listener started anyway ({status})"
    );
    assert!(
        stderr.to_lowercase().contains("token"),
        "the refusal does not say a token is what was missing: {stderr}"
    );
    Ok(())
}

/// A daemon told where its home is must keep everything there. The failure
/// this guards against does not look like a bug — it looks like a file
/// appearing somewhere nobody is looking, which is why `~` is checked by
/// name: a path that is written rather than expanded lands in a directory
/// literally called that.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn a_spawned_run_leaves_nothing_outside_the_world_it_was_given() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("contained")?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).mode(Mode::Spawned),
    )
    .await?;
    provider.script([Reply::text("contained")]);

    let mut client = daemon.connect().await?;
    let session = client.session_create(json!({})).await?;
    client
        .session_run_turn(&session, "hello", "t1", None)
        .await?;
    daemon.stop().await;

    let mut literal_tildes = Vec::new();
    let mut stack = vec![world.root().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.file_name().is_some_and(|n| n == "~") {
                literal_tildes.push(path.clone());
            }
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    assert!(
        literal_tildes.is_empty(),
        "a `~` was written as a directory name instead of being expanded: {literal_tildes:?}"
    );
    assert!(
        world.global_root().join("projects").exists()
            || world.global_root().join("sessions").exists(),
        "the run left no state under the global root it was given; it went somewhere else"
    );
    Ok(())
}

// ── The carrier matrix ───────────────────────────────────────────────────
//
// A build carries one extension carrier or none, decided at compile time.
// That makes the interesting question — what does a build *without* a
// carrier do with a settings file that asks for one — unanswerable from
// inside a build that has one, which is why `docs/testing_scripts.md` lists
// it as outside the script carrier's own net. A spawned daemon can be a
// different build, so here it is.

const SCRIPT_MARK: &str = "CARRIER-SCRIPT-REACHED-THE-PROMPT";

/// A project whose settings bind one script to the prompt.
fn a_project_with_a_script(world: &World) -> anyhow::Result<std::path::PathBuf> {
    let project = world.project("scripted")?;
    world.write_project_file(
        "scripted",
        ".atta/scripts/house.js",
        &format!(
            r#"function onAssemble(blocks) {{
                 var out = blocks.map(function (b) {{ return b; }});
                 out.push({{ name: "daemon.house", content: "{SCRIPT_MARK}" }});
                 return out;
               }}"#
        ),
    )?;
    world.write_project_settings(
        "scripted",
        &json!({
            "memory_enabled": false,
            "scripts": [
                {
                    "path": ".atta/scripts/house.js",
                    "point": "prompt.assemble",
                    "entry": "onAssemble"
                }
            ]
        }),
    )?;
    Ok(project)
}

/// What the daemon did with that project: whether the mark arrived, and what
/// the session says its scripts have been doing.
async fn run_the_scripted_project(
    world: &World,
    provider: &ProviderStub,
    daemon: &Daemon,
    project: &std::path::Path,
) -> anyhow::Result<(bool, Option<serde_json::Value>)> {
    let _ = world;
    provider.script([Reply::text("done")]);
    let mut client = daemon.connect().await?;
    let session = client
        .session_create(json!({ "project_root": project.to_string_lossy() }))
        .await?;
    client
        .session_run_turn(&session, "hello", "t1", None)
        .await?;

    let arrived = provider
        .calls()
        .last()
        .is_some_and(|call| call.system_text().contains(SCRIPT_MARK));
    let detail = client.session_get(&session).await?;
    let ledger = detail
        .result
        .and_then(|r| r.get("scripts").cloned())
        .filter(|v| !v.is_null());
    Ok((arrived, ledger))
}

async fn a_bound_script_reaches_the_prompt_and_the_ledger(mode: Mode) -> anyhow::Result<()> {
    let world = World::new()?;
    let project = a_project_with_a_script(&world)?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).mode(mode),
    )
    .await?;

    let (arrived, ledger) = run_the_scripted_project(&world, &provider, &daemon, &project).await?;
    assert!(
        arrived,
        "the project's script never reached the prompt the daemon sent"
    );
    let ledger = ledger.ok_or_else(|| {
        anyhow::anyhow!("session.get reports no scripts at all on a session that bound one")
    })?;
    assert!(
        ledger["applied"].as_u64().unwrap_or(0) >= 1,
        "the ledger does not record the script as having applied anything: {ledger}"
    );
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bound_script_reaches_the_prompt_and_the_ledger_here() -> anyhow::Result<()> {
    a_bound_script_reaches_the_prompt_and_the_ledger(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn a_bound_script_reaches_the_prompt_and_the_ledger_in_a_spawned_daemon() -> anyhow::Result<()>
{
    a_bound_script_reaches_the_prompt_and_the_ledger(Mode::Spawned).await
}

/// The same project, against a daemon with no script engine in it.
///
/// Both halves are needed. That the mark is missing only says the script did
/// not run; that `session.get` reports no scripts at all says why — there was
/// nothing to run it, rather than a script that ran and did nothing. In a
/// build with a carrier those two are told apart by the ledger, and this is
/// the build where the ledger does not exist.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a second daemon, built without the script carrier"]
async fn a_build_with_no_script_carrier_honors_no_scripts_section() -> anyhow::Result<()> {
    let carrierless = daemon_harness::alternate_daemon_binary(
        "ATTA_TEST_DAEMON_BIN_NO_CARRIER",
        "CARGO_TARGET_DIR=$PWD/target/no-carrier cargo build -p daemon --no-default-features\n  \
         ATTA_TEST_DAEMON_BIN_NO_CARRIER=$PWD/target/no-carrier/debug/attacored cargo test \
         -p daemon-harness -- --ignored",
    )?;

    let world = World::new()?;
    let project = a_project_with_a_script(&world)?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).binary(carrierless),
    )
    .await?;

    let (arrived, ledger) = run_the_scripted_project(&world, &provider, &daemon, &project).await?;
    assert!(
        !arrived,
        "a build with no script engine ran a script anyway — \
         the `scripts` feature is not the only thing gating the carrier"
    );
    assert!(
        ledger.is_none(),
        "a build with no script engine reported a script ledger: {ledger:?}"
    );
    daemon.stop().await;
    Ok(())
}

// ── Telemetry, reload, and state that outlives a process ─────────────────

/// Telemetry is a report about a turn, so it is checked against an
/// independent witness of that turn rather than against itself: the stub
/// counted the calls, and the file has to agree.
async fn telemetry_counts_what_the_turn_actually_did(mode: Mode) -> anyhow::Result<()> {
    let (world, provider, daemon, project) = stage("telemetry-counts", mode).await?;
    let target = project.join("counted.txt");
    provider.script([
        Reply::Blocks(vec![Block::tool(
            "call-1",
            "Write",
            json!({ "file_path": target.to_string_lossy(), "content": "counted\n" }),
        )]),
        Reply::text("done"),
    ]);
    let output = world.telemetry_file("counts.jsonl");

    let mut client = daemon.connect().await?;
    let session = client
        .session_create(json!({
            "options": { "telemetry": { "output": output.to_string_lossy() } }
        }))
        .await?;
    client
        .session_run_turn(&session, "write it", "t1", None)
        .await?;

    let events = telemetry_until(&output, |events| kinds(events).contains(&"turn_complete")).await;
    let of_type = |kind: &str| -> Vec<&serde_json::Value> {
        events
            .iter()
            .filter(|e| e.get("type").and_then(|t| t.as_str()) == Some(kind))
            .collect()
    };

    // Both sides are scoped to the turn, and neither is a raw count. The
    // engine makes one more model call after the turn returns — memory
    // extraction, on a smaller model, with no system prompt and no tools —
    // and it lands asynchronously, so `provider.call_count()` grows on its
    // own schedule. The turn's own calls are the ones carrying its tool
    // table; that call is not one of them.
    let turn_calls = provider
        .calls()
        .into_iter()
        .filter(|c| !c.tool_names().is_empty())
        .count();
    let turn_requests = of_type("api_request")
        .into_iter()
        .filter(|e| e.get("turn_id").and_then(|v| v.as_str()) == Some("t1"))
        .count();
    assert_eq!(
        turn_requests, turn_calls,
        "telemetry and the provider disagree about how many calls this turn made"
    );
    assert_eq!(
        turn_calls, 2,
        "the scripted turn should have made two calls"
    );
    let executions = of_type("tool_execution");
    assert!(
        executions.iter().any(|e| e["tool_name"] == "Write"),
        "the tool that ran is not in the telemetry: {executions:?}"
    );

    let complete = of_type("turn_complete");
    let complete = complete
        .first()
        .ok_or_else(|| anyhow::anyhow!("no turn_complete event"))?;
    assert_eq!(
        complete["api_calls"].as_u64(),
        Some(2),
        "turn_complete miscounts its calls: {complete}"
    );
    assert_eq!(
        complete["tool_calls"].as_u64(),
        Some(1),
        "turn_complete miscounts its tools: {complete}"
    );
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn telemetry_counts_what_the_turn_actually_did_here() -> anyhow::Result<()> {
    telemetry_counts_what_the_turn_actually_did(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn telemetry_counts_what_the_turn_actually_did_in_a_spawned_daemon() -> anyhow::Result<()> {
    telemetry_counts_what_the_turn_actually_did(Mode::Spawned).await
}

/// A settings file edited while the daemon runs, and `config.reload` to pick
/// it up. The same session runs both turns: a reload that only reached new
/// sessions would leave every open one on the old configuration, which is
/// the state an operator is least likely to suspect.
async fn a_reload_reaches_a_session_that_is_already_open(mode: Mode) -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("reloaded")?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).mode(mode),
    )
    .await?;
    provider.script([
        Reply::text("before"),
        Reply::text("after"),
        Reply::text("spare"),
        Reply::text("spare"),
    ]);

    let mut client = daemon.connect().await?;
    let session = client.session_create(json!({})).await?;
    client
        .session_run_turn(&session, "first", "t1", None)
        .await?;

    // A watermark rather than an index: a turn is not the only thing that
    // calls the model, and a case that assumed call *n* was its own would
    // break the first time the engine did anything else in between.
    let before_reload = provider.call_count();

    world.write_project_settings(
        "reloaded",
        &json!({ "prompt_append": "RELOADED-MARK-PLUGH" }),
    )?;
    let reloaded = client.config_reload().await?;
    anyhow::ensure!(
        reloaded.error.is_none(),
        "config.reload failed: {reloaded:?}"
    );

    client
        .session_run_turn(&session, "second", "t2", None)
        .await?;

    let calls = provider.calls();
    let (before, after) = calls.split_at(before_reload);
    assert!(
        !before
            .iter()
            .any(|c| c.system_text().contains("RELOADED-MARK-PLUGH")),
        "the mark was in the prompt before it was ever configured"
    );
    assert!(
        after
            .iter()
            .any(|c| c.system_text().contains("RELOADED-MARK-PLUGH")),
        "an open session kept its old settings across config.reload"
    );
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reload_reaches_a_session_that_is_already_open_here() -> anyhow::Result<()> {
    a_reload_reaches_a_session_that_is_already_open(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn a_reload_reaches_a_session_that_is_already_open_in_a_spawned_daemon() -> anyhow::Result<()>
{
    a_reload_reaches_a_session_that_is_already_open(Mode::Spawned).await
}

/// Installing is a write to disk, and the only way to prove it is a write to
/// disk is to ask a daemon that was not running when it happened.
///
/// The RPC surface of install/list/enable/disable is covered in
/// `daemon/tests/daemon_e2e.rs`, in one process. That cannot distinguish a
/// package registered on disk from one remembered in a `HashMap`, because
/// nothing there ever forgets.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns two real daemon processes"]
async fn an_installed_package_outlives_the_daemon_that_installed_it() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("packaged")?;
    let provider = ProviderStub::start().await?;
    let (archive, checksum) = test_runner::plugin_fixture::package_demo_plugin(world.root())?;

    let first = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).mode(Mode::Spawned),
    )
    .await?;
    let mut client = first.connect().await?;
    let installed = client
        .call(
            "plugin.install",
            json!({
                "name": "demo-plugin",
                "version": "1.0.0",
                "download_url": format!("file://{}", archive.display()),
                "checksum": checksum,
            }),
        )
        .await?;
    anyhow::ensure!(
        installed.error.is_none(),
        "plugin.install failed: {installed:?}"
    );
    first.stop().await;

    let second = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project).mode(Mode::Spawned),
    )
    .await?;
    let mut client = second.connect().await?;
    let listed = client.call("plugin.list", json!({})).await?;
    let plugins = listed
        .result
        .as_ref()
        .and_then(|r| r.get("plugins"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let demo = plugins
        .iter()
        .find(|p| p["name"] == "demo-plugin")
        .ok_or_else(|| {
            anyhow::anyhow!("a package installed by the previous daemon is gone: {plugins:?}")
        })?;
    assert_eq!(
        demo["enabled"], true,
        "the package came back disabled: {demo}"
    );
    second.stop().await;
    Ok(())
}
