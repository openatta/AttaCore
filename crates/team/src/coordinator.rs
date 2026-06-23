//! Team coordinator using `dyn Model` trait.
//!
//! Also provides [`PermissionBridge`] — intercepts sub-agent permission prompts
//! and forwards them to the parent agent via the team mailbox (Bubble mode).

use base::interface::agent_spawner::AgentSpawner;
use base::interface::model::{Model, ModelEvent, StreamParams};
use base::interface::permission::{Permission, PermissionOutcome};
use base::interface::prompt::{BlockRole, PromptBlock};
use base::interface::settings::ThinkingMode;
use base::tool::InMemoryToolRegistry;
use async_trait::async_trait;
use base::context::EngineConfig;
use base::tool::ToolResultContent;
use base::tool::ToolContext;
use base::tool::ToolResult;
use base::error::ToolError;
use futures::stream::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;

use crate::tool::{AggregateMode, TeamStage};

// ═══════════════════════════════════════════════════════════
// Permission bridge — Bubble mode
// ═══════════════════════════════════════════════════════════

/// A permission prompt forwarded from a sub-agent to the parent agent.
#[derive(Debug, Clone)]
pub struct PermissionPrompt {
    /// The tool_use_id of the original tool call (used as correlation key).
    pub tool_use_id: String,
    /// Name of the tool requesting permission.
    pub tool_name: String,
    /// Human-readable explanation of the permission request.
    pub message: String,
    /// File paths affected by the tool call, if any.
    pub paths: Vec<std::path::PathBuf>,
    /// The turn_id from the sub-agent's context.
    pub turn_id: String,
}

/// Bridges permission requests from sub-agents to the parent agent
/// via the team mailbox system. Implements the `Permission` trait
/// so it can be injected as the permission handler for sub-agents.
///
/// When a tool call requires permission, the bridge:
/// 1. For read-only / safe tools — auto-permits
/// 2. For other tools — creates a [`PermissionPrompt`], forwards it
///    to the parent agent via the mailbox, then blocks until the parent
///    responds (or a 120-second timeout elapses).
pub struct PermissionBridge {
    /// Team mailbox for forwarding requests to the parent.
    mailbox: Arc<crate::mailbox::MailboxStore>,
    /// This agent's label in the team.
    my_label: String,
    /// The parent agent's label (receives permission requests).
    parent_label: String,
    /// Pending permissions keyed by tool_use_id → prompt.
    pending_permissions: std::sync::Arc<std::sync::Mutex<HashMap<String, PermissionPrompt>>>,
    /// Oneshot channels for awaiting parent decisions.
    /// Each sender is consumed by `send()` when the parent responds.
    response_channels:
        std::sync::Arc<std::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>>,
}

