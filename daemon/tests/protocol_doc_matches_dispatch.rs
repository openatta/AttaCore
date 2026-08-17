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
