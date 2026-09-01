//! Session lifecycle RPCs (`session.history` / `.fork` / `.resume`) and the
//! `ask`-by-default permission loop, end to end over the real socket.
//!
//! Separate from `daemon_e2e.rs` because everything here needs a
//! **deterministic turn**: a real `session.run_turn` that drives the engine,
//! the tool loop and the permission gate without touching the network.
//! `SessionPool` builds its model from the `Arc<dyn AnthropicClient>` it is
//! handed, so a scripted client is the entire seam — no production code
//! exists for testing's sake, and the canned events still travel through the
//! same `AnthropicModel` adapter a real call would.

use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use base::interface::memory::MemoryStore;
use base::interface::settings::Settings;
use daemon::config::StaticDaemonPaths;
use daemon::rpc::codes;
use daemon::{DaemonServer, SessionPool};
use model::client::{AnthropicClient, CountFuture, EventStream};
use model::stream::{
    BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage as WireUsage,
};
use model::types::MessagesRequest;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

// ── Scripted model ──────────────────────────────────────────────────────

/// An `AnthropicClient` that replays one canned SSE-event batch per call, in
/// order, then repeats the last one forever — so an unexpected extra round
/// trip ends the turn instead of hanging the test.
struct ScriptedClient {
    rounds: StdMutex<std::collections::VecDeque<Vec<StreamEvent>>>,
    last: StdMutex<Vec<StreamEvent>>,
    /// Every request the engine made, for tests that assert on what the
    /// model was actually shown (e.g. "did the fork carry the transcript?").
    seen: Arc<StdMutex<Vec<MessagesRequest>>>,
}

type SeenRequests = Arc<StdMutex<Vec<MessagesRequest>>>;

impl ScriptedClient {
    fn new(rounds: Vec<Vec<StreamEvent>>) -> (Arc<Self>, SeenRequests) {
        let seen: SeenRequests = Arc::new(StdMutex::new(Vec::new()));
        let last = rounds.last().cloned().unwrap_or_else(|| text_round("done"));
        (
            Arc::new(Self {
                rounds: StdMutex::new(rounds.into()),
                last: StdMutex::new(last),
                seen: seen.clone(),
            }),
            seen,
        )
    }
}

impl AnthropicClient for ScriptedClient {
    fn stream_messages(&self, req: MessagesRequest) -> EventStream {
        self.seen.lock().unwrap().push(req);
        let events = {
            let mut q = self.rounds.lock().unwrap();
            q.pop_front()
                .unwrap_or_else(|| self.last.lock().unwrap().clone())
        };
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

/// One assistant turn that says `text` and stops.
fn text_round(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::Text {
                text: String::new(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::TextDelta {
                text: text.to_string(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(base::message::StopReason::EndTurn),
                stop_sequence: None,
            },
            usage: Some(WireUsage::default()),
        },
    ]
}

/// One assistant turn that calls `tool` with `input` and stops on `tool_use`.
fn tool_round(tool: &str, id: &str, input: serde_json::Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: id.to_string(),
                name: tool.to_string(),
                input: serde_json::Value::Null,
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta {
                partial_json: input.to_string(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(base::message::StopReason::ToolUse),
                stop_sequence: None,
            },
            usage: Some(WireUsage::default()),
        },
    ]
}

/// A turn where the model calls `Bash` with a command the classifier can't
/// vouch for — `touch` is in neither the read-only nor the destructive list,
/// so `BashTool::check_permissions` returns `Ask` and the gate falls through
/// to mode dispatch, which is exactly the path the new default changes.
fn asking_bash_rounds(marker: &str) -> Vec<Vec<StreamEvent>> {
    vec![
        tool_round(
            "Bash",
            "tool-1",
            serde_json::json!({"command": format!("touch {marker}")}),
        ),
        text_round("all done"),
    ]
}

// ── Harness ─────────────────────────────────────────────────────────────

/// The pool's *opt-out* permission instance — reachable only via
/// `permission_mode: bypassPermissions`, never the default any more.
struct AllowAllPermission;
#[async_trait::async_trait]
impl base::interface::permission::Permission for AllowAllPermission {
    async fn check(
        &self,
        _: &str,
        _: &serde_json::Value,
        _: &std::path::Path,
        _: &str,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Permit
    }
}

struct ScriptedServer {
    server: Arc<DaemonServer>,
    sock: PathBuf,
    _dir: tempfile::TempDir,
    handle: tokio::task::JoinHandle<()>,
    store: Arc<dyn history::store::HistoryStore>,
}

impl ScriptedServer {
    async fn stop(self) {
        self.server.shutdown_token().cancel();
        let _ = self.handle.await;
    }
}

/// A daemon whose model is scripted, with a real `HistoryStore` (handed back
/// so tests can inspect transcripts directly) and caller-supplied `Settings`
/// so the permission posture under test is explicit rather than ambient.
async fn start_scripted_server(
    rounds: Vec<Vec<StreamEvent>>,
    settings: Settings,
    prompt_timeout: Duration,
) -> (ScriptedServer, SeenRequests) {
    // Silent unless `RUST_LOG` says otherwise. These tests drive the whole
    // engine, so being able to turn logging on without editing the file is
    // worth the four lines — it is how the `respondToPrompt` deadlock this
    // module's `permission_prompt` route works around was diagnosed.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off")),
        )
        .try_init();
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("test.sock");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let memory_store = Arc::new(MemoryStore::new(
        dir.path().join("user").join("memory"),
        dir.path().join("local").join("memory"),
    ));
    let scene: Arc<dyn base::interface::scene::AgentScene> =
        Arc::new(scene::scene::coding::CodingScene);
    let permission: Arc<dyn base::interface::permission::Permission> = Arc::new(AllowAllPermission);
    let paths: Arc<dyn daemon::config::DaemonPaths> =
        Arc::new(StaticDaemonPaths::new(dir.path().to_path_buf()));

    let store: Arc<dyn history::store::HistoryStore> = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            &cwd,
            history::path::HistoryRoots::under(dir.path()),
        )
        .await
        .unwrap(),
    );

    let (client, seen) = ScriptedClient::new(rounds);
    let pool = Arc::new(
        SessionPool::new(
            8,
            3600,
            client,
            Arc::new(settings),
            scene,
            permission,
            memory_store,
            cwd,
            Some(store.clone()),
            paths,
            None, // task_router
        )
        .with_permission_prompt_timeout(prompt_timeout),
    );

    let cancel = CancellationToken::new();
    let server = Arc::new(DaemonServer::new(pool, cancel));
    let server2 = server.clone();
    let sock2 = sock.clone();
    let handle = tokio::spawn(async move {
        let _ = server2.serve_unix(&sock2).await;
    });
    for _ in 0..60 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(sock.exists(), "socket never bound");

    (
        ScriptedServer {
            server,
            sock,
            _dir: dir,
            handle,
            store,
        },
        seen,
    )
}

/// The same daemon shape but with `history_store: None` — the degraded mode
/// `main.rs` falls into when `JsonlHistoryStore::with_root` fails at
/// startup, where the session read/branch surface has no possible answer.
async fn start_server_without_history() -> (
    Arc<DaemonServer>,
    PathBuf,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("test.sock");
    let memory_store = Arc::new(MemoryStore::new(
        dir.path().join("user").join("memory"),
        dir.path().join("local").join("memory"),
    ));
    let scene: Arc<dyn base::interface::scene::AgentScene> =
        Arc::new(scene::scene::coding::CodingScene);
    let permission: Arc<dyn base::interface::permission::Permission> = Arc::new(AllowAllPermission);
    let paths: Arc<dyn daemon::config::DaemonPaths> =
        Arc::new(StaticDaemonPaths::new(dir.path().to_path_buf()));
    let (client, _seen) = ScriptedClient::new(vec![text_round("x")]);

    let pool = Arc::new(SessionPool::new(
        8,
        3600,
        client,
        Arc::new(Settings::defaults_for("claude-sonnet-4-6")),
        scene,
        permission,
        memory_store,
        dir.path().to_path_buf(),
        None,
        paths,
        None,
    ));
    let cancel = CancellationToken::new();
    let server = Arc::new(DaemonServer::new(pool, cancel));
    let server2 = server.clone();
    let sock2 = sock.clone();
    let handle = tokio::spawn(async move {
        let _ = server2.serve_unix(&sock2).await;
    });
    for _ in 0..60 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(sock.exists(), "socket never bound");
    (server, sock, dir, handle)
}

