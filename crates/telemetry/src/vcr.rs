//! VCR (record/replay) — transparent Model wrapper for deterministic testing.
//!
//! - JSONL storage at `<data_dir>/vcr/<scenario>.jsonl`
//! - SHA-256 hash (first 16 chars) for request matching
//! - Dehydrate: replace [CWD]/[CONFIG_HOME]/[UUID]/[TIMESTAMP] for portability
//! - Hydrate: reverse substitution when replaying
//! - Env vars: `ATTA_VCR_RECORD=<name>`, `ATTA_VCR_REPLAY=<name>`
//! - CI protection: missing fixture → hard error with VCR_RECORD=1 hint
//! - Default: pass-through (zero overhead when no VCR config)

use async_trait::async_trait;
use base::interface::model::{
    Model, ModelError, ModelEvent, ModelMessage, ModelStream, StreamParams, ToolDef, Usage,
};
use base::interface::prompt::PromptBlock;
use base::interface::settings::{VcrConfig, VcrMode};
use base::provider::ApiType;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

// ── VcrModel: Model wrapper ──

pub struct VcrModel {
    inner: Arc<dyn Model>,
    config: Option<VcrConfig>,
    user_vcr_dir: PathBuf,
    local_vcr_dir: PathBuf,
    /// 当前 turn 的 BASE58(UUID) ID（可选，用于 VCR 按 turn 分组）。
    current_turn_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VcrEntry {
    request_hash: String,
    /// 所属 turn 的 BASE58(UUID) ID（可选，用于按 turn 分组）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn_id: Option<String>,
    request: VcrRequest,
    response: VcrResponse,
    #[serde(default)]
    chunks: Vec<VcrChunk>,
    timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VcrRequest {
    system_text: String,
    model: String,
    tools: Vec<String>,
    messages_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VcrResponse {
    stop_reason: String,
    input_tokens: u32,
    output_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum VcrChunk {
    TextDelta {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    EndTurn {
        stop_reason: String,
    },
}

/// The four environment facts that decide whether a replay miss silently
/// becomes a real network call. Split out from the env lookups themselves so
/// the *policy* is a pure function that can be unit-tested exhaustively
/// without mutating process-global environment variables (which would race
/// with every other test in the binary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FallbackInputs {
    /// `ATTA_VCR_STRICT` — never fall back.
    strict: bool,
    /// `ATTA_VCR_FALLBACK` — always fall back (deliberate recording/refresh).
    fallback_opt_in: bool,
    /// Running under a cargo test runner / `ATTA_VCR_AUTO_DETECT`.
    in_test: bool,
    /// `CI` is set and non-empty.
    in_ci: bool,
}

impl FallbackInputs {
    fn from_env() -> Self {
        fn flag(name: &str) -> bool {
            std::env::var(name).is_ok_and(|v| v != "0" && !v.is_empty())
        }
        Self {
            strict: flag("ATTA_VCR_STRICT"),
            fallback_opt_in: flag("ATTA_VCR_FALLBACK"),
            in_test: std::env::var("CARGO_TEST_RUNNER").is_ok()
                || std::env::var("ATTA_VCR_AUTO_DETECT").is_ok(),
            in_ci: flag("CI"),
        }
    }
}

/// Policy for [`VcrModel::replay_fallback_on_miss`] — see its doc comment for
/// the reasoning; this function is just the decision table.
fn resolve_replay_fallback(inputs: FallbackInputs) -> bool {
    if inputs.strict {
        return false;
    }
    if inputs.fallback_opt_in {
        return true;
    }
    // Default: loud failure wherever a miss means a broken test, silent
    // fallback only for a human driving a replay by hand.
    !(inputs.in_test || inputs.in_ci)
}

impl VcrModel {
    pub fn new(
        inner: Arc<dyn Model>,
        config: Option<VcrConfig>,
        user_vcr_dir: PathBuf,
        local_vcr_dir: PathBuf,
    ) -> Self {
        let config = config.or_else(Self::env_config);
        Self {
            inner,
            config,
            user_vcr_dir,
            local_vcr_dir,
            current_turn_id: None,
        }
    }

    /// 设置当前 turn 的 ID，VCR 录制时携带此 ID 以支持按 turn 分组。
    pub fn set_turn_id(&mut self, turn_id: Option<String>) {
        self.current_turn_id = turn_id;
    }

    fn env_config() -> Option<VcrConfig> {
        if let Ok(name) = std::env::var("ATTA_VCR_RECORD") {
            // Record mode calls the real model by definition — `fallback_on_miss`
            // isn't even read on that branch.
            Some(VcrConfig {
                mode: VcrMode::Record,
                scenario: name,
                fallback_on_miss: true,
            })
        } else if let Ok(name) = std::env::var("ATTA_VCR_REPLAY") {
            Some(VcrConfig {
                mode: VcrMode::Replay,
                scenario: name,
                fallback_on_miss: Self::replay_fallback_on_miss(),
            })
        } else if FallbackInputs::from_env().in_test {
            // Auto-detect test mode: running under `cargo test` (CARGO_TEST_RUNNER
            // set) or explicitly opted in via ATTA_VCR_AUTO_DETECT enables replay.
            // Rust's `cfg(test)` is compile-time only, so we detect at runtime.
            Some(VcrConfig {
                mode: VcrMode::Replay,
                scenario: "auto".into(),
                fallback_on_miss: Self::replay_fallback_on_miss(),
            })
        } else {
            None
        }
    }

    /// Whether a replay miss is allowed to silently fall through to a real
    /// model call.
    ///
    /// **The default is "no" anywhere a miss means a broken test** — under a
    /// cargo test runner, under `ATTA_VCR_AUTO_DETECT`, or in CI. A silent
    /// fallback there is a real, slow, non-deterministic network call whose
    /// only visible symptom is "this run took unusually long"; that exact
    /// failure mode is why the hit/miss logging and `ATTA_VCR_STRICT` had to
    /// be invented in the first place. Outside those contexts — a developer typing
    /// `ATTA_VCR_REPLAY=<case>` by hand — the fallback stays on, so the first
    /// local run of a brand-new case still works without a cassette.
    ///
    /// Explicit overrides, highest precedence first:
    /// - `ATTA_VCR_STRICT=1` — never fall back (unchanged meaning; still the
    ///   flag to reach for when diagnosing *why* something misses).
    /// - `ATTA_VCR_FALLBACK=1` — always fall back. The opt-in for "I am
    ///   deliberately (re-)recording / refreshing a cassette and expect real
    ///   calls", including from inside a test binary.
    ///
    /// Public because the test harness (`tests/runner`) always constructs an
    /// explicit `VcrConfig` and therefore never reaches `env_config()` — it
    /// has to ask for the same answer rather than re-deriving it, which is
    /// exactly the duplication that made `ATTA_VCR_STRICT` a no-op on its
    /// first implementation.
    pub fn replay_fallback_on_miss() -> bool {
        resolve_replay_fallback(FallbackInputs::from_env())
    }

    /// Check if running in CI (no tty or CI=true).
    fn is_ci() -> bool {
        FallbackInputs::from_env().in_ci
    }

    fn storage_dir(&self) -> &Path {
        if self.local_vcr_dir.exists() {
            &self.local_vcr_dir
        } else {
            &self.user_vcr_dir
        }
    }

    /// SHA-256 first 16 hex chars of (system_text + sorted_tool_names + model + messages).
    /// Messages are dehydrated before hashing for portability across machines.
    fn hash_request(
        system_text: &str,
        tools: &[ToolDef],
        model: &str,
        messages: &[ModelMessage],
    ) -> String {
        let mut h = Sha256::new();
        h.update(system_text.as_bytes());
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        names.sort();
        for n in &names {
            h.update(n.as_bytes());
        }
        h.update(model.as_bytes());
        // Include dehydrated message contents in hash (T0.4)
        if !messages.is_empty() {
            let msg_bodies: Vec<String> = messages
                .iter()
                .map(|m| dehydrate(&format!("{:?}", m)))
                .collect();
            h.update(msg_bodies.join("||").as_bytes());
        }
        hex::encode(&h.finalize()[..8])
    }

    fn load_entries(&self, scenario: &str) -> HashMap<String, VcrEntry> {
        let mut entries = HashMap::new();
        for dir in [&self.local_vcr_dir, &self.user_vcr_dir] {
            let path = dir.join(format!("{scenario}.jsonl"));
            if let Ok(content) = std::fs::read_to_string(&path) {
                for line in content.lines() {
                    if let Ok(entry) = serde_json::from_str::<VcrEntry>(line) {
                        entries.entry(entry.request_hash.clone()).or_insert(entry);
                    }
                }
            }
        }
        entries
    }

    fn save_entry(&self, scenario: &str, entry: &VcrEntry) {
        let dir = self.storage_dir();
        let _ = std::fs::create_dir_all(dir);
        let path = dir.join(format!("{scenario}.jsonl"));
        let line = serde_json::to_string(entry).unwrap_or_default();
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map(|mut f| {
                use std::io::Write;
                let _ = writeln!(f, "{line}");
            });
    }
}

#[async_trait]
impl Model for VcrModel {
    fn api_type(&self) -> ApiType {
        self.inner.api_type()
    }

    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        params: StreamParams,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let system_text = prompt_blocks
            .iter()
            .map(|b| &b.content)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let model_name = params.model.clone();
        // Dehydrate before hashing: replace CWD and config home for portable fixtures
        let dehydrated_system = dehydrate(&system_text);
        let req_hash = Self::hash_request(&dehydrated_system, &tools, &model_name, &messages);

        match &self.config {
            Some(VcrConfig {
                mode: VcrMode::Replay,
                scenario,
                fallback_on_miss,
                ..
            }) => {
                let entries = self.load_entries(scenario);
                if let Some(entry) = entries.get(&req_hash) {
                    tracing::debug!(
                        scenario = %scenario,
                        hash = %req_hash,
                        turn_id = ?self.current_turn_id,
                        "VCR replay hit"
                    );
                    let chunks: Vec<Result<ModelEvent, ModelError>> = entry
                        .chunks
                        .iter()
                        .map(|c| {
                            Ok(match c {
                                VcrChunk::TextDelta { text } => ModelEvent::TextDelta {
                                    text: hydrate(text),
                                },
                                VcrChunk::ToolUse { id, name, input } => ModelEvent::ToolUse {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                },
                                VcrChunk::EndTurn { stop_reason } => ModelEvent::EndTurn {
                                    stop_reason: stop_reason.clone(),
                                    usage: Usage {
                                        input_tokens: entry.response.input_tokens,
                                        output_tokens: entry.response.output_tokens,
                                    },
                                },
                            })
                        })
                        .collect();
                    return Ok(Box::new(futures::stream::iter(chunks)));
                }
                // Miss: without this, the only symptom is "replay took way longer
                // than expected" (silent fallback_on_miss network call) — the
                // exact scenario that took hours to diagnose forensically. Log
                // enough to make the *next* miss diagnosable in seconds instead:
                // whether a cassette even loaded (available_entries), and the
                // hash that didn't match anything in it. The hash itself can't
                // be reverse-engineered into "which field differs", but
                // `available_entries == 0` immediately rules out "wrong
                // scenario/round" vs "content actually diverged".
                tracing::warn!(
                    scenario = %scenario,
                    hash = %req_hash,
                    turn_id = ?self.current_turn_id,
                    available_entries = entries.len(),
                    fallback_on_miss,
                    "VCR replay miss (falls back to a real model call if fallback_on_miss is set)"
                );
                if std::env::var("ATTA_DEBUG_VCR_MISS").is_ok() {
                    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
                    names.sort();
                    eprintln!("VCR_MISS_DEBUG tools: {names:?}");
                    eprintln!("VCR_MISS_DEBUG model: {model_name}");
                    eprintln!(
                        "VCR_MISS_DEBUG system_text ({} bytes): {dehydrated_system}",
                        dehydrated_system.len()
                    );
                    let msg_bodies: Vec<String> = messages
                        .iter()
                        .map(|m| dehydrate(&format!("{:?}", m)))
                        .collect();
                    eprintln!(
                        "VCR_MISS_DEBUG messages ({} msgs): {}",
                        messages.len(),
                        msg_bodies.join("||")
                    );
                }
                if !fallback_on_miss {
                    return Err(ModelError::Internal(format!(
                        "VCR replay miss: no fixture for hash {req_hash}. Run with ATTA_VCR_RECORD={scenario}"
                    )));
                }
                // fallback_on_miss: pass through to real API
            }
            Some(VcrConfig {
                mode: VcrMode::Record,
                scenario,
                ..
            }) => {
                let mut chunks: Vec<VcrChunk> = Vec::new();
                let inner_stream = self
                    .inner
                    .stream(prompt_blocks, tools.clone(), messages, params, cancel)
                    .await?;
                tokio::pin!(inner_stream);
                let captured: Vec<Result<ModelEvent, ModelError>> = inner_stream.collect().await;

                let mut stop_reason = String::new();
                let mut usage = Usage::default();
                for e in captured.iter().flatten() {
                    match e {
                        ModelEvent::TextDelta { text } => {
                            let t = dehydrate(text);
                            // Merge consecutive text_delta into one chunk so
                            // fixtures are readable instead of one token per line.
                            match chunks.last_mut() {
                                Some(VcrChunk::TextDelta { text: last }) => last.push_str(&t),
                                _ => chunks.push(VcrChunk::TextDelta { text: t }),
                            }
                        }
                        ModelEvent::ToolUse { id, name, input } => chunks.push(VcrChunk::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                        }),
                        ModelEvent::EndTurn {
                            stop_reason: sr,
                            usage: u,
                        } => {
                            stop_reason = sr.clone();
                            usage = u.clone();
                            chunks.push(VcrChunk::EndTurn {
                                stop_reason: sr.clone(),
                            });
                        }
                        _ => {}
                    }
                }
                self.save_entry(
                    scenario,
                    &VcrEntry {
                        request_hash: req_hash,
                        turn_id: self.current_turn_id.clone(),
                        request: VcrRequest {
                            system_text: dehydrated_system,
                            model: model_name,
                            tools: tools.iter().map(|t| t.name.clone()).collect(),
                            messages_count: 0,
                        },
                        response: VcrResponse {
                            stop_reason,
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                        },
                        chunks,
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64,
                    },
                );
                return Ok(Box::new(futures::stream::iter(captured)));
            }
            None => {}
        }
        self.inner
            .stream(prompt_blocks, tools, messages, params, cancel)
            .await
    }
}

// ── Dehydrate / Hydrate ──

use regex::Regex;
use std::sync::LazyLock;

/// Replace machine-specific and environment-specific text with portable placeholders.
///
/// These replacements ensure the VCR hash is stable across machines and runs,
/// so recorded fixtures can be used for cross-version regression testing.
fn dehydrate(s: &str) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".into());
    let mut result = s.replace(&cwd, "[CWD]").replace(&home, "[HOME]");

    // ── Numeric / counters ──
    static RE_NUM_FILES: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"num_files="\d+""#).unwrap());
    static RE_DURATION_MS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"duration_ms="\d+""#).unwrap());
    static RE_COST_USD: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"cost_usd="[\d.]+""#).unwrap());
    result = RE_NUM_FILES
        .replace_all(&result, r#"num_files="[NUM]""#)
        .to_string();
    result = RE_DURATION_MS
        .replace_all(&result, r#"duration_ms="[DURATION]""#)
        .to_string();
    result = RE_COST_USD
        .replace_all(&result, r#"cost_usd="[COST]""#)
        .to_string();

    // ── Lists / dynamic content ──
    static RE_COMMANDS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"Available commands: .+").unwrap());
    static RE_FILES_MODIFIED: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"Files modified by user: .+").unwrap());
    result = RE_COMMANDS
        .replace_all(&result, "Available commands: [COMMANDS]")
        .to_string();
    result = RE_FILES_MODIFIED
        .replace_all(&result, "Files modified by user: [FILES]")
        .to_string();

