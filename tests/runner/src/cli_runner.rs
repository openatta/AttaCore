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
    pub vcr_mode: Option<String>, // "record" | "replay"
    /// 本地/CI 缓存的 VCR 录制数据目录（tests/fixtures/cassettes/{scenario}/cli/{round}/，已 gitignore）。
    pub cassette_dir: PathBuf,
    /// 纯生成物目录（遥测日志），继续被 .gitignore 排除。
    pub output_dir: PathBuf,
    /// 模板项目 fixture 目录。设置后会被拷贝到一个临时工作目录，daemon 子进程
    /// 以该目录为 cwd 启动（daemon 没有 `--cwd` flag，天然用进程 cwd 当项目根，
    /// 见 docs/CONFIG_LAYOUT.md）——这样 daemon 侧读到的 `.atta/settings.json`
    /// 就是拷贝出来的那份，同一份 fixture 在 api/cli 两种模式下语义一致。
    pub fixture_dir: Option<PathBuf>,
    /// 传给 daemon 子进程的 `--scene`。之前这里完全没传，daemon 一直用它自己
    /// 的默认值 "coding" 启动——`--mode cli` 从没能测过 chat/research 场景，
    /// 不是因为它们跑不了，是因为这条路径压根没给它们留传参的口子。
    pub scene: String,
    /// 整个用例用同一个 daemon session（对话历史跨轮延续），而不是每轮一个新
    /// session。由 `main.rs::shared_session` 解析（用例文件的 `session: shared`
    /// 声明 / `--same-session`）——`.test` 用例声明的会话语义必须在 api/cli 两种
    /// 模式下一致，否则同一个用例在 `--mode cli` 下会安静地退回逐轮隔离，
    /// 跨轮断言无从谈起。
    pub same_session: bool,
}