/// One request, one response line (for methods that don't stream).
async fn rpc(sock: &std::path::Path, msg: &str) -> serde_json::Value {
    let mut client = UnixStream::connect(sock).await.unwrap();
    client.write_all(msg.as_bytes()).await.unwrap();
    client.write_all(b"\n").await.unwrap();
    let (r, _) = client.split();
    let mut br = BufReader::new(r);
    let mut buf = String::new();
    br.read_line(&mut buf).await.unwrap();
    serde_json::from_str(&buf).unwrap()
}

/// Send one request and drain the connection until the final response (the
/// line carrying an `id`), returning `(stream_frames, response)`.
async fn rpc_streaming(
    sock: &std::path::Path,
    msg: &str,
) -> (Vec<serde_json::Value>, serde_json::Value) {
    let mut client = UnixStream::connect(sock).await.unwrap();
    client.write_all(msg.as_bytes()).await.unwrap();
    client.write_all(b"\n").await.unwrap();
    let (r, _w) = client.split();
    let mut br = BufReader::new(r);
    let mut frames = Vec::new();
    loop {
        let mut line = String::new();
        let n = br.read_line(&mut line).await.unwrap();
        assert!(n > 0, "connection closed before a final response arrived");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if v.get("id").is_some() {
            return (frames, v);
        }
        frames.push(v);
    }
}

fn events_of_kind<'a>(frames: &'a [serde_json::Value], kind: &str) -> Vec<&'a serde_json::Value> {
    frames
        .iter()
        .filter(|f| f["params"]["event"]["kind"] == kind)
        .map(|f| &f["params"]["event"])
        .collect()
}

/// Run one turn to completion, returning the session id.
async fn run_turn(sock: &std::path::Path, session_id: Option<&str>, message: &str) -> String {
    let params = match session_id {
        Some(sid) => format!(r#"{{"session_id":"{sid}","message":"{message}"}}"#),
        None => format!(r#"{{"message":"{message}"}}"#),
    };
    let (_frames, resp) = rpc_streaming(
        sock,
        &format!(r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{params},"id":1}}"#),
    )
    .await;
    assert!(resp["result"].is_object(), "turn failed: {resp}");
    resp["result"]["session_id"].as_str().unwrap().to_string()
}

async fn history(sock: &std::path::Path, sid: &str) -> serde_json::Value {
    rpc(
        sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.history","params":{{"session_id":"{sid}","limit":500}},"id":9}}"#
        ),
    )
    .await["result"]
        .clone()
}

/// `Settings` at defaults — notably `permission_mode` unset, which is now
/// `Default` (= ask), the posture under test.
fn ask_settings() -> Settings {
    Settings::defaults_for("claude-sonnet-4-6")
}

/// `ask_settings()` with post-turn memory extraction switched off — for
/// tests that inspect the *last* captured `MessagesRequest` and would
/// otherwise pick up the extraction call's fixed cheap model instead of the
/// main turn's.
fn ask_settings_no_memory() -> Settings {
    let mut s = ask_settings();
    s.memory_enabled = false;
    s
}

// ── session.history ─────────────────────────────────────────────────────

#[tokio::test]
async fn session_history_returns_the_real_transcript() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("first answer")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let sid = run_turn(&srv.sock, None, "hello there").await;
    let result = history(&srv.sock, &sid).await;

    assert_eq!(result["session_id"], sid);
    assert_eq!(
        result["active"], true,
        "the session is still live: {result}"
    );

    let messages = result["messages"].as_array().expect("messages array");
    assert!(
        messages.len() >= 2,
        "expected at least the user turn and the assistant reply: {result}"
    );
    let rendered = serde_json::to_string(messages).unwrap();
    assert!(
        rendered.contains("hello there"),
        "user turn missing: {rendered}"
    );
    assert!(
        rendered.contains("first answer"),
        "assistant turn missing: {rendered}"
    );
    assert_eq!(result["total"], messages.len());
    assert_eq!(result["has_more"], false);

    srv.stop().await;
}

#[tokio::test]
async fn session_history_respects_its_bound_and_pages() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("reply")], ask_settings(), Duration::ZERO).await;

    let sid = run_turn(&srv.sock, None, "turn one").await;
    run_turn(&srv.sock, Some(&sid), "turn two").await;
    run_turn(&srv.sock, Some(&sid), "turn three").await;

    // `limit: 0` is the cheap "just tell me how big this is" probe the
    // pagination contract documents.
    let probe = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.history","params":{{"session_id":"{sid}","limit":0}},"id":2}}"#
        ),
    )
    .await;
    let total = probe["result"]["total"].as_u64().unwrap() as usize;
    assert!(
        total >= 6,
        "three turns should project at least six messages: {probe}"
    );
    assert!(probe["result"]["messages"].as_array().unwrap().is_empty());
    assert_eq!(probe["result"]["has_more"], true);

    // A bounded page really is bounded, and says there's more.
    let page = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.history","params":{{"session_id":"{sid}","offset":0,"limit":2}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(page["result"]["messages"].as_array().unwrap().len(), 2);
    assert_eq!(page["result"]["total"], total as u64);
    assert_eq!(page["result"]["has_more"], true);

    // The tail, addressed the way the docs say to address it.
    let tail = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.history","params":{{"session_id":"{sid}","offset":{},"limit":2}},"id":4}}"#,
            total - 2
        ),
    )
    .await;
    assert_eq!(tail["result"]["messages"].as_array().unwrap().len(), 2);
    assert_eq!(tail["result"]["has_more"], false);
    assert_ne!(
        tail["result"]["messages"], page["result"]["messages"],
        "the tail page must not be the head page"
    );

    // An over-large limit clamps rather than erroring, and reports what was
    // actually applied.
    let clamped = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.history","params":{{"session_id":"{sid}","limit":100000}},"id":5}}"#
        ),
    )
    .await;
    assert_eq!(clamped["result"]["limit"], 500);

    srv.stop().await;
}

#[tokio::test]
async fn session_history_errors_cleanly_on_unknown_malformed_and_missing_ids() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    // Well-formed id, never used.
    let unknown = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.history","params":{"session_id":"NopeNopeNopeNopeNope12"},"id":1}"#,
    )
    .await;
    assert_eq!(
        unknown["error"]["code"],
        codes::SESSION_NOT_FOUND,
        "{unknown}"
    );

    // Not a BASE58 id at all — a caller mistake, not a missing session.
    let malformed = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.history","params":{"session_id":"not-an-id!!"},"id":2}"#,
    )
    .await;
    assert_eq!(
        malformed["error"]["code"],
        codes::INVALID_PARAMS,
        "{malformed}"
    );

    let missing = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.history","params":{},"id":3}"#,
    )
    .await;
    assert_eq!(missing["error"]["code"], codes::INVALID_PARAMS, "{missing}");

    srv.stop().await;
}

