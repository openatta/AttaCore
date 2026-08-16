//! The compiler and the runtime are separate binaries. This is the check
//! that what one writes, the other reads.
//!
//! It is the whole load-bearing assumption of splitting them: if the two
//! disagreed about where an artifact goes or what it looks like, a
//! runtime-only build would refuse every plugin, and no unit test on either
//! side would notice.

use std::path::{Path, PathBuf};

fn fixture_component() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/wasm_echo_plugin/target/wasm32-wasip2/release/wasm_echo_plugin.wasm")
}

fn install_plugin(dir: &Path, component: &Path) {
    std::fs::copy(component, dir.join("echo.wasm")).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        "[plugin]\nname = \"echo-plugin\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\n[[wasm]]\ncomponent = \"echo.wasm\"\n",
    )
    .unwrap();
}

#[test]
fn what_the_compiler_writes_the_runtime_loads_without_compiling() {
    let component = fixture_component();
    if !component.exists() {
        eprintln!("skipping: build the fixture first (cargo test -p wasm-host)");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    install_plugin(dir.path(), &component);

    let written = plugin_compiler::compile_plugin(dir.path()).expect("compiling should succeed");
    assert_eq!(written.len(), 1);
    assert!(written[0].is_file(), "the artifact must actually be there");
    assert!(
        written[0].starts_with(dir.path().join(".aot")),
        "artifacts belong beside the plugin so uninstalling takes them: {:?}",
        written[0]
    );

    // The runtime finds it without compiling anything of its own.
    let engine = wasm_host::WasmEngine::new().unwrap();
    let handle = engine.load(&dir.path().join("echo.wasm"), dir.path()).unwrap();
    assert!(
        handle.was_cached(),
        "the runtime must reuse the compiler's artifact, not compile its own"
    );
}

/// Compiling twice is idempotent — the artifact is content-addressed, so a
/// reinstall of an unchanged plugin lands on the same file.
#[test]
fn compiling_the_same_plugin_twice_produces_the_same_artifact() {
    let component = fixture_component();
    if !component.exists() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    install_plugin(dir.path(), &component);

    let first = plugin_compiler::compile_plugin(dir.path()).unwrap();
    let second = plugin_compiler::compile_plugin(dir.path()).unwrap();
    assert_eq!(first, second);
}

/// A plugin with nothing to compile is not an error — MCP-only plugins are
/// ordinary — but it must not silently look like success at compiling
/// something.
#[test]
fn a_plugin_with_no_components_compiles_nothing_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("plugin.toml"),
        "[plugin]\nname = \"mcp-only\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n",
    )
    .unwrap();
    assert!(plugin_compiler::compile_plugin(dir.path()).unwrap().is_empty());
}

#[test]
fn a_component_that_is_not_wasm_fails_with_its_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("echo.wasm"), b"not a component").unwrap();
    std::fs::write(
        dir.path().join("plugin.toml"),
        "[plugin]\nname = \"broken\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\n[[wasm]]\ncomponent = \"echo.wasm\"\n",
    )
    .unwrap();
    let err = format!("{:#}", plugin_compiler::compile_plugin(dir.path()).unwrap_err());
    assert!(err.contains("echo.wasm"), "{err}");
}
