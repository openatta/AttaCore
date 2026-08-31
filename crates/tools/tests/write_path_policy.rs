//! What a write tool refuses, asked through the tool.
//!
//! The engine carried two write-path checks with the same name. The thorough
//! one — credential filenames, system directories, Unicode normalization —
//! documented itself as the guard for `FileWrite` / `FileEdit` and was never
//! called by it; the tools used a ten-line one that only asked whether the
//! path stayed inside the project.
//!
//! It survived that long because every one of its rules had a unit test, and
//! every one of those called the function directly. Nothing asked the tool.
//! So these do.

use base::interface::tool::{PermissionDecision, Tool, ToolContext};
use std::path::PathBuf;

fn ctx_in(cwd: &std::path::Path) -> ToolContext {
    let mut ctx = ToolContext::for_test(cwd.to_path_buf());
    ctx.permission_mode = base::permission::PermissionMode::Default;
    ctx
}

async fn write_decision(ctx: &ToolContext, path: &std::path::Path) -> PermissionDecision {
    tools::file_write::FileWriteTool
        .check_permissions(
            &serde_json::json!({ "file_path": path.display().to_string(), "content": "x" }),
            ctx,
        )
        .await
}

fn denied(d: &PermissionDecision) -> bool {
    matches!(d, PermissionDecision::Deny { .. })
}

/// The credential list, reached the way a model reaches it.
#[tokio::test]
async fn a_credential_file_inside_the_project_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    for name in [".env", ".env.production", "id_rsa", ".npmrc"] {
        let d = write_decision(&ctx, &dir.path().join(name)).await;
        assert!(
            denied(&d),
            "{name} is on the credential list and must not be writable: {d:?}"
        );
    }
    // A nested one too — the rule is about any component, not just the leaf.
    assert!(denied(&write_decision(&ctx, &dir.path().join(".ssh/config")).await));
}

/// And the ordinary case still works, which is what makes the refusals worth
/// anything.
#[tokio::test]
async fn an_ordinary_file_inside_the_project_is_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    for name in ["src/main.rs", "README.md", ".editorconfig"] {
        let d = write_decision(&ctx, &dir.path().join(name)).await;
        assert!(
            matches!(d, PermissionDecision::Allow { .. }),
            "{name} should be an ordinary write: {d:?}"
        );
    }
}

/// The control list is the point: a deployment says which of the defaults do
/// not apply to it. Without this the defaults are a wall, and the only
/// available answer is to stop checking — which is what the engine had been
/// doing.
#[tokio::test]
async fn a_deployment_can_permit_one_of_the_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = ctx_in(dir.path());
    ctx.sandbox.allow_write = vec![dir.path().join(".env.example")];

    assert!(
        matches!(
            write_decision(&ctx, &dir.path().join(".env.example")).await,
            PermissionDecision::Allow { .. }
        ),
        "the exemption has to reach the tool"
    );
    assert!(
        denied(&write_decision(&ctx, &dir.path().join(".env")).await),
        "and it exempts what it names, not the whole rule"
    );
}

/// The other half of a control list: adding to it.
#[tokio::test]
async fn a_deployment_can_add_a_name_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = ctx_in(dir.path());
    ctx.sandbox.deny_write = vec!["secrets".into()];
    assert!(denied(&write_decision(&ctx, &dir.path().join("secrets")).await));
    assert!(denied(&write_decision(&ctx, &dir.path().join("secrets.yaml")).await));
    assert!(matches!(
        write_decision(&ctx, &dir.path().join("public.yaml")).await,
        PermissionDecision::Allow { .. }
    ));
}

/// Leaving the project is a question for a person, not a refusal — that
/// distinction predates this and has to survive it.
#[tokio::test]
async fn writing_outside_the_project_still_asks_rather_than_refusing() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let d = write_decision(&ctx_in(dir.path()), &elsewhere.path().join("note.txt")).await;
    assert!(matches!(d, PermissionDecision::Ask { .. }), "{d:?}");
}

/// An exemption lifts the deny rules and not the project boundary, asked
/// through the tool because that is where the two could be confused.
#[tokio::test]
async fn an_exemption_does_not_open_a_path_outside_the_project() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let mut ctx = ctx_in(dir.path());
    ctx.sandbox.allow_write = vec![elsewhere.path().to_path_buf()];
    let d = write_decision(&ctx, &elsewhere.path().join(".env")).await;
    assert!(
        matches!(d, PermissionDecision::Ask { .. }),
        "still outside the project, so still a question for a person: {d:?}"
    );
}

/// The same policy governs the other two write tools. They had their own
/// copies of the call, so they get their own case.
#[tokio::test]
async fn edit_and_notebook_refuse_the_same_files() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let target = dir.path().join(".env");

    let edit = tools::file_edit::FileEditTool
        .check_permissions(
            &serde_json::json!({
                "file_path": target.display().to_string(),
                "old_string": "a",
                "new_string": "b"
            }),
            &ctx,
        )
        .await;
    assert!(denied(&edit), "{edit:?}");

    let notebook = tools::notebook_edit::NotebookEditTool
        .check_permissions(
            &serde_json::json!({
                "file_path": dir.path().join(".env.ipynb").display().to_string(),
                "mode": "edit",
                "cell_index": 0,
                "new_source": "x"
            }),
            &ctx,
        )
        .await;
    assert!(denied(&notebook), "{notebook:?}");
}

/// A project reached through a symlink is still the project.
///
/// The thorough checker resolved the target and not the roots, so on any
/// machine where the working directory goes through a symlink — `/tmp` is
/// `/private/tmp` on macOS — every write looked like it was leaving a project
/// it was inside.
#[cfg(unix)]
#[tokio::test]
async fn a_project_reached_through_a_symlink_is_still_the_project() {
    let real = tempfile::tempdir().unwrap();
    let link_parent = tempfile::tempdir().unwrap();
    let link: PathBuf = link_parent.path().join("project");
    std::os::unix::fs::symlink(real.path(), &link).unwrap();

    let d = write_decision(&ctx_in(&link), &link.join("src/main.rs")).await;
    assert!(
        matches!(d, PermissionDecision::Allow { .. }),
        "both sides have to be resolved by the same filesystem: {d:?}"
    );
}
