//! 引擎只读配置 + 模型窗口推断。

use crate::permission::PermissionMode;
use std::path::PathBuf;

/// 引擎只读配置。从 settings.json + CLI flags 合并出来后，整个会话不再变。
#[derive(Debug, Clone)]
pub struct EngineConfig {
    // ---- model / runtime ----
    /// Provider id (e.g. "anthropic", "deepseek", "xai").
    pub provider: Option<String>,
    pub model: String,
    pub max_tokens: u32,
    /// Maximum number of concurrency-safe tools to run in parallel
    /// within a single batch. 0 = unlimited. Default 8.
    pub max_parallelism: usize,
    /// +** — per-turn API call budget. Default **200**.
    ///   When exceeded, engine returns gracefully with
    ///   `StopReason::MaxTurnsReached` instead of an error — partial work
    ///   is preserved and surfaced to the caller. Lower this for sub-
    ///   agents you want to cap (e.g. compactors at 1, memory-extractors
    ///   at 5).
    pub max_api_calls_per_turn: u32,
    /// + §3 — auto-routing**: optional "strong" tier model. When set,
    ///   the engine starts each turn at `model` (cheap/fast tier, e.g.
    ///   `ds-v4-flash`) and **escalates to `strong_model`** mid-turn on any
    ///   of these signals:
    ///   - `tool_calls_so_far >= 3` (multi-step exploration)
    ///   - previous tool_result was an error (need smarter retry)
    ///   - turn message count >= 8 (deep conversation)
    ///
    /// Once escalated, stays at strong for rest of the turn.
    /// `None` = no routing (use `model` for everything; current default).
    /// User-facing API: surface as `--strong-model` CLI flag, or in
    /// `~/.atta/code/settings.json`.
    pub strong_model: Option<String>,
    /// **P2 (Phase 2)**: optional fallback model for retry on overloaded /
    /// transport errors. When the primary model returns 503/529/transport
    /// error, the engine falls back to this model for the remaining turns.
    /// Once activated, stays active for the rest of the session to avoid
    /// oscillation.
    /// `None` = no fallback (propagate the error immediately).
    /// User-facing API: surface as `--fallback-model` CLI flag.
    pub fallback_model: Option<String>,
    /// **L1 **: thinking-mode policy.
    /// - `Auto` — current default; engine picks based on model id (DS V4
    ///   gets `Disabled` to avoid the multi-turn 400 reasoning_content
    ///   protocol mismatch; everyone else gets `None` = let model decide).
    /// - `Off` — explicit `thinking: {"type":"disabled"}` in request.
    /// - `On` — explicit `thinking: Adaptive`.
    /// - `OnBudget(N)` — explicit `thinking: Enabled{budget_tokens=N}`.
    pub thinking_mode: ThinkingModeConfig,

    // ---- permission / sandbox ----
    pub permission_mode: PermissionMode,
    /// 是否禁用 BashTool 沙盒（才有效；占位）
    pub dangerously_disable_sandbox: bool,
    /// **Hardening **: Bash sandbox extended policy. Default:
    /// `default_deny_read` baked-in (~/.ssh, ~/.aws, etc), network unrestricted.
    /// Settings.json `sandbox.{deny_read,allow_read,allowed_domains,network_mode}`
    /// overrides any field. Tool layer (`attacode-tools::bash::sandbox`) reads
    /// these fields to build its platform-specific profile.
    pub sandbox_policy: SandboxPolicyConfig,

    // ---- file-op limits (grouped 2026-05-09; was 3 flat fields) ----
    /// File read/write byte/line limits. Sub-config keeps related field
    /// values together so cross-tool tuning (Read/Write/Edit) is one
    /// import path.
    pub file_limits: FileLimits,

    // ---- compaction (grouped 2026-05-09; was 2 flat fields) ----
    /// autoCompact configuration: when to fire + which model to summarize.
    pub compact: CompactSettings,

    // ---- system-prompt assembly (grouped 2026-05-09; was 5 flat fields) ----
    /// system prompt overrides + extras (ATTA.md walk, output styles,
    /// append/override). All affect what gets baked into block [1-5] of
    /// the system prompt — see docs/SYSTEM_PROMPT.md.
    pub system_prompt: SystemPromptSettings,

