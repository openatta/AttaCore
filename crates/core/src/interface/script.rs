//! `ScriptEngine` — the cheap tier of extension backend.
//!
//! A hook point today can be answered three ways: a Rust implementation
//! compiled in, a command hook that pays for a subprocess, or a prompt hook
//! that pays for a model call. There is nothing between "recompile the engine"
//! and "spawn a process", so doing something small at a hook point — rewrite
//! one line of a prompt, tag a tool result, decide whether to retry — costs
//! either a release or several milliseconds and a PID.
//!
//! This is the missing tier: run a piece of the operator's own code, in this
//! process, in microseconds.
//!
//! # No engine ships with the engine
//!
//! This module is the contract and the governance around it, not an
//! interpreter. Bundling one is a dependency decision — a JavaScript runtime
//! is a large amount of third-party code to link into every build — and the
//! workspace rule is that new dependencies are approved per work order, not
//! taken. So a host supplies the engine, [`RefusingEngine`] is what runs when
//! nobody has, and the "no engine linked unless you asked for one" property
//! holds today because there is no engine to link.
//!
//! # Governance belongs here, not in the engine
//!
//! The timeout and the call quota are enforced by [`ScriptCarrier`], on this
//! side of the trait, because an engine that enforced its own limits would be
//! an engine that could choose not to. What the carrier *cannot* do from here
//! is stop a script that never yields: `tokio::time::timeout` abandons a
//! future, it does not interrupt a busy loop. An engine implementation must
//! provide its own interruption — every serious one has a fuel or watchdog
//! mechanism — and the carrier's timeout is the second line, not the first.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::prompt::BlockOrigin;

/// A piece of code to run, and whose it is.
#[derive(Debug, Clone)]
pub struct ScriptSource {
    /// What to call this in errors and logs — usually its path.
    pub id: String,
    /// Whose code this is. Decides what it is allowed to do at a hook point;
    /// see [`crate::interface::prompt_assembly::Authority`].
    pub origin: BlockOrigin,
    pub code: String,
}

/// What a script may spend.
#[derive(Debug, Clone, Copy)]
pub struct ScriptLimits {
    /// Wall clock for one call.
    pub timeout: Duration,
    /// How many times it may run in a turn. Bounds a hook that is called per
    /// tool result from turning a pathological turn into a pathological bill.
    pub calls_per_turn: u32,
}

impl Default for ScriptLimits {
    fn default() -> Self {
        Self {
            // Generous for the work this tier is for — a string rewrite, a
            // decision — and short enough that a stuck script is a hiccup
            // rather than an outage.
            timeout: Duration::from_millis(100),
            calls_per_turn: 1_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptError {
    /// No engine is available to run it.
    NoEngine,
    /// It ran too long and was abandoned.
    TimedOut { after: Duration },
    /// It has already run as often as it may this turn.
    QuotaExhausted { calls_per_turn: u32 },
    /// The engine refused or the script failed.
    Failed(String),
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoEngine => f.write_str(
                "no script engine is available in this build; a host supplies one through \
                 `ScriptCarrier`",
            ),
            Self::TimedOut { after } => write!(f, "script exceeded its {after:?} budget"),
            Self::QuotaExhausted { calls_per_turn } => {
                write!(f, "script already ran {calls_per_turn} times this turn")
            }
            Self::Failed(e) => write!(f, "script failed: {e}"),
        }
    }
}

/// Runs a script.
///
/// One call, one entry point, JSON in and JSON out — the same shape the
/// command and wasm backends already speak, so a hook point does not need a
/// third input contract to gain a third backend.
#[async_trait]
pub trait ScriptEngine: Send + Sync {
    /// Call `entry` in `script` with `input`.
    ///
    /// Implementations must be able to interrupt a script that does not
    /// return — see the module docs. `limits` is passed for that reason, not
    /// so the engine can decide whether to honor it; the carrier enforces the
    /// same budget from outside regardless.
    async fn eval(
        &self,
        script: &ScriptSource,
        entry: &str,
        input: serde_json::Value,
        limits: &ScriptLimits,
    ) -> Result<serde_json::Value, ScriptError>;
}

/// The engine when there is none.
///
/// Every call fails, with a message that says what is missing rather than
/// what went wrong. A hook point wired to scripts in a build with no engine
/// should be inert and legible, not mysterious.
pub struct RefusingEngine;

#[async_trait]
impl ScriptEngine for RefusingEngine {
    async fn eval(
        &self,
        _script: &ScriptSource,
        _entry: &str,
        _input: serde_json::Value,
        _limits: &ScriptLimits,
    ) -> Result<serde_json::Value, ScriptError> {
        Err(ScriptError::NoEngine)
    }
}

/// An "engine" that is a Rust closure.
///
/// The second implementation the contract needs, and not only for tests: a
/// host that wants the hook-point plumbing — the quota, the budget, the
/// origin-based authority — without an interpreter can express its logic
/// natively and get all of it.
pub struct FnScriptEngine<F>(pub F);

#[async_trait]
impl<F, Fut> ScriptEngine for FnScriptEngine<F>
where
    F: Fn(&ScriptSource, &str, serde_json::Value) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<serde_json::Value, ScriptError>> + Send,
{
    async fn eval(
        &self,
        script: &ScriptSource,
        entry: &str,
        input: serde_json::Value,
        _limits: &ScriptLimits,
    ) -> Result<serde_json::Value, ScriptError> {
        (self.0)(script, entry, input).await
    }
}

/// One script bound to a hook point, with its budget enforced around it.
pub struct ScriptCarrier {
    engine: Arc<dyn ScriptEngine>,
    script: ScriptSource,
    limits: ScriptLimits,
    calls_this_turn: AtomicU32,
}

impl ScriptCarrier {
    pub fn new(engine: Arc<dyn ScriptEngine>, script: ScriptSource, limits: ScriptLimits) -> Self {
        Self {
            engine,
            script,
            limits,
            calls_this_turn: AtomicU32::new(0),
        }
    }

