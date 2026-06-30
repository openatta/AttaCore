//! VCR (record/replay) — transparent Model wrapper for deterministic testing.
//!
//! TS parity: Claude Code's `vcr.ts` dehydrate/hydrate + SHA-hash fixture matching.
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
        // Auto-detect test mode: if running under `cargo test` (CARGO_TEST_RUNNER set)
        // or explicitly opted in via ATTA_VCR_AUTO_DETECT, enable VCR replay.
        // TS parity: claude-code's `shouldUseVCR()` checks `NODE_ENV === 'test'`.
        // Rust's `cfg(test)` is compile-time only, so we detect at runtime.
        let in_test = std::env::var("CARGO_TEST_RUNNER").is_ok()
            || std::env::var("ATTA_VCR_AUTO_DETECT").is_ok();
        if let Ok(name) = std::env::var("ATTA_VCR_RECORD") {
            Some(VcrConfig {
                mode: VcrMode::Record,
                scenario: name,
                fallback_on_miss: true,
            })
        } else if let Ok(name) = std::env::var("ATTA_VCR_REPLAY") {
            Some(VcrConfig {
                mode: VcrMode::Replay,
                scenario: name,
                fallback_on_miss: true,
            })
        } else if in_test {
            Some(VcrConfig {
                mode: VcrMode::Replay,
                scenario: "auto".into(),
                fallback_on_miss: true,
            })
        } else {
            None
        }
    }

    /// Check if running in CI (no tty or CI=true).
    fn is_ci() -> bool {
        std::env::var("CI").is_ok_and(|v| !v.is_empty())
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

// ── Dehydrate / Hydrate (TS parity: vcr.ts dehydrateValue / hydrateValue) ──

use regex::Regex;
use std::sync::LazyLock;

/// Replace machine-specific and environment-specific text with portable placeholders.
/// TS parity: `dehydrateValue()` in vcr.ts.
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
    result
}

/// Replace placeholders with machine-specific paths.
fn hydrate(s: &str) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".into());
    let mut result = s.replace("[CWD]", &cwd).replace("[HOME]", &home);
    // Numerical placeholders → non-zero dummy values (TS parity)
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
    result
}

// ── Streaming VCR wrapper ──

/// Record or replay a streaming API response.
/// TS parity: `withStreamingVCR()` in vcr.ts.
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
/// TS parity: CI fixture check in vcr.ts lines 133-137.
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
        // If ATTA_VCR_RECORD/REPLAY is set, we skip the assertion.
        if std::env::var("ATTA_VCR_RECORD").is_err() && std::env::var("ATTA_VCR_REPLAY").is_err() {
            assert!(config.is_none());
        }
    }
}