    // ── Environment (cross-run stable) ──
    static RE_DATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Date: \S+").unwrap());
    static RE_OS_VERSION: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"OS Version: \S[^\n]*").unwrap());
    static RE_GIT_BRANCH: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"Git branch: \S[^\n]*").unwrap());
    static RE_GIT_STATUS_BLOCK: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"gitStatus: [^\n]*(\n\s*[^\n]*)*").unwrap());
    static RE_KNOWLEDGE_CUTOFF: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(Assistant )?knowledge cutoff is \S[^\n.]*").unwrap());
    static RE_POWERED_BY_MODEL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"You are powered by the model \S[^\n]*").unwrap());
    static RE_MODEL_DESC: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"The most recent Claude models are [^\n]*").unwrap());

    result = RE_DATE.replace_all(&result, "Date: [DATE]").to_string();
    result = RE_OS_VERSION
        .replace_all(&result, "OS Version: [OS]")
        .to_string();
    result = RE_GIT_BRANCH
        .replace_all(&result, "Git branch: [BRANCH]")
        .to_string();
    result = RE_GIT_STATUS_BLOCK
        .replace_all(&result, "gitStatus: [GIT_STATUS]")
        .to_string();
    result = RE_KNOWLEDGE_CUTOFF
        .replace_all(&result, "knowledge cutoff is [CUTOFF]")
        .to_string();
    result = RE_POWERED_BY_MODEL
        .replace_all(&result, "You are powered by the model [MODEL]")
        .to_string();
    result = RE_MODEL_DESC
        .replace_all(&result, "The most recent Claude models are [MODELS]")
        .to_string();

    // ── `ls -l`-style timestamps embedded in tool_result content ──
    // VCR only mocks the *model's* response, never tool execution — every
    // replay actually re-runs Bash/Write/etc. for real. A command like
    // `ls -la` embeds a real modification timestamp ("Aug  5 22:59" or, for
    // files >6mo old, "Aug  5  2025") in its stdout, which then becomes part
    // of a tool_result message baked into every subsequent request's hash.
    // Record-time and replay-time are two different real timestamps, so that
    // one field alone desyncs the hash for that turn *and every turn after
    // it* (the model then gets a fresh, real response with a new tool_use id
    // that can never match the rest of the original cassette either).
    // Matches both BSD/macOS and GNU `ls -l` default
    // date formats.
    static RE_LS_TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"\b(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)\s+\d{1,2}\s+(?:\d{1,2}:\d{2}|\d{4})\b",
        )
        .unwrap()
    });
    result = RE_LS_TIMESTAMP
        .replace_all(&result, "[LS_TIMESTAMP]")
        .to_string();

    // ── `ls -la`'s `..` entry's link count / size ── The project's own tmp
    // dir (e.g. `/tmp/atta_test_runner`) lives directly under the *shared*
    // system temp dir, so `..`'s reported link count and byte size reflect
    // `/tmp`'s current subdirectory count — a number that drifts constantly
    // from unrelated processes on the machine creating/removing their own
    // tmp entries, completely outside this test's control. Unlike the
    // timestamp case above (record-time vs replay-time, two fixed points),
    // this field can differ between *any* two runs, including a cassette
    // replayed seconds after recording it, and desyncs the hash the same
    // way. Only the `..` line is normalized — `.` reflects the test's own
    // directory, which genuinely should stay stable across replays, and a
    // real divergence there is signal worth keeping. Requires
    // `RE_LS_TIMESTAMP` to have already run (matches its `[LS_TIMESTAMP]`
    // output, not a raw date).
    static RE_LS_PARENT_DIR_STATS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^(d\S*\s+)\d+(\s+\S+\s+\S+\s+)\d+(\s+\[LS_TIMESTAMP\]\s+\.\.)\s*$")
            .unwrap()
    });
    result = RE_LS_PARENT_DIR_STATS
        .replace_all(&result, "${1}[N]${2}[N]${3}")
        .to_string();

    // ── "Today's date is X" (chat/research scene preambles, and the
    // `<system-reminder># currentDate` block `turn.rs` injects once per
    // session) ── Unlike the `Date: \S+` field above (a different, less
    // common format), this string is a real `chrono_now()`/wall-clock value
    // baked into the very first system prompt of every session, so — unlike
    // the `ls -l` case above, which only bites turns that actually ran a
    // listing command — this desyncs the hash of *every single cassette's
    // first turn* the day after it was recorded (found while investigating
    // why every one of §14's freshly-verified 23/23-hit cassettes started
    // missing again with no code change: the sandbox clock had simply
    // advanced a day). Matches both `is 2026-08-07.` and `is 2026-08-07\.
    // Host OS: ...` forms without needing two patterns.
    static RE_TODAYS_DATE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"Today's date is \S+\.").unwrap());
    result = RE_TODAYS_DATE
        .replace_all(&result, "Today's date is [TODAY].")
        .to_string();

    // ── UUIDs embedded in tool_result content ── Same class of bug as the
    // `ls -l` timestamp case above, different trigger: a model can decide
    // *on its own* (not prompted to) to run something like
    // `python3 -c "import uuid; print(uuid.uuid4())"` to generate a task ID
    // before delegating to a subagent, and that value is genuinely random
    // every real execution — no dehydration of the *skill/agent* content
    // itself would catch this, since the randomness originates from the
    // model's own tool call, not from anything this codebase renders.
    // Found via a real recording (agent delegation turn) that missed on
    // strict replay immediately after being recorded, with the miss traced
    // to a `uuid.uuid4()`-generated string in a Bash tool_result. Matches standard
    // 8-4-4-4-12 hex UUID form (v1-v5, case-insensitive), which is
    // specific enough to not risk false-positive matches on unrelated hex
    // content.
    static RE_UUID: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b").unwrap()
    });
    result = RE_UUID.replace_all(&result, "[UUID]").to_string();

    // ── `TeamCreate`'s own team id (`team::coordinator::chrono_id()` — a
    // raw nanosecond epoch timestamp, hex-encoded) ── Same class of bug as
    // the `ls -l` timestamp/UUID cases above: `TeamCreate`'s `tool_result`
    // embeds a fresh id on every real execution (VCR mocks the model, not
    // the tool), so even replaying the cassette moments after recording it
    // misses on the turn right after the tool call. The id appears twice in
    // that one tool_result — the "Team id: `...`" summary line and the
    // scratchpad path (`.atta/teams/{team_id}/SCRATCHPAD.md`) — both
    // becoming part of the next turn's request hash. Anchored to the two
    // literal templates that produce them
    // (`crates/team/src/coordinator.rs`'s `orchestrate()`/
    // `read_scratchpad()`) rather than a bare hex pattern, since a raw
    // "team-...-hexdigits" shape alone risks matching unrelated content.
    static RE_TEAM_ID_LINE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"Team id: `[^`\n]+`").unwrap());
    static RE_TEAM_SCRATCHPAD_PATH: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\.atta/teams/[^/\s]+/SCRATCHPAD\.md").unwrap());
    result = RE_TEAM_ID_LINE
        .replace_all(&result, "Team id: `[TEAM_ID]`")
        .to_string();
    result = RE_TEAM_SCRATCHPAD_PATH
        .replace_all(&result, ".atta/teams/[TEAM_ID]/SCRATCHPAD.md")
        .to_string();

    result
}