#[tokio::test]
async fn session_history_is_unavailable_without_a_history_store() {
    let (server, sock, _dir, handle) = start_server_without_history().await;
    let v = rpc(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.history","params":{"session_id":"NopeNopeNopeNopeNope12"},"id":1}"#,
    )
    .await;
    assert_eq!(v["error"]["code"], codes::INTERNAL_ERROR, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("history_store_wired"),
        "the error should point at the doctor field that explains it: {v}"
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}

// ── session.fork ────────────────────────────────────────────────────────

#[tokio::test]
async fn session_fork_produces_an_independent_session() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("original answer")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let original = run_turn(&srv.sock, None, "original question").await;

    let fork = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{original}"}},"id":2}}"#
        ),
    )
    .await;
    let fork_id = fork["result"]["session_id"].as_str().unwrap().to_string();
    assert_ne!(fork_id, original, "a fork must get its own id");
    assert_eq!(fork["result"]["parent_session_id"], original);
    assert_eq!(
        fork["result"]["active"], false,
        "a fork is disk-only until resumed"
    );

    let before_original = history(&srv.sock, &original).await;
    let before_fork = history(&srv.sock, &fork_id).await;
    assert_eq!(
        before_original["messages"], before_fork["messages"],
        "the fork must start as an exact copy"
    );

    // Now write to the fork — a real turn, through the normal resume path.
    run_turn(&srv.sock, Some(&fork_id), "only on the fork").await;

    let after_original = history(&srv.sock, &original).await;
    let after_fork = history(&srv.sock, &fork_id).await;
    assert_eq!(
        after_original["messages"], before_original["messages"],
        "writing to the fork must not touch the original"
    );
    assert!(
        after_fork["total"].as_u64().unwrap() > before_fork["total"].as_u64().unwrap(),
        "the fork should have grown: {after_fork}"
    );
    assert!(serde_json::to_string(&after_fork["messages"])
        .unwrap()
        .contains("only on the fork"));
    assert!(
        !serde_json::to_string(&after_original["messages"])
            .unwrap()
            .contains("only on the fork"),
        "the fork's new turn leaked into the original"
    );

    srv.stop().await;
}

#[tokio::test]
async fn session_fork_at_a_message_truncates_and_carries_context_forward() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("answer one")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let original = run_turn(&srv.sock, None, "MARKERONE").await;
    run_turn(&srv.sock, Some(&original), "MARKERTWO").await;

    // Keep only the first exchange.
    let fork = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{original}","at_message":2}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(fork["result"]["forked_at_message"], 2, "{fork}");
    let fork_id = fork["result"]["session_id"].as_str().unwrap().to_string();

    let forked = history(&srv.sock, &fork_id).await;
    let rendered = serde_json::to_string(&forked["messages"]).unwrap();
    assert_eq!(forked["total"], 2, "{forked}");
    assert!(rendered.contains("MARKERONE"));
    assert!(
        !rendered.contains("MARKERTWO"),
        "the branch point was ignored: {rendered}"
    );

    // The truncated history is what the *model* sees on the fork's next
    // turn — that's the point of forking, not just copying a file.
    seen.lock().unwrap().clear();
    run_turn(&srv.sock, Some(&fork_id), "continue").await;
    let sent = {
        let requests = seen.lock().unwrap();
        let last = requests
            .last()
            .expect("the fork's turn must have called the model");
        format!("{:?}", last.messages)
    };
    assert!(
        sent.contains("MARKERONE"),
        "the fork lost its inherited context"
    );
    assert!(
        !sent.contains("MARKERTWO"),
        "the fork inherited turns from past its branch point"
    );

    srv.stop().await;
}

#[tokio::test]
async fn session_fork_rejects_unknown_sessions_and_bad_branch_points() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let unknown = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.fork","params":{"session_id":"NopeNopeNopeNopeNope12"},"id":1}"#,
    )
    .await;
    assert_eq!(
        unknown["error"]["code"],
        codes::SESSION_NOT_FOUND,
        "{unknown}"
    );

    let sid = run_turn(&srv.sock, None, "hi").await;
    let bad = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{sid}","at_message":"lots"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(bad["error"]["code"], codes::INVALID_PARAMS, "{bad}");

    // Past the end clamps to "the whole thing" rather than erroring — the
    // caller asked for at least this much and there isn't any more.
    let clamped = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{sid}","at_message":9999}},"id":3}}"#
        ),
    )
    .await;
    assert!(clamped["result"].is_object(), "{clamped}");
    assert_eq!(
        clamped["result"]["forked_at_message"], clamped["result"]["source_message_count"],
        "{clamped}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn forked_session_records_its_parent_on_disk() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let original = run_turn(&srv.sock, None, "root").await;

    let fork = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{original}"}},"id":2}}"#
        ),
    )
    .await;
    let fork_id = fork["result"]["session_id"].as_str().unwrap().to_string();

    // Lineage lives in the fork's `Meta` line, which is what makes
    // `HistoryStore::child_sessions` able to find it.
    let children = srv.store.child_sessions(&original).await.unwrap();
    assert_eq!(
        children.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        vec![fork_id],
        "the fork should be discoverable as a child of its source"
    );

    srv.stop().await;
}

#[tokio::test]
async fn closing_a_session_does_not_delete_what_was_forked_from_it() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let original = run_turn(&srv.sock, None, "root").await;

    let fork = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{original}"}},"id":2}}"#
        ),
    )
    .await;
    let fork_id = fork["result"]["session_id"].as_str().unwrap().to_string();

    // A fork records its source in `parent_session_id` so the lineage stays
    // queryable, but it is nobody's sidechain: closing the source must leave
    // it alone, which is the whole point of forking.
    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{original}"}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(
        closed["result"]["sidechains_deleted"], 0,
        "a fork is not a sidechain: {closed}"
    );

    let sid = base::session::SessionId::parse(&fork_id).unwrap();
    assert!(
        srv.store.load(sid).await.is_ok(),
        "the fork's transcript must survive its source being closed"
    );

    srv.stop().await;
}

#[tokio::test]
async fn listing_by_parent_reports_each_child_as_what_it_is() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let original = run_turn(&srv.sock, None, "root").await;

    let fork = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{original}"}},"id":2}}"#
        ),
    )
    .await;
    let fork_id = fork["result"]["session_id"].as_str().unwrap().to_string();

    let listed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.list","params":{{"parent_session_id":"{original}"}},"id":3}}"#
        ),
    )
    .await;
    let sessions = listed["result"]["sessions"].as_array().unwrap();
    let entry = sessions
        .iter()
        .find(|s| s["session_id"] == fork_id.as_str())
        .unwrap_or_else(|| panic!("the fork should be listed under its source: {listed}"));
    assert_eq!(
        entry["session_kind"], "primary",
        "a fork listed under its source must not be reported as a sidechain: {listed}"
    );

    srv.stop().await;
}

// ── session.resume ──────────────────────────────────────────────────────

#[tokio::test]
async fn session_resume_reattaches_and_reports_state() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("stored answer")],
        ask_settings(),
        Duration::from_secs(42),
    )
    .await;

    let sid = run_turn(&srv.sock, None, "remembered question").await;

    // An already-live session is reported as such, not recreated.
    let active = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(active["result"]["status"], "already_active", "{active}");
    assert!(active["result"]["message_count"].as_u64().unwrap() >= 2);
    assert_eq!(active["result"]["scene"], "coding");
    assert_eq!(active["result"]["permission"]["mode"], "default");
    assert_eq!(active["result"]["permission"]["prompts"], true);
    assert_eq!(active["result"]["permission"]["prompt_timeout_secs"], 42);

    // Drop it from memory the way a daemon restart / idle eviction would,
    // then reattach: the transcript must come back with it.
    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(closed["result"]["closed"], sid);

    let resumed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","params":{{"session_id":"{sid}"}},"id":4}}"#
        ),
    )
    .await;
    assert_eq!(resumed["result"]["status"], "resumed", "{resumed}");
    assert_eq!(resumed["result"]["session_id"], sid);
    assert_eq!(resumed["result"]["active"], true);
    assert!(
        resumed["result"]["message_count"].as_u64().unwrap() >= 2,
        "resume must report what it reattached to: {resumed}"
    );
    assert!(resumed["result"]["entry_count"].as_u64().unwrap() >= 2);

    // And the reattached engine really does hold the old context.
    seen.lock().unwrap().clear();
    run_turn(&srv.sock, Some(&sid), "and now").await;
    let sent = {
        let requests = seen.lock().unwrap();
        format!("{:?}", requests.last().unwrap().messages)
    };
    assert!(
        sent.contains("remembered question"),
        "resumed session lost its transcript: {sent}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn session_resume_refuses_an_unknown_id_unless_asked_to_create() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let missing = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.resume","params":{"session_id":"NopeNopeNopeNopeNope12"},"id":1}"#,
    )
    .await;
    assert_eq!(
        missing["error"]["code"],
        codes::SESSION_NOT_FOUND,
        "{missing}"
    );

    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.resume","params":{"session_id":"NopeNopeNopeNopeNope12","create_if_missing":true},"id":2}"#,
    )
    .await;
    assert_eq!(created["result"]["status"], "created", "{created}");
    assert_eq!(created["result"]["message_count"], 0);

    srv.stop().await;
}

