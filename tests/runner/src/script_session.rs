//! Drive a real session with scripts bound, and hand back what can be seen
//! from outside it.
//!
//! Three test binaries ask the same question in different shapes — does a
//! bound script change what happens — and the answer is only worth anything if
//! all three go through the path a session really takes: real `Settings`, the
//! `Builder`'s own installation of the adapters, a real turn loop. A driver
//! written once per binary drifts, and a drifted driver is worse than no test,
//! because it keeps reporting on an engine that does not exist.
//!
//! What a run can be observed through is deliberately small, and it is the
//! whole observable surface a script has:
//!
//! - the requests that reached the model, as text and as tool lists;
//! - the transcript, when the run was asked to keep one;
//! - the working directory the tools acted on;
//! - the ledger, which is the only place a script that did *not* run says so.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base::interface::memory::MemoryStore;
use base::interface::model::Model;
use base::interface::scene::AgentScene;
use base::interface::script::{ScriptLedger, ScriptOutcome, ScriptRecord};
use base::interface::settings::{PathSettings, ScriptBinding, Settings, ThinkingMode};
use base::interface::tool::{InMemoryToolRegistry, Tool};
use runtime::agent::{Builder, InputMessage};
use tokio_util::sync::CancellationToken;

use crate::scripted_model::{Call, Reply, ScriptedModel};

/// One session to drive: what is bound, what the user says, what the model
/// answers.
pub struct Session {
    /// Where relative script paths resolve from — and, because authority
    /// follows the file rather than the binding, what decides whether a
    /// script counts as the operator's own.
    fixtures: PathBuf,
    bindings: Vec<ScriptBinding>,
    turns: Vec<String>,
    replies: Vec<Reply>,
    tools: Vec<Arc<dyn Tool>>,
    memory: Option<Arc<MemoryStore>>,
    keep_log: bool,
    project: Option<PathBuf>,
    scene: Option<Arc<dyn AgentScene>>,
    asking: bool,
    #[allow(clippy::type_complexity)]
    between_turns: Option<Box<dyn Fn(usize, &Path) + Send>>,
}

impl Session {
    pub fn new(fixtures: impl Into<PathBuf>, turns: &[&str], replies: Vec<Reply>) -> Self {
        Self {
            fixtures: fixtures.into(),
            bindings: Vec::new(),
            turns: turns.iter().map(|t| (*t).to_string()).collect(),
            replies,
            tools: Vec::new(),
            memory: None,
            keep_log: false,
            project: None,
            scene: None,
            asking: false,
            between_turns: None,
        }
    }

    /// Bind `path` at `point`. Order is the order of these calls, which is
    /// what decides which of two scripts on one point is the outer one.
    pub fn bind(self, path: &str, point: &str, entry: &str) -> Self {
        self.bind_within(path, point, entry, None, None)
    }

    /// The same, with this binding's own budget.
    pub fn bind_within(
        mut self,
        path: &str,
        point: &str,
        entry: &str,
        timeout_ms: Option<u64>,
        calls_per_turn: Option<u32>,
    ) -> Self {
        self.bindings.push(ScriptBinding {
            path: path.into(),
            point: point.into(),
            entry: entry.into(),
            timeout_ms,
            calls_per_turn,
        });
        self
    }

    pub fn tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// Recall against a store the caller has already seeded.
    ///
    /// The shipped retriever asks the model which memories are relevant, and
    /// a scripted model has no answer to give, so this switches to the
    /// substring one — the other shipped implementation, and the one that
    /// makes recall a pure function of the query. That is what lets a
    /// rewritten query be the thing under test.
    pub fn memory(mut self, store: Arc<MemoryStore>) -> Self {
        self.memory = Some(store);
        self
    }

    /// Attach a history store, so the run leaves a transcript to read back.
    pub fn logged(mut self) -> Self {
        self.keep_log = true;
        self
    }

    /// Leave the permission mode where a real interactive session has it, so
    /// a tool that wants approval actually asks for it.
    ///
    /// The driver answers every prompt with a denial and writes down that it
    /// was asked. Denial rather than approval because what these cases are
    /// about is whether the question was *reached*, and a run that approved
    /// everything would go on to execute the tool it was asking about.
    pub fn asking(mut self) -> Self {
        self.asking = true;
        self
    }