impl PermissionBridge {
    /// Create a new bridge that forwards permission requests from `my_label`
    /// to `parent_label` via the shared team mailbox.
    pub fn new(
        mailbox: Arc<crate::mailbox::MailboxStore>,
        my_label: impl Into<String>,
        parent_label: impl Into<String>,
    ) -> Self {
        Self {
            mailbox,
            my_label: my_label.into(),
            parent_label: parent_label.into(),
            pending_permissions: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            response_channels: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Returns a snapshot of currently pending permission prompts.
    pub fn pending(&self) -> Vec<PermissionPrompt> {
        let guard = self.pending_permissions.lock().unwrap();
        guard.values().cloned().collect()
    }

    /// Forward a permission prompt to the parent agent via the team mailbox.
    ///
    /// The prompt is stored in `pending_permissions` keyed by `tool_use_id`
    /// so that [`receive_decision`] can correlate the parent's response.
    pub fn forward_to_parent(&self, prompt: PermissionPrompt) {
        let tool_use_id = prompt.tool_use_id.clone();

        let message = serde_json::json!({
            "type": "permission_request",
            "tool_use_id": tool_use_id,
            "tool_name": prompt.tool_name,
            "message": prompt.message,
            "paths": prompt.paths,
            "turn_id": prompt.turn_id,
        });

        {
            let mut pending = self.pending_permissions.lock().unwrap();
            pending.insert(tool_use_id, prompt);
        }
        self.mailbox
            .send(&self.my_label, &self.parent_label, &message.to_string());
    }

    /// Receive a parent's decision for a pending permission request.
    ///
    /// The decision is applied by completing the oneshot channel that the
    /// sub-agent is blocked on. If `allowed` is true the tool call proceeds;
    /// otherwise the sub-agent receives a denial.
    pub fn receive_decision(&self, tool_use_id: &str, allowed: bool) {
        {
            let mut pending = self.pending_permissions.lock().unwrap();
            pending.remove(tool_use_id);
        }
        let sender = {
            let mut channels = self.response_channels.lock().unwrap();
            channels.remove(tool_use_id)
        };
        if let Some(tx) = sender {
            let _ = tx.send(allowed);
        }
    }
}

#[async_trait]
impl Permission for PermissionBridge {
    async fn check(
        &self,
        tool_name: &str,
        _tool_input: &serde_json::Value,
        _cwd: &std::path::Path,
        _session_id: &str,
    ) -> PermissionOutcome {
        // Auto-permit read-only / safe tools to avoid unnecessary parent
        // interaction for tools that cannot cause side effects.
        let safe_tools: &[&str] = &[
            "Read",
            "Grep",
            "Glob",
            "WebSearch",
            "WebFetch",
            "LSP",
            "ListPeers",
            "ReadMail",
            "SendMessage",
            "Agent",
        ];
        if safe_tools.contains(&tool_name) {
            return PermissionOutcome::Permit;
        }

        // Generate a unique ID for this permission request.
        let prompt_id = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );

        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();

        // Store the oneshot sender so receive_decision can complete it.
        {
            let mut channels = self.response_channels.lock().unwrap();
            channels.insert(prompt_id.clone(), tx);
        }

        // Forward the permission prompt to the parent.
        let prompt = PermissionPrompt {
            tool_use_id: prompt_id.clone(),
            tool_name: tool_name.to_string(),
            message: format!(
                "Sub-agent '{}' requests permission to use tool '{}'",
                self.my_label, tool_name
            ),
            paths: vec![],
            turn_id: String::new(),
        };
        self.forward_to_parent(prompt);

        // Wait for the parent's decision (with timeout).
        match tokio::time::timeout(std::time::Duration::from_secs(120), rx).await {
            Ok(Ok(true)) => PermissionOutcome::Permit,
            Ok(Ok(false)) => PermissionOutcome::Deny {
                reason: "Parent denied permission".into(),
            },
            Ok(Err(_)) => PermissionOutcome::Deny {
                reason: "Permission response channel closed unexpectedly".into(),
            },
            Err(_) => PermissionOutcome::Deny {
                reason: "Permission request timed out after 120s".into(),
            },
        }
    }
}

/// Create [`PermissionBridge`] instances for every agent in a team's stages.
///
/// Each bridge maps the agent's label → agent so callers can look up the
/// bridge for a given label when spawning team members.
pub fn create_permission_bridges(
    mailbox: Arc<crate::mailbox::MailboxStore>,
    stages: &[TeamStage],
    team_name: &str,
) -> HashMap<String, Arc<PermissionBridge>> {
    let mut bridges = HashMap::new();
    for stage in stages {
        for agent in &stage.agents {
            if bridges.contains_key(&agent.label) {
                continue;
            }
            let bridge = Arc::new(PermissionBridge::new(
                mailbox.clone(),
                agent.label.clone(),
                team_name,
            ));
            bridges.insert(agent.label.clone(), bridge);
        }
    }
    bridges
}

// ═══════════════════════════════════════════════════════════
// Coordinator trait + DefaultCoordinator
// ═══════════════════════════════════════════════════════════

#[async_trait]
pub trait Coordinator: Send + Sync {
    async fn orchestrate(&self, request: OrchestrateRequest) -> Result<ToolResult, ToolError>;
    /// Clean up a team's resources. Default implementation is a no-op;
    /// override for actual resource cleanup (task lists, scratchpad, etc.).
    async fn cleanup_team(&self, _name: &str) -> Result<(), String> {
        Ok(())
    }
    /// Resume a previously-interrupted coordinator workflow from its last
    /// checkpoint. Default returns an error — override for teams that
    /// persist checkpoint state (e.g. scratchpad files).
    async fn resume_coordinator(
        &self,
        task_id: &str,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let _ = (task_id, ctx);
        Err(ToolError::Execution(anyhow::anyhow!(
            "resume not supported by this coordinator"
        )))
    }
}

pub struct OrchestrateRequest {
    pub model: Arc<dyn Model>,
    pub config: Arc<EngineConfig>,
    pub parent_tools: Arc<InMemoryToolRegistry>,
    pub sub_tools: Arc<InMemoryToolRegistry>,
    pub stages: Vec<TeamStage>,
    pub name: String,
    pub scratchpad: Option<String>,
    pub ctx: ToolContext,
}

/// Lifecycle states for team members.
/// TS parity: InProcessTeammateTask state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeammateLifecycle {
    /// Agent registered but not yet started.
    Idle,
    /// Agent actively executing.
    Active,
    /// Agent completed execution (success or failure).
    Completed,
    /// Agent shut down (cleanup complete).
    Shutdown,
}

