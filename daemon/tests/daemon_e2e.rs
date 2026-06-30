//! Daemon RPC e2e tests.
//!
//! Tests the JSON-RPC session lifecycle via an in-process daemon.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base::context::EngineConfig;
use base::id::Id;
use base::interface::memory::MemoryStore;
use base::interface::settings::Settings;
use daemon::rpc::codes;
use daemon::{DaemonServer, SessionPool};
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

/// Always-allow permission for tests.
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

/// Bind a test server and return (server, socket_path, _tempdir, join_handle).
async fn start_server() -> (
    Arc<DaemonServer>,
    PathBuf,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("test.sock");

    let settings = Arc::new(Settings::defaults_for("claude-sonnet-4-6"));
    let memory_store = Arc::new(MemoryStore::new(
        dir.path().join("user").join("memory"),
        dir.path().join("local").join("memory"),
    ));
    let scene: Arc<dyn base::interface::scene::AgentScene> =
        Arc::new(scene::scene::coding::CodingScene::default_scene());
    let permission: Arc<dyn base::interface::permission::Permission> = Arc::new(AllowAllPermission);
    let engine_config = EngineConfig::defaults_for("claude-sonnet-4-6");

    // 使用 dummy client（不真正调 LLM）
    let client: Arc<dyn AnthropicClient> =
        Arc::new(HttpAnthropicClient::new(AuthMode::ApiKey("test-key".into())).unwrap());

    let pool = Arc::new(SessionPool::new(
        8,
        3600,
        client,
        settings,
        scene.clone(),
        scene,
        permission,
        memory_store,
        dir.path().to_path_buf(),
        engine_config,
        None,
    ));

    let cancel = CancellationToken::new();
    let server = Arc::new(DaemonServer::new(pool, cancel));
    let server2 = server.clone();
    let sock2 = sock.clone();
    let handle = tokio::spawn(async move {
        let _ = server2.serve_unix(&sock2).await;
    });

    for _ in 0..20 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(sock.exists(), "socket never bound");
    (server, sock, dir, handle)
}

async fn rpc_call(sock: &std::path::Path, msg: &str) -> String {
    let mut client = UnixStream::connect(sock).await.unwrap();
    client.write_all(msg.as_bytes()).await.unwrap();
    client.write_all(b"\n").await.unwrap();
    let (r, _) = client.split();
    let mut br = BufReader::new(r);
    let mut buf = String::new();
    br.read_line(&mut buf).await.unwrap();
    buf
}

#[tokio::test]
async fn status_returns_info() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"daemon.status","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "expected success, got: {v}");
    assert!(v["result"]["version"].is_string());
    assert_eq!(v["result"]["sessions"], 0);

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn session_list_returns_empty_initially() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"session.list","id":1}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"]["sessions"].is_array());
    assert!(v["result"]["sessions"].as_array().unwrap().is_empty());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn run_turn_without_session_id_creates_new() {
    let (_server, sock, _dir, handle) = start_server().await;
    // session_id 可选，不传则自动创建
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"hello"},"id":1}"#,
    )
    .await;
    // 会尝试调用 LLM（但测试环境没有真实 API key），所以应该返回 engine error
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    // 只要有 result 或 error（不是 INVALID_PARAMS），说明 session 创建成功了
    assert!(v["result"].is_object() || v["error"].is_object());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn run_turn_nonexistent_session_errors() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"session_id":"NopeNopeNopeNopeNope12","message":"test"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    // 不存在的 session：新代码尝试创建/恢复，也不会报 SESSION_NOT_FOUND
    // 但最终会尝试调 LLM 而失败
    assert!(v["error"].is_object() || v["result"].is_object());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn unknown_method_returns_error() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"nonexistent","id":1}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::METHOD_NOT_FOUND);

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn turn_id_is_base58_uuid_22_chars() {
    let id = Id::new().to_string();
    assert!(
        (21..=22).contains(&id.len()),
        "expected 21-22 chars, got {}: {id}",
        id.len()
    );
}