    /// Run `f(turn_index, root)` after each turn completes, before the next
    /// one is sent.
    ///
    /// For the cases that are about *when* something is read: a file changed
    /// between two turns of one session answers a question no amount of
    /// setup before the session can.
    pub fn between_turns(mut self, f: impl Fn(usize, &Path) + Send + 'static) -> Self {
        self.between_turns = Some(Box::new(f));
        self
    }

    /// Which scene to run under. Chat by default, because most cases only
    /// need a turn loop; a case about tools needs the scene that offers them.
    pub fn scene(mut self, scene: Arc<dyn AgentScene>) -> Self {
        self.scene = Some(scene);
        self
    }

    /// Root the session at a project directory, the way a real one is rooted:
    /// `Settings::load` from its `.atta`, so tools, rules and skills all
    /// resolve against it.
    pub fn in_project(mut self, dir: impl Into<PathBuf>) -> Self {
        self.project = Some(dir.into());
        self
    }
}

/// What one run left behind.
pub struct Ran {
    /// Every request that reached the model, as text: system prompt and
    /// messages together.
    pub requests: Vec<String>,
    /// The tools each of those requests offered, by name.
    pub tools: Vec<Vec<String>>,
    /// The same requests reduced to what a decision can move.
    pub calls: Vec<Call>,
    /// The transcript the run left, or empty if it kept none.
    pub log: String,
    /// The tools this run stopped to ask about, in order.
    pub prompts: Vec<String>,
    pub ledger: Arc<ScriptLedger>,
}

impl Ran {
    pub fn holds(&self, needle: &str) -> bool {
        self.requests.iter().any(|r| r.contains(needle))
    }

    /// How many calls the session's own model answered.
    ///
    /// Not every request in a run belongs to the turn: memory work and
    /// summarization reach for the small model on their own schedule, and a
    /// case that counted every request would be counting those too.
    pub fn calls_to(&self, model: &str) -> usize {
        self.calls.iter().filter(|c| c.model == model).count()
    }

    pub fn offers(&self, tool: &str) -> bool {
        self.tools.iter().any(|t| t.iter().any(|n| n == tool))
    }

    pub fn records_at(&self, point: &str) -> Vec<ScriptRecord> {
        self.ledger
            .records()
            .into_iter()
            .filter(|r| r.point == point)
            .collect()
    }

    pub fn outcomes_at(&self, point: &str) -> Vec<ScriptOutcome> {
        self.records_at(point)
            .into_iter()
            .map(|r| r.outcome)
            .collect()
    }
}

