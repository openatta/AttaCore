//! End-to-end against a real component.
//!
//! Everything else in this crate tests a piece in isolation. This builds the
//! fixture in `tests/fixtures/wasm_echo_plugin`, links it, and provokes it —
//! because the claims that matter (a spinning plugin is survivable, an
//! undeclared host is unreachable, state does not leak between calls) are
//! claims about the whole path, and each is exactly the kind of thing that
//! passes in a unit test and fails against a real guest.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wasm_host::capabilities::ResolvedCapabilities;
use wasm_host::{CallFailure, PluginInstance, WasmEngine};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/wasm_echo_plugin")
        .canonicalize()
        .expect("the fixture directory should exist")
}

/// Build the fixture on demand. A missing `wasm32-wasip2` target is the one
/// likely failure, so it gets its own message rather than a wall of cargo
/// output.
fn component_path() -> PathBuf {
    let dir = fixture_dir();
    let out = dir.join("target/wasm32-wasip2/release/wasm_echo_plugin.wasm");
    if out.exists() {
        return out;
    }
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "--target", "wasm32-wasip2"])
        .current_dir(&dir)
        .status()
        .expect("cargo should be runnable");
    assert!(
        status.success(),
        "could not build the fixture component. If the target is missing: \
         rustup target add wasm32-wasip2"
    );
    out
}

fn caps(net: Vec<String>, timeout_ms: u64) -> Arc<ResolvedCapabilities> {
    let mut c = plugin::manifest::Capabilities::default();
    c.net = net;
    c.timeout_ms = timeout_ms;
    Arc::new(
        ResolvedCapabilities::resolve(
            &c,
            &std::env::temp_dir(),
            &std::env::temp_dir(),
        )
        .unwrap(),
    )
}

fn instance(caps: Arc<ResolvedCapabilities>) -> PluginInstance {
    let engine = WasmEngine::new().unwrap();
    let dir = fixture_dir();
    let component = engine.load(&component_path(), &dir).unwrap();
    PluginInstance::link(&engine, &component, "echo-plugin".into(), caps).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_component_reports_the_tools_it_exports() {
    let inst = instance(caps(vec![], 5_000));
    let tools = inst.list_tools(&CancellationToken::new()).await.unwrap();

    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"echo"), "{names:?}");
    assert!(names.contains(&"spin"), "{names:?}");

    let echo = tools.iter().find(|t| t.name == "echo").unwrap();
    assert!(echo.read_only, "the component's own declaration comes through");
    assert!(
        echo.doc.as_ref().is_some_and(|d| d.contains("verbatim")),
        "the long doc is what ToolSearch fetches on demand"
    );
    assert!(echo.input_schema.contains("\"text\""));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_call_round_trips_content_and_structured_output() {
    let inst = instance(caps(vec![], 5_000));
    let out = inst
        .call_tool("echo", r#"{"text":"hello"}"#, "call-1", &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(out.content, "hello");
    assert!(!out.is_error);
    assert_eq!(
        out.structured.as_deref(),
        Some(r#"{"echoed":"hello"}"#),
        "a plugin tool must be able to return both a rendering and a value"
    );
}

/// The claim the whole execution model rests on: a plugin that never returns
/// costs one call, not the process.
#[tokio::test(flavor = "multi_thread")]
async fn a_spinning_plugin_is_stopped_at_its_declared_timeout() {
    let inst = instance(caps(vec![], 150));
    let started = std::time::Instant::now();

    let err = inst
        .call_tool("spin", "{}", "call-spin", &CancellationToken::new())
        .await
        .unwrap_err();

    assert_eq!(err, CallFailure::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deadline must actually interrupt, not merely be reported after the fact"
    );

    // And the engine is still usable afterwards.
    let out = inst
        .call_tool("echo", r#"{"text":"still here"}"#, "call-after", &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(out.content, "still here");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_running_call_returns_promptly() {
    let inst = instance(caps(vec![], 60_000));
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
    });

    let started = std::time::Instant::now();
    let err = inst.call_tool("spin", "{}", "call-cancel", &cancel).await.unwrap_err();

    assert_eq!(err, CallFailure::Cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancellation must not wait out the plugin's timeout"
    );
}

/// A trap is a tool error, not a process problem — the store is discarded and
/// the next call proceeds.
#[tokio::test(flavor = "multi_thread")]
async fn a_trapping_tool_is_contained() {
    let inst = instance(caps(vec![], 5_000));
    let err = inst
        .call_tool("explode", "{}", "call-boom", &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, CallFailure::Faulted(_)), "{err:?}");

    let out = inst
        .call_tool("echo", r#"{"text":"survived"}"#, "call-next", &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(out.content, "survived");
}

/// The capability list is enforced against the running guest, not just
/// against the manifest parser.
#[tokio::test(flavor = "multi_thread")]
async fn an_undeclared_host_is_unreachable_from_inside_the_component() {
    let inst = instance(caps(vec![], 5_000));
    let out = inst
        .call_tool(
            "fetch",
            r#"{"url":"https://example.com/"}"#,
            "call-fetch",
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(out.is_error, "an undeclared host must not be fetchable");
    assert!(
        out.content.contains("net"),
        "the guest should learn which capability would have allowed it: {}",
        out.content
    );
}

/// Instance-per-call means no linear memory survives a call. Anything that
/// must persist goes through the host's key-value namespace — which is how
/// the host stays able to see and clear it.
#[tokio::test(flavor = "multi_thread")]
async fn state_survives_only_through_the_host_namespace() {
    let inst = instance(caps(vec![], 5_000));
    let cancel = CancellationToken::new();

    let first = inst
        .call_tool("remember", r#"{"value":"one"}"#, "c1", &cancel)
        .await
        .unwrap();
    assert_eq!(first.content, "(none)", "the first call has nothing stored yet");

    let second = inst
        .call_tool("remember", r#"{"value":"two"}"#, "c2", &cancel)
        .await
        .unwrap();
    assert_eq!(
        second.content, "one",
        "the previous call's value must come back through the host, not through memory"
    );

    inst.kv().clear();
    let third = inst
        .call_tool("remember", r#"{"value":"three"}"#, "c3", &cancel)
        .await
        .unwrap();
    assert_eq!(
        third.content, "(none)",
        "clearing the namespace is what unloading a plugin must be able to do"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn init_reports_a_configuration_the_plugin_refuses() {
    let inst = instance(caps(vec![], 5_000));
    let cancel = CancellationToken::new();

    inst.init(r#"{"ok":true}"#, &cancel).await.unwrap();

    let err = inst.init(r#"{"fail":true}"#, &cancel).await.unwrap_err();
    match err {
        CallFailure::Faulted(msg) => assert!(msg.contains("configuration"), "{msg}"),
        other => panic!("expected the plugin's own rejection, got {other:?}"),
    }
}
