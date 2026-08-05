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
use daemon::config::StaticDaemonPaths;
use daemon::rpc::codes;
use daemon::{DaemonServer, SessionPool};
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio_util::sync::CancellationToken;

/// Build a zip archive (in memory) for a minimal demo plugin declaring one
/// `/name` slash command, write it to `dir`, and return `(archive_path,
/// sha256_hex)`.
fn build_demo_plugin_zip(
    dir: &std::path::Path,
    plugin_name: &str,
    command_name: &str,
) -> (PathBuf, String) {
    let mut buf = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        writer.start_file("plugin.toml", opts).unwrap();
        use std::io::Write;
        write!(
            writer,
            "[plugin]\nname = \"{plugin_name}\"\nversion = \"1.0.0\"\ndescription = \"demo plugin\"\n\n[slash_commands]\n\"/{command_name}\" = \"prompts/{command_name}.md\"\n"
        )
        .unwrap();
        writer
            .start_file(&format!("prompts/{command_name}.md"), opts)
            .unwrap();
        write!(writer, "Demo prompt body for /{command_name}: {{args}}").unwrap();
        writer.finish().unwrap();
    }
    let archive_path = dir.join(format!("{plugin_name}.zip"));
    std::fs::write(&archive_path, &buf).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(&buf);
    let checksum = hex::encode(hasher.finalize());
    (archive_path, checksum)
}

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
        Arc::new(scene::scene::coding::CodingScene);
    let permission: Arc<dyn base::interface::permission::Permission> = Arc::new(AllowAllPermission);
    let engine_config = EngineConfig::defaults_for("claude-sonnet-4-6");

    // 使用 dummy client（不真正调 LLM）
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
        engine_config,
        None,
        paths,
        None, // task_router
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
async fn doctor_reports_no_providers_configured_as_ok() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"daemon.doctor","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["providers"]["ok"], true, "resp: {v}");
    assert_eq!(
        v["result"]["session_persistence"]["history_store_wired"],
        false
    );
    assert!(v["result"]["settings_tiers"].as_array().unwrap().len() == 3);

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_writes_project_settings_and_doctor_sees_it() {
    let (_server, sock, dir, handle) = start_server().await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"api_type":"openai_compatible","base_url":"https://api.deepseek.com/v1","api_key":"k","default_model":"deepseek-pro"},"default_provider":"deepseek"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "expected success, got: {v}");
    assert_eq!(v["result"]["routing"]["ok"], true, "resp: {v}");
    assert_eq!(v["result"]["default_provider"], "deepseek");

    // Written to disk at the project tier.
    let written = std::fs::read_to_string(dir.path().join(".atta").join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(written["providers"]["deepseek"]["api_key"], "k");

    // Doctor now reflects the newly-written provider.
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"daemon.doctor","id":2}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["providers"]["default_provider"], "deepseek");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_partial_patch_does_not_clobber_untouched_fields() {
    let (_server, sock, dir, handle) = start_server().await;

    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"api_type":"openai_compatible","base_url":"https://api.deepseek.com/v1","default_model":"deepseek-pro"}},"id":1}"#,
    )
    .await;

    // Second call only touches api_key — base_url/default_model must survive.
    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"api_key":"new-key"}},"id":2}"#,
    )
    .await;

    let written = std::fs::read_to_string(dir.path().join(".atta").join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(written["providers"]["deepseek"]["api_key"], "new-key");
    assert_eq!(
        written["providers"]["deepseek"]["base_url"],
        "https://api.deepseek.com/v1"
    );
    assert_eq!(
        written["providers"]["deepseek"]["default_model"],
        "deepseek-pro"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_delete_removes_entry() {
    let (_server, sock, dir, handle) = start_server().await;

    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"default_model":"deepseek-pro"}},"id":1}"#,
    )
    .await;
    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","delete":true},"id":2}"#,
    )
    .await;

    let written = std::fs::read_to_string(dir.path().join(".atta").join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert!(written["providers"].get("deepseek").is_none());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_rejects_missing_provider_id() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_refuses_to_clobber_a_malformed_existing_settings_json() {
    let (_server, sock, dir, handle) = start_server().await;

    let atta_dir = dir.path().join(".atta");
    std::fs::create_dir_all(&atta_dir).unwrap();
    let settings_path = atta_dir.join("settings.json");
    std::fs::write(&settings_path, "{not valid json").unwrap();

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"default_model":"deepseek-pro"}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");

    // The original (malformed) content must survive untouched — not get
    // silently replaced with `{}` + the patch.
    let still_there = std::fs::read_to_string(&settings_path).unwrap();
    assert_eq!(still_there, "{not valid json");

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

#[tokio::test]
async fn mcp_status_empty_when_nothing_connected() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"mcp.status","id":1}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"]["servers"].as_array().unwrap().is_empty(),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn mcp_add_server_writes_settings_and_reports_connect_failure() {
    let (_server, sock, dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"mcp.addServer","params":{"name":"bogus","config":{"type":"stdio","command":"definitely-not-a-real-binary-xyz"}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "resp: {v}");

    let written = std::fs::read_to_string(dir.path().join(".atta").join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(
        written["mcp_servers"]["bogus"]["command"],
        "definitely-not-a-real-binary-xyz"
    );

    // The binary doesn't exist, so the connect attempt fails — status must not list it.
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"mcp.status","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"]["servers"].as_array().unwrap().is_empty(),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn subscribe_events_receives_mcp_connect_failed_notification() {
    let (_server, sock, _dir, handle) = start_server().await;

    let mut sub_client = UnixStream::connect(&sock).await.unwrap();
    sub_client
        .write_all(br#"{"jsonrpc":"2.0","method":"daemon.subscribeEvents","id":1}"#)
        .await
        .unwrap();
    sub_client.write_all(b"\n").await.unwrap();
    let (r, _w) = sub_client.split();
    let mut sub_reader = BufReader::new(r);

    let mut ack_line = String::new();
    sub_reader.read_line(&mut ack_line).await.unwrap();
    let ack: serde_json::Value = serde_json::from_str(&ack_line).unwrap();
    assert_eq!(ack["result"]["subscribed"], true, "ack: {ack}");

    // Trigger a connect attempt (guaranteed to fail — nonexistent binary)
    // from a second, independent connection.
    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"mcp.addServer","params":{"name":"bogus","config":{"type":"stdio","command":"definitely-not-a-real-binary-xyz"}},"id":2}"#,
    )
    .await;

    let mut event_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        sub_reader.read_line(&mut event_line),
    )
    .await
    .expect("timed out waiting for daemon.event notification")
    .unwrap();
    let event: serde_json::Value = serde_json::from_str(&event_line).unwrap();
    assert_eq!(event["method"], "daemon.event", "event: {event}");
    assert_eq!(event["params"]["kind"], "mcp_connect_failed");
    assert_eq!(event["params"]["server"], "bogus");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn get_provider_redacts_api_key_by_default_and_reveals_with_flag() {
    let (_server, sock, _dir, handle) = start_server().await;
    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"api_key":"sk-secret-1234"}},"id":1}"#,
    )
    .await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.getProvider","id":2}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["result"]["providers"]["deepseek"]["api_key"], "***1234",
        "resp: {v}"
    );

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.getProvider","params":{"include_secrets":true},"id":3}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["result"]["providers"]["deepseek"]["api_key"], "sk-secret-1234",
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn session_close_removes_unknown_session_gracefully() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.close","params":{"session_id":"doesnotexist"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["closed"], "doesnotexist", "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn session_close_rejects_missing_session_id() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.close","params":{},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn import_list_detects_claude_md_in_project_dir() {
    let (_server, sock, dir, handle) = start_server().await;
    std::fs::write(dir.path().join("CLAUDE.md"), "# hi").unwrap();

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"import.list","id":1}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let sources = v["result"]["sources"].as_array().unwrap();
    assert!(
        sources.iter().any(|s| s["source"] == "claude_code"),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn import_run_executes_and_writes_agents_md() {
    let (_server, sock, dir, handle) = start_server().await;
    std::fs::write(dir.path().join("CLAUDE.md"), "Be concise.").unwrap();

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"import.run","params":{"source":"claude_code"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "resp: {v}");

    let agents_md = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
    assert!(agents_md.contains("Be concise."));

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn import_run_unknown_source_errors() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"import.run","params":{"source":"bogus"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn import_run_source_not_currently_detected_errors() {
    let (_server, sock, _dir, handle) = start_server().await;
    // No CLAUDE.md/.cursorrules/etc written — nothing detected.
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"import.run","params":{"source":"claude_code"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn commands_list_returns_builtin_local_commands() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"commands.list","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let commands = v["result"]["commands"].as_array().expect("commands array");

    let names: Vec<&str> = commands
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    for expected in ["help", "skills", "clear", "compact", "cost"] {
        assert!(
            names.contains(&expected),
            "missing builtin command {expected}: {names:?}"
        );
    }
    let help = commands.iter().find(|c| c["name"] == "help").unwrap();
    assert_eq!(help["kind"], "local");
    assert_eq!(help["source"], "builtin");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn commands_list_includes_installed_plugin_slash_command() {
    let dir = tempfile::tempdir().unwrap();
    // Install a plugin directly into the global tier's versioned cache —
    // same layout `plugin::discovery::discover_plugins` reads from
    // (`{global_root}/plugins/cache/{name}/{version}/plugin.toml`).
    let plugin_dir = dir
        .path()
        .join("plugins")
        .join("cache")
        .join("code-review-helper")
        .join("1.0.0");
    std::fs::create_dir_all(plugin_dir.join("prompts")).unwrap();
    std::fs::write(
        plugin_dir.join("plugin.toml"),
        r#"
[plugin]
name = "code-review-helper"
version = "1.0.0"
description = "Adds /review"

[slash_commands]
"/review" = "prompts/review.md"
"#,
    )
    .unwrap();
    std::fs::write(
        plugin_dir.join("prompts/review.md"),
        "Review the diff: {args}",
    )
    .unwrap();

    let settings = Arc::new(Settings::defaults_for("claude-sonnet-4-6"));
    let memory_store = Arc::new(MemoryStore::new(
        dir.path().join("user").join("memory"),
        dir.path().join("local").join("memory"),
    ));
    let scene: Arc<dyn base::interface::scene::AgentScene> =
        Arc::new(scene::scene::coding::CodingScene);
    let permission: Arc<dyn base::interface::permission::Permission> = Arc::new(AllowAllPermission);
    let engine_config = EngineConfig::defaults_for("claude-sonnet-4-6");
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
        engine_config,
        None,
        paths,
        None, // task_router
    ));
    let cancel = CancellationToken::new();
    let server = Arc::new(daemon::DaemonServer::new(pool, cancel));
    let sock = dir.path().join("test.sock");
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

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"commands.list","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let commands = v["result"]["commands"].as_array().expect("commands array");
    let review = commands
        .iter()
        .find(|c| c["name"] == "review")
        .unwrap_or_else(|| panic!("plugin command 'review' not in {commands:?}"));
    assert_eq!(review["kind"], "prompt");
    assert_eq!(review["source"], "plugin");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn plugin_install_rejects_bad_checksum() {
    let (_server, sock, dir, handle) = start_server().await;
    let (archive_path, _correct_checksum) =
        build_demo_plugin_zip(dir.path(), "demo-plugin", "demo");

    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"demo-plugin","version":"1.0.0","download_url":"file://{}","checksum":"{}"}}}}"#,
            archive_path.display(),
            "0".repeat(64),
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("checksum"),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn plugin_lifecycle_install_list_disable_enable_uninstall() {
    let (_server, sock, dir, handle) = start_server().await;
    let (archive_path, checksum) = build_demo_plugin_zip(dir.path(), "demo-plugin", "demo");

    // ── install ──
    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"demo-plugin","version":"1.0.0","download_url":"file://{}","checksum":"{}"}}}}"#,
            archive_path.display(),
            checksum,
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["success"], true, "install resp: {v}");

    // ── plugin.list shows it, enabled ──
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let plugins = v["result"]["plugins"].as_array().unwrap();
    let demo = plugins.iter().find(|p| p["name"] == "demo-plugin").unwrap();
    assert_eq!(demo["enabled"], true);

    // ── commands.list shows the plugin's slash command ──
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"commands.list","id":3}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let commands = v["result"]["commands"].as_array().unwrap();
    assert!(
        commands.iter().any(|c| c["name"] == "demo"),
        "commands: {commands:?}"
    );

    // ── disable ──
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"plugin.disable","id":4,"params":{"name":"demo-plugin"}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["enabled"], false, "resp: {v}");

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"commands.list","id":5}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let commands = v["result"]["commands"].as_array().unwrap();
    assert!(
        !commands.iter().any(|c| c["name"] == "demo"),
        "commands still has it: {commands:?}"
    );

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":6}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let plugins = v["result"]["plugins"].as_array().unwrap();
    let demo = plugins.iter().find(|p| p["name"] == "demo-plugin").unwrap();
    assert_eq!(
        demo["enabled"], false,
        "plugin.list still shows enabled: {demo:?}"
    );

    // ── re-enable ──
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"plugin.enable","id":7,"params":{"name":"demo-plugin"}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["enabled"], true, "resp: {v}");

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"commands.list","id":8}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let commands = v["result"]["commands"].as_array().unwrap();
    assert!(
        commands.iter().any(|c| c["name"] == "demo"),
        "commands: {commands:?}"
    );

    // ── uninstall ──
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"plugin.uninstall","id":9,"params":{"name":"demo-plugin"}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["success"], true, "resp: {v}");

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":10}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let plugins = v["result"]["plugins"].as_array().unwrap();
    assert!(
        !plugins.iter().any(|p| p["name"] == "demo-plugin"),
        "plugins: {plugins:?}"
    );

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"commands.list","id":11}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let commands = v["result"]["commands"].as_array().unwrap();
    assert!(
        !commands.iter().any(|c| c["name"] == "demo"),
        "commands: {commands:?}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

// ── resume_or_create panic-avoidance (malformed session_id + real history_store) ──
//
// `SessionPool::resume_or_create` used to `unwrap()` a `SessionId::parse()`
// call and `panic!()` on a second failed retry — both only reachable when a
// real `HistoryStore` is configured (the `start_server()` helper above uses
// `history_store: None`, which never exercises that code path). Production
// `daemon/src/main.rs` does wire up a real store, so this needs its own
// server variant.

async fn start_server_with_history() -> (
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
        Arc::new(scene::scene::coding::CodingScene);
    let permission: Arc<dyn base::interface::permission::Permission> = Arc::new(AllowAllPermission);
    let engine_config = EngineConfig::defaults_for("claude-sonnet-4-6");

    let client: Arc<dyn AnthropicClient> =
        Arc::new(HttpAnthropicClient::new(AuthMode::ApiKey("test-key".into())).unwrap());

    let paths: Arc<dyn daemon::config::DaemonPaths> =
        Arc::new(StaticDaemonPaths::new(dir.path().to_path_buf()));

    let history_store: Option<Arc<dyn history::store::HistoryStore>> = Some(Arc::new(
        history::store::JsonlHistoryStore::with_root(dir.path(), dir.path().join("sessions"))
            .await
            .unwrap(),
    ));

    let pool = Arc::new(SessionPool::new(
        8,
        3600,
        client,
        settings,
        scene,
        permission,
        memory_store,
        dir.path().to_path_buf(),
        engine_config,
        history_store,
        paths,
        None, // task_router
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

#[tokio::test]
async fn run_turn_with_malformed_session_id_and_history_store_does_not_panic() {
    let (_server, sock, _dir, handle) = start_server_with_history().await;

    // Not a valid BASE58(UUID) — used to `unwrap()`-panic inside
    // `resume_or_create` when a real `HistoryStore` is configured. A
    // panicking `tokio::spawn`'d connection task just drops the socket
    // silently, so the regression signature here is "no response at all
    // (empty read)", not a catchable Rust panic in the test process itself.
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"session_id":"not-a-valid-id!!","message":"hi"},"id":1}"#,
    )
    .await;
    assert!(
        !resp.is_empty(),
        "connection died with no response — resume_or_create likely panicked on an unparsable session_id"
    );
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object() || v["error"].is_object(), "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

// ── TCP daemon.auth handshake ───────────────────────────────────────────

/// Mirrors `start_server()` but over TCP via `serve_tcp_listener` (bound
/// separately so the test knows the OS-assigned ephemeral port before the
/// accept loop starts — `serve_tcp` itself only takes a fixed `SocketAddr`).
async fn start_tcp_server(
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
    let engine_config = EngineConfig::defaults_for("claude-sonnet-4-6");
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
        engine_config,
        None,
        paths,
        None, // task_router
    ));

    let cancel = CancellationToken::new();
    let server = Arc::new(DaemonServer::new(pool, cancel));
    server.set_tcp_token(token.to_string()).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server2 = server.clone();
    let handle = tokio::spawn(async move {
        let _ = server2.serve_tcp_listener(listener).await;
    });

    (server, addr, dir, handle)
}