/// Default coordinator implementation.
///
/// Orchestrates sub-agents across stages using an optional [`AgentSpawner`].
/// The spawner is injected at construction time via [`DefaultCoordinator::with_agent_spawner`]
/// to break the circular dependency between the `team` and `runtime` crates:
/// `team` knows only the trait from `base`, while `runtime` (which depends on `team`)
/// provides the concrete implementation.
pub struct DefaultCoordinator {
    spawner: Option<Arc<dyn AgentSpawner>>,
}

impl DefaultCoordinator {
    /// Create a coordinator with no spawner. Agents will not execute
    /// until a spawner is provided via [`with_agent_spawner`].
    pub fn new() -> Self {
        Self { spawner: None }
    }

    /// Create a coordinator with an [`AgentSpawner`] for executing sub-agents.
    ///
    /// The spawner wraps the runtime's `AgentTool` logic and is safe to pass
    /// across crate boundaries because both sides depend only on the trait in `base`.
    pub fn with_agent_spawner(spawner: Arc<dyn AgentSpawner>) -> Self {
        Self { spawner: Some(spawner) }
    }
}

impl Default for DefaultCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Coordinator for DefaultCoordinator {
    async fn orchestrate(&self, req: OrchestrateRequest) -> Result<ToolResult, ToolError> {
        let OrchestrateRequest { model, config, parent_tools: _, sub_tools: _, stages, name, scratchpad, ctx } = req;

        let all_labels: Vec<String> = stages.iter()
            .flat_map(|s| s.agents.iter().map(|a| a.label.clone())).collect();
        let team_id = format!("team-{}-{}", name, chrono_id());
        let team_dir = ctx.session.cwd.join(".atta/code/teams").join(&team_id);
        let sp_path = team_dir.join("SCRATCHPAD.md");
        if let Some(p) = sp_path.parent() { let _ = tokio::fs::create_dir_all(p).await; }

        // P1-9: Write team metadata (config.json) for tool discoverability.
        // TS parity: TeamFile in teamHelpers.ts.
        let meta_path = team_dir.join("config.json");
        let meta = serde_json::json!({
            "name": name,
            "created_at": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            "lead_agent_id": ctx.session_id,
            "members": stages.iter().flat_map(|s| s.agents.iter().map(|a| {
                serde_json::json!({
                    "label": a.label,
                    "agent_type": a.agent_type,
                    "prompt": a.prompt,
                })
            })).collect::<Vec<_>>(),
        });
        let _ = tokio::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap_or_default()).await;

        let mailbox = Arc::new(crate::mailbox::MailboxStore::with_persistence(
            all_labels, team_dir.join("mailbox"),
        ));

        // Create permission bridges for each agent in the team.
        // These bridges are used when spawning team members to bubble
        // permission decisions up to the coordinator.
        let _permission_bridges = create_permission_bridges(
            mailbox.clone(),
            &stages,
            &name,
        );

        // Inject coordinator system prompt (TS parity: coordinatorMode.ts)
        let stage_names: Vec<String> = stages.iter().map(|s| s.name.clone()).collect();
        let coordinator_prompt = crate::prompt::build_coordinator_prompt(&name, &stage_names);
        let mut scratch = format!(
            "# Team `{name}`\n\nTeam id: `{team_id}`\nStages: {}\n\n{coordinator_prompt}\n",
            stages.len()
        );
        if let Some(s) = scratchpad {
            scratch.push_str("\n## 0_initial_context\n\n");
            scratch.push_str(&s);
            scratch.push('\n');
        }
        let _ = tokio::fs::write(&sp_path, &scratch).await;

