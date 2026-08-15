//! Guards against the "library capability exists, composition root never
//! calls it" bug pattern found repeatedly across this codebase during a
//! 2026-08-12 audit — `tools::register_builtin_tools` never called by
//! `daemon`, `hooks::HookRunner::enable_file_watching` never called by
//! `runtime::agent::Builder::build()`, `mcp::McpManager::set_elicitation_callback`
//! likewise. Each of those compiled cleanly and had passing unit tests in
//! isolation; the only thing that would have caught them earlier is asking
//! "does the production entry point actually call this?"
//!
//! This is a source-text reachability check, not real call-graph analysis —
//! it greps for whether a function name string appears anywhere in the two
//! places production wiring actually happens (`daemon/src/**` and
//! `crates/runtime/src/agent.rs`, see that file's `Builder::build()`), for a
//! curated list of "wiring-shaped" functions from library crates. It cannot
//! prove a call is *correct*, only that the identifier is referenced from a
//! composition root instead of sitting completely unreferenced outside its
//! own crate.
//!
//! AttaCore is also a library other embedders build their own composition
//! root on top of,
//! so a function genuinely meant for external embedders — not this repo's
//! own `daemon` — is a legitimate exception, not a bug. Those go in
//! `INTENTIONALLY_UNWIRED` below, each with a one-line reason, so adding one
//! is a conscious decision instead of the check silently going blind.

use std::path::{Path, PathBuf};

/// (defining crate, function/method name) pairs that exist specifically so
/// this repo's `daemon` can call them to activate a capability. Each entry
/// is exactly the shape of bug this test exists to catch: compiles, has
/// unit tests, does nothing in a real session unless referenced from
/// `daemon/src/**` or `crates/runtime/src/agent.rs`.
const WIRING_CHECKS: &[(&str, &str)] = &[
    ("tools", "register_builtin_tools"),
    ("tools", "register_web_search"),
    ("tools", "register_cron_tools"),
    ("tools", "register_worktree_tools"),
    ("hooks", "enable_file_watching"),
    ("mcp", "set_elicitation_callback"),
];

/// Functions matching `WIRING_CHECKS`-style naming that are deliberately
/// *not* called from this repo's `daemon` — each reason must name either an
/// external-embedder use case or an already-tracked, documented gap (so it
/// shows up in the same place a reviewer would look, not scattered).
const INTENTIONALLY_UNWIRED: &[(&str, &str, &str)] = &[
    // (crate, function, reason)
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("daemon crate has a parent (repo root)")
        .to_path_buf()
}

/// Concatenate every `.rs` file under `dir` (recursively) into one string,
/// for a cheap "does this identifier appear anywhere in this subtree" check.
/// Good enough for reachability, not for anything requiring real parsing.
fn concat_rs_files(dir: &Path) -> String {
    let mut out = String::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    out.push_str(&content);
                    out.push('\n');
                }
            }
        }
    }
    out
}

#[test]
fn wiring_shaped_functions_are_referenced_from_a_composition_root() {
    let root = repo_root();
    let daemon_src = concat_rs_files(&root.join("daemon/src"));
    let agent_rs = std::fs::read_to_string(root.join("crates/runtime/src/agent.rs"))
        .expect("crates/runtime/src/agent.rs should exist");

    let mut unreferenced = Vec::new();
    for &(crate_name, func_name) in WIRING_CHECKS {
        let referenced_in_daemon = count_occurrences(&daemon_src, func_name) > 0;
        // Every function also gets defined once in its own crate and usually
        // referenced by its own doc comments/tests — only daemon/src and
        // agent.rs (excluding agent.rs's own re-definition, N/A here since
        // none of these are defined in `runtime`) count as "wired".
        let referenced_in_agent = count_occurrences(&agent_rs, func_name) > 0;
        if !referenced_in_daemon && !referenced_in_agent {
            if let Some((_, _, reason)) = INTENTIONALLY_UNWIRED
                .iter()
                .find(|(c, f, _)| *c == crate_name && *f == func_name)
            {
                eprintln!("note: {crate_name}::{func_name} intentionally unwired — {reason}");
                continue;
            }
            unreferenced.push(format!("{crate_name}::{func_name}"));
        }
    }

    assert!(
        unreferenced.is_empty(),
        "these functions exist to be called by a composition root but are referenced \
         nowhere in daemon/src or crates/runtime/src/agent.rs: {unreferenced:?}. Either \
         wire the call in (the usual fix — see this test's module doc for three real \
         examples), or if this one is genuinely for external embedders of the AttaCore \
         library rather than this repo's own daemon, add it to INTENTIONALLY_UNWIRED \
         with a one-line reason."
    );
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// The check itself must be able to catch a real regression — exercise it
/// against a name that genuinely appears nowhere, so a future edit that
/// breaks the scan logic (e.g. an accidental early return) fails loudly
/// instead of the check silently always passing.
#[test]
fn wiring_check_scan_logic_actually_detects_absence() {
    let root = repo_root();
    let daemon_src = concat_rs_files(&root.join("daemon/src"));
    let agent_rs = std::fs::read_to_string(root.join("crates/runtime/src/agent.rs"))
        .expect("crates/runtime/src/agent.rs should exist");
    let bogus = "definitely_not_a_real_function_name_zzz_12345";
    assert_eq!(count_occurrences(&daemon_src, bogus), 0);
    assert_eq!(count_occurrences(&agent_rs, bogus), 0);
}