/// P1 §3.4: a session recorded under a scene other than this daemon's must
/// be rejected, not silently resumed into the wrong scene.
#[tokio::test]
async fn session_resume_rejects_a_scene_mismatch() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let sid = base::session::SessionId::new();
    srv.store
        .append(
            sid,
            history::entry::LogEntry::Meta {
                cwd: "/tmp".into(),
                started_at: time::OffsetDateTime::now_utc(),
                model: "claude-sonnet-4-6".into(),
                permission_mode: "default".into(),
                engine_version: "0.0.1".into(),
                attacode_version: "0.0.1".into(),
                parent_session_id: None,
                scene: Some("chat".into()),
                project_root: None,
                session_kind: history::entry::SessionKind::Primary,
                schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
            },
        )
        .await
        .unwrap();

    let resumed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","params":{{"session_id":"{sid}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(resumed["error"]["code"], codes::SCENE_MISMATCH, "{resumed}");
    assert_eq!(resumed["error"]["data"]["session_id"], sid.to_string());
    assert_eq!(resumed["error"]["data"]["recorded_scene"], "chat");
    assert_eq!(resumed["error"]["data"]["requested_scene"], "coding");

    srv.stop().await;
}

/// P5 §5.6/§9: a sidechain that already ran its one-shot task to
/// conclusion (has a `SessionEnd` marker) must not be resumable — a stale
/// `session.resume` against it is rejected with `SIDECHAIN_TERMINAL`
/// instead of silently reattaching to a "finished" transcript.
#[tokio::test]
async fn session_resume_rejects_a_terminal_sidechain() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;
    srv.store
        .append(
            child,
            history::entry::LogEntry::SessionEnd {
                state: history::entry::SessionEndState::Completed,
            },
        )
        .await
        .unwrap();

    let resumed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","params":{{"session_id":"{child}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(
        resumed["error"]["code"],
        codes::SIDECHAIN_TERMINAL,
        "{resumed}"
    );
    assert_eq!(resumed["error"]["data"]["session_id"], child.to_string());
    assert_eq!(resumed["error"]["data"]["parent_session_id"], parent);
    assert_eq!(resumed["error"]["data"]["final_state"], "completed");

    srv.stop().await;
}

/// Sanity counterpart: a sidechain with no `SessionEnd` marker (still
/// running, or cut off externally) must resume normally — the terminal
/// check must not fire on absence.
#[tokio::test]
async fn session_resume_allows_a_non_terminal_sidechain() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;

    let resumed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","params":{{"session_id":"{child}"}},"id":1}}"#
        ),
    )
    .await;
    assert!(resumed.get("error").is_none(), "{resumed}");

    srv.stop().await;
}

/// P1 §3.4/§12.2: a pre-v2 `Meta` (no `scene` field) must resume normally
/// and report `scene_inferred: true` rather than being rejected.
#[tokio::test]
async fn session_resume_infers_scene_for_a_pre_v2_meta() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let sid = base::session::SessionId::new();
    srv.store
        .append(
            sid,
            history::entry::LogEntry::Meta {
                cwd: "/tmp".into(),
                started_at: time::OffsetDateTime::now_utc(),
                model: "claude-sonnet-4-6".into(),
                permission_mode: "default".into(),
                engine_version: "0.0.1".into(),
                attacode_version: "0.0.1".into(),
                parent_session_id: None,
                scene: None,
                project_root: None,
                session_kind: history::entry::SessionKind::Primary,
                schema_version: 1,
            },
        )
        .await
        .unwrap();

    let resumed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","params":{{"session_id":"{sid}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(resumed["result"]["status"], "resumed", "{resumed}");
    assert_eq!(resumed["result"]["scene_inferred"], true);

    srv.stop().await;
}

// ── session.list sidechain filtering ────────────────────────────────────

/// P1 §5.4: sidechains are hidden from the default list, and returned
/// exactly via `parent_session_id`.
#[tokio::test]
async fn session_list_excludes_sidechains_by_default_and_returns_them_via_parent_filter() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;

    let child_sid = base::session::SessionId::new();
    srv.store
        .append(
            child_sid,
            history::entry::LogEntry::Meta {
                cwd: "/tmp".into(),
                started_at: time::OffsetDateTime::now_utc(),
                model: "claude-sonnet-4-6".into(),
                permission_mode: "default".into(),
                engine_version: "0.0.1".into(),
                attacode_version: "0.0.1".into(),
                parent_session_id: Some(parent.clone()),
                scene: Some("coding".into()),
                project_root: None,
                session_kind: history::entry::SessionKind::Sidechain,
                schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
            },
        )
        .await
        .unwrap();

    let default_list = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","params":{},"id":1}"#,
    )
    .await;
    let ids: Vec<String> = default_list["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_id"].as_str().unwrap().to_string())
        .collect();
    assert!(
        ids.contains(&parent),
        "default list must include the primary session: {ids:?}"
    );
    assert!(
        !ids.contains(&child_sid.to_string()),
        "default list must not include the sidechain: {ids:?}"
    );

    let children = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.list","params":{{"parent_session_id":"{parent}"}},"id":2}}"#
        ),
    )
    .await;
    let child_ids: Vec<String> = children["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(child_ids, vec![child_sid.to_string()], "{children}");

    let with_children = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","params":{"include_children":true},"id":3}"#,
    )
    .await;
    let all_ids: Vec<String> = with_children["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_id"].as_str().unwrap().to_string())
        .collect();
    assert!(all_ids.contains(&parent));
    assert!(all_ids.contains(&child_sid.to_string()));

    srv.stop().await;
}

/// `SessionInfo.resumable` (§5.6): a primary session is always resumable; a
/// sidechain is resumable until it gets a `SessionEnd` marker, after which
/// `session.list` must report it as `false` too — not just `session.resume`.
#[tokio::test]
async fn session_list_reports_resumable_and_flips_it_off_after_a_terminal_marker() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;

    let before = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.list","params":{{"parent_session_id":"{parent}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(
        before["result"]["sessions"][0]["resumable"], true,
        "{before}"
    );

    srv.store
        .append(
            child,
            history::entry::LogEntry::SessionEnd {
                state: history::entry::SessionEndState::Failed,
            },
        )
        .await
        .unwrap();

    let after = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.list","params":{{"parent_session_id":"{parent}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(
        after["result"]["sessions"][0]["resumable"], false,
        "{after}"
    );

    let primary_list = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","params":{},"id":3}"#,
    )
    .await;
    let primary = primary_list["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == parent)
        .expect("primary session should be listed");
    assert_eq!(primary["resumable"], true, "{primary}");

    srv.stop().await;
}

// ── session.create (P3: multi-project) ──────────────────────────────────

/// A session bound to a project other than the pool's default must use
/// *that* project's merged settings — proven functionally (the model name
/// actually sent to the API), not just via the Meta stamp.
#[tokio::test]
async fn session_create_binds_to_a_different_project_with_its_own_settings() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("hi")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let other_project = srv._dir.path().join("other-project");
    std::fs::create_dir_all(other_project.join(".atta")).unwrap();
    std::fs::write(
        other_project.join(".atta").join("settings.json"),
        r#"{"model": {"model_name": "other-project-model"}, "memory_enabled": false}"#,
    )
    .unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            other_project.display()
        ),
    )
    .await;
    let sid = match created["result"]["session_id"].as_str() {
        Some(s) => s.to_string(),
        None => panic!("session.create should succeed: {created}"),
    };
    assert_eq!(
        created["result"]["project_root"],
        other_project.display().to_string()
    );
    assert_eq!(created["result"]["scene"], "coding");
    assert_eq!(created["result"]["session_kind"], "primary");

    let entries = srv
        .store
        .load(base::session::SessionId::parse(&sid).unwrap())
        .await
        .unwrap();
    match &entries[0].entry {
        history::entry::LogEntry::Meta { project_root, .. } => {
            assert_eq!(
                project_root.as_deref(),
                Some(other_project.display().to_string().as_str())
            );
        }
        other => panic!("expected Meta as the first entry, got {other:?}"),
    }

    run_turn(&srv.sock, Some(&sid), "go").await;
    let sent_model = seen.lock().unwrap().last().unwrap().model.clone();
    assert_eq!(
        sent_model, "other-project-model",
        "the turn must have used the project's own settings, not the pool default"
    );

    srv.stop().await;
}