/// Run the session under `root`, which the caller owns — a task case needs to
/// read the working directory after the run, and a directory that vanished
/// with the driver could not be read.
pub async fn drive(root: &Path, session: Session) -> Ran {
    let workdir = root.join("workdir");
    std::fs::create_dir_all(&workdir).expect("workdir");

    let model_arc = ScriptedModel::new(session.replies);
    let model: Arc<dyn Model> = model_arc.clone();

    let mut settings = match &session.project {
        Some(dir) => Settings::load(
            root.join("global_empty"),
            root.join("scene_empty"),
            dir.join(".atta"),
            "code",
            "script-model",
        ),
        None => Settings::defaults_for("script-model"),
    };
    settings.model.model_name = "script-model".into();
    settings.model.max_tokens = 1024;
    settings.model.thinking_mode = ThinkingMode::Off;
    settings.paths = PathSettings {
        user_data_dir: root.join("user"),
        global_data_dir: root.join("global"),
        local_data_dir: root.join("local"),
        scope: "code".into(),
    };
    settings.memory_enabled = session.memory.is_some();
    // The same choice the daemon makes for a non-interactive host: nothing is
    // here to answer a prompt, and a case that hung on one would look like a
    // case whose turn never finished.
    if !session.asking {
        settings.permission_mode = base::interface::settings::PermissionMode::BypassPermissions;
    } else {
        settings.permission_mode = base::interface::settings::PermissionMode::Default;
    }
    settings.scripts = session.bindings;
    let settings = Arc::new(settings);

    let registry = Arc::new(InMemoryToolRegistry::new());
    for tool in session.tools {
        registry.register(tool);
    }

    let scene: Arc<dyn AgentScene> = session
        .scene
        .unwrap_or_else(|| Arc::new(scene::scene::chat::ChatScene));

    // The gate the daemon builds for a session that is not bypassing, rather
    // than the builder's own allow-everything default: a case about the
    // ordering between a script and the gate needs a gate that really asks.
    let permission: Option<Arc<dyn base::interface::permission::Permission>> = session
        .asking
        .then(|| {
            Arc::new(permissions::rule_set_permission::RuleSetPermission::from_settings(
                &settings,
                base::interface::settings::PermissionMode::Default.into(),
                registry.clone(),
                Vec::new(),
            )) as Arc<dyn base::interface::permission::Permission>
        });
    let mut builder = Builder::new()
        .scene(scene)
        .model(model)
        .tools(registry)
        .settings(settings.clone())
        .skip_warmup(true);

    if let Some(p) = permission {
        builder = builder.permission(p);
    }

    if session.keep_log {
        let store = history::store::JsonlHistoryStore::with_roots(
            &workdir,
            history::path::HistoryRoots::under(&root.join("global")),
        )
        .await
        .expect("history store");
        builder = builder.history_store(Arc::new(store));
    }

    if let Some(store) = session.memory {
        builder = builder.memory_store(store);
        builder = builder.memory_retriever(Arc::new(
            base::interface::memory_contracts::SubstringRetriever,
        ));
    }

    // The same call the daemon makes. Installing the adapters is `Builder`'s
    // job precisely so a test cannot bind a point a session would not.
    let ledger = match script_host::bindings::bind_quickjs(&settings.scripts, &session.fixtures)
        .unwrap_or_else(|e| panic!("a binding in this session is invalid: {e}"))
    {
        Some(bound) => {
            let ledger = bound.ledger.clone();
            builder = builder.bound_scripts(bound);
            ledger
        }
        None => Arc::new(ScriptLedger::new()),
    };

    let (mut agent, mut event_rx, input_tx) = builder.build().expect("agent builds");

    let cancel = CancellationToken::new();
    let run_cancel = cancel.clone();
    let join = tokio::spawn(async move { agent.run(run_cancel).await });

    let mut prompts = Vec::new();
    for (i, turn) in session.turns.iter().enumerate() {
        input_tx
            .send(InputMessage::User {
                content: turn.to_string(),
                attachments: vec![],
                turn_id: format!("t{i}"),
            })
            .expect("send turn");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let ev = tokio::time::timeout_at(deadline, event_rx.recv())
                .await
                .expect("the turn never finished")
                .expect("event channel closed early");
            match ev {
                base::event::AgentEvent::PermissionPrompt {
                    prompt_id,
                    tool_name,
                    ..
                } => {
                    prompts.push(tool_name);
                    // Nothing here can ask a person, and an unanswered prompt
                    // is a turn that never ends — which would report as a hang
                    // rather than as the question it is.
                    let _ = input_tx.send(InputMessage::PermissionResponse {
                        prompt_id,
                        decision: runtime::agent::PermissionDecision::Deny {
                            reason: "no one is here to approve this".into(),
                        },
                    });
                }
                base::event::AgentEvent::TurnComplete { .. }
                | base::event::AgentEvent::Error { .. } => break,
                _ => {}
            }
        }

        if let Some(f) = &session.between_turns {
            f(i, root);
        }
    }

    cancel.cancel();
    drop(input_tx);
    let _ = join.await;

    Ran {
        requests: model_arc.request_texts(),
        tools: model_arc.request_tools(),
        calls: model_arc.calls(),
        log: read_log(&root.join("global")),
        prompts,
        ledger,
    }
}

fn read_log(dir: &Path) -> String {
    fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect(&p, out);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                out.push(p);
            }
        }
    }
    let mut logs = Vec::new();
    collect(dir, &mut logs);
    logs.iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect::<Vec<_>>()
        .join("\n")
}