/// 启动 daemon，执行所有轮次，返回输出，最后停止 daemon 并清理 session。
pub async fn run_test_case(
    config: CliRunnerConfig,
    case: &TestCase,
) -> anyhow::Result<Vec<TurnOutput>> {
    // 0. 解析配置文件拿到 env vars 传给 daemon 子进程
    let env_vars = crate::config::parse_env_file(&config.config_path)?;

    // `scenario` 可能带目录层级（`skills/001_startup`——见 main.rs 里 scenario 的
    // 计算）。cassette_dir/output_dir 已经按完整 scenario 分好目录了，这里再用
    // 完整名字去拼**文件名**就会变成 `.../skills/001_startup/cli/<round>/skills/
    // 001_startup.jsonl`——一个没人创建的嵌套目录，写入失败还被 save_entry 吞掉，
    // 录制"成功"但回放 100% miss（`api_runner.rs` 侧已经踩过并修掉了同一个坑，
    // cli 侧一直没跟上）。session_id/临时目录名同理，斜杠会变成路径分隔符。
    let scenario_leaf = config
        .scenario
        .rsplit('/')
        .next()
        .unwrap_or(&config.scenario)
        .to_string();

    // 0b. daemon 子进程的 cwd 永远是一个新建的空临时目录（不是当前仓库！），
    // 有 fixture 时把 fixture 拷进去。这不只是隔离 —— 之前 daemon 没设置 cwd
    // 时会继承 `cargo run` 的 cwd（本仓库根，一个有大量未提交改动的真实 git
    // repo），系统提示词里的 env 块会带上完整的 gitStatus，跟 `api_runner.rs`
    // 用的 `/tmp/atta_test_runner`（非 git 目录）在结构上就不一样 —— 不是
    // dehydrate 能抹平的路径差异，是内容本身不同，VCR 哈希永远对不上。见
    // 2026-08-06 定位这个问题时的第一次 replay miss。
    let dir = std::env::temp_dir().join(format!("atta_test_runner_cli_{scenario_leaf}"));
    let _ = std::fs::remove_dir_all(&dir);
    if let Some(fixture) = &config.fixture_dir {
        crate::fixture::copy_dir_recursive(fixture, &dir)?;
        crate::fixture::resolve_mcp_toy_server_placeholder(&dir)?;
    } else {
        std::fs::create_dir_all(&dir)?;
    }
    // Force `thinking_mode: off`, merged into whatever `.atta/settings.json`
    // is already there (fixture-provided or none) — matches what
    // `api_runner.rs` hardcodes for the direct-API path. Without this, the
    // default is `Auto`, and this relay (deepseek-v4-pro[1m]) sends thinking
    // blocks under `Auto` that this codebase's stream mapping never captures
    // as a real content block (only `TextDelta`/`ToolUse` are assembled —
    // `ThinkingDelta`/`SignatureDelta` are silently dropped, same class of
    // bug as the `InputJsonDelta` one fixed earlier today, just not fixed
    // here) — the next API call in the same turn then gets rejected with
    // "content[].thinking must be passed back". Real, separate bug, flagged
    // for its own fix; this override just keeps this path testable meanwhile.
    {
        let settings_path = dir.join(".atta").join("settings.json");
        let mut value: serde_json::Value = std::fs::read_to_string(&settings_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        value["model"]["thinking_mode"] = serde_json::Value::String("off".into());
        std::fs::create_dir_all(dir.join(".atta"))?;
        std::fs::write(&settings_path, serde_json::to_string_pretty(&value)?)?;
    }
    let workdir = Some(dir);

    // 1. 启动 daemon —— `daemon_binary` 必须先转成绝对路径：`Command::new` 对
    // 相对路径的 program 参数是相对子进程的 cwd 解析的（不是当前进程的
    // cwd），现在 cwd 永远会被上面那段改掉，相对路径（默认值
    // "target/debug/attacored"）会在新 cwd 下找不到,不转换的话每次都
    // "No such file or directory"。
    let daemon_binary = std::fs::canonicalize(&config.daemon_binary).map_err(|e| {
        anyhow::anyhow!(
            "daemon binary not found at {} ({e}) — run `cargo build -p daemon` first",
            config.daemon_binary.display()
        )
    })?;
    let _ = std::fs::remove_file(&config.socket_path);
    let mut cmd = tokio::process::Command::new(&daemon_binary);
    cmd.arg("--socket")
        .arg(&config.socket_path)
        .arg("--scene")
        .arg(&config.scene);
    // daemon has its own `--model` flag (default "claude-sonnet-4-6"), separate
    // from the `ANTHROPIC_MODEL` env-var convention `api_runner.rs`/`.env` use —
    // it does NOT read `ANTHROPIC_MODEL` at all. Without this, the daemon path
    // always used the CLI default regardless of what `.env` said, silently
    // recording/replaying under the wrong model name — which the VCR hash
    // includes, so cross-path (api vs cli) replay always missed. Found while
    // trying to replay an already-recorded agent-mode cassette through `--mode
    // cli`: `ATTA_DEBUG_VCR_MISS=1` showed the daemon's actual request used
    // `claude-sonnet-4-6` while the cassette was recorded under
    // `deepseek-v4-pro[1m]`.
    if let Some((_, model)) = env_vars.iter().find(|(k, _)| k == "ANTHROPIC_MODEL") {
        cmd.arg("--model").arg(model);
    }
    if std::env::var("ATTA_DEBUG_VCR_MISS").is_ok() {
        cmd.stdout(Stdio::null()).stderr(Stdio::inherit());
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    cmd.kill_on_drop(true);
    if let Some(dir) = &workdir {
        cmd.current_dir(dir);
    }
    for (k, v) in &env_vars {
        cmd.env(k, v);
    }
    // Isolate the daemon's *global* config root too — `DefaultDaemonPaths::from_env`
    // (daemon/src/config.rs) defaults to `$HOME/.atta` when `ATTA_CONFIG_HOME` is
    // unset, which is the real interactive user's actual AttaCore config
    // directory on this machine. Without this override, a test run's daemon
    // writes real `daemon.lock`/memory files there — confirmed happening (a
    // stray `~/.atta/scenes/coding/daemon.lock` from an earlier run before this
    // fix, cleaned up manually). `cmd.current_dir(workdir)` above only isolates
    // the *project*-level root, a separate layer.
    if let Some(dir) = &workdir {
        cmd.env("ATTA_CONFIG_HOME", dir.join("global_config"));
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
    let mut client = DaemonRpcClient::connect(&config.socket_path).await?;

    let mut results = Vec::new();
    let mut session_ids = Vec::new();

    if config.same_session {
        eprintln!("Shared-session mode: one daemon session for the whole case");
    }

    for turn in &case.turns {
        // 默认每轮独立 session，保持对话历史短小；`session: shared` 的用例整个
        // 用例复用同一个 session_id，daemon 侧的 SessionPool 会带着上一轮的
        // 对话历史继续跑。
        let session_id = if config.same_session {
            format!("test-{scenario_leaf}-shared")
        } else {
            format!("test-{}-t{}", scenario_leaf, turn.index)
        };
        if !session_ids.contains(&session_id) {
            session_ids.push(session_id.clone());
        }

        let options = config.vcr_mode.as_ref().map(|mode| {
            let cassette_dir = std::fs::canonicalize(&config.cassette_dir)
                .unwrap_or_else(|_| config.cassette_dir.clone());
            let _ = std::fs::create_dir_all(&cassette_dir);
            let telemetry_path = {
                let _ = std::fs::create_dir_all(&config.output_dir);
                std::fs::canonicalize(&config.output_dir)
                    .unwrap_or_else(|_| config.output_dir.clone())
                    .join(format!("{scenario_leaf}.telemetry.md"))
            };
            // Same fallback policy as the direct-API path (`api_runner.rs`),
            // resolved by the one function that owns it
            // (`VcrModel::replay_fallback_on_miss` — strict by default under a
            // test runner / CI, `ATTA_VCR_STRICT=1` to force,
            // `ATTA_VCR_FALLBACK=1` to opt back in). Record mode ignores it.
            let strict =
                mode.as_str() != "record" && !telemetry::vcr::VcrModel::replay_fallback_on_miss();
            RunTurnOptions {
                vcr: Some(VcrRunOptions {
                    mode: mode.clone(),
                    scenario: scenario_leaf.clone(),
                    dir: cassette_dir.to_string_lossy().to_string(),
                    strict,
                }),
                telemetry: Some(TelemetryRunOptions {
                    output: telemetry_path.to_string_lossy().to_string(),
                }),
            }
        });

        let turn_id = format!("turn_{}", turn.index);
        let events = client
            .session_run_turn(&session_id, &turn.input, &turn_id, options)
            .await?;

        if let Some(resp) = &events.response {
            if let Some(err) = &resp.error {
                anyhow::bail!(
                    "session.run_turn failed: {} (code {})",
                    err.message,
                    err.code
                );
            }
        }

        results.push(TurnOutput {
            text: events.text,
            tool_uses: events.tool_uses,
        });
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
    cleanup_test_artifacts(&format!("test-{scenario_leaf}"));
    if let Some(dir) = &workdir {
        let _ = std::fs::remove_dir_all(dir);
    }

    Ok(results)
}

/// 清理测试过程中生成的 session/memory 等文件。
fn cleanup_test_artifacts(session_id: &str) {
    let base = std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
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