    /// 最大 agent 嵌套深度。主 agent 深度 0，每经 AgentTool spawn 加 1。
    /// 超限时 AgentTool 返回 tool error（防模型在子 agent prompt 中写"用 AgentTool"
    /// 导致无限递归）。默认 3。设 0 禁用 AgentTool。
    pub max_agent_depth: u32,

    // ---- misc behavior ----
    /// **RC-23**: 是否保留完整工具结果文本。默认 false —— 工具结果会被截断到
    /// 2KB 再送回给模型。设 true 时原样透传（长输出场景可能显著消耗 context）。
    pub verbose_tool_results: bool,
}

/// **§5 (2026-05-09)**: file-op limits sub-config. Was 3 flat fields on
/// `EngineConfig`; grouping them surfaces the "tuning the FileRead/Write
/// envelope" cluster as one knob.
#[derive(Debug, Clone)]
pub struct FileLimits {
    /// 文件读 / 写的字节上限。FileRead 检查 metadata.len() 用。
    pub max_file_read_bytes: u64,
    /// 单次 FileRead 默认行数（offset/limit 没填时）
    pub default_read_lines: usize,
    /// 单行字符数上限（超了截断 + `[truncated]` 标记）
    pub max_line_chars: usize,
    /// Tool result 文本内容字符数上限。超长的 result text 在回灌 messages 前
    /// 截断为 `[Output truncated to N chars]`，防止撑爆 context。
    /// 0 = 不截断。默认 50KB。
    pub max_tool_result_chars: usize,
}

impl Default for FileLimits {
    fn default() -> Self {
        Self {
            max_file_read_bytes: 10 * 1024 * 1024,
            default_read_lines: 2000,
            max_line_chars: 2000,
            max_tool_result_chars: 50 * 1024,
        }
    }
}

/// **§5 (2026-05-09)**: autoCompact sub-config.
#[derive(Debug, Clone)]
pub struct CompactSettings {
    /// 触发 autoCompact 的输入 token 阈值。0 = 禁用 autoCompact。
    pub threshold_tokens: usize,
    /// 执行压缩用的模型 id（比主模型便宜的 haiku 类）。空 → 复用 model
    pub model: Option<String>,
    /// **P2 **: collapse/micro compact 保留尾部多少条 messages verbatim。
    /// 默认 6（≈3 个 user/assistant pair）。调大保留更多上下文但压缩收益减少。
    pub micro_keep_recent: usize,
}

impl Default for CompactSettings {
    fn default() -> Self {
        Self {
            // 由 main.rs 按模型 context window 再覆写；这里保底给一个中等值。
            threshold_tokens: 150_000,
            model: None,
            micro_keep_recent: 6,
        }
    }
}