/// Replace placeholders with machine-specific paths.
fn hydrate(s: &str) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".into());
    let mut result = s.replace("[CWD]", &cwd).replace("[HOME]", &home);
    // Numerical placeholders → non-zero dummy values.
    result = result.replace("[NUM]", "1");
    result = result.replace("[DURATION]", "100");
    result = result.replace("[COST]", "0.01");
    result = result.replace("[COMMANDS]", "git, cargo, ls");
    result = result.replace("[FILES]", "src/main.rs, README.md");
    result = result.replace("[DATE]", "2026-06-21");
    result = result.replace("[OS]", "linux");
    result = result.replace("[BRANCH]", "main");
    result = result.replace("[GIT_STATUS]", "");
    result = result.replace("[CUTOFF]", "April 2025");
    result = result.replace("[MODEL]", "claude-sonnet-4-6");
    result = result.replace("[MODELS]", "Fable 5 and the Claude 4.X family");
    result = result.replace("[LS_TIMESTAMP]", "Jan  1 00:00");
    result
}

// ── Streaming VCR wrapper ──

/// Record or replay a streaming API response.
pub async fn with_streaming_vcr<F, Fut>(
    vcr_model: &VcrModel,
    _scenario: &str,
    f: F,
) -> Result<Vec<Result<ModelEvent, ModelError>>, ModelError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<Result<ModelEvent, ModelError>>, ModelError>>,
{
    match &vcr_model.config {
        Some(VcrConfig {
            mode: VcrMode::Replay,
            ..
        }) => {
            // Replay handled by VcrModel::stream — just delegate
            f().await
        }
        Some(VcrConfig {
            mode: VcrMode::Record,
            ..
        }) => {
            // Record: execution happens in VcrModel::stream
            f().await
        }
        None => f().await,
    }
}

