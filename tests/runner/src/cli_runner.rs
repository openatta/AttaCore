//! CliRunner — 启动 daemon + 通过 `rpc_client::DaemonRpcClient` 执行测试用例。

use crate::api_runner::TurnOutput;
use crate::script::TestCase;
use rpc_client::{DaemonRpcClient, RunTurnOptions, TelemetryRunOptions, VcrRunOptions};
use std::path::PathBuf;
use std::process::Stdio;

pub struct CliRunnerConfig {
    pub socket_path: PathBuf,
    pub daemon_binary: PathBuf,
    pub config_path: PathBuf,
    pub scenario: String,
    pub vcr_mode: Option<String>,   // "record" | "replay"
    /// 本地/CI 缓存的 VCR 录制数据目录（tests/fixtures/cassettes/{scenario}/cli/{round}/，已 gitignore）。
    pub cassette_dir: PathBuf,
    /// 纯生成物目录（遥测日志），继续被 .gitignore 排除。
    pub output_dir: PathBuf,
    /// 模板项目 fixture 目录。设置后会被拷贝到一个临时工作目录，daemon 子进程
    /// 以该目录为 cwd 启动（daemon 没有 `--cwd` flag，天然用进程 cwd 当项目根，
    /// 见 docs/CONFIG_LAYOUT.md）——这样 daemon 侧读到的 `.atta/settings.json`
    /// 就是拷贝出来的那份，同一份 fixture 在 api/cli 两种模式下语义一致。
    pub fixture_dir: Option<PathBuf>,
}

/// 启动 daemon，执行所有轮次，返回输出，最后停止 daemon 并清理 session。
pub async fn run_test_case(config: CliRunnerConfig, case: &TestCase) -> anyhow::Result<Vec<TurnOutput>> {
    // 0. 解析配置文件拿到 env vars 传给 daemon 子进程
    let env_vars = crate::config::parse_env_file(&config.config_path)?;

    // 0b. 按需拷贝模板项目 fixture，daemon 以拷贝出来的目录为 cwd 启动
    let workdir = if let Some(fixture) = &config.fixture_dir {
        let dir = std::env::temp_dir().join(format!("atta_test_runner_cli_{}", config.scenario));
        let _ = std::fs::remove_dir_all(&dir);
        crate::fixture::copy_dir_recursive(fixture, &dir)?;
        crate::fixture::resolve_mcp_toy_server_placeholder(&dir)?;
        Some(dir)
    } else {
        None
    };

    // 1. 启动 daemon
    let _ = std::fs::remove_file(&config.socket_path);
    let mut cmd = tokio::process::Command::new(&config.daemon_binary);
    cmd.arg("--socket").arg(&config.socket_path);
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    cmd.kill_on_drop(true);
    if let Some(dir) = &workdir {
        cmd.current_dir(dir);
    }
    for (k, v) in &env_vars {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;

    // 2. 等待 socket ready
    let mut attempts = 0;
    while attempts < 100 {
        if config.socket_path.exists() { break; }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        attempts += 1;
    }
    anyhow::ensure!(config.socket_path.exists(), "daemon socket not ready after 10s");

    // 3. 连接
    let mut client = DaemonRpcClient::connect(&config.socket_path).await?;

    let mut results = Vec::new();
    let mut session_ids = Vec::new();

    for turn in &case.turns {
        // 每轮独立 session，保持对话历史短小
        let session_id = format!("test-{}-t{}", config.scenario, turn.index);
        session_ids.push(session_id.clone());

        let options = config.vcr_mode.as_ref().map(|mode| {
            let cassette_dir = std::fs::canonicalize(&config.cassette_dir)
                .unwrap_or_else(|_| config.cassette_dir.clone());
            let _ = std::fs::create_dir_all(&cassette_dir);
            let telemetry_path = {
                let _ = std::fs::create_dir_all(&config.output_dir);
                std::fs::canonicalize(&config.output_dir)
                    .unwrap_or_else(|_| config.output_dir.clone())
                    .join(format!("{}.telemetry.md", config.scenario))
            };
            RunTurnOptions {
                vcr: Some(VcrRunOptions {
                    mode: mode.clone(),
                    scenario: config.scenario.clone(),
                    dir: cassette_dir.to_string_lossy().to_string(),
                }),
                telemetry: Some(TelemetryRunOptions {
                    output: telemetry_path.to_string_lossy().to_string(),
                }),
            }
        });

        let turn_id = format!("turn_{}", turn.index);
        let events = client.session_run_turn(&session_id, &turn.input, &turn_id, options).await?;

        if let Some(resp) = &events.response {
            if let Some(err) = &resp.error {
                anyhow::bail!("session.run_turn failed: {} (code {})", err.message, err.code);
            }
        }

        results.push(TurnOutput { text: events.text, tool_uses: events.tool_uses });
    }

    // 4. 关闭所有测试 session（真正存在的方法是 session.close，不是 session.delete）
    for session_id in &session_ids {
        let _ = client.session_close(session_id).await;
    }

    // 5. 停止 daemon
    let _ = child.kill().await;
    let _ = std::fs::remove_file(&config.socket_path);

    // 6. 清理测试 artifacts（session/memory 落盘文件，RPC 层面的 session.close
    // 只关运行时状态，不删磁盘持久化文件）
    cleanup_test_artifacts(&format!("test-{}", config.scenario));
    if let Some(dir) = &workdir {
        let _ = std::fs::remove_dir_all(dir);
    }

    Ok(results)
}

/// 清理测试过程中生成的 session/memory 等文件。
fn cleanup_test_artifacts(session_id: &str) {
    let base = std::env::var("HOME").ok().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
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
    let mem_index = memory_dir.join("MEMORY.md");
    let _ = std::fs::remove_file(mem_index);
}