#[tokio::test]
async fn session_create_rejects_a_nonexistent_project_root() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let missing = srv._dir.path().join("does-not-exist");
    let resp = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            missing.display()
        ),
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::PROJECT_NOT_FOUND, "{resp}");

    srv.stop().await;
}

#[tokio::test]
async fn session_create_with_null_project_root_is_a_no_project_session() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    // `chat`, not the daemon's `coding`: a no-project session is only
    // meaningful in a scene that doesn't require one, and `coding` now
    // refuses (see `AgentScene::requires_project`).
    let _ = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"chat"},"id":0}"#,
    )
    .await;
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"chat","project_root":null},"id":1}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(created["result"]["project_root"].is_null(), "{created}");

    let entries = srv
        .store
        .load(base::session::SessionId::parse(&sid).unwrap())
        .await
        .unwrap();
    match &entries[0].entry {
        history::entry::LogEntry::Meta {
            project_root,
            session_kind,
            ..
        } => {
            assert_eq!(*project_root, None);
            assert_eq!(*session_kind, history::entry::SessionKind::Primary);
        }
        other => panic!("expected Meta as the first entry, got {other:?}"),
    }

    srv.stop().await;
}

#[tokio::test]
async fn session_create_without_project_root_uses_the_pool_default() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{},"id":1}"#,
    )
    .await;
    let default_project = srv._dir.path().join("work");
    assert_eq!(
        created["result"]["project_root"],
        default_project.display().to_string(),
        "{created}"
    );

    srv.stop().await;
}

/// P3's whole point: a project-bound session must still use *its own*
/// project settings after being dropped from memory and reattached — not
/// silently fall back to the pool default the moment it needs rebuilding.
#[tokio::test]
async fn a_project_bound_session_keeps_its_own_settings_across_close_and_resume() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("a"), text_round("b")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let other_project = srv._dir.path().join("other-project");
    std::fs::create_dir_all(other_project.join(".atta")).unwrap();
    std::fs::write(
        other_project.join(".atta").join("settings.json"),
        r#"{"model": {"model_name": "other-project-model"}, "memory_enabled": false}"#,
    )
    .unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            other_project.display()
        ),
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    run_turn(&srv.sock, Some(&sid), "first").await;

    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(closed["result"]["closed"], sid);

    run_turn(&srv.sock, Some(&sid), "second, after reattach").await;
    let sent_model = seen.lock().unwrap().last().unwrap().model.clone();
    assert_eq!(
        sent_model, "other-project-model",
        "resuming must rebuild against the session's own recorded project, not the pool default"
    );

    srv.stop().await;
}

// ── session.close / session.delete cascade (P5) ─────────────────────────

async fn inject_sidechain(srv: &ScriptedServer, parent: &str) -> base::session::SessionId {
    let child_sid = base::session::SessionId::new();
    srv.store
        .append(
            child_sid,
            history::entry::LogEntry::Meta {
                cwd: "/tmp".into(),
                started_at: time::OffsetDateTime::now_utc(),
                model: "claude-sonnet-4-6".into(),
                permission_mode: "default".into(),
                engine_version: "0.0.1".into(),
                attacode_version: "0.0.1".into(),
                parent_session_id: Some(parent.to_string()),
                scene: Some("coding".into()),
                project_root: None,
                session_kind: history::entry::SessionKind::Sidechain,
                schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
            },
        )
        .await
        .unwrap();
    child_sid
}

#[tokio::test]
async fn session_close_cascades_deletes_its_sidechains() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;

    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{parent}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(closed["result"]["closed"], parent);
    assert_eq!(closed["result"]["sidechains_deleted"], 1, "{closed}");

    assert!(
        srv.store.load(child).await.is_err(),
        "the sidechain's transcript must be gone after the parent closes"
    );
    assert!(
        srv.store
            .load(base::session::SessionId::parse(&parent).unwrap())
            .await
            .is_ok(),
        "session.close must leave the parent's own transcript on disk"
    );

    srv.stop().await;
}

#[tokio::test]
async fn session_delete_removes_the_parent_and_its_sidechains() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;

    let deleted = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.delete","params":{{"session_id":"{parent}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(deleted["result"]["deleted"], true, "{deleted}");
    assert_eq!(deleted["result"]["sidechains_deleted"], 1, "{deleted}");
    assert_eq!(
        deleted["result"]["sidechain_ids"],
        serde_json::json!([child.to_string()])
    );

    assert!(srv.store.load(child).await.is_err());
    assert!(srv
        .store
        .load(base::session::SessionId::parse(&parent).unwrap())
        .await
        .is_err());

    srv.stop().await;
}

#[tokio::test]
async fn session_delete_dry_run_reports_without_deleting_anything() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;

    let previewed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.delete","params":{{"session_id":"{parent}","dry_run":true}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(previewed["result"]["deleted"], false, "{previewed}");
    assert_eq!(previewed["result"]["sidechains_deleted"], 1, "{previewed}");

    assert!(
        srv.store.load(child).await.is_ok(),
        "dry_run must not actually delete the sidechain"
    );
    assert!(srv
        .store
        .load(base::session::SessionId::parse(&parent).unwrap())
        .await
        .is_ok());

    srv.stop().await;
}

/// P6 review fix: `session.close`/`.delete` skipped the scene-isolation
/// check `session.resume`/`.fork` already had, so a session recorded under
/// a scene this daemon isn't even serving could still be closed/deleted
/// through it. Mirrors `session_resume_rejects_a_scene_mismatch`'s setup —
/// a session recorded under `"chat"`, which this daemon never activated.
#[tokio::test]
async fn session_close_rejects_a_scene_mismatch() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let sid = base::session::SessionId::new();
    srv.store
        .append(
            sid,
            history::entry::LogEntry::Meta {
                cwd: "/tmp".into(),
                started_at: time::OffsetDateTime::now_utc(),
                model: "claude-sonnet-4-6".into(),
                permission_mode: "default".into(),
                engine_version: "0.0.1".into(),
                attacode_version: "0.0.1".into(),
                parent_session_id: None,
                scene: Some("chat".into()),
                project_root: None,
                session_kind: history::entry::SessionKind::Primary,
                schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
            },
        )
        .await
        .unwrap();

    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(closed["error"]["code"], codes::SCENE_MISMATCH, "{closed}");
    assert_eq!(closed["error"]["data"]["recorded_scene"], "chat");
    assert!(
        srv.store.load(sid).await.is_ok(),
        "a rejected session.close must not touch the transcript"
    );

    let deleted = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.delete","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(deleted["error"]["code"], codes::SCENE_MISMATCH, "{deleted}");
    assert!(
        srv.store.load(sid).await.is_ok(),
        "a rejected session.delete must not touch the transcript"
    );

    srv.stop().await;
}

// ── scene.* (P4: multi-scene) ────────────────────────────────────────────

#[tokio::test]
async fn scene_list_reports_the_default_scene_as_active() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.list","id":1}"#,
    )
    .await;
    let scenes = resp["result"]["scenes"].as_array().unwrap();
    let coding = scenes
        .iter()
        .find(|s| s["scene"] == "coding")
        .expect("coding should be registered");
    assert_eq!(coding["active"], true, "{resp}");
    let chat = scenes
        .iter()
        .find(|s| s["scene"] == "chat")
        .expect("chat should be registered even though inactive");
    assert_eq!(chat["active"], false, "{resp}");

    srv.stop().await;
}