    pub fn script(&self) -> &ScriptSource {
        &self.script
    }

    /// Whose code this is, which is what decides its authority at a point.
    pub fn origin(&self) -> &BlockOrigin {
        &self.script.origin
    }

    /// A new turn has started; the quota resets.
    pub fn begin_turn(&self) {
        self.calls_this_turn.store(0, Ordering::Relaxed);
    }

    /// Run the script, within its budget.
    ///
    /// A script that exhausts its quota or its clock fails *this call* and
    /// nothing else: the turn continues without whatever the script would have
    /// contributed. A hook point that cannot survive its extension failing is
    /// a hook point that hands every extension author a way to break the
    /// engine.
    pub async fn call(
        &self,
        entry: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, ScriptError> {
        let used = self.calls_this_turn.fetch_add(1, Ordering::Relaxed);
        if used >= self.limits.calls_per_turn {
            return Err(ScriptError::QuotaExhausted {
                calls_per_turn: self.limits.calls_per_turn,
            });
        }
        match tokio::time::timeout(
            self.limits.timeout,
            self.engine.eval(&self.script, entry, input, &self.limits),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(ScriptError::TimedOut {
                after: self.limits.timeout,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(origin: BlockOrigin) -> ScriptSource {
        ScriptSource {
            id: "./.atta/scripts/prompt.js".into(),
            origin,
            code: "export function onAssemble(p) { return p }".into(),
        }
    }

    fn carrier(engine: Arc<dyn ScriptEngine>, limits: ScriptLimits) -> ScriptCarrier {
        ScriptCarrier::new(
            engine,
            script(BlockOrigin::Script("./.atta/scripts/prompt.js".into())),
            limits,
        )
    }

    #[tokio::test]
    async fn with_no_engine_a_call_says_what_is_missing() {
        let c = carrier(Arc::new(RefusingEngine), ScriptLimits::default());
        let err = c.call("onAssemble", serde_json::json!({})).await.unwrap_err();
        assert_eq!(err, ScriptError::NoEngine);
        assert!(err.to_string().contains("no script engine"), "{err}");
    }

    #[tokio::test]
    async fn a_script_that_never_returns_is_abandoned_at_its_budget() {
        let engine = Arc::new(FnScriptEngine(|_: &ScriptSource, _: &str, _| async {
            std::future::pending::<()>().await;
            unreachable!()
        }));
        let c = carrier(
            engine,
            ScriptLimits {
                timeout: Duration::from_millis(20),
                ..Default::default()
            },
        );
        let started = std::time::Instant::now();
        let err = c.call("onAssemble", serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, ScriptError::TimedOut { .. }), "{err:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the caller must not wait on a script that never returns"
        );
    }

    /// The isolation that matters: one carrier's stuck script costs its own
    /// call and nothing else's.
    #[tokio::test]
    async fn one_stuck_script_does_not_stop_another_from_running() {
        let stuck = carrier(
            Arc::new(FnScriptEngine(|_: &ScriptSource, _: &str, _| async {
                std::future::pending::<()>().await;
                unreachable!()
            })),
            ScriptLimits {
                timeout: Duration::from_millis(50),
                ..Default::default()
            },
        );
        let fine = carrier(
            Arc::new(FnScriptEngine(|_: &ScriptSource, _: &str, input| async move {
                Ok(input)
            })),
            ScriptLimits::default(),
        );

        let (stuck_out, fine_out) = tokio::join!(
            stuck.call("onAssemble", serde_json::json!({})),
            fine.call("onAssemble", serde_json::json!({"ok": true}))
        );
        assert!(matches!(stuck_out, Err(ScriptError::TimedOut { .. })));
        assert_eq!(fine_out, Ok(serde_json::json!({"ok": true})));
    }

    #[tokio::test]
    async fn the_quota_bounds_a_turn_and_resets_with_it() {
        let engine = Arc::new(FnScriptEngine(|_: &ScriptSource, _: &str, _| async {
            Ok(serde_json::json!("ran"))
        }));
        let c = carrier(
            engine,
            ScriptLimits {
                calls_per_turn: 2,
                ..Default::default()
            },
        );

        assert!(c.call("f", serde_json::json!({})).await.is_ok());
        assert!(c.call("f", serde_json::json!({})).await.is_ok());
        assert_eq!(
            c.call("f", serde_json::json!({})).await.unwrap_err(),
            ScriptError::QuotaExhausted { calls_per_turn: 2 }
        );

        c.begin_turn();
        assert!(
            c.call("f", serde_json::json!({})).await.is_ok(),
            "a new turn is a new budget"
        );
    }

    /// Provenance rides with the script, because it is what a hook point uses
    /// to decide what the script may do — see `prompt_assembly::Authority`.
    #[test]
    fn a_script_carries_whose_it_is() {
        let own = carrier(Arc::new(RefusingEngine), ScriptLimits::default());
        assert!(own.origin().is_local());

        let downloaded = ScriptCarrier::new(
            Arc::new(RefusingEngine),
            script(BlockOrigin::Plugin("example".into())),
            ScriptLimits::default(),
        );
        assert!(!downloaded.origin().is_local());
    }
}

/// A script bound to the prompt-assembly hook point.
///
/// The bridge between "the operator wrote some code" and "the prompt came out
/// different": the script is handed the assembled blocks as JSON, returns the
/// blocks it wants, and every difference is applied through
/// [`PromptAssembly`](crate::interface::prompt_assembly::PromptAssembly) —
/// which is what subjects it to the same authority rules as any other
/// extension. A script the operator wrote may rewrite anything; a script that
/// arrived with a downloaded plugin may add.
///
/// A script that fails, times out or returns nonsense leaves the prompt as it
/// was. That is not politeness: a prompt half-edited by a script that died
/// mid-pass is worse than an unedited one, because nothing downstream can tell
/// which it is looking at.
pub struct PromptAssemblyScript {
    carrier: ScriptCarrier,
    entry: String,
}

impl PromptAssemblyScript {
    /// `entry` is the function the script exports, e.g. `onAssemble`.
    pub fn new(carrier: ScriptCarrier, entry: impl Into<String>) -> Self {
        Self {
            carrier,
            entry: entry.into(),
        }
    }

    /// What the script is given: one object per block, in order.
    fn encode(blocks: &[crate::prompt::PromptBlock]) -> serde_json::Value {
        serde_json::Value::Array(
            blocks
                .iter()
                .map(|b| {
                    serde_json::json!({
                        "name": b.name,
                        "content": b.content,
                    })
                })
                .collect(),
        )
    }
}

/// What came back, in the shape the applier can act on.
#[derive(serde::Deserialize)]
struct ReturnedBlock {
    name: Option<String>,
    content: String,
}

#[async_trait]
impl crate::interface::prompt_assembly::AsyncAssemblyHook for PromptAssemblyScript {
    async fn on_assemble_async(
        &self,
        assembly: &mut crate::interface::prompt_assembly::PromptAssembly,
        _ctx: &crate::interface::scene::ScenePromptContext<'_>,
    ) -> Result<(), String> {
        let before = Self::encode(assembly.blocks());
        let returned = self
            .carrier
            .call(&self.entry, before)
            .await
            .map_err(|e| e.to_string())?;

        let returned: Vec<ReturnedBlock> = serde_json::from_value(returned)
            .map_err(|e| format!("script returned something that is not a block list: {e}"))?;

        // Diffed rather than replaced wholesale, so every change goes through
        // the authority checks one at a time. Handing back a list and swapping
        // it in would let a script "add" a prompt that happens to be missing
        // the block it was not allowed to remove.
        let existing: Vec<(String, String)> = assembly
            .blocks()
            .iter()
            .filter_map(|b| b.name.clone().map(|n| (n, b.content.clone())))
            .collect();

        // A denied edit does not cancel the permitted ones. A script that
        // asked for one thing it may not do still gets everything it may —
        // the alternative punishes an author for a single overreach by
        // silently dropping the rest of their pass.
        let mut refused = Vec::new();
        for block in &returned {
            let Some(name) = block.name.as_deref() else {
                continue;
            };
            match existing.iter().find(|(n, _)| n == name) {
                // Handing a block back unchanged is not an edit, and must not
                // be charged as one: a script that returns the whole list to
                // change one line would otherwise need `modify` for all of it.
                Some((_, content)) if *content == block.content => {}
                Some(_) => {
                    if let Err(e) = assembly.modify(name, block.content.clone()) {
                        refused.push(e.to_string());
                    }
                }
                None => assembly.push(
                    crate::prompt::PromptBlock::system(block.content.clone())
                        .named(name)
                        .from_origin(self.carrier.origin().clone()),
                ),
            }
        }
        for (name, _) in &existing {
            if !returned
                .iter()
                .any(|b| b.name.as_deref() == Some(name.as_str()))
            {
                if let Err(e) = assembly.remove(name) {
                    refused.push(e.to_string());
                }
            }
        }
        if refused.is_empty() {
            Ok(())
        } else {
            Err(refused.join("; "))
        }
    }
}

#[cfg(test)]
mod prompt_hook_tests {
    use super::*;
    use crate::interface::prompt_assembly::{
        run_async_assembly_hooks, AssemblyCapabilities, AsyncAssemblyHook, Authority,
    };
    use crate::prompt::{names, PromptBlock};

    fn ctx() -> crate::interface::scene::ScenePromptContext<'static> {
        use std::borrow::Cow;
        crate::interface::scene::ScenePromptContext {
            cwd: Cow::Borrowed("/tmp"),
            os: Cow::Borrowed("linux"),
            shell: Cow::Borrowed("bash"),
            home_dir: Cow::Borrowed("/home/user"),
            date: Cow::Borrowed("2026-06-10"),
            model_name: Cow::Borrowed("test-model"),
            skills_text: None,
            mcp_instructions: None,
            session_memory: None,
            is_git: false,
            git_branch: None,
            is_worktree: false,
            git_status: None,
            language: None,
            scratchpad_dir: None,
            output_style_content: None,
            available_tools: None,
            tool_results_ever_cleared: false,
        }
    }

    fn prompt() -> Vec<PromptBlock> {
        vec![
            PromptBlock::system("you are an agent").named(names::SCENE_SKELETON),
            PromptBlock::system("skills: a, b").named(names::SKILLS_CATALOG),
            PromptBlock::system("rules: no rm -rf").named(names::RULES),
        ]
    }

    /// Stands in for a real interpreter: it receives the blocks as JSON,
    /// transforms them, and hands them back — exactly the contract a scripted
    /// `onAssemble` would satisfy.
    fn engine_that(
        f: impl Fn(Vec<serde_json::Value>) -> Vec<serde_json::Value> + Send + Sync + Clone + 'static,
    ) -> Arc<dyn ScriptEngine> {
        Arc::new(FnScriptEngine(
            move |_s: &ScriptSource, _e: &str, input: serde_json::Value| {
                let f = f.clone();
                async move {
                    let blocks: Vec<serde_json::Value> =
                        serde_json::from_value(input).map_err(|e| ScriptError::Failed(e.to_string()))?;
                    Ok(serde_json::Value::Array(f(blocks)))
                }
            },
        ))
    }

    fn script_hook(engine: Arc<dyn ScriptEngine>, origin: BlockOrigin) -> PromptAssemblyScript {
        PromptAssemblyScript::new(
            ScriptCarrier::new(
                engine,
                ScriptSource {
                    id: "./.atta/scripts/prompt.js".into(),
                    origin,
                    code: String::new(),
                },
                ScriptLimits::default(),
            ),
            "onAssemble",
        )
    }

    /// The acceptance case: a script rewrites a prompt block, and the prompt
    /// that goes out is different. No recompilation of the engine involved —
    /// the code came from outside it.
    #[tokio::test]
    async fn a_script_the_operator_wrote_rewrites_a_block() {
        let engine = engine_that(|blocks| {
            blocks
                .into_iter()
                .map(|mut b| {
                    if b["name"] == serde_json::json!(names::SKILLS_CATALOG) {
                        b["content"] = serde_json::json!("skills: (curated by script)");
                    }
                    b
                })
                .collect()
        });
        let hook = script_hook(
            engine,
            BlockOrigin::Script("./.atta/scripts/prompt.js".into()),
        );
        let hooks: Vec<(Arc<dyn AsyncAssemblyHook>, Authority)> = vec![(
            Arc::new(hook),
            Authority::local(BlockOrigin::Script("./.atta/scripts/prompt.js".into())),
        )];

        let out = run_async_assembly_hooks(prompt(), &hooks, &ctx()).await;
        let skills = out
            .iter()
            .find(|b| b.name.as_deref() == Some(names::SKILLS_CATALOG))
            .unwrap();
        assert_eq!(skills.content, "skills: (curated by script)");
    }

    /// The same script, arriving with a downloaded plugin that declared
    /// nothing: its additions land, its deletion does not.
    #[tokio::test]
    async fn a_downloaded_script_is_held_to_the_same_rules_as_any_plugin() {
        let engine = engine_that(|blocks| {
            let mut kept: Vec<serde_json::Value> = blocks
                .into_iter()
                .filter(|b| b["name"] != serde_json::json!(names::RULES))
                .collect();
            kept.push(serde_json::json!({"name": "example.note", "content": "hello"}));
            kept
        });
        let hook = script_hook(engine, BlockOrigin::Plugin("example".into()));
        let hooks: Vec<(Arc<dyn AsyncAssemblyHook>, Authority)> = vec![(
            Arc::new(hook),
            Authority::plugin("example", AssemblyCapabilities::default()),
        )];

        let out = run_async_assembly_hooks(prompt(), &hooks, &ctx()).await;
        let names_out: Vec<&str> = out.iter().filter_map(|b| b.name.as_deref()).collect();
        assert!(
            names_out.contains(&names::RULES),
            "an undeclared removal must not take effect: {names_out:?}"
        );
        assert!(
            names_out.contains(&"example.note"),
            "its addition must stand: {names_out:?}"
        );
    }

    #[tokio::test]
    async fn a_script_that_returns_nonsense_leaves_the_prompt_alone() {
        let engine: Arc<dyn ScriptEngine> = Arc::new(FnScriptEngine(
            |_s: &ScriptSource, _e: &str, _i: serde_json::Value| async {
                Ok(serde_json::json!("not a block list"))
            },
        ));
        let hook = script_hook(
            engine,
            BlockOrigin::Script("./.atta/scripts/prompt.js".into()),
        );
        let hooks: Vec<(Arc<dyn AsyncAssemblyHook>, Authority)> = vec![(
            Arc::new(hook),
            Authority::local(BlockOrigin::Script("./.atta/scripts/prompt.js".into())),
        )];

        let out = run_async_assembly_hooks(prompt(), &hooks, &ctx()).await;
        assert_eq!(out, prompt(), "a broken script is not a prompt edit");
    }

    #[tokio::test]
    async fn a_script_that_hangs_leaves_the_prompt_alone() {
        let engine: Arc<dyn ScriptEngine> = Arc::new(FnScriptEngine(
            |_s: &ScriptSource, _e: &str, _i: serde_json::Value| async {
                std::future::pending::<()>().await;
                unreachable!()
            },
        ));
        let hook = PromptAssemblyScript::new(
            ScriptCarrier::new(
                engine,
                ScriptSource {
                    id: "./.atta/scripts/slow.js".into(),
                    origin: BlockOrigin::Script("./.atta/scripts/slow.js".into()),
                    code: String::new(),
                },
                ScriptLimits {
                    timeout: Duration::from_millis(20),
                    ..Default::default()
                },
            ),
            "onAssemble",
        );
        let hooks: Vec<(Arc<dyn AsyncAssemblyHook>, Authority)> = vec![(
            Arc::new(hook),
            Authority::local(BlockOrigin::Script("./.atta/scripts/slow.js".into())),
        )];

        let out = run_async_assembly_hooks(prompt(), &hooks, &ctx()).await;
        assert_eq!(out, prompt(), "the turn goes on without the script's edits");
    }
}
