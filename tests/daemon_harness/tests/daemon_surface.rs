//! What a client gets when it talks to a daemon.
//!
//! These are the harness's own cases: each one exists because a piece of the
//! harness would be useless if it were wrong, and asserting that piece here
//! keeps every later scenario from having to.
//!
//! They are also the first tests in this workspace that reach the model over
//! HTTP. Everything else fakes it at the `AnthropicClient` seam, which skips
//! request serialization, the SSE decoder and the retry loop — three things a
//! daemon depends on and none of which had an end-to-end test.

use daemon_harness::{Block, Daemon, DaemonOptions, ProviderStub, Reply, World};
use serde_json::json;

/// A daemon, a stub, and a project — the three every case starts with.
async fn stage(name: &str) -> anyhow::Result<(World, ProviderStub, Daemon, std::path::PathBuf)> {
    let world = World::new()?;
    let project = world.project(name)?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(project.clone())).await?;
    Ok((world, provider, daemon, project))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_goes_out_over_http_and_comes_back() -> anyhow::Result<()> {
    let (_world, provider, daemon, _project) = stage("plain").await?;
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

#[tokio::test(flavor = "multi_thread")]
async fn the_session_offers_the_builtin_tools_to_the_model() -> anyhow::Result<()> {
    let (_world, provider, daemon, _project) = stage("tools").await?;
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
#[tokio::test(flavor = "multi_thread")]
async fn a_tool_call_runs_in_the_project_and_its_result_goes_back_to_the_model(
) -> anyhow::Result<()> {
    let (_world, provider, daemon, project) = stage("tool-loop").await?;
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
#[tokio::test(flavor = "multi_thread")]
async fn a_projects_own_settings_reach_the_prompt_and_only_that_projects() -> anyhow::Result<()> {
    let world = World::new()?;
    let marked = world.project("marked")?;
    let plain = world.project("plain")?;
    world.write_project_settings("marked", &json!({ "prompt_append": "SETTINGS-MARK-XYZZY" }))?;

    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(plain.clone())).await?;
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

#[tokio::test(flavor = "multi_thread")]
async fn a_session_writes_the_telemetry_it_was_asked_for() -> anyhow::Result<()> {
    let (world, provider, daemon, _project) = stage("telemetry").await?;
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

#[tokio::test(flavor = "multi_thread")]
async fn a_session_reports_itself_and_its_transcript() -> anyhow::Result<()> {
    let (_world, provider, daemon, _project) = stage("detail").await?;
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
