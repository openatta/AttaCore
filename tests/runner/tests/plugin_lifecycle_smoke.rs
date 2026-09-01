//! 端到端验证插件 harness：真的拉起 `attacored`，用 `rpc_client::DaemonRpcClient`
//! 把 `tests/fixtures/plugins/demo-plugin/` 打包安装、确认出现在 `plugin.list`、
//! 再卸载——跟 `daemon/tests/daemon_e2e.rs` 里同类测试的区别是这里走的是真实子进程
//! + 提交的插件源码目录，不是 daemon crate 内部直接调用 + 内联现造的 zip。
//!
//! `#[ignore]`：拉子进程，比纯单测慢；`cargo test -- --ignored` 显式跑。

use rpc_client::DaemonRpcClient;
use std::path::PathBuf;
use std::process::Stdio;

#[tokio::test]
#[ignore]
async fn plugin_install_list_uninstall_round_trips() {
    let status = std::process::Command::new("cargo")
        .args(["build", "-p", "daemon", "--quiet"])
        .status()
        .expect("failed to invoke cargo build -p daemon");
    assert!(status.success(), "cargo build -p daemon failed");
    let daemon_binary = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/attacored")
        .canonicalize()
        .expect("attacored binary should exist after build");

    let tmp = std::env::temp_dir().join(format!("atta_plugin_smoke_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let config_home = tmp.join("config_home"); // ATTA_CONFIG_HOME — isolates from the real ~/.atta
    let socket_path = tmp.join("daemon.sock");
    std::fs::create_dir_all(&config_home).unwrap();

    let (zip_path, checksum) =
        test_runner::plugin_fixture::package_demo_plugin(&tmp.join("packaged"))
            .expect("package demo-plugin");
    let download_url = test_runner::plugin_fixture::file_url(&zip_path);

    let workdir = tmp.join("workdir");
    std::fs::create_dir_all(&workdir).unwrap();

    let mut child = tokio::process::Command::new(&daemon_binary)
        .arg("--socket")
        .arg(&socket_path)
        // A daemon's project is its working directory, and a child that
        // inherits this one's works on the crate it was launched from — it
        // writes `.atta/` into the repository. `ATTA_CONFIG_HOME` redirects
        // the user-level root and says nothing about the project-level one.
        .current_dir(&workdir)
        .env("ATTA_CONFIG_HOME", &config_home)
        // Startup requires *some* auth token even though this test never
        // triggers a real model call (plugin install/list/uninstall is pure
        // local file I/O) — a dummy value is enough to pass the check.
        .env("ANTHROPIC_AUTH_TOKEN", "test-dummy-token-plugin-smoke")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn attacored");

    let mut attempts = 0;
    while attempts < 100 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        attempts += 1;
    }
    assert!(socket_path.exists(), "daemon socket not ready after 10s");

    let mut client = DaemonRpcClient::connect(&socket_path)
        .await
        .expect("connect");

    let install_resp = client
        .call(
            "plugin.install",
            serde_json::json!({
                "name": "demo-plugin",
                "version": "1.0.0",
                "download_url": download_url,
                "checksum": checksum,
            }),
        )
        .await
        .expect("plugin.install RPC");
    assert!(
        install_resp.error.is_none(),
        "plugin.install failed: {:?}",
        install_resp.error
    );

    let list_resp = client.plugin_list().await.expect("plugin.list RPC");
    let plugins = list_resp.result.expect("plugin.list result");
    let names: Vec<&str> = plugins["plugins"]
        .as_array()
        .expect("plugins is an array")
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(
        names.contains(&"demo-plugin"),
        "expected demo-plugin in plugin.list, got: {names:?}"
    );

    let uninstall_resp = client
        .call(
            "plugin.uninstall",
            serde_json::json!({ "name": "demo-plugin" }),
        )
        .await
        .expect("plugin.uninstall RPC");
    assert!(
        uninstall_resp.error.is_none(),
        "plugin.uninstall failed: {:?}",
        uninstall_resp.error
    );

    let _ = child.kill().await;
    let _ = std::fs::remove_dir_all(&tmp);
}
