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

    let written = std::fs::read_to_string(&output)
        .map_err(|e| anyhow::anyhow!("no telemetry at {}: {e}", output.display()))?;
    let kinds: Vec<String> = written
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v.get("type").and_then(|k| k.as_str()).map(str::to_string))
        .collect();
    for expected in [
        "session_start",
        "turn_start",
        "api_request",
        "turn_complete",
    ] {
        assert!(
            kinds.iter().any(|k| k == expected),
            "no `{expected}` in the telemetry a turn wrote: {kinds:?}"
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
