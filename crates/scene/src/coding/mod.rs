//! CodingScene v2.0.0 — task-routed coding agent orchestration.
//!
//! Sub-modules:
//! - `config`  — ModelProfile, ModelProfileRegistry, CodingSceneConfig
//! - `task`    — CodingTaskKind, RuleBasedTaskRouter
//! - `prompt`  — TaskProfile, PromptProfile, built-in prompt templates
//! - `context` — ContextPack, ContextPackBuilder (Phase 2)
//! - `verify`  — VerificationPolicy, VerificationRecord (Phase 3)
//! - `policy`  — PolicyHook, built-in hooks (Phase 4)
//! - `trace`   — CodingTaskTrace (minimal trace for hook decisions)

pub mod config;
pub mod context;
pub mod escalation;
pub mod policy;
pub mod prompt;
pub mod task;
pub mod tier;
pub mod verify;