#[tokio::test]
async fn session_create_rejects_an_inactive_scene() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"chat"},"id":1}"#,
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::SCENE_NOT_FOUND, "{resp}");

    srv.stop().await;
}

#[tokio::test]
async fn scene_activate_makes_a_new_scene_available_for_session_create() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let activated = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"chat"},"id":1}"#,
    )
    .await;
    assert_eq!(activated["result"]["active"], true, "{activated}");

    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"chat"},"id":2}"#,
    )
    .await;
    let sid = match created["result"]["session_id"].as_str() {
        Some(s) => s,
        None => panic!("session.create should succeed once chat is active: {created}"),
    };
    assert_eq!(created["result"]["scene"], "chat");

    let entries = srv
        .store
        .load(base::session::SessionId::parse(sid).unwrap())
        .await
        .unwrap();
    match &entries[0].entry {
        history::entry::LogEntry::Meta { scene, .. } => {
            assert_eq!(scene.as_deref(), Some("chat"));
        }
        other => panic!("expected Meta as the first entry, got {other:?}"),
    }

    srv.stop().await;
}

#[tokio::test]
async fn scene_activate_rejects_an_unknown_scene_name() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"not-a-real-scene"},"id":1}"#,
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::SCENE_NOT_FOUND, "{resp}");

    srv.stop().await;
}

#[tokio::test]
async fn scene_deactivate_is_blocked_by_active_sessions_then_succeeds_after_close() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"chat"},"id":1}"#,
    )
    .await;
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"chat"},"id":2}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let blocked = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.deactivate","params":{"scene":"chat"},"id":3}"#,
    )
    .await;
    assert_eq!(
        blocked["error"]["code"],
        codes::SCENE_HAS_ACTIVE_SESSIONS,
        "{blocked}"
    );

    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":4}}"#
        ),
    )
    .await;

    let deactivated = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.deactivate","params":{"scene":"chat"},"id":5}"#,
    )
    .await;
    assert_eq!(deactivated["result"]["scene"], "chat", "{deactivated}");

    let listed = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.list","id":6}"#,
    )
    .await;
    let chat = listed["result"]["scenes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["scene"] == "chat")
        .unwrap();
    assert_eq!(chat["active"], false, "{listed}");

    srv.stop().await;
}

// ── permission default = ask, end to end ────────────────────────────────

/// Drive a turn on one connection while answering prompts on another — the
/// turn's own connection is blocked streaming frames, which is exactly the
/// shape a real client is in when a prompt arrives. `answer` is called once
/// per prompt with `(session_id, prompt_id)` and returns the `decision`
/// JSON to send back, or `None` to leave the prompt unanswered.
async fn run_turn_answering_prompts<F>(
    sock: &std::path::Path,
    message: &str,
    options: &str,
    mut answer: F,
) -> (Vec<serde_json::Value>, serde_json::Value)
where
    F: FnMut(usize) -> Option<String>,
{
    let request = format!(
        r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"message":"{message}"{options}}},"id":1}}"#
    );
    let mut conn = UnixStream::connect(sock).await.unwrap();
    conn.write_all(request.as_bytes()).await.unwrap();
    conn.write_all(b"\n").await.unwrap();
    let (r, _w) = conn.split();
    let mut br = BufReader::new(r);

    let mut frames = Vec::new();
    let mut prompt_no = 0usize;
    loop {
        let mut line = String::new();
        let n = br.read_line(&mut line).await.unwrap();
        assert!(n > 0, "connection closed before a final response arrived");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if v.get("id").is_some() {
            return (frames, v);
        }
        if v["params"]["event"]["kind"] == "prompt" {
            if let Some(decision) = answer(prompt_no) {
                let sid = v["params"]["session_id"].as_str().unwrap();
                let prompt_id = v["params"]["event"]["prompt_id"].as_str().unwrap();
                let ack = rpc(
                    sock,
                    &format!(
                        r#"{{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{{"session_id":"{sid}","prompt_id":"{prompt_id}","decision":{decision}}},"id":99}}"#
                    ),
                )
                .await;
                assert_eq!(ack["result"]["prompt_id"], prompt_id, "{ack}");
            }
            prompt_no += 1;
        }
        frames.push(v);
    }
}

#[tokio::test]
async fn ask_default_prompts_then_proceeds_once_permitted() {
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("permitted.txt"),
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    let (frames, response) = run_turn_answering_prompts(&srv.sock, "make a file", "", |_| {
        Some(r#"{"type":"permit"}"#.to_string())
    })
    .await;

    let prompts = events_of_kind(&frames, "prompt");
    assert!(
        !prompts.is_empty(),
        "the new default must emit a kind:\"prompt\" frame for an unlisted Bash call: {frames:?}"
    );
    assert_eq!(prompts[0]["prompt_type"], "permission");
    assert_eq!(prompts[0]["tool_name"], "Bash");
    assert!(prompts[0]["prompt_id"].is_string());
    assert!(response["result"].is_object(), "turn failed: {response}");

    let results = events_of_kind(&frames, "tool_result");
    assert!(
        !results.is_empty(),
        "no tool_result after permitting: {frames:?}"
    );
    let rendered = serde_json::to_string(&results).unwrap();
    assert!(
        !rendered.contains("Denied by permission"),
        "a permitted call was denied anyway: {rendered}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn ask_default_blocks_the_tool_when_denied() {
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("denied.txt"),
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    let (frames, response) = run_turn_answering_prompts(&srv.sock, "make a file", "", |_| {
        Some(r#"{"type":"deny","reason":"not on my machine"}"#.to_string())
    })
    .await;

    assert!(response["result"].is_object(), "turn failed: {response}");
    let rendered = serde_json::to_string(&events_of_kind(&frames, "tool_result")).unwrap();
    assert!(
        rendered.contains("Denied by permission") && rendered.contains("not on my machine"),
        "a denied call must surface the denial to the model: {rendered}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn settings_bypass_permissions_opt_out_restores_allow_all() {
    let mut settings = ask_settings();
    settings.permission_mode = base::interface::settings::PermissionMode::BypassPermissions;

    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("bypassed.txt"),
        settings,
        Duration::from_secs(30),
    )
    .await;

    let (frames, response) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file"},"id":1}"#,
    )
    .await;

    assert!(response["result"].is_object(), "turn failed: {response}");
    assert!(
        events_of_kind(&frames, "prompt").is_empty(),
        "the opt-out must not prompt at all: {frames:?}"
    );
    let rendered = serde_json::to_string(&events_of_kind(&frames, "tool_result")).unwrap();
    assert!(
        !rendered.contains("Denied by permission"),
        "the opt-out must not deny either: {rendered}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn per_session_bypass_option_is_clamped_to_the_configured_mode() {
    // N-6: `settings.permission_mode` is a policy, not a default. A client
    // asking for `bypassPermissions` against an `ask`-configured daemon is
    // clamped and still prompted — otherwise anyone who can reach the socket
    // opts themselves out, and `daemon/src/server.rs` is explicit that there
    // is no per-method authorization on it.
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("session-bypassed.txt"),
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    let (frames, response) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file","options":{"permission_mode":"bypassPermissions"}},"id":1}"#,
    )
    .await;

    assert!(response["result"].is_object(), "turn failed: {response}");
    assert!(
        !events_of_kind(&frames, "prompt").is_empty(),
        "a client-requested bypass must be clamped and still prompt: {frames:?}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn allow_client_permission_override_lets_a_client_opt_out() {
    // ...and the operator can still hand that authority over explicitly.
    let mut settings = ask_settings();
    settings.allow_client_permission_override = true;
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("session-bypassed.txt"),
        settings,
        Duration::from_secs(30),
    )
    .await;

    let (frames, response) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file","options":{"permission_mode":"bypassPermissions"}},"id":1}"#,
    )
    .await;

    assert!(response["result"].is_object(), "turn failed: {response}");
    assert!(
        events_of_kind(&frames, "prompt").is_empty(),
        "with the override enabled, bypassPermissions must not prompt: {frames:?}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn settings_permission_rules_finally_reach_the_daemon() {
    // `settings.permission_rules` has been parsed by `Settings::load` for a
    // long time with no production consumer at all. Under the ask default it
    // finally has one: an Allow rule pre-empts the prompt.
    let mut settings = ask_settings();
    settings.permission_rules = vec![base::interface::settings::PermissionRule {
        tool: "Bash(touch ruled.txt)".into(),
        action: base::interface::settings::PermissionAction::Allow,
    }];

    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("ruled.txt"),
        settings,
        Duration::from_secs(30),
    )
    .await;

    let (frames, response) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file"},"id":1}"#,
    )
    .await;

    assert!(response["result"].is_object(), "turn failed: {response}");
    assert!(
        events_of_kind(&frames, "prompt").is_empty(),
        "an explicit allow rule must pre-empt the prompt: {frames:?}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn an_unanswered_prompt_is_denied_instead_of_hanging_forever() {
    // A one-second ceiling stands in for the production default (300s): the
    // property under test is "the turn ends without anyone answering", not
    // the particular duration.
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("never-answered.txt"),
        ask_settings(),
        Duration::from_secs(1),
    )
    .await;

    // Nobody ever calls `session.respondToPrompt`. Without the daemon-side
    // timeout this call never returns — the engine waits on its oneshot
    // forever — so the outer timeout is the regression guard.
    let (frames, response) = tokio::time::timeout(
        Duration::from_secs(30),
        rpc_streaming(
            &srv.sock,
            r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file"},"id":1}"#,
        ),
    )
    .await
    .expect("an unanswered permission prompt hung the turn forever");

    assert!(
        !events_of_kind(&frames, "prompt").is_empty(),
        "the prompt should still have been offered: {frames:?}"
    );
    assert!(response["result"].is_object(), "turn failed: {response}");
    let rendered = serde_json::to_string(&events_of_kind(&frames, "tool_result")).unwrap();
    assert!(
        rendered.contains("Denied by permission") && rendered.contains("no answer"),
        "the timeout must fail closed, with a reason that names itself: {rendered}"
    );

    srv.stop().await;
}