// ── CI protection ──

/// Verify that all expected VCR fixtures exist. Call in CI after tests.
pub fn verify_fixtures_in_ci(scenarios: &[&str], vcr_dir: &Path) -> Result<(), String> {
    if !VcrModel::is_ci() {
        return Ok(());
    }
    let mut missing = Vec::new();
    for scenario in scenarios {
        let path = vcr_dir.join(format!("{scenario}.jsonl"));
        if !path.exists() {
            missing.push(scenario.to_string());
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "VCR fixtures missing in CI: {}. Re-run tests with VCR_RECORD=1, then commit the result.",
            missing.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::ModelEvent;

    /// The property that actually matters: two `ls -la` runs against the
    /// exact same directory contents, minutes/days apart (so the timestamp
    /// column differs but everything else is identical), must dehydrate to
    /// the *same* string — otherwise `hash_request()` diverges between
    /// record-time and replay-time purely because of wall-clock timing, not
    /// because anything about the request actually changed.
    #[test]
    fn dehydrate_normalizes_ls_l_timestamps_so_repeated_runs_hash_the_same() {
        let recorded = "-rw-r--r--  1 xbitshans  staff  45 Aug  5 22:59 greet.h\n\
                         drwxr-xr-x  3 xbitshans  staff  96 Aug  5 20:21 build";
        let replayed = "-rw-r--r--  1 xbitshans  staff  45 Aug  6 09:03 greet.h\n\
                         drwxr-xr-x  3 xbitshans  staff  96 Aug  6 09:03 build";
        assert_ne!(
            recorded, replayed,
            "sanity: the two raw ls outputs actually differ"
        );
        assert_eq!(
            dehydrate(recorded),
            dehydrate(replayed),
            "ls -l timestamps must dehydrate identically regardless of the actual date/time"
        );
    }

    /// GNU/BSD `ls -l` switches to a year column instead of a time-of-day
    /// column once a file is more than ~6 months old — must also normalize.
    #[test]
    fn dehydrate_normalizes_ls_l_year_form_too() {
        let old_file = "-rw-r--r--  1 xbitshans  staff  45 Jan  3  2025 old.txt";
        let recent_file = "-rw-r--r--  1 xbitshans  staff  45 Jan  3 10:00 old.txt";
        assert_eq!(dehydrate(old_file), dehydrate(recent_file));
    }

    /// The bug found via two live `cargo run` replays of the exact same
    /// just-recorded cassette missing with two *different* hashes each
    /// time: `..`'s link count and size reflect `/tmp` itself (the shared
    /// system temp dir the test's own tmp dir lives under), which drifts
    /// from unrelated processes on the machine — nothing to do with the
    /// test's own content, and not a fixed record-vs-replay-day delta like
    /// the timestamp case, so it can desync on *any* two runs.
    #[test]
    fn dehydrate_normalizes_the_parent_dir_entrys_link_count_and_size_in_ls_la_output() {
        let recorded = "total 8\n\
                         drwxr-xr-x   7 xbitshans  wheel   224 Aug 11 02:53 .\n\
                         drwxrwxrwt  75 root       wheel  2400 Aug 11 02:53 ..\n\
                         -rw-r--r--   1 xbitshans  wheel   393 Aug 11 02:53 Makefile\n";
        let replayed = "total 8\n\
                         drwxr-xr-x   7 xbitshans  wheel   224 Aug 11 02:54 .\n\
                         drwxrwxrwt  76 root       wheel  2432 Aug 11 02:54 ..\n\
                         -rw-r--r--   1 xbitshans  wheel   393 Aug 11 02:54 Makefile\n";
        assert_ne!(
            recorded, replayed,
            "sanity: the two raw ls outputs actually differ"
        );
        assert_eq!(
            dehydrate(recorded),
            dehydrate(replayed),
            "`..`'s link count/size must normalize identically regardless of /tmp's \
             unrelated sibling-entry churn"
        );
    }

    /// The `.` entry (the test's own directory) must NOT be touched by the
    /// `..` normalization above — its link count/size are meaningful,
    /// test-controlled content, and a real divergence there should still
    /// surface as a genuine VCR miss rather than being silently masked.
    #[test]
    fn dehydrate_leaves_the_self_dir_entrys_stats_untouched() {
        let a = "drwxr-xr-x   7 xbitshans  wheel   224 Aug 11 02:53 .\n";
        let b = "drwxr-xr-x   9 xbitshans  wheel   288 Aug 11 02:53 .\n";
        assert_ne!(
            dehydrate(a),
            dehydrate(b),
            "a genuinely different link count/size on `.` itself must still change the hash"
        );
    }

    /// Round-trip sanity for the new placeholder, matching the pattern of
    /// every other dehydrate/hydrate pair in this module.
    #[test]
    fn ls_timestamp_placeholder_hydrates_to_something_not_the_literal_placeholder() {
        let dehydrated = dehydrate("Aug  5 22:59 greet.h");
        assert!(dehydrated.contains("[LS_TIMESTAMP]"));
        let hydrated = hydrate(&dehydrated);
        assert!(!hydrated.contains("[LS_TIMESTAMP]"));
    }

    /// The chat/research scene preamble and the `<system-reminder>#
    /// currentDate` block both bake a real wall-clock date into the very
    /// first system prompt of a session — must normalize so a cassette
    /// recorded on one day still replays cleanly on any later day.
    #[test]
    fn dehydrate_normalizes_todays_date_line_so_replay_survives_a_day_boundary() {
        let recorded = "Today's date is 2026-08-06. Host OS: macos. Shell: /bin/bash.";
        let replayed = "Today's date is 2026-08-07. Host OS: macos. Shell: /bin/bash.";
        assert_ne!(
            recorded, replayed,
            "sanity: the two raw strings actually differ"
        );
        assert_eq!(
            dehydrate(recorded),
            dehydrate(replayed),
            "the 'Today's date is X' preamble must dehydrate identically regardless of the actual date"
        );
    }

    /// Same normalization, for the other call site's slightly different
    /// sentence (`turn.rs`'s `<system-reminder># currentDate` block, no
    /// trailing "Host OS/Shell" clause).
    #[test]
    fn dehydrate_normalizes_todays_date_line_in_system_reminder_form_too() {
        let recorded = "# currentDate\nToday's date is 2026-08-06.\n\nIMPORTANT: ...";
        let replayed = "# currentDate\nToday's date is 2026-08-07.\n\nIMPORTANT: ...";
        assert_eq!(dehydrate(recorded), dehydrate(replayed));
    }

    /// A model can spontaneously run something like
    /// `python3 -c "import uuid; print(uuid.uuid4())"` before delegating to
    /// a subagent — the resulting UUID in that tool_result is genuinely
    /// random every real execution, no different between two runs of
    /// "the same" recorded conversation than the ls -l timestamp case.
    #[test]
    fn dehydrate_normalizes_uuids_so_a_models_own_random_generation_hashes_the_same() {
        let recorded = "ToolResult content: 7e71e056-c7f9-4d61-8731-431cc023b43b\n";
        let replayed = "ToolResult content: a1b2c3d4-e5f6-4789-a012-3456789abcde\n";
        assert_ne!(
            recorded, replayed,
            "sanity: the two raw UUIDs actually differ"
        );
        assert_eq!(dehydrate(recorded), dehydrate(replayed));
    }

    #[test]
    fn dehydrate_uuid_normalization_is_case_insensitive() {
        let lower = "id: 7e71e056-c7f9-4d61-8731-431cc023b43b";
        let upper = "id: 7E71E056-C7F9-4D61-8731-431CC023B43B";
        assert_eq!(dehydrate(lower), dehydrate(upper));
    }

    /// `TeamCreate`'s `tool_result` embeds a fresh `chrono_id()`-based team
    /// id on every real execution (recording and replay both actually run
    /// the tool — VCR only mocks the model) — without this, even a cassette
    /// replayed seconds after it was recorded misses on the turn right
    /// after the tool call, because the id differs both in the "Team id:
    /// `...`" summary line and inside the scratchpad path.
    #[test]
    fn dehydrate_normalizes_team_id_so_a_fresh_teamcreate_hashes_the_same() {
        let recorded = "# Team `sum-team`\n\nTeam id: `team-sum-team-18cab6e8a42db208`\n\
                         Stages: 1\n\n...\n\n\
                         _(scratchpad: /tmp/atta_test_runner/workdir/.atta/teams/\
                         team-sum-team-18cab6e8a42db208/SCRATCHPAD.md)_";
        let replayed = "# Team `sum-team`\n\nTeam id: `team-sum-team-9f3e1a08b7c02155`\n\
                         Stages: 1\n\n...\n\n\
                         _(scratchpad: /tmp/atta_test_runner/workdir/.atta/teams/\
                         team-sum-team-9f3e1a08b7c02155/SCRATCHPAD.md)_";
        assert_ne!(
            recorded, replayed,
            "sanity: the two raw team ids actually differ"
        );
        assert_eq!(dehydrate(recorded), dehydrate(replayed));
    }

    struct MockModel;

    #[async_trait::async_trait]
    impl base::interface::model::Model for MockModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _: Vec<base::interface::prompt::PromptBlock>,
            _: Vec<base::interface::model::ToolDef>,
            _: Vec<base::interface::model::ModelMessage>,
            _: base::interface::model::StreamParams,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            Ok(Box::new(futures::stream::iter(vec![
                Ok(ModelEvent::TextDelta {
                    text: "Hello, World!".into(),
                }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Default::default(),
                }),
            ])))
        }
    }

    fn test_vcr_dir() -> std::path::PathBuf {
        std::path::PathBuf::from("/tmp/atta_vcr_unit_test")
    }

    #[test]
    fn dehydrate_replaces_cwd() {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "/tmp".into());
        let input = format!("Read file at {cwd}/foo.txt");
        let result = dehydrate(&input);
        assert!(!result.contains(&cwd));
        assert!(result.contains("[CWD]/foo.txt"));
    }

    #[test]
    fn hydrate_restores_cwd() {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "/tmp".into());
        let dehydrated = "Read [CWD]/foo.txt and [HOME]/bar.txt";
        let result = hydrate(dehydrated);
        assert!(result.contains(&format!("{cwd}/foo.txt")));
    }

    #[test]
    fn roundtrip_is_idempotent() {
        let original = "Test string with paths and tokens";
        assert_eq!(hydrate(&dehydrate(original)), original);
    }

    #[tokio::test]
    async fn record_then_replay_same_process() {
        let scenario = "unit_test_record_replay";
        let dir = test_vcr_dir();
        let _ = std::fs::create_dir_all(&dir);
        let fixture = dir.join(format!("{scenario}.jsonl"));
        let _ = std::fs::remove_file(&fixture);

        let prompt = vec![base::interface::prompt::PromptBlock {
            role: base::interface::prompt::BlockRole::System,
            content: "You are a helpful assistant.".into(),
            cache_strategy: None,
        }];
        let tools: Vec<base::interface::model::ToolDef> = vec![base::interface::model::ToolDef {
            name: "Bash".into(),
            description: "Run shell".into(),
            input_schema: serde_json::json!({}),
        }];
        let messages: Vec<base::interface::model::ModelMessage> = vec![];
        let params = base::interface::model::StreamParams {
            model: "test-model".into(),
            max_tokens: 100,
            thinking_mode: base::interface::settings::ThinkingMode::Off,
            fallback_model: None,
            cache_edits: vec![],
        };
        let cancel = tokio_util::sync::CancellationToken::new();

        // Phase 1: Record
        let mock: Arc<dyn base::interface::model::Model> = Arc::new(MockModel);
        let record_vcr = VcrModel::new(
            mock,
            Some(VcrConfig {
                mode: VcrMode::Record,
                scenario: scenario.into(),
                fallback_on_miss: true,
            }),
            PathBuf::from("/tmp/atta_vcr_nonexistent"),
            dir.clone(),
        );
        let mut stream = record_vcr
            .stream(
                prompt.clone(),
                tools.clone(),
                messages.clone(),
                params.clone(),
                cancel.clone(),
            )
            .await
            .unwrap();
        let mut text = String::new();
        while let Some(e) = futures::StreamExt::next(&mut stream).await {
            if let Ok(ModelEvent::TextDelta { text: t }) = e {
                text.push_str(&t);
            }
        }
        assert_eq!(text, "Hello, World!");
        assert!(fixture.exists(), "fixture should exist after record");

        // Phase 2: Replay with a panic model
        struct Panic;
        #[async_trait::async_trait]
        impl base::interface::model::Model for Panic {
            fn api_type(&self) -> base::provider::ApiType {
                base::provider::ApiType::Anthropic
            }
            async fn stream(
                &self,
                _: Vec<base::interface::prompt::PromptBlock>,
                _: Vec<base::interface::model::ToolDef>,
                _: Vec<base::interface::model::ModelMessage>,
                _: base::interface::model::StreamParams,
                _: tokio_util::sync::CancellationToken,
            ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
            {
                panic!("should not be called");
            }
        }

        let replay_vcr = VcrModel::new(
            Arc::new(Panic),
            Some(VcrConfig {
                mode: VcrMode::Replay,
                scenario: scenario.into(),
                fallback_on_miss: false,
            }),
            PathBuf::from("/tmp/atta_vcr_nonexistent"),
            dir,
        );
        let mut stream = replay_vcr
            .stream(prompt, tools, messages, params, cancel)
            .await
            .unwrap();
        let mut text = String::new();
        while let Some(e) = futures::StreamExt::next(&mut stream).await {
            if let Ok(ModelEvent::TextDelta { text: t }) = e {
                text.push_str(&t);
            }
        }
        assert_eq!(text, "Hello, World!", "replay should return same text");
    }

    #[test]
    fn default_config_is_none() {
        // When no env var and no explicit config, VCR is pass-through (noop).
        // We can verify the config resolution logic without a real model.
        let config = VcrModel::env_config();
        // In test environments without ATTA_VCR_* set, this should be None.
        // If ATTA_VCR_RECORD/REPLAY is set, we skip the assertion. Same for a
        // test-runner/auto-detect environment (including the one
        // `strict_is_the_default_under_a_test_runner_env` briefly creates in
        // this very binary) — that legitimately resolves to auto-replay.
        if std::env::var("ATTA_VCR_RECORD").is_err()
            && std::env::var("ATTA_VCR_REPLAY").is_err()
            && !FallbackInputs::from_env().in_test
        {
            assert!(config.is_none());
        }
    }

    // ── Replay-miss fallback policy ──
    //
    // The default used to be `fallback_on_miss: true` *everywhere*, including
    // the auto-enabled-under-`cargo test` path: a cassette miss inside a test
    // silently issued a real API call. Slow, non-deterministic, and invisible
    // except as "this run took unusually long" — which is precisely how a
    // whole class of bugs stayed hidden. The default is now inverted for
    // test/CI; `ATTA_VCR_FALLBACK=1` is the deliberate opt-in.

    fn inputs(strict: bool, opt_in: bool, in_test: bool, in_ci: bool) -> FallbackInputs {
        FallbackInputs {
            strict,
            fallback_opt_in: opt_in,
            in_test,
            in_ci,
        }
    }

    #[test]
    fn miss_does_not_fall_back_to_the_network_under_a_test_runner() {
        assert!(!resolve_replay_fallback(inputs(false, false, true, false)));
    }

    #[test]
    fn miss_does_not_fall_back_to_the_network_in_ci() {
        assert!(!resolve_replay_fallback(inputs(false, false, false, true)));
    }

    #[test]
    fn interactive_replay_still_falls_back_so_a_brand_new_case_can_run() {
        assert!(resolve_replay_fallback(inputs(false, false, false, false)));
    }

    #[test]
    fn explicit_fallback_opt_in_beats_the_test_default() {
        assert!(resolve_replay_fallback(inputs(false, true, true, true)));
    }

    #[test]
    fn strict_beats_everything_including_the_fallback_opt_in() {
        // `ATTA_VCR_STRICT=1` keeps its original meaning unchanged — it is
        // still the flag documented for diagnosing a miss, and it must not be
        // overridable by the new opt-in.
        assert!(!resolve_replay_fallback(inputs(true, true, true, true)));
        assert!(!resolve_replay_fallback(inputs(true, false, false, false)));
    }

    /// End-to-end version of the policy test: a replay miss under a
    /// test-runner environment, with no explicit override, must return an
    /// error rather than reaching the wrapped model at all (the inner model
    /// here panics if called, which is what "fell through to the network"
    /// would look like).
    #[tokio::test]
    async fn replay_miss_under_test_env_errors_instead_of_calling_the_model() {
        struct NeverCall;
        #[async_trait::async_trait]
        impl base::interface::model::Model for NeverCall {
            fn api_type(&self) -> base::provider::ApiType {
                base::provider::ApiType::Anthropic
            }
            async fn stream(
                &self,
                _: Vec<base::interface::prompt::PromptBlock>,
                _: Vec<base::interface::model::ToolDef>,
                _: Vec<base::interface::model::ModelMessage>,
                _: base::interface::model::StreamParams,
                _: tokio_util::sync::CancellationToken,
            ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
            {
                panic!("replay miss fell through to a real model call");
            }
        }

        let fallback = resolve_replay_fallback(inputs(false, false, true, false));
        let vcr = VcrModel::new(
            Arc::new(NeverCall),
            Some(VcrConfig {
                mode: VcrMode::Replay,
                scenario: "unit_test_no_such_cassette".into(),
                fallback_on_miss: fallback,
            }),
            PathBuf::from("/tmp/atta_vcr_nonexistent"),
            PathBuf::from("/tmp/atta_vcr_nonexistent"),
        );

        let params = base::interface::model::StreamParams {
            model: "test-model".into(),
            max_tokens: 100,
            thinking_mode: base::interface::settings::ThinkingMode::Off,
            fallback_model: None,
            cache_edits: vec![],
        };
        let err = vcr
            .stream(
                vec![],
                vec![],
                vec![],
                params,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .err()
            .expect("a miss with no cassette must be an error, not a real call");
        assert!(
            format!("{err:?}").contains("VCR replay miss"),
            "expected a replay-miss error, got: {err:?}"
        );
    }

    /// Wiring check: the env-var lookups actually feed the policy above.
    /// Mutating process-global env vars races with the rest of this binary,
    /// so this is the single test that does it, under a mutex, and it puts
    /// the environment back afterwards.
    #[test]
    fn strict_is_the_default_under_a_test_runner_env() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let had_auto = std::env::var("ATTA_VCR_AUTO_DETECT").ok();
        let had_strict = std::env::var("ATTA_VCR_STRICT").ok();
        let had_fallback = std::env::var("ATTA_VCR_FALLBACK").ok();
        std::env::set_var("ATTA_VCR_AUTO_DETECT", "1");
        std::env::remove_var("ATTA_VCR_STRICT");
        std::env::remove_var("ATTA_VCR_FALLBACK");

        let default_fallback = VcrModel::replay_fallback_on_miss();
        // Only meaningful when no explicit ATTA_VCR_REPLAY/RECORD is in play.
        let auto_config = VcrModel::env_config();

        std::env::set_var("ATTA_VCR_FALLBACK", "1");
        let opted_in = VcrModel::replay_fallback_on_miss();

        match had_auto {
            Some(v) => std::env::set_var("ATTA_VCR_AUTO_DETECT", v),
            None => std::env::remove_var("ATTA_VCR_AUTO_DETECT"),
        }
        match had_strict {
            Some(v) => std::env::set_var("ATTA_VCR_STRICT", v),
            None => std::env::remove_var("ATTA_VCR_STRICT"),
        }
        match had_fallback {
            Some(v) => std::env::set_var("ATTA_VCR_FALLBACK", v),
            None => std::env::remove_var("ATTA_VCR_FALLBACK"),
        }

        assert!(
            !default_fallback,
            "test-runner env must default to no network fallback"
        );
        assert!(opted_in, "ATTA_VCR_FALLBACK=1 must re-enable the fallback");
        if let Some(cfg) = auto_config {
            assert!(
                !cfg.fallback_on_miss,
                "auto-detected replay config must carry the strict default"
            );
        }
    }
}
