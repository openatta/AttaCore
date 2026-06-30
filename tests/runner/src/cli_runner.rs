//! CliRunner — 启动 daemon + 通过 Unix socket JSON-RPC 执行测试用例。

use crate::api_runner::TurnOutput;
use crate::script::TestCase;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub struct CliRunnerConfig {
    pub socket_path: PathBuf,
    pub daemon_binary: PathBuf,
    pub config_path: PathBuf,
    pub scenario: String,
    pub vcr_mode: Option<String>, // "record" | "replay"
    pub output_dir: PathBuf,      // tests/output/{scenario}/cli
}

/// 启动 daemon，执行所有轮次，返回输出，最后停止 daemon 并清理 session。
pub async fn run_test_case(
    config: CliRunnerConfig,
    case: &TestCase,
) -> anyhow::Result<Vec<TurnOutput>> {
    // 0. Source config file to get env vars
    let env_vars = source_config_file(&config.config_path)?;

    // 1. 启动 daemon (with env vars from config)
    let _ = std::fs::remove_file(&config.socket_path);
    let mut cmd = tokio::process::Command::new(&config.daemon_binary);
    cmd.arg("--socket").arg(&config.socket_path);
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    cmd.kill_on_drop(true);
    for (k, v) in &env_vars {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;

    // 2. 等待 socket ready
    let mut attempts = 0;
    while attempts < 100 {
        if config.socket_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        attempts += 1;
    }
    anyhow::ensure!(
        config.socket_path.exists(),
        "daemon socket not ready after 10s"
    );

    // 3. 连接
    let stream = UnixStream::connect(&config.socket_path).await?;
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);

    let mut results = Vec::new();

    for turn in &case.turns {
        // Use a fresh session per turn to keep conversation history small
        let session_id = format!("test-{}-t{}", config.scenario, turn.index);

        // 4. 发送 session.run_turn (with options for VCR + telemetry)
        let mut params = serde_json::json!({
            "session_id": &session_id,
            "message": &turn.input,
            "turn_id": format!("turn_{}", turn.index),
        });

        // Inject VCR + telemetry options if configured
        if let Some(ref mode) = config.vcr_mode {
            // Canonicalize to absolute path for daemon
            let vcr_dir = std::fs::canonicalize(&config.output_dir)
                .unwrap_or_else(|_| config.output_dir.clone())
                .to_string_lossy()
                .to_string();
            let _ = std::fs::create_dir_all(&vcr_dir);
            let telemetry_path = std::fs::canonicalize(&config.output_dir)
                .unwrap_or_else(|_| config.output_dir.clone())
                .join(format!("{}.telemetry.md", config.scenario));
            params["options"] = serde_json::json!({
                "vcr": {
                    "mode": mode,
                    "scenario": &config.scenario,
                    "dir": vcr_dir,
                },
                "telemetry": {
                    "output": telemetry_path.to_string_lossy(),
                }
            });
        }

        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session.run_turn",
            "id": 1,
            "params": params,
        });
        let mut req_line = serde_json::to_string(&req)?;
        req_line.push('\n');
        writer.write_all(req_line.as_bytes()).await?;

        // 5. 逐行读响应
        let mut text = String::new();
        let mut tool_uses: Vec<(String, Value)> = Vec::new();
        let mut turn_complete = false;
        let mut line = String::new();

        loop {
            line.clear();
            match buf_reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let msg: Value = serde_json::from_str(trimmed)?;

                    // StreamFrame: {"jsonrpc":"2.0","method":"session.event","params":{...}}
                    if msg.get("method").and_then(|m| m.as_str()) == Some("session.event") {
                        if let Some(event) = msg.get("params").and_then(|p| p.get("event")) {
                            match event.get("kind").and_then(|k| k.as_str()) {
                                Some("text_delta") => {
                                    if let Some(t) = event.get("text").and_then(|v| v.as_str()) {
                                        text.push_str(t);
                                    }
                                }
                                Some("tool_use") => {
                                    let name = event
                                        .get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("?")
                                        .to_string();
                                    let input = event.get("input").cloned().unwrap_or_default();
                                    tool_uses.push((name, input));
                                }
                                Some("turn_complete") => {
                                    turn_complete = true;
                                }
                                _ => {}
                            }
                        }
                    }
                    // RpcResponse: has "id" and "result" or "error"
                    if msg.get("id").is_some()
                        && (msg.get("result").is_some() || msg.get("error").is_some())
                    {
                        if turn_complete {
                            break;
                        }
                    }
                }
                Err(e) => anyhow::bail!("read error: {e}"),
            }
        }

        results.push(TurnOutput { text, tool_uses });
    }

    // 6. 删除所有测试 session
    for turn in &case.turns {
        let sid = format!("test-{}-t{}", config.scenario, turn.index);
        let del_req = serde_json::json!({
            "jsonrpc": "2.0", "method": "session.delete", "id": 2,
            "params": {"session_id": &sid}
        });
        let mut del_line = serde_json::to_string(&del_req)?;
        del_line.push('\n');
        let _ = writer.write_all(del_line.as_bytes()).await;
    }

    // 7. 停止 daemon
    let _ = child.kill().await;
    let _ = std::fs::remove_file(&config.socket_path);

    // 8. 清理测试 artifacts
    cleanup_test_artifacts(&format!("test-{}", config.scenario));

    Ok(results)
}

/// 清理测试过程中生成的 session/memory 等文件。
fn cleanup_test_artifacts(session_id: &str) {
    // 清理 session JSONL 文件
    let base = dirs_next().unwrap_or_else(|| PathBuf::from("/tmp"));
    let session_dir = base.join(".atta").join("code").join("sessions");
    if session_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&session_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.contains(session_id) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
    // 清理 memory 文件
    let memory_dir = base.join(".atta").join("code").join("memory");
    if memory_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&memory_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.contains(session_id) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
    // 清理 MEMORY.md
    let mem_index = memory_dir.join("MEMORY.md");
    let _ = std::fs::remove_file(mem_index);
}

fn dirs_next() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// Parse a .sh config file (export KEY=VALUE format) into env vars.
fn source_config_file(path: &PathBuf) -> anyhow::Result<Vec<(String, String)>> {
    let content = std::fs::read_to_string(path)?;
    let mut vars = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("export ") {
            if let Some((k, v)) = rest.split_once('=') {
                let v = v.trim().trim_matches('"').trim_matches('\'');
                vars.push((k.to_string(), v.to_string()));
            }
        }
    }
    Ok(vars)
}