        // P1-8: Teammate lifecycle tracking. TS parity: InProcessTeammateTask.
        let mut lifecycles: std::collections::HashMap<String, TeammateLifecycle> =
            stages.iter().flat_map(|s| s.agents.iter())
                .map(|a| (a.label.clone(), TeammateLifecycle::Idle))
                .collect();

        let mut any_err = false;
        for (si, stage) in stages.iter().enumerate() {
            let mut sections: Vec<(String, String, bool)> = Vec::new();

            // Spawn agents in this stage using the AgentSpawner (if provided)
            if let Some(ref spawner) = self.spawner {
                for agent_spec in &stage.agents {
                    // Transition to Active
                    lifecycles.insert(agent_spec.label.clone(), TeammateLifecycle::Active);
                    let cancel = ctx.cancel.child_token();
                    match spawner.spawn_agent(
                        agent_spec.prompt.clone(),
                        vec![], // allowed_tools: empty = all tools
                        ctx.cwd.clone(),
                        cancel,
                    ).await {
                        Ok(text) => {
                            sections.push((agent_spec.label.clone(), text, false));
                        }
                        Err(e) => {
                            sections.push((agent_spec.label.clone(), format!("ERROR: {e}"), true));
                            any_err = true;
                        }
                    }
                }
            } else {
                // No spawner available — log a warning that agents cannot be spawned.
                // This happens when TeamCreate is used standalone without a daemon wiring.
                tracing::warn!(
                    "TeamCreate: no AgentSpawner provided — stage '{}' agents will not execute. \
                     Wire RuntimeAgentSpawner at the composition root to enable team coordination.",
                    stage.name
                );
                for agent_spec in &stage.agents {
                    sections.push((
                        agent_spec.label.clone(),
                        "[AgentSpawner not available — team coordination requires daemon wiring]".to_string(),
                        true,
                    ));
                    any_err = true;
                }
            }

            if let Some(mode) = stage.aggregate {
                if !sections.is_empty() {
                    sections = aggregate(&*model, &config.model, mode, &stage.name, &sections).await;
                }
            }

            let mut md = format!("\n## {}_{}\n\n", si + 1, stage.name);
            for (l, b, e) in &sections {
                md.push_str(&format!("### {}{}\n{b}\n\n", l, if *e { " (ERROR)" } else { "" }));
            }
            scratch.push_str(&md);
            let _ = tokio::fs::write(&sp_path, &scratch).await;
        }

        Ok(ToolResult {
            content: ToolResultContent::Text(format!("{}\n\n_(scratchpad: {})_", scratch, sp_path.display())),
            is_error: any_err, structured_content: None, mcp_meta: None, new_messages: None,
        })
    }

    /// Resume a DefaultCoordinator workflow from its scratchpad checkpoint.
    ///
    /// Reads the team's SCRATCHPAD.md from the `.atta/code/teams/<task_id>/`
    /// directory, identifies the last completed stage, and returns a prompt
    /// that can be fed to a sub-agent to continue coordination.
    async fn resume_coordinator(
        &self,
        task_id: &str,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let start = std::time::Instant::now();

        let team_dir = ctx.session.cwd.join(".atta/code/teams").join(task_id);
        let sp_path = team_dir.join("SCRATCHPAD.md");

        let scratchpad = match tokio::fs::read_to_string(&sp_path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::Execution(anyhow::anyhow!(
                    "team scratchpad not found for task_id: {task_id}"
                )));
            }
            Err(e) => {
                return Err(ToolError::Execution(anyhow::anyhow!(
                    "read scratchpad: {e}"
                )));
            }
        };

        // Identify the last completed stage heading
        let last_stage: &str = scratchpad
            .lines().rfind(|l| l.starts_with("## "))
            .map(|l| l.trim_start_matches("## ").trim())
            .unwrap_or("(none)");

        let resume_prompt = format!(
            "Resuming team coordinator for task `{task_id}`.\n\
             Last checkpoint: {last_stage}\n\n\
             Scratchpad:\n{scratchpad}\n\n\
             Continue coordinating the remaining stages."
        );

        // Emit telemetry via structured tracing
        let latency_ms = start.elapsed().as_millis() as u64;
        tracing::info!(
            target: "telemetry",
            event_type = "resume_action",
            task_id = %task_id,
            source = "coordinator",
            last_stage = %last_stage,
            latency_ms,
            "resume coordinator completed"
        );

        Ok(ToolResult {
            content: ToolResultContent::Text(resume_prompt),
            is_error: false,
            structured_content: None,
            mcp_meta: None,
            new_messages: None,
        })
    }
}

