//! WebSocket transport e2e tests.
//!
//! The transport's whole claim is that it changes framing and nothing else,
//! so what is worth testing is the two places it could fail that claim: the
//! handshake (same token check as TCP, or the browser gets in with weaker
//! credentials) and the Origin check (which has no counterpart on the other
//! transports, because only a browser is made to connect by a third party).

use std::sync::Arc;

use base::interface::memory::MemoryStore;
use base::interface::settings::Settings;
use daemon::config::StaticDaemonPaths;
use daemon::rpc::codes;
use daemon::{DaemonServer, SessionPool};
use futures::{SinkExt, StreamExt};
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

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

async fn start_ws_server(
    token: &str,
) -> (
    Arc<DaemonServer>,
    std::net::SocketAddr,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::tempdir().unwrap();

    let settings = Arc::new(Settings::defaults_for("claude-sonnet-4-6"));
    let memory_store = Arc::new(MemoryStore::new(
        dir.path().join("user").join("memory"),
        dir.path().join("local").join("memory"),
    ));
    let scene: Arc<dyn base::interface::scene::AgentScene> =
        Arc::new(scene::scene::coding::CodingScene);
    let permission: Arc<dyn base::interface::permission::Permission> = Arc::new(AllowAllPermission);
    let client: Arc<dyn AnthropicClient> =
        Arc::new(HttpAnthropicClient::new(AuthMode::ApiKey("test-key".into())).unwrap());
    let paths: Arc<dyn daemon::config::DaemonPaths> =
        Arc::new(StaticDaemonPaths::new(dir.path().to_path_buf()));

    let pool = Arc::new(SessionPool::new(
        8,
        3600,
        client,
        settings,
        scene,
        permission,
        memory_store,
        dir.path().to_path_buf(),
        None,
        paths,
        None,
    ));

    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
    server.set_tcp_token(token.to_string()).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let s = server.clone();
    let handle = tokio::spawn(async move {
        let _ = daemon::ws::serve_ws_listener(s, listener).await;
    });

    (server, addr, dir, handle)
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(
    addr: std::net::SocketAddr,
    origin: Option<&str>,
) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://{addr}/").into_client_request().unwrap();
    if let Some(origin) = origin {
        req.headers_mut().insert("origin", origin.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
}

async fn send(ws: &mut Ws, json: &str) {
    ws.send(Message::Text(json.into())).await.unwrap();
}

async fn recv(ws: &mut Ws) -> serde_json::Value {
    match ws.next().await {
        Some(Ok(Message::Text(t))) => serde_json::from_str(&t).unwrap(),
        other => panic!("expected a text frame, got {other:?}"),
    }
}

async fn handshake(ws: &mut Ws, token: &str) -> serde_json::Value {
    send(
        ws,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"daemon.auth","params":{{"token":"{token}"}},"id":0}}"#
        ),
    )
    .await;
    recv(ws).await
}

#[tokio::test]
async fn the_correct_token_opens_the_connection_and_requests_dispatch() {
    let (server, addr, _dir, handle) = start_ws_server("s3cr3t").await;

    let mut ws = connect(addr, Some("http://localhost:5173")).await.unwrap();
    let v = handshake(&mut ws, "s3cr3t").await;
    assert_eq!(v["result"]["authenticated"], true, "resp: {v}");

    send(
        &mut ws,
        r#"{"jsonrpc":"2.0","method":"daemon.status","id":1}"#,
    )
    .await;
    let v = recv(&mut ws).await;
    assert!(v["result"]["version"].is_string(), "resp: {v}");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn a_wrong_token_is_refused_and_the_connection_closes() {
    let (server, addr, _dir, handle) = start_ws_server("s3cr3t").await;

    let mut ws = connect(addr, None).await.unwrap();
    let v = handshake(&mut ws, "wrong").await;
    assert_eq!(v["error"]["code"], codes::UNAUTHORIZED, "resp: {v}");

    // Closed, not merely ignored: a page that guessed wrong must not be left
    // holding a connection it can keep trying on.
    match ws.next().await {
        None | Some(Ok(Message::Close(_))) => {}
        other => panic!("connection should be closed after a refused handshake, got {other:?}"),
    }

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// Skipping the handshake must not work either — otherwise the token is
/// optional in practice.
#[tokio::test]
async fn a_request_before_the_handshake_is_refused() {
    let (server, addr, _dir, handle) = start_ws_server("s3cr3t").await;

    let mut ws = connect(addr, None).await.unwrap();
    send(
        &mut ws,
        r#"{"jsonrpc":"2.0","method":"daemon.status","id":1}"#,
    )
    .await;
    let v = recv(&mut ws).await;
    assert_eq!(v["error"]["code"], codes::UNAUTHORIZED, "resp: {v}");
    assert!(
        v["result"].is_null(),
        "daemon.status must not have run: {v}"
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// The case loopback binding does not cover: any page the user visits can open
/// a WebSocket to 127.0.0.1, and the browser will have told us where it came
/// from.
#[tokio::test]
async fn an_upgrade_from_a_foreign_origin_is_refused() {
    let (server, addr, _dir, handle) = start_ws_server("s3cr3t").await;

    let err = connect(addr, Some("https://evil.example"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, tokio_tungstenite::tungstenite::Error::Http(_)),
        "the upgrade itself should fail, got {err:?}"
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A CLI or an editor sends no `Origin` at all; the header is evidence about
/// browsers and its absence is not evidence about anything.
#[tokio::test]
async fn a_client_without_an_origin_header_still_connects() {
    let (server, addr, _dir, handle) = start_ws_server("s3cr3t").await;

    let mut ws = connect(addr, None).await.unwrap();
    let v = handshake(&mut ws, "s3cr3t").await;
    assert_eq!(v["result"]["authenticated"], true, "resp: {v}");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// Without a token this transport would be reachable, unauthenticated, by
/// every process on the machine and every page in the browser. It has to fail
/// loudly rather than serve.
#[tokio::test]
async fn serving_without_a_token_fails_instead_of_refusing_everyone_forever() {
    let (server, _addr, _dir, handle) = start_ws_server("s3cr3t").await;
    server.shutdown_token().cancel();
    let _ = handle.await;

    let dir = tempfile::tempdir().unwrap();
    let settings = Arc::new(Settings::defaults_for("claude-sonnet-4-6"));
    let memory_store = Arc::new(MemoryStore::new(
        dir.path().join("user").join("memory"),
        dir.path().join("local").join("memory"),
    ));
    let client: Arc<dyn AnthropicClient> =
        Arc::new(HttpAnthropicClient::new(AuthMode::ApiKey("test-key".into())).unwrap());
    let pool = Arc::new(SessionPool::new(
        8,
        3600,
        client,
        settings,
        Arc::new(scene::scene::coding::CodingScene),
        Arc::new(AllowAllPermission),
        memory_store,
        dir.path().to_path_buf(),
        None,
        Arc::new(StaticDaemonPaths::new(dir.path().to_path_buf())),
        None,
    ));
    let untokened = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();

    let err = daemon::ws::serve_ws_listener(untokened, listener)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("token"), "error: {err}");
}
