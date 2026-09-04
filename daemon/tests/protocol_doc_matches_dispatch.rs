//! The protocol doc and the dispatch table describe the same set of methods.
//!
//! `docs/daemon_rpc_protocol.md` is what someone writes a client against.
//! When it drifts, both directions hurt and neither is visible from the
//! code: a documented method that does not exist fails at runtime with
//! METHOD_NOT_FOUND, and an implemented method nobody documented is a
//! capability clients never learn about.
//!
//! Both had happened. At the time this test was written the doc described
//! fifteen methods with no dispatch arm — `daemon.ping` among them, the one
//! it recommends for keepalive — and eleven methods were implemented with
//! no entry in the doc.
//!
//! Source-text scanning: match arms out of `dispatch`, `#### \`name\``
//! headings out of §6. It cannot check that a documented *shape* is right,
//! only that both sides agree on what exists. That is the failure that kept
//! happening.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("daemon crate has a parent (repo root)")
        .to_path_buf()
}

/// Method names from `dispatch`'s match arms.
///
/// Reads the arms rather than a hand-kept list, so a new method is covered
/// by this test the moment it is dispatchable — a list would need the same
/// discipline the test exists to enforce.
fn dispatched_methods(server_rs: &str) -> Vec<String> {
    let body = server_rs
        .split_once("match req.method.as_str() {")
        .expect("dispatch still matches on req.method")
        .1;

    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        // `"a.b" => ...` and `"a.b" | "c.d" => ...`, with or without a guard.
        if !line.starts_with('"') || !line.contains("=>") && !line.contains('|') {
            continue;
        }
        let arm = line.split("=>").next().unwrap_or(line);
        for part in arm.split('|') {
            let part = part.trim();
            let Some(name) = part
                .strip_prefix('"')
                .and_then(|rest| rest.split('"').next())
            else {
                continue;
            };
            // A method name, not some other string literal in the arm.
            if name.contains('.') && !out.contains(&name.to_string()) {
                out.push(name.to_string());
            }
        }
        // The wildcard arm ends the table.
        if line.starts_with("_ =>") {
            break;
        }
    }
    out.sort();
    out
}

