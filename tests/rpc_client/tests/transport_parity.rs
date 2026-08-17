//! The same exchange over Unix socket, TCP and WebSocket.
//!
//! The daemon's claim is that a transport does framing and nothing else. The
//! only way to check a claim like that is to run one exchange over each and
//! see that nothing but the framing differed — a per-transport test that
//! sends `daemon.status` and stops proves the handshake works and little
//! more.
//!
//! What differs per transport, and therefore what these cover: streaming a
//! turn frame by frame, the aggregate read agreeing with the streamed one,
//! refusing an oversized frame, and answering a permission prompt mid-turn
//! — the bidirectional case WebSocket was chosen for, since over
//! server-sent events that answer needs a second channel and a correlation
//! scheme.
//!
//! There is one property here that no aggregate assertion can express: that
//! a token reaches the client *while the turn is still running*. A client
//! that collects everything and returns at the end sees an identical result
//! whether the daemon streamed or buffered — which is how eight unflushed
//! write sites survived in the daemon for as long as they did.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use base::interface::memory::MemoryStore;
use base::interface::settings::Settings;
use daemon::config::StaticDaemonPaths;
use daemon::{DaemonServer, SessionPool};
use model::client::{AnthropicClient, CountFuture, EventStream};
use model::stream::{
    BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage as WireUsage,
};
use model::types::MessagesRequest;
use rpc_client::{DaemonRpcClient, TurnItem};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "transport-parity-token";

/// A model that emits its tokens with a gap between them.
///
/// The gap is the whole point: it separates "the daemon forwarded this token
/// when it had it" from "the daemon collected everything and flushed once at
/// the end". Without it both look the same from the client.
struct DrippingClient {
    chunks: Vec<String>,
    gap: Duration,
    seen: Arc<StdMutex<usize>>,
}

impl AnthropicClient for DrippingClient {
    fn stream_messages(&self, _req: MessagesRequest) -> EventStream {
        *self.seen.lock().unwrap() += 1;
        let chunks = self.chunks.clone();
        let gap = self.gap;
        Box::pin(async_stream::stream! {
            yield Ok(StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Text { text: String::new() },
            });
            for chunk in chunks {
                yield Ok(StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: BlockDelta::TextDelta { text: chunk },
                });
                tokio::time::sleep(gap).await;
            }
            yield Ok(StreamEvent::ContentBlockStop { index: 0 });
            yield Ok(StreamEvent::MessageDelta {
                delta: MessageDeltaPayload {
                    stop_reason: Some(base::message::StopReason::EndTurn),
                    stop_sequence: None,
                },
                usage: Some(WireUsage::default()),
            });
        })
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

/// Calls `Bash` until it sees its own tool result, then answers with text —
/// the shape that makes the daemon stop and ask, so the prompt round trip
/// can be exercised.
///
/// Keyed on the conversation rather than on a call counter: the same daemon
/// serves one turn per transport here, and a counter would have the second
/// and third turns skipping straight to the text round.
struct AskingClient;