/// **§5 (2026-05-09)**: system-prompt assembly knobs sub-config.
#[derive(Debug, Clone, Default)]
pub struct SystemPromptSettings {
    pub append: Option<String>,
    pub override_text: Option<String>,
    /// Preferred response language. None = use the user's language/context.
    pub language: Option<String>,
    /// Enable Anthropic prompt-caching `scope: "global"` for the static system
    /// prompt prefix. Requires the prompt-caching-scope beta header and may not
    /// be supported by non-Anthropic-compatible endpoints.
    pub global_cache_scope: bool,
    /// True when at least one connected MCP tool is rendered in the tool list.
    /// MCP tools are per-user/session, so system prompt global caching is
    /// downgraded to normal org/provider caching in this case.
    pub mcp_tools_present: bool,
    /// Instructions returned by connected MCP servers during initialize.
    pub mcp_instructions: Vec<McpServerInstruction>,
    /// 是否在 cwd 之上向上爬找 ATTA.md。默认 true。
    /// monorepo 子目录不想吃父级 monorepo 上下文时设 false。
    pub memory_walk_up: bool,
    /// **A-5 **: 选中的 output style 名称（user/project 级别均可）。
    /// 引擎在 collect FrozenContext 时按此名称从
    /// `~/.atta/code/output-styles/<name>.md` 或 `<cwd>/.atta/code/output-styles/<name>.md`
    /// 读取内容，注入 system prompt 末段。None = 不注入额外 style。
    pub output_style: Option<String>,
    /// When true (default), use TaskCreate/Update/List/Get/Stop instead of
    /// TodoWrite for task tracking. Mirrors TS's `isTodoV2Enabled()` gating.
    /// When false, TodoWrite is used and the V2 Task tools are hidden.
    pub todo_v2_enabled: bool,
    /// When true, register TeamCreate/TeamDelete for multi-agent orchestration.
    /// Mirrors TS's `isAgentSwarmsEnabled()` gating. Default false.
    pub agent_teams_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct McpServerInstruction {
    pub name: String,
    pub instructions: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ThinkingModeConfig {
    /// Engine decides based on model id. Default.
    #[default]
    Auto,
    /// Explicit "no thinking" — sends `thinking: {type: "disabled"}`.
    Off,
    /// Explicit adaptive thinking.
    On,
    /// Explicit budgeted thinking.
    OnBudget(u32),
}

/// **Hardening **: data-only sandbox policy in core (so `EngineConfig`
/// doesn't need to depend on the tools crate). Lifted to a real `SandboxPolicy`
/// inside `attacode-tools::bash::sandbox` at call-site.
#[derive(Debug, Clone, Default)]
pub struct SandboxPolicyConfig {
    pub allow_read: Vec<PathBuf>,
    /// Empty = use built-in defaults. Non-empty = use this list verbatim
    /// (caller can disable defaults entirely by passing `[PathBuf::from("")]`
    /// — actually no, we use Option semantics: None means "use defaults",
    /// Some(vec) means "use exactly this").
    pub deny_read: Option<Vec<PathBuf>>,
    pub network_mode: NetworkModeConfig,
    pub allowed_domains: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkModeConfig {
    #[default]
    Unrestricted,
    DenyAll,
    Allowlist,
}

impl EngineConfig {
    /// 合理默认值；具体使用方根据 CLI 与 settings 覆盖。
    pub fn defaults_for(model: impl Into<String>) -> Self {
        let model = model.into();
        let compact_threshold = default_auto_compact_threshold(&model, 65535);
        Self {
            // model / runtime
            provider: None,
            model,
            max_tokens: 16384,
            // **P7 **: 4 → 8 to align closer to TS reference (which defaults
            // to 10). 8 leaves a 2-slot safety margin for tokio scheduler under
            // heavy parallel Read/Glob/Grep batches.
            max_parallelism: 10,
            max_api_calls_per_turn: 200,
            strong_model: None,
            fallback_model: None,
            thinking_mode: ThinkingModeConfig::default(),

            // permission / sandbox
            permission_mode: PermissionMode::Default,
            dangerously_disable_sandbox: false,
            sandbox_policy: SandboxPolicyConfig::default(),

            // grouped sub-configs
            file_limits: FileLimits::default(),
            compact: CompactSettings {
                threshold_tokens: compact_threshold,
                ..Default::default()
            },
            system_prompt: SystemPromptSettings {
                memory_walk_up: true,
                todo_v2_enabled: true,
                agent_teams_enabled: false,
                ..Default::default()
            },

            // misc
            max_agent_depth: 3,
            verbose_tool_results: false,
        }
    }
}

/// 依据模型名推断其上下文窗口大小。
///
/// 依据"模型能装多少"来确定 auto-compact / blocking limit 阈值，
/// 而非对所有模型使用相同的固定值。
///
/// 当前实现是保守启发式，允许用户通过 CLI / settings 覆盖。
pub fn infer_context_window_tokens(model: &str) -> usize {
    let m = model.to_ascii_lowercase();
    if m.contains("deepseek") || m.starts_with("ds-v") || m.starts_with("ds_") {
        1_000_000
    } else if m.contains("codex") {
        400_000
    } else {
        200_000
    }
}

/// Proactive compact threshold:
/// effective_window - 13k buffer.
pub fn default_auto_compact_threshold(model: &str, max_tokens: u32) -> usize {
    let effective_window = infer_context_window_tokens(model).saturating_sub(max_tokens as usize);
    effective_window.saturating_sub(13_000)
}

/// Blocking limit:
/// effective_window - 3k buffer, used to preserve room for manual /compact.
pub fn default_blocking_limit(model: &str, max_tokens: u32) -> usize {
    let effective_window = infer_context_window_tokens(model).saturating_sub(max_tokens as usize);
    effective_window.saturating_sub(3_000)
}