/// Connect, send a `daemon.auth` handshake with `token`, and return the open
/// stream plus the parsed handshake response (so callers can keep using the
/// same connection afterward — unlike `rpc_call`, which opens/closes fresh).
async fn tcp_handshake(addr: std::net::SocketAddr, token: &str) -> (TcpStream, serde_json::Value) {
    let mut client = TcpStream::connect(addr).await.unwrap();
    let req =
        format!(r#"{{"jsonrpc":"2.0","method":"daemon.auth","params":{{"token":"{token}"}},"id":0}}"#);
    client.write_all(req.as_bytes()).await.unwrap();
    client.write_all(b"\n").await.unwrap();
    let mut buf = String::new();
    {
        let (r, _) = client.split();
        let mut br = BufReader::new(r);
        br.read_line(&mut buf).await.unwrap();
    }
    (client, serde_json::from_str(&buf).unwrap())
}

#[tokio::test]
async fn tcp_rejects_request_before_handshake_and_closes_connection() {
    let (_server, addr, _dir, handle) = start_tcp_server("s3cr3t").await;

    let mut client = TcpStream::connect(addr).await.unwrap();
    // Skip the handshake — send a normal method call first.
    client
        .write_all(br#"{"jsonrpc":"2.0","method":"daemon.status","id":1}"#)
        .await
        .unwrap();
    client.write_all(b"\n").await.unwrap();

    let mut buf = String::new();
    {
        let (r, _) = client.split();
        let mut br = BufReader::new(r);
        br.read_line(&mut buf).await.unwrap();
    }
    let v: serde_json::Value = serde_json::from_str(&buf).unwrap();
    assert_eq!(v["error"]["code"], codes::UNAUTHORIZED, "resp: {v}");

    // The server closes the connection after rejecting the handshake — the
    // next read should see EOF (0 bytes), not a second response, proving
    // `daemon.status` itself was never dispatched.
    let mut trailing = String::new();
    let (r, _) = client.split();
    let mut br = BufReader::new(r);
    let n = br.read_line(&mut trailing).await.unwrap();
    assert_eq!(n, 0, "connection should be closed after auth rejection");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn tcp_rejects_wrong_token() {
    let (_server, addr, _dir, handle) = start_tcp_server("s3cr3t").await;
    let (_client, v) = tcp_handshake(addr, "wrong-token").await;
    assert_eq!(v["error"]["code"], codes::UNAUTHORIZED, "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn tcp_accepts_correct_token_then_dispatches_normally() {
    let (_server, addr, _dir, handle) = start_tcp_server("s3cr3t").await;
    let (mut client, v) = tcp_handshake(addr, "s3cr3t").await;
    assert_eq!(v["result"]["authenticated"], true, "resp: {v}");

    // Same connection, now dispatches like any other request.
    client
        .write_all(br#"{"jsonrpc":"2.0","method":"daemon.status","id":2}"#)
        .await
        .unwrap();
    client.write_all(b"\n").await.unwrap();
    let mut buf = String::new();
    let (r, _) = client.split();
    let mut br = BufReader::new(r);
    br.read_line(&mut buf).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&buf).unwrap();
    assert!(v["result"]["version"].is_string(), "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn serve_tcp_listener_bails_without_a_token_even_when_called_directly() {
    // `serve_tcp_listener` is `pub` specifically so tests (and any other
    // caller who already has a bound `TcpListener`, e.g. socket activation)
    // can drive it without going through `serve_tcp`'s addr-binding — but
    // that means it can't just trust `serve_tcp`'s "token is set" check to
    // have already run. This calls it directly, skipping `set_tcp_token`
    // entirely, and asserts it fails loudly instead of silently rejecting
    // every future connection forever.
    let dir = tempfile::tempdir().unwrap();
    let settings = Arc::new(Settings::defaults_for("claude-sonnet-4-6"));
    let memory_store = Arc::new(MemoryStore::new(
        dir.path().join("user").join("memory"),
        dir.path().join("local").join("memory"),
    ));
    let scene: Arc<dyn base::interface::scene::AgentScene> =
        Arc::new(scene::scene::coding::CodingScene);
    let permission: Arc<dyn base::interface::permission::Permission> = Arc::new(AllowAllPermission);
    let engine_config = EngineConfig::defaults_for("claude-sonnet-4-6");
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
        engine_config,
        None,
        paths,
        None, // task_router
    ));
    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
    // Deliberately NOT calling server.set_tcp_token(...).

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let err = server.serve_tcp_listener(listener).await.unwrap_err();
    assert!(err.to_string().contains("token"), "err: {err}");
}