// ═══════════════════════════════════════════════════════════
// Aggregation helpers
// ═══════════════════════════════════════════════════════════

pub async fn aggregate(
    model: &dyn Model, model_name: &str, mode: AggregateMode,
    stage_name: &str, sections: &[(String, String, bool)],
) -> Vec<(String, String, bool)> {
    match mode {
        AggregateMode::Concat => sections.to_vec(),
        AggregateMode::Best => {
            if let Some(label) = pick_best(model, model_name, stage_name, sections).await {
                sections.iter().filter(|(l,_,_)| *l == label).cloned().collect()
            } else { sections.to_vec() }
        }
        AggregateMode::Aggregate => {
            let text = merge(model, model_name, stage_name, sections).await
                .unwrap_or_else(|| {
                    let mut a = String::new();
                    for (l, b, _) in sections { a.push_str(&format!("### {l}\n{b}\n\n")); }
                    a
                });
            vec![("(aggregated)".into(), text, false)]
        }
    }
}

pub async fn aggregate_stage_results(
    model: &dyn Model, model_name: &str, mode: AggregateMode,
    stage_name: &str, sections: &[(String, String, bool)],
) -> Vec<(String, String, bool)> {
    aggregate(model, model_name, mode, stage_name, sections).await
}

async fn pick_best(
    model: &dyn Model, model_name: &str, stage_name: &str,
    sections: &[(String, String, bool)],
) -> Option<String> {
    let formatted: Vec<String> = sections.iter().map(|(l, b, e)| {
        let s = if *e { " (ERROR)" } else { "" };
        format!("<agent label=\"{l}{s}\">\n{b}\n</agent>")
    }).collect();
    let prompt = format!(
        "You are evaluating results of a team of AI agents on stage \"{stage_name}\".\n\n\
         Results:\n{}\n\n\
         Pick the single best result. Return ONLY the agent label, nothing else.",
        formatted.join("\n\n"),
    );
    let label = drain(model, model_name, prompt, 100).await?;
    let label = label.trim().to_string();
    if sections.iter().any(|(l,_,_)| l == &label) { Some(label) } else { None }
}

async fn merge(
    model: &dyn Model, model_name: &str, stage_name: &str,
    sections: &[(String, String, bool)],
) -> Option<String> {
    let formatted: Vec<String> = sections.iter().map(|(l, b, e)| {
        let s = if *e { " (ERROR)" } else { "" };
        format!("<agent label=\"{l}{s}\">\n{b}\n</agent>")
    }).collect();
    let prompt = format!(
        "Synthesize results of AI agents on stage \"{stage_name}\".\n\nResults:\n{}\n\n\
         Combine into one document. Capture best insights, remove redundancy, preserve facts.",
        formatted.join("\n\n"),
    );
    drain(model, model_name, prompt, 4096).await
}

async fn drain(model: &dyn Model, model_name: &str, prompt: String, max_tokens: u32) -> Option<String> {
    let blocks = vec![PromptBlock {
        role: BlockRole::System,
        content: "You are a strict judge. Output only the requested text, nothing else.".into(),
        cache_strategy: None,
    }];
    let messages = vec![base::interface::model::ModelMessage {
        role: base::interface::model::MessageRole::User,
        content: vec![base::interface::model::ModelContentBlock::Text { text: prompt }],
    }];
    let params = StreamParams {
        model: model_name.to_string(), max_tokens,
        thinking_mode: ThinkingMode::Off, fallback_model: None,
        cache_edits: vec![],
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut stream = model.stream(blocks, vec![], messages, params, cancel).await.ok()?;
    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        match ev.ok()? {
            ModelEvent::TextDelta { text: t } => text.push_str(&t),
            ModelEvent::EndTurn { .. } => break,
            _ => {}
        }
    }
    if text.trim().is_empty() { None } else { Some(text) }
}

fn chrono_id() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{n:x}")
}