/// Method names documented as `#### \`name\`` headings.
///
/// A heading may name a pair that is documented together
/// (``#### `config.getProvider` / `config.setProvider` ``), so every
/// backticked name on the line counts.
fn documented_methods(doc: &str) -> Vec<String> {
    let mut out: Vec<String> = doc
        .lines()
        .filter(|line| line.starts_with("#### "))
        .flat_map(|line| {
            line.split('`')
                .skip(1)
                .step_by(2)
                .map(str::trim)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|name| name.contains('.'))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Methods the daemon answers that the doc does not describe.
///
/// `daemon.auth` is the one exception: it is the handshake, specified in
/// §5.1 as part of connecting rather than as a callable method, so it has no
/// §6 heading by design.
const UNDOCUMENTED_BY_DESIGN: &[&str] = &["daemon.auth"];

#[test]
fn every_dispatched_method_is_documented() {
    let root = repo_root();
    let server = std::fs::read_to_string(root.join("daemon/src/server.rs")).unwrap();
    let doc = std::fs::read_to_string(root.join("docs/daemon_rpc_protocol.md")).unwrap();

    let dispatched = dispatched_methods(&server);
    assert!(
        dispatched.len() > 10,
        "only found {dispatched:?} — the dispatch scan is broken, not the doc"
    );
    let documented = documented_methods(&doc);

    let missing: Vec<&String> = dispatched
        .iter()
        .filter(|m| !documented.contains(m) && !UNDOCUMENTED_BY_DESIGN.contains(&m.as_str()))
        .collect();

    assert!(
        missing.is_empty(),
        "these methods are dispatched but not in docs/daemon_rpc_protocol.md §6: {missing:?}\n\
         A capability clients never learn about is not much better than one that does not exist."
    );
}

#[test]
fn every_documented_method_is_dispatched() {
    let root = repo_root();
    let server = std::fs::read_to_string(root.join("daemon/src/server.rs")).unwrap();
    let doc = std::fs::read_to_string(root.join("docs/daemon_rpc_protocol.md")).unwrap();

    let dispatched = dispatched_methods(&server);
    let documented = documented_methods(&doc);
    assert!(
        documented.len() > 10,
        "only found {documented:?} — the doc scan is broken, not the code"
    );

    let phantom: Vec<&String> = documented
        .iter()
        .filter(|m| !dispatched.contains(m))
        .collect();

    assert!(
        phantom.is_empty(),
        "these methods are documented but have no dispatch arm: {phantom:?}\n\
         A client written against the doc gets METHOD_NOT_FOUND. Implement them, or move them \
         out of §6 into a clearly-marked not-implemented list."
    );
}

/// Error codes the doc lists must exist in `rpc::codes`, for the same reason:
/// a client cannot branch on a constant nothing ever returns.
#[test]
fn every_documented_error_code_exists() {
    let root = repo_root();
    let rpc = std::fs::read_to_string(root.join("daemon/src/rpc.rs")).unwrap();
    let doc = std::fs::read_to_string(root.join("docs/daemon_rpc_protocol.md")).unwrap();

    let mut missing = Vec::new();
    for line in doc.lines() {
        // The error table rows: `| -32015 | SESSION_BUSY | … |`
        let mut cells = line.split('|').map(str::trim);
        let (Some(""), Some(code), Some(name)) = (cells.next(), cells.next(), cells.next()) else {
            continue;
        };
        if !code.starts_with("-32") || !name.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            continue;
        }
        if name.is_empty() {
            continue;
        }
        let declaration = format!("{name}: i32 = {code};");
        if !rpc.contains(&declaration) {
            missing.push(format!("{name} ({code})"));
        }
    }

    assert!(
        missing.is_empty(),
        "these error codes are documented but not declared in daemon/src/rpc.rs: {missing:?}"
    );
}

/// Every method the daemon answers is invoked by some test.
///
/// The two tests above keep the document and the dispatch table agreeing
/// about what *exists*. Neither notices a method that exists, is documented,
/// and has nothing driving it — and `plugin.reload` was in exactly that
/// state: its only two call sites sat behind mutually exclusive `#[cfg]`s, so
/// no single build compiled either one in, and CI built both configurations
/// without running either.
///
/// A method counts as driven when a test names it as a JSON-RPC method
/// string, or calls the `rpc-client` wrapper that sends it. Those are the two
/// ways a test reaches the daemon; there is no third.
///
/// `daemon.auth` is checked here even though it never reaches `dispatch` —
/// it is the first message on every network connection, so a client that
/// cannot send it cannot send anything else either.
#[test]
fn every_method_a_client_can_send_is_driven_by_a_test() {
    let root = repo_root();
    let mut required =
        dispatched_methods(&std::fs::read_to_string(root.join("daemon/src/server.rs")).unwrap());
    required.push("daemon.auth".to_string());

    let wrappers = client_wrappers(&root);
    let corpus = test_sources(&root);

    let undriven: Vec<&String> = required
        .iter()
        .filter(|method| {
            let literal = format!("\"{method}\"");
            !corpus.iter().any(|(_, src)| {
                src.contains(&literal)
                    || wrappers
                        .iter()
                        .any(|(f, m)| m == *method && src.contains(&format!(".{f}(")))
            })
        })
        .collect();

    assert!(
        undriven.is_empty(),
        "these methods are dispatched but no test sends them: {undriven:?}\n\
         A method whose only proof is that it compiles is a method nobody has run."
    );
}

/// `rpc-client`'s typed wrappers, paired with the method each one sends, so a
/// test that calls `client.session_get(..)` counts as driving `session.get`.
fn client_wrappers(root: &Path) -> Vec<(String, String)> {
    let src = std::fs::read_to_string(root.join("tests/rpc_client/src/lib.rs"))
        .expect("the typed client exists");
    let mut out = Vec::new();
    // Split rather than slice a window: each chunk already ends where the
    // next wrapper begins, and this file has multi-byte comments that a
    // fixed byte length lands in the middle of.
    for chunk in src.split("pub async fn ").skip(1) {
        let Some(name_end) = chunk.find(['(', '<', ' ']) else {
            continue;
        };
        let name = &chunk[..name_end];
        // The first method string in the body is the one this wrapper sends.
        if let Some(method) = chunk
            .split('"')
            .skip(1)
            .step_by(2)
            .find(|s| s.contains('.') && !s.contains(' ') && !s.contains('/'))
        {
            out.push((name.to_string(), method.to_string()));
        }
    }
    out
}

/// Every test file that can drive a daemon.
///
/// This file is excluded: it names methods to reason about them and sends
/// none of them, which would make it evidence for itself.
fn test_sources(root: &Path) -> Vec<(String, String)> {
    let dirs = [
        "daemon/tests",
        "tests/daemon_harness/tests",
        "tests/rpc_client/tests",
    ];
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "rs")
                && path
                    .file_name()
                    .is_some_and(|n| n != "protocol_doc_matches_dispatch.rs")
            {
                if let Ok(src) = std::fs::read_to_string(&path) {
                    out.push((path.display().to_string(), src));
                }
            }
        }
    }
    assert!(out.len() > 5, "the test corpus scan found almost nothing");
    out
}