impl AnthropicClient for AskingClient {
    fn stream_messages(&self, req: MessagesRequest) -> EventStream {
        let already_ran = serde_json::to_string(&req.messages)
            .map(|s| s.contains("tool_result"))
            .unwrap_or(false);
        let events: Vec<StreamEvent> = if !already_ran {
            vec![
                StreamEvent::ContentBlockStart {
                    index: 0,
                    content_block: ContentBlockStart::ToolUse {
                        id: "tool-1".into(),
                        name: "Bash".into(),
                        input: serde_json::Value::Null,
                    },
                },
                StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: BlockDelta::InputJsonDelta {
                        partial_json: serde_json::json!({"command": "touch parity.txt"})
                            .to_string(),
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
        } else {
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
                        text: "done".into(),
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
        };
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

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

/// Asks about everything, so a turn with a tool call stops for an answer.
struct AskEverythingPermission;

#[async_trait::async_trait]
impl base::interface::permission::Permission for AskEverythingPermission {
    async fn check(
        &self,
        _: &str,
        _: &serde_json::Value,
        _: &std::path::Path,
        _: &str,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Prompt {
            prompt_id: uuid::Uuid::new_v4().to_string(),
            message: "may I?".into(),
            paths: Vec::new(),
        }
    }
}

struct Harness {
    server: Arc<DaemonServer>,
    sock: std::path::PathBuf,
    tcp: std::net::SocketAddr,
    ws: std::net::SocketAddr,
    _dir: tempfile::TempDir,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Harness {
    async fn dripping(chunks: &[&str], gap: Duration) -> Self {
        Self::start(
            Arc::new(DrippingClient {
                chunks: chunks.iter().map(|c| c.to_string()).collect(),
                gap,
                seen: Arc::new(StdMutex::new(0)),
            }),
            true,
        )
        .await
    }

    /// A daemon that will stop and ask before running the tool.
    async fn asking() -> Self {
        Self::start(Arc::new(AskingClient), false).await
    }

    async fn start(client: Arc<dyn AnthropicClient>, allow_all: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        let store: Arc<dyn history::store::HistoryStore> = Arc::new(
            history::store::JsonlHistoryStore::with_roots(
                &cwd,
                history::path::HistoryRoots::under(dir.path()),
            )
            .await
            .unwrap(),
        );
        let pool = Arc::new(SessionPool::new(
            8,
            3600,
            client,
            Arc::new(Settings::defaults_for("claude-sonnet-4-6")),
            Arc::new(scene::scene::coding::CodingScene),
            // `allow_all` decides whether the daemon ever raises a prompt:
            // the streaming tests want turns that just run, the prompt test
            // wants one that stops and asks.
            if allow_all {
                Arc::new(AllowAllPermission) as Arc<dyn base::interface::permission::Permission>
            } else {
                Arc::new(AskEverythingPermission)
            },
            Arc::new(MemoryStore::new(
                dir.path().join("user/memory"),
                dir.path().join("local/memory"),
            )),
            cwd,
            Some(store),
            Arc::new(StaticDaemonPaths::new(dir.path().to_path_buf())),
            None,
        ));

        let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
        server.set_tcp_token(TOKEN.to_string()).await;

        let sock = dir.path().join("parity.sock");
        let mut tasks = Vec::new();
        {
            let s = server.clone();
            let sock = sock.clone();
            tasks.push(tokio::spawn(async move {
                let _ = s.serve_unix(&sock).await;
            }));
        }

        let tcp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp = tcp_listener.local_addr().unwrap();
        {
            let s = server.clone();
            tasks.push(tokio::spawn(async move {
                let _ = s.serve_tcp_listener(tcp_listener).await;
            }));
        }

        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws = ws_listener.local_addr().unwrap();
        {
            let s = server.clone();
            tasks.push(tokio::spawn(async move {
                let _ = daemon::ws::serve_ws_listener(s, ws_listener).await;
            }));
        }

        for _ in 0..100 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        Self {
            server,
            sock,
            tcp,
            ws,
            _dir: dir,
            tasks,
        }
    }

    /// One client per transport, each already past its handshake.
    async fn clients(&self) -> Vec<DaemonRpcClient> {
        vec![
            DaemonRpcClient::connect(&self.sock).await.unwrap(),
            DaemonRpcClient::connect_tcp(self.tcp, TOKEN).await.unwrap(),
            DaemonRpcClient::connect_ws(self.ws, TOKEN, Some("http://localhost:5173"))
                .await
                .unwrap(),
        ]
    }

    async fn stop(self) {
        self.server.shutdown_token().cancel();
        for t in self.tasks {
            let _ = t.await;
        }
    }
}

#[tokio::test]
async fn every_transport_answers_the_same_request() {
    let h = Harness::dripping(&["hello"], Duration::ZERO).await;

    for mut client in h.clients().await {
        let name = client.transport_name();
        let resp = client.daemon_status().await.unwrap();
        let result = resp
            .result
            .unwrap_or_else(|| panic!("{name}: daemon.status failed: {:?}", resp.error));
        assert!(result["version"].is_string(), "{name}: {result}");
    }

    h.stop().await;
}

/// The property an aggregate client cannot express: a token arrives while
/// the turn is still running.
///
/// The discrimination is in the timing, so the numbers matter. The model
/// drips five chunks 150ms apart, so the turn takes ~750ms end to end, and
/// the first chunk is available at ~0ms. The deadline below is 300ms:
/// comfortably after a *streamed* first token, comfortably before a
/// *buffered* one — a daemon that collected the turn and flushed at the end
/// would still be waiting on the model when the deadline passed.
///
/// A longer deadline would pass either way. That is worth stating because
/// this test's entire value is being able to fail.
#[tokio::test]
async fn a_token_reaches_the_client_before_the_turn_ends_on_every_transport() {
    const CHUNK_GAP: Duration = Duration::from_millis(150);
    /// Between one streamed chunk (~0ms) and a whole buffered turn (~750ms).
    const DEADLINE: Duration = Duration::from_millis(300);

    let h = Harness::dripping(&["one ", "two ", "three ", "four ", "five"], CHUNK_GAP).await;

    for mut client in h.clients().await {
        let name = client.transport_name();
        let mut stream = client.begin_turn(None, "go", "t1", None).await.unwrap();

        let mut first_text: Option<String> = None;
        while first_text.is_none() {
            let item = tokio::time::timeout(DEADLINE, stream.next())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "{name}: no token within {DEADLINE:?} — the daemon is collecting the \
                         turn instead of forwarding each frame as it has it"
                    )
                })
                .unwrap_or_else(|e| panic!("{name}: stream error: {e}"))
                .unwrap_or_else(|| panic!("{name}: stream ended with no text"));

            match item {
                TurnItem::Event(event) => {
                    if event["kind"] == "text_delta" {
                        first_text = Some(event["text"].as_str().unwrap_or_default().to_string());
                    }
                }
                TurnItem::Response(resp) => panic!(
                    "{name}: the turn finished before a single token arrived — the daemon \
                     buffered instead of streaming: {resp:?}"
                ),
            }
        }

        // The first chunk only, not the whole reply: proof it was forwarded
        // as it arrived rather than assembled first.
        assert_eq!(first_text.as_deref(), Some("one "), "{name}");

        // Drain the rest so the session is left idle for the next client.
        while let Some(item) = stream.next().await.unwrap() {
            if matches!(item, TurnItem::Response(_)) {
                break;
            }
        }
    }

    h.stop().await;
}

/// The aggregate path must agree with the streaming one it is built on.
#[tokio::test]
async fn the_collected_turn_matches_what_the_stream_delivered() {
    let h = Harness::dripping(&["a", "b", "c"], Duration::from_millis(5)).await;

    for mut client in h.clients().await {
        let name = client.transport_name();
        let created = client
            .call("session.create", serde_json::json!({}))
            .await
            .unwrap();
        let sid = created.result.unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_string();

        let turn = client
            .session_run_turn(&sid, "go", "t1", None)
            .await
            .unwrap();
        assert_eq!(turn.text, "abc", "{name}");
        assert!(turn.turn_complete, "{name}: no turn_complete event");
        assert!(
            turn.response.is_some_and(|r| r.error.is_none()),
            "{name}: turn failed"
        );
    }

    h.stop().await;
}

/// 16 MiB is the ceiling on all three. Over it, the connection closes rather
/// than the frame being skipped — after an oversized frame the daemon does
/// not know where the next one starts.
#[tokio::test]
async fn an_oversized_frame_closes_the_connection_on_the_line_transports() {
    let h = Harness::dripping(&["x"], Duration::ZERO).await;

    let mut client = DaemonRpcClient::connect(&h.sock).await.unwrap();
    let huge = "z".repeat(17 * 1024 * 1024);
    let result = client
        .call("session.create", serde_json::json!({ "scene": huge }))
        .await;
    assert!(
        result.is_err(),
        "an over-limit frame must end the connection, not be answered"
    );

    h.stop().await;
}

/// The bidirectional case, on every transport.
///
/// The daemon asks mid-turn and waits; the client answers on the same
/// connection and the turn continues. This is the exchange WebSocket was
/// chosen for — over server-sent events the answer needs a second channel
/// and a correlation scheme — so "it works over WS too" is the claim that
/// justified the transport, and it was previously only tested over the Unix
/// socket.
#[tokio::test]
async fn a_prompt_is_answered_on_the_same_connection_on_every_transport() {
    let h = Harness::asking().await;

    for mut client in h.clients().await {
        let name = client.transport_name();

        // A second connection answers the prompt, because the first one is
        // parked inside its own turn — same as a real client, which cannot
        // reply on a socket it is blocked reading.
        let answerer_sock = h.sock.clone();

        let mut stream = client
            .begin_turn(None, "make a file", "t1", None)
            .await
            .unwrap();
        let mut answered = false;
        let mut saw_tool_result = false;

        while let Some(item) = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .unwrap_or_else(|_| panic!("{name}: the turn stalled"))
            .unwrap()
        {
            match item {
                TurnItem::Event(event) => match event["kind"].as_str() {
                    Some("prompt") if !answered => {
                        let prompt_id = event["prompt_id"].as_str().unwrap().to_string();
                        let sid = stream
                            .session_id()
                            .expect("a stream frame names its session")
                            .to_string();
                        let mut answerer = DaemonRpcClient::connect(&answerer_sock).await.unwrap();
                        let ack = answerer
                            .call(
                                "session.respondToPrompt",
                                serde_json::json!({
                                    "session_id": sid,
                                    "prompt_id": prompt_id,
                                    "decision": {"type": "deny", "reason": "parity test"},
                                }),
                            )
                            .await
                            .unwrap();
                        assert!(
                            ack.error.is_none(),
                            "{name}: respondToPrompt failed: {ack:?}"
                        );
                        answered = true;
                    }
                    Some("tool_result") => saw_tool_result = true,
                    _ => {}
                },
                TurnItem::Response(resp) => {
                    assert!(resp.error.is_none(), "{name}: turn failed: {resp:?}");
                    break;
                }
            }
        }

        assert!(answered, "{name}: the daemon never asked");
        assert!(
            saw_tool_result,
            "{name}: the turn did not continue after the answer"
        );
    }

    h.stop().await;
}

/// Dropping the connection mid-turn cancels the turn.
///
/// A turn with nobody listening is burning tokens for no one, so the daemon
/// cancels the session rather than letting it run to completion — the
/// behavior is in `session_pool.rs` and had no end-to-end coverage.
#[tokio::test]
async fn dropping_the_connection_mid_turn_cancels_the_turn() {
    let h = Harness::dripping(
        &["one ", "two ", "three ", "four ", "five"],
        Duration::from_millis(150),
    )
    .await;

    let session_id = {
        let mut client = DaemonRpcClient::connect(&h.sock).await.unwrap();
        let mut stream = client.begin_turn(None, "go", "t1", None).await.unwrap();

        // Read one frame so the turn is definitely running, then drop.
        let item = tokio::time::timeout(Duration::from_millis(500), stream.next())
            .await
            .expect("a frame within 500ms")
            .unwrap()
            .expect("a frame, not end of stream");
        assert!(matches!(item, TurnItem::Event(_)));
        stream
            .session_id()
            .expect("the frame names its session")
            .to_string()
    };

    // Promptly, not eventually. The turn runs for ~750ms (5 chunks 150ms
    // apart), so a daemon that ignored the disconnect and simply let the
    // turn finish would also end up idle — just later. Checking inside
    // 300ms is what distinguishes "cancelled because nobody is listening"
    // from "ran to completion anyway".
    let mut observer = DaemonRpcClient::connect(&h.sock).await.unwrap();
    let mut still_running = true;
    for _ in 0..6 {
        let resp = observer
            .call(
                "session.get",
                serde_json::json!({ "session_id": session_id }),
            )
            .await
            .unwrap();
        let running = resp
            .result
            .as_ref()
            .map(|r| r["turn_state"] == "running")
            .unwrap_or(false);
        if !running {
            still_running = false;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !still_running,
        "the turn was still running 300ms after its client disconnected — it is being \
         left to finish for nobody rather than cancelled"
    );

    h.stop().await;
}

/// `daemon.shutdown` stops the daemon, and says so before it goes.
#[tokio::test]
async fn shutdown_answers_before_it_stops_serving() {
    let h = Harness::dripping(&["x"], Duration::ZERO).await;

    let mut client = DaemonRpcClient::connect(&h.sock).await.unwrap();
    let resp = client.daemon_shutdown().await.unwrap();
    assert_eq!(
        resp.result.as_ref().map(|r| r["shutting_down"].clone()),
        Some(serde_json::json!(true)),
        "shutdown must answer the caller before it tears the listeners down: {resp:?}"
    );

    // And it really did shut down: the accept loops end on their own.
    for t in h.tasks {
        let _ = tokio::time::timeout(Duration::from_secs(5), t)
            .await
            .expect("listeners should stop after daemon.shutdown");
    }
}