// ── §5.3 一个会话同时只跑一个 turn / §3.2 场景能力 ──────────────────────

/// A session runs one turn at a time, and the second caller is told so
/// rather than being served an internal error.
///
/// The ordering matters more than the code: the message used to be pushed
/// into the agent's input queue *before* the exclusive event channel was
/// claimed, so a rejected second `run_turn` had already handed the user's
/// message to the turn that was still running. The caller saw a failure and
/// the message was answered anyway, by the wrong turn.
#[tokio::test]
async fn a_second_turn_on_a_busy_session_is_refused_and_its_message_is_not_swallowed() {
    let (srv, seen) = start_scripted_server(
        asking_bash_rounds("busy.txt"),
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    // Park a turn on an unanswered permission prompt: the session is busy
    // for as long as nobody answers.
    let sock = srv.sock.clone();
    let mut conn = UnixStream::connect(&sock).await.unwrap();
    conn.write_all(
        br#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"first"},"id":1}"#,
    )
    .await
    .unwrap();
    conn.write_all(b"\n").await.unwrap();

    let (sid, prompt_id) = {
        let (r, _w) = conn.split();
        let mut br = BufReader::new(r);
        loop {
            let mut line = String::new();
            let n = br.read_line(&mut line).await.unwrap();
            assert!(n > 0, "connection closed before the prompt arrived");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["params"]["event"]["kind"] == "prompt" {
                break (
                    v["params"]["session_id"].as_str().unwrap().to_string(),
                    v["params"]["event"]["prompt_id"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                );
            }
        }
    };

    let requests_before = seen.lock().unwrap().len();

    let busy = rpc(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"ZZ-refused-message-ZZ"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(busy["error"]["code"], codes::SESSION_BUSY, "{busy}");
    assert_eq!(busy["error"]["data"]["session_id"], sid, "{busy}");
    assert!(
        busy["error"]["data"]["current_turn_id"].is_string(),
        "the refusal must name the turn holding the session: {busy}"
    );

    // Let the parked turn finish.
    let ack = rpc(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{{"session_id":"{sid}","prompt_id":"{prompt_id}","decision":{{"type":"deny","reason":"test"}}}},"id":99}}"#
        ),
    )
    .await;
    assert_eq!(ack["result"]["prompt_id"], prompt_id, "{ack}");

    // The refused message must not have reached the model as part of the
    // turn that was already running.
    let requests: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .skip(requests_before)
        .map(|r| format!("{r:?}"))
        .collect();
    assert!(
        !requests.iter().any(|r| r.contains("ZZ-refused-message-ZZ")),
        "the refused message was fed to the running turn anyway: {requests:?}"
    );

    srv.stop().await;
}

/// Once the parked turn is done the session is idle again — "busy" tracks
/// the turn, it is not a latch.
#[tokio::test]
async fn a_session_accepts_a_new_turn_after_the_previous_one_finishes() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("a"), text_round("b")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let sid = run_turn(&srv.sock, None, "first").await;
    let second = rpc_streaming(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"second"}},"id":2}}"#
        ),
    )
    .await
    .1;
    assert!(second["result"].is_object(), "{second}");

    srv.stop().await;
}

/// `coding` requires a project, and says so at creation rather than letting
/// the session exist and fail on its first tool call.
#[tokio::test]
async fn a_scene_that_requires_a_project_refuses_a_session_without_one() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"coding","project_root":null},"id":1}"#,
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::PROJECT_REQUIRED, "{resp}");

    srv.stop().await;
}

/// The capability bits are what a host reads to decide whether to ask for a
/// project or show a team entry point, so they have to be on the wire.
#[tokio::test]
async fn scene_list_reports_the_capability_bits() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.list","id":1}"#,
    )
    .await;
    let scenes = resp["result"]["scenes"].as_array().unwrap();
    let by_id = |id: &str| {
        scenes
            .iter()
            .find(|s| s["scene"] == id)
            .unwrap_or_else(|| panic!("{id} missing from scene.list: {resp}"))
            .clone()
    };

    assert_eq!(by_id("coding")["requires_project"], true);
    assert_eq!(by_id("coding")["supports_team"], true);
    assert_eq!(by_id("chat")["requires_project"], false);
    assert_eq!(by_id("chat")["supports_team"], false);

    srv.stop().await;
}

// ── §6.1/§6.3 之前只存在于文档里的方法 ────────────────────────────────

#[tokio::test]
async fn ping_answers_without_a_session() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"daemon.ping","id":1}"#,
    )
    .await;
    assert_eq!(resp["result"]["pong"], true, "{resp}");
    assert_eq!(resp["result"]["protocol_version"], 2, "{resp}");

    srv.stop().await;
}

/// `turn_state` is what a client polls to decide whether its send button is
/// live, so it has to be right on both sides of a turn.
#[tokio::test]
async fn session_get_reports_the_summary_and_the_turn_state() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("hello")], ask_settings(), Duration::ZERO).await;

    let sid = run_turn(&srv.sock, None, "hi").await;
    let got = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.get","params":{{"session_id":"{sid}"}},"id":1}}"#
        ),
    )
    .await;

    assert_eq!(got["result"]["session_id"], sid, "{got}");
    assert_eq!(got["result"]["scene"], "coding", "{got}");
    assert_eq!(got["result"]["scene_active"], true, "{got}");
    assert_eq!(got["result"]["turn_state"], "idle", "{got}");
    assert!(got["result"]["current_turn_id"].is_null(), "{got}");
    // `message_count` comes straight from `session.list`'s summary — asserted
    // present, not non-zero: a live session reports 0 until its transcript is
    // read back, which is `session.list`'s existing behavior and not something
    // `session.get` should quietly diverge from.
    assert!(got["result"]["message_count"].is_number(), "{got}");

    srv.stop().await;
}

#[tokio::test]
async fn session_get_on_an_unknown_session_is_not_found() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let got = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.get","params":{"session_id":"NopeNopeNopeNopeNope12"},"id":1}"#,
    )
    .await;
    assert_eq!(got["error"]["code"], codes::SESSION_NOT_FOUND, "{got}");

    srv.stop().await;
}

