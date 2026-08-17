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
//! and refusing an oversized frame. The mid-turn permission prompt — the
//! bidirectional case WebSocket was chosen for — is covered over the Unix
//! socket in `daemon/tests/session_lifecycle.rs` and not yet over the other
//! two.
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

struct Harness {
    server: Arc<DaemonServer>,
    sock: std::path::PathBuf,
    tcp: std::net::SocketAddr,
    ws: std::net::SocketAddr,
    _dir: tempfile::TempDir,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Harness {
    async fn start(chunks: &[&str], gap: Duration) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        let client: Arc<dyn AnthropicClient> = Arc::new(DrippingClient {
            chunks: chunks.iter().map(|c| c.to_string()).collect(),
            gap,
            seen: Arc::new(StdMutex::new(0)),
        });
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
            Arc::new(AllowAllPermission),
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
    let h = Harness::start(&["hello"], Duration::ZERO).await;

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

    let h = Harness::start(&["one ", "two ", "three ", "four ", "five"], CHUNK_GAP).await;

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
    let h = Harness::start(&["a", "b", "c"], Duration::from_millis(5)).await;

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
    let h = Harness::start(&["x"], Duration::ZERO).await;

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