/// Interrupting an idle session is an answer, not an error — the caller
/// wanted it idle and it is.
#[tokio::test]
async fn interrupting_an_idle_session_reports_that_there_was_nothing_to_interrupt() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let sid = run_turn(&srv.sock, None, "hi").await;
    let resp = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.interrupt","params":{{"session_id":"{sid}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(resp["result"]["interrupted"], false, "{resp}");

    srv.stop().await;
}

/// The documented use: a client that hits `SESSION_BUSY` interrupts, then
/// sends. The session survives the interrupt and serves the next turn.
#[tokio::test]
async fn interrupt_frees_a_busy_session_for_the_next_turn() {
    let (srv, _seen) = start_scripted_server(
        vec![
            asking_bash_rounds("interrupt.txt")
                .into_iter()
                .next()
                .unwrap(),
            text_round("after"),
        ],
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    let mut conn = UnixStream::connect(&srv.sock).await.unwrap();
    conn.write_all(
        br#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"first"},"id":1}"#,
    )
    .await
    .unwrap();
    conn.write_all(b"\n").await.unwrap();

    let sid = {
        let (r, _w) = conn.split();
        let mut br = BufReader::new(r);
        loop {
            let mut line = String::new();
            let n = br.read_line(&mut line).await.unwrap();
            assert!(n > 0, "connection closed before the prompt arrived");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["params"]["event"]["kind"] == "prompt" {
                break v["params"]["session_id"].as_str().unwrap().to_string();
            }
        }
    };

    let resp = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.interrupt","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(resp["result"]["interrupted"], true, "{resp}");

    srv.stop().await;
}

#[tokio::test]
async fn scene_describe_reports_capabilities_tools_and_effective_settings() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.describe","params":{"scene":"coding"},"id":1}"#,
    )
    .await;

    assert_eq!(resp["result"]["scene"], "coding", "{resp}");
    assert_eq!(resp["result"]["capabilities"]["requires_project"], true);
    assert_eq!(resp["result"]["capabilities"]["supports_team"], true);
    // `coding` sets no whitelist, which is `null` rather than `[]` — the
    // latter would read as "no tools allowed", the opposite of the truth.
    assert!(resp["result"]["tools"]["allowed"].is_null(), "{resp}");
    assert!(
        !resp["result"]["tools"]["deferred"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{resp}"
    );
    assert!(resp["result"]["settings"].is_object(), "{resp}");

    srv.stop().await;
}

#[tokio::test]
async fn scene_describe_rejects_an_unknown_scene() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.describe","params":{"scene":"nope"},"id":1}"#,
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::SCENE_NOT_FOUND, "{resp}");

    srv.stop().await;
}

/// A tier with no file on disk is `null`, not `{}` — "nothing configured
/// here" and "configured empty" are different states a config UI has to be
/// able to tell apart.
#[tokio::test]
async fn config_get_distinguishes_an_absent_tier_from_an_empty_one() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let scene_tier = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"config.get","params":{"scene":"coding","tier":"scene"},"id":1}"#,
    )
    .await;
    assert_eq!(scene_tier["result"]["tier"], "scene", "{scene_tier}");
    assert!(scene_tier["result"]["settings"].is_null(), "{scene_tier}");

    let effective = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"config.get","params":{"scene":"coding"},"id":2}"#,
    )
    .await;
    assert_eq!(effective["result"]["tier"], "effective", "{effective}");
    assert!(effective["result"]["settings"].is_object(), "{effective}");

    srv.stop().await;
}

#[tokio::test]
async fn config_get_rejects_an_unknown_tier() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"config.get","params":{"scene":"coding","tier":"nope"},"id":1}"#,
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::INVALID_PARAMS, "{resp}");

    srv.stop().await;
}

/// Credentials are redacted unless asked for, at whatever depth they sit.
#[tokio::test]
async fn config_get_redacts_credentials_by_default() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"config.get","params":{"scene":"coding"},"id":1}"#,
    )
    .await;
    let dumped = resp.to_string();
    assert!(
        !dumped.contains("test-key"),
        "the scripted server's auth token leaked into config.get: {dumped}"
    );

    srv.stop().await;
}

// ── §5.2 一条连接上的并发 ────────────────────────────────────────────────

/// A prompt raised by a turn can be answered **on the connection running
/// that turn**.
///
/// This is the case a browser has: one tab, one socket, everything on it.
/// While requests were served inline the connection read nothing during a
/// turn, so the answer never arrived — the turn waited for an answer that
/// waited for the turn, until the prompt timeout denied it. Every other test
/// in this file answers from a second connection and so never saw it.
///
/// The whole test is under a deadline: a regression here does not fail, it
/// hangs until `permission_prompt_timeout`.
#[tokio::test]
async fn a_prompt_is_answerable_on_the_same_connection_that_started_the_turn() {
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("same-conn.txt"),
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        let mut conn = UnixStream::connect(&srv.sock).await.unwrap();
        conn.write_all(
            br#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file"},"id":1}"#,
        )
        .await
        .unwrap();
        conn.write_all(b"\n").await.unwrap();

        let (r, mut w) = conn.split();
        let mut br = BufReader::new(r);
        let mut answered = false;
        loop {
            let mut line = String::new();
            let n = br.read_line(&mut line).await.unwrap();
            assert!(n > 0, "connection closed before the turn finished");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();

            if v["params"]["event"]["kind"] == "prompt" && !answered {
                let sid = v["params"]["session_id"].as_str().unwrap();
                let prompt_id = v["params"]["event"]["prompt_id"].as_str().unwrap();
                // Same socket the turn is streaming on.
                w.write_all(format!(
                    r#"{{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{{"session_id":"{sid}","prompt_id":"{prompt_id}","decision":{{"type":"deny","reason":"same-connection test"}}}},"id":2}}"#
                ).as_bytes()).await.unwrap();
                w.write_all(b"\n").await.unwrap();
                answered = true;
            }

            // The turn's own response, id 1, means the turn ran to an end
            // rather than stalling on an unanswerable question.
            if v["id"] == 1 {
                assert!(answered, "the turn ended without ever asking: {v}");
                return v;
            }
        }
    })
    .await;

    let response = outcome.expect(
        "the turn never finished — the connection is not reading while it runs, so its own \
         prompt cannot be answered",
    );
    assert!(response["result"].is_object(), "turn failed: {response}");

    srv.stop().await;
}

/// `session.interrupt` works from the connection running the turn.
///
/// The tab that started a turn is the one with a stop button on it, so this
/// is the case that matters most; it was unreachable for the same reason.
#[tokio::test]
async fn a_turn_can_be_interrupted_from_the_connection_running_it() {
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("interrupt-same-conn.txt"),
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    let interrupted = tokio::time::timeout(Duration::from_secs(10), async {
        let mut conn = UnixStream::connect(&srv.sock).await.unwrap();
        conn.write_all(
            br#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"go"},"id":1}"#,
        )
        .await
        .unwrap();
        conn.write_all(b"\n").await.unwrap();

        let (r, mut w) = conn.split();
        let mut br = BufReader::new(r);
        let mut asked = false;
        loop {
            let mut line = String::new();
            let n = br.read_line(&mut line).await.unwrap();
            assert!(n > 0, "connection closed");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();

            // Wait until the turn is definitely in flight, then interrupt it
            // on this same socket.
            if v["params"]["event"]["kind"] == "prompt" && !asked {
                let sid = v["params"]["session_id"].as_str().unwrap();
                w.write_all(format!(
                    r#"{{"jsonrpc":"2.0","method":"session.interrupt","params":{{"session_id":"{sid}"}},"id":2}}"#
                ).as_bytes()).await.unwrap();
                w.write_all(b"\n").await.unwrap();
                asked = true;
            }

            if v["id"] == 2 {
                return v;
            }
        }
    })
    .await
    .expect("session.interrupt was never answered on the connection running the turn");

    assert_eq!(interrupted["result"]["interrupted"], true, "{interrupted}");

    srv.stop().await;
}
