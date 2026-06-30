//! 一组"轻量"工具集合：要么 alias 到已有能力（Skill/Brief），要么是简单
//! cross-platform spawn（PowerShell/REPL），要么是 Task 系扩展（TaskOutput），
//! 要么是 Config 的 read-only 视图。
//!
//! 与对应工具对齐 schema 字段名 + 语义。

use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

/// Default PowerShell timeout (60s).
const DEFAULT_POWERSHELL_TIMEOUT_MS: u64 = 60_000;
/// Maximum PowerShell timeout (600s).
const MAX_POWERSHELL_TIMEOUT_MS: u64 = 600_000;
/// REPL timeout (30s, both default and max).
const REPL_TIMEOUT_MS: u64 = 30_000;

// ============ BriefTool placeholder marker ============
//
// Brief 是"短消息给用户看"——本质是 AskUserQuestion 的简化形式（不强制
// 用户回 y/n，仅显示）。Rust 直接调 effects.append_system_message。

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BriefInput {
    /// Short message to surface to the user (1-2 sentences)
    pub message: String,
}

pub struct BriefTool;

#[async_trait]
impl Tool for BriefTool {
    fn description(&self) -> &str {
        "Send a message to the user"
    }
    fn name(&self) -> &str {
        "SendUserMessage"
    }

    /// **P3b **: 标 deferred —— 系统 prompt 仅暴露 name + 短描述，模型用
    /// ToolSearch 激活后下一轮拉 full schema。减少静态 prompt 占用。
    fn is_deferred(&self) -> bool {
        true
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(BriefInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        "Send a short message the user will read. Use this to surface important \
         information: background task completion, blocking issues, or key decisions \
         the user needs to know about. The message is displayed prominently — keep \
         it concise (1-3 sentences, markdown supported)."
            .into()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<BriefInput>(input.clone()) {
            Ok(p) if p.message.trim().is_empty() => {
                ValidationResult::err("message must not be empty", 1)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: BriefInput = serde_json::from_value(input)?;
        if let Some(ref effects) = ctx.effects {
            effects.append_system_message("notice", &input.message);
        }
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(format!("Briefed: {}", input.message)),
            is_error: false,
            structured_content: Some(json!({"message": input.message})),
            mcp_meta: None,
            new_messages: Some(vec![]),
        })
    }
}

// ============ TaskOutputTool ============
//
// Retrieves output from background tasks (shell, agent, or remote session).
// TS parity: Claude Code's TaskOutput — polls running background task output.
// Takes a task_id identifying the background task; returns its output and status.
// For declarative tasks (TaskCreate/TaskGet), use the TaskList/TaskGet tools directly.

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskOutputInput {
    pub task_id: String,
}

pub struct TaskOutputTool;

#[async_trait]
impl Tool for TaskOutputTool {
    fn description(&self) -> &str {
        "Get output from a running background task"
    }
    fn name(&self) -> &str {
        "TaskOutput"
    }

    /// **P3f **: deferred -- only Bash/Read/Edit/ToolSearch 4 eager.
    /// Other tools activated via ToolSearch, saving ~13KB tools schema.
    fn is_deferred(&self) -> bool {
        true
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(TaskOutputInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        "DEPRECATED: Prefer using the Read tool on the task's output file path \
         instead. Background tasks return their output file path in the tool \
         result, and you receive a <task-notification> with the same path when \
         the task completes — Read that file directly.\n\
         \n\
         - Retrieves output from a running or completed task (background shell, \
         agent, or remote session)\n\
         - Takes a task_id parameter identifying the task\n\
         - Returns the task output along with status information\n\
         - Use block=true (default) to wait for task completion\n\
         - Use block=false for non-blocking check of current status\n\
         - Task IDs can be found using the /tasks command\n\
         - Works with all task types: background shells, async agents, and remote sessions"
            .into()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<TaskOutputInput>(input.clone()) {
            Ok(p) if p.task_id.trim().is_empty() => {
                ValidationResult::err("task_id must not be empty", 1)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: TaskOutputInput = serde_json::from_value(input)?;

        // Query running background tasks via the RunningTasksCallback (injected
        // by the CLI layer). Returns (output, events_log, status) for the task.
        if let Some(running_tasks) = &ctx.running_tasks {
            if let Some((output, events, status)) = running_tasks.find(&input.task_id) {
                let body = format!(
                    "Background task {} — status={:?}\n\nEvents:\n{}\n\nOutput so far:\n{}",
                    input.task_id,
                    status,
                    if events.is_empty() {
                        "(none yet)".to_string()
                    } else {
                        events.join("\n")
                    },
                    if output.is_empty() {
                        "(no output yet)".to_string()
                    } else {
                        output.clone()
                    }
                );
                return Ok(ToolResult {
                    content: base::tool::ToolResultContent::Text(body),
                    is_error: matches!(status, base::context::RunningStatus::Failed(_)),
                    structured_content: Some(json!({
                        "task_id": input.task_id,
                        "status": format!("{status:?}"),
                        "events": events,
                        "output": output,
                        "background": true})),
                    mcp_meta: None,
                    new_messages: Some(vec![]),
                });
            }
        }

        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(format!(
                "no running background task with id={}",
                input.task_id
            )),
            is_error: true,
            structured_content: None,
            mcp_meta: None,
            new_messages: Some(vec![]),
        })
    }
}

// ============ ConfigTool ============
//
// TS 的 ConfigTool 是 interactive UI；我们做 read-only：返回当前 settings 摘要
// （和 /config slash 同信息）+ 指向 settings.json 路径让用户手改。
// 写操作不做（避免 attacode 自己改 settings 形成怪圈）。

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct ConfigInput {
    /// 字段名（点分路径），如 `permissions.default_mode`。None = 输出完整摘要
    #[serde(default)]
    pub field: Option<String>,
}

pub struct ConfigTool {
    /// 启动时 cli 把"effective config snapshot"传进来；in-memory 只读
    snapshot: Arc<Value>,
}

impl ConfigTool {
    /// Construct a new instance.
    pub fn new(snapshot: Value) -> Self {
        Self {
            snapshot: Arc::new(snapshot),
        }
    }
}

#[async_trait]
impl Tool for ConfigTool {
    fn description(&self) -> &str {
        "View current configuration settings"
    }
    fn name(&self) -> &str {
        "Config"
    }

    /// **P3b **: 标 deferred —— 系统 prompt 仅暴露 name + 短描述，模型用
    /// ToolSearch 激活后下一轮拉 full schema。减少静态 prompt 占用。
    fn is_deferred(&self) -> bool {
        true
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ConfigInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        "Get or set Claude Code configuration settings.\n\
         View or change Claude Code settings. Use when the user requests \
         configuration changes, asks about current settings, or when adjusting \
         a setting would benefit them.\n\
         \n\
         ## Usage\n\
         - **Get current value:** Omit the \"value\" parameter\n\
         - **Set new value:** Include the \"value\" parameter"
            .into()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    async fn validate_input(&self, _: &Value, _: &ToolContext) -> ValidationResult {
        ValidationResult::Ok
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn call(
        &self,
        input: Value,
        _ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: ConfigInput = serde_json::from_value(input).unwrap_or_default();
        let snapshot = self.snapshot.as_ref();
        let target = match &input.field {
            Some(path) => walk_dotted(snapshot, path),
            None => Some(snapshot.clone()),
        };
        let body = match &target {
            Some(v) => serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()),
            None => format!("(no such field: {})", input.field.as_deref().unwrap_or("?")),
        };
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(body),
            is_error: false,
            structured_content: target.map(|v| json!({"field": input.field, "value": v})),
            mcp_meta: None,
            new_messages: Some(vec![]),
        })
    }
}

fn walk_dotted(v: &Value, path: &str) -> Option<Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur.clone())
}

// ============ PowerShellTool ============
//
// 在 PATH 上有 `pwsh`（PowerShell Core，跨平台）就可以跑；未找到时返回清晰错误。
// Windows 上还兼容老的 `powershell.exe`（Windows PowerShell 5.x）。
// 与 TS 同 schema：command。

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PowerShellInput {
    pub command: String,
    /// timeout in milliseconds (default 60s, max 600s)
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

pub struct PowerShellTool;

#[async_trait]
impl Tool for PowerShellTool {
    fn description(&self) -> &str {
        "Run a PowerShell command"
    }
    fn name(&self) -> &str {
        "PowerShell"
    }

    /// **P3b **: 标 deferred —— 系统 prompt 仅暴露 name + 短描述，模型用
    /// ToolSearch 激活后下一轮拉 full schema。减少静态 prompt 占用。
    fn is_deferred(&self) -> bool {
        true
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(PowerShellInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        "Executes a given PowerShell command with optional timeout. Working directory \
         persists between commands; shell state (variables, functions) does not.\n\
         \n\
         IMPORTANT: This tool is for terminal operations via PowerShell: git, npm, \
         docker, and PS cmdlets. DO NOT use it for file operations (reading, writing, \
         editing, searching, finding files) - use the specialized tools for this instead.\n\
         \n\
         PowerShell eddition: PowerShell 7+ or Windows PowerShell 5.1\n\
         \n\
         Usage notes:\n\
         - The command argument is required.\n\
         - You can specify an optional timeout in milliseconds.\n\
         - Avoid using PowerShell to run commands that have dedicated tools:\n\
           - File search: Use Glob (NOT Get-ChildItem -Recurse)\n\
           - Content search: Use Grep (NOT Select-String)\n\
           - Read files: Use Read (NOT Get-Content)\n\
           - Edit files: Use Edit\n\
           - Write files: Use Write (NOT Set-Content/Out-File)\n\
         - For git commands:\n\
           - Prefer to create a new commit rather than amending\n\
           - Never skip hooks unless the user explicitly asks\n\
         - Interactive and blocking commands will hang — use -NonInteractive"
            .into()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn permission_match_content(&self, input: &Value) -> Option<String> {
        serde_json::from_value::<PowerShellInput>(input.clone())
            .ok()
            .map(|i| i.command)
    }
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<PowerShellInput>(input.clone()) {
            Ok(p) if p.command.trim().is_empty() => {
                ValidationResult::err("command must not be empty", 1)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::Ask {
            message: "PowerShell command".into(),
            decision_reason: None,
        }
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: PowerShellInput = serde_json::from_value(input)?;
        let timeout = std::time::Duration::from_millis(
            input
                .timeout_ms
                .unwrap_or(DEFAULT_POWERSHELL_TIMEOUT_MS)
                .min(MAX_POWERSHELL_TIMEOUT_MS),
        );

        let program = pick_powershell().ok_or_else(|| {
            #[allow(clippy::useless_format)]
            ToolError::exec(format!(
                "no PowerShell binary on PATH (looked for `pwsh`, `pwsh-preview`, and on Windows \
                 `powershell.exe`). Install PowerShell Core from https://aka.ms/powershell or use \
                 the Bash tool for shell tasks."
            ))
        })?;

        let mut cmd = tokio::process::Command::new(&program);
        cmd.arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(&input.command);
        cmd.current_dir(&ctx.cwd);
        let out = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| ToolError::exec(format!("{program} timed out after {timeout:?}")))?
            .map_err(|e| ToolError::exec(format!("{program} spawn: {e}")))?;
        let stdout = truncate_output(&String::from_utf8_lossy(&out.stdout));
        let stderr = truncate_output(&String::from_utf8_lossy(&out.stderr));
        let body = if !stderr.is_empty() {
            format!("{stdout}\n--- stderr ---\n{stderr}")
        } else {
            stdout
        };
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(body),
            is_error: !out.status.success(),
            structured_content: Some(json!({
                "program": program,
                "exit_code": out.status.code()})),
            mcp_meta: None,
            new_messages: Some(vec![]),
        })
    }
}

/// Pick the first usable PowerShell binary on PATH. PowerShell Core (`pwsh`) is
/// cross-platform; `powershell.exe` is Windows PowerShell 5.x as a fallback.
fn pick_powershell() -> Option<String> {
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &["pwsh", "pwsh-preview", "powershell"]
    } else {
        &["pwsh", "pwsh-preview"]
    };
    for c in candidates {
        if which::which(c).is_ok() {
            return Some(c.to_string());
        }
    }
    None
}

/// Cap process output at 200 KiB to keep tool results LLM-friendly.
const MAX_OUTPUT_BYTES: usize = 200 * 1024;
fn truncate_output(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s.to_string();
    }
    let cut = MAX_OUTPUT_BYTES;
    // Walk back from the byte limit to the nearest UTF-8 char boundary
    // to avoid panicking on multi-byte text (e.g. CJK, emoji).
    let safe_cut = (0..=cut)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    let mut out = String::with_capacity(safe_cut + 64);
    out.push_str(&s[..safe_cut]);
    out.push_str(&format!(
        "\n\n[... output truncated: {} bytes total, showing first {} bytes ...]",
        s.len(),
        safe_cut
    ));
    out
}

// ============ REPLTool ============
//
// 跑 `python -c <expr>` / `node -e <expr>` —— 单 shot eval，不维护 REPL 状态。
// TS 的实现可能更花哨；我们 minimal viable。

#[derive(Debug, serde::Serialize, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReplLanguage {
    Python,
    Node,
    Ruby,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReplInput {
    pub language: ReplLanguage,
    /// Code to evaluate
    pub code: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

pub struct ReplTool;

#[async_trait]
impl Tool for ReplTool {
    fn description(&self) -> &str {
        "Evaluate a single expression in python/node/ruby"
    }
    fn name(&self) -> &str {
        "REPL"
    }

    /// **P3b **: 标 deferred —— 系统 prompt 仅暴露 name + 短描述，模型用
    /// ToolSearch 激活后下一轮拉 full schema。减少静态 prompt 占用。
    fn is_deferred(&self) -> bool {
        true
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ReplInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        "- Eval a short snippet in Python, Node.js, or Ruby (one-shot — no REPL \
         state maintained between calls)\n\
         - Use for quick math / data shaping / string manipulation when shelling out \
         is overkill\n\
         - Capped at 30s and 200KB of output\n\
         - Errors come back via stderr in tool result\n\
         \n\
         When NOT to use:\n\
         - For file operations — use Read/Write/Edit instead\n\
         - For multi-step scripting — use Bash instead\n\
         - For installing packages or running servers — use Bash instead"
            .into()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn permission_match_content(&self, input: &Value) -> Option<String> {
        serde_json::from_value::<ReplInput>(input.clone())
            .ok()
            .map(|i| i.code)
    }
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<ReplInput>(input.clone()) {
            Ok(p) if p.code.trim().is_empty() => ValidationResult::err("code must not be empty", 1),
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::Ask {
            message: "REPL eval".into(),
            decision_reason: None,
        }
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: ReplInput = serde_json::from_value(input)?;
        let timeout = std::time::Duration::from_millis(
            input
                .timeout_ms
                .unwrap_or(REPL_TIMEOUT_MS)
                .min(REPL_TIMEOUT_MS),
        );
        let (program, args): (&str, Vec<&str>) = match input.language {
            ReplLanguage::Python => ("python3", vec!["-c", &input.code]),
            ReplLanguage::Node => ("node", vec!["-e", &input.code]),
            ReplLanguage::Ruby => ("ruby", vec!["-e", &input.code]),
        };
        let mut cmd = tokio::process::Command::new(program);
        for a in &args {
            cmd.arg(a);
        }
        cmd.current_dir(&ctx.cwd);
        let out = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| ToolError::exec(format!("REPL ({program}) timed out")))?
            .map_err(|e| {
                #[allow(clippy::useless_format)]
                ToolError::exec(format!(
                    "REPL spawn ({program}): {e}; ensure {program} is installed"
                ))
            })?;
        let stdout = truncate_output(&String::from_utf8_lossy(&out.stdout));
        let stderr = truncate_output(&String::from_utf8_lossy(&out.stderr));
        let body = if !stderr.is_empty() {
            format!("{stdout}\n--- stderr ---\n{stderr}")
        } else {
            stdout
        };
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(body),
            is_error: !out.status.success(),
            structured_content: Some(
                json!({"language": input.language, "exit_code": out.status.code()}),
            ),
            mcp_meta: None,
            new_messages: Some(vec![]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> ToolContext {
        ToolContext::for_test(PathBuf::from("/tmp"))
    }

    // ---- BriefTool ----

    #[tokio::test]
    async fn brief_validates_empty() {
        let tool = BriefTool;
        let r = tool.validate_input(&json!({"message": "  "}), &ctx()).await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn brief_emits_system_message() {
        let tool = BriefTool;
        let r = tool
            .call(
                json!({"message": "refactoring"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
    }

    // ---- ConfigTool ----

    #[tokio::test]
    async fn config_returns_full_snapshot_when_no_field() {
        let snap = json!({"model": "test", "permissions": {"default_mode": "auto"}});
        let tool = ConfigTool::new(snap);
        let r = tool
            .call(json!({}), ctx(), ProgressSender::noop("t"))
            .await
            .unwrap();
        match r.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("test"));
                assert!(s.contains("default_mode"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn config_drills_dotted_path() {
        let snap = json!({"permissions": {"default_mode": "auto"}});
        let tool = ConfigTool::new(snap);
        let r = tool
            .call(
                json!({"field": "permissions.default_mode"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("auto"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn config_unknown_field() {
        let snap = json!({"x": 1});
        let tool = ConfigTool::new(snap);
        let r = tool
            .call(
                json!({"field": "no.such"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            base::tool::ToolResultContent::Text(s) => assert!(s.contains("no such")),
            _ => panic!(),
        }
    }

    // ---- PowerShell ----

    #[tokio::test]
    async fn powershell_runs_when_pwsh_present_or_errors_clearly() {
        // Skip the case where pwsh isn't installed in CI — we just check the
        // tool's response is shaped sensibly (either a clean run or a clear
        // "no PowerShell binary" error).
        let tool = PowerShellTool;
        let r = tool
            .call(
                json!({"command": "Write-Output 'pwsh-ok'"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await;
        match r {
            Ok(tr) => {
                if !tr.is_error {
                    if let base::tool::ToolResultContent::Text(s) = tr.content {
                        assert!(s.contains("pwsh-ok"));
                    }
                }
            }
            Err(e) => {
                let msg = format!("{e}");
                assert!(msg.contains("PowerShell") || msg.contains("pwsh"));
            }
        }
    }

    #[test]
    fn truncate_output_caps_long_strings() {
        let s = "x".repeat(MAX_OUTPUT_BYTES + 1000);
        let t = truncate_output(&s);
        assert!(t.len() <= MAX_OUTPUT_BYTES + 200);
        assert!(t.contains("output truncated"));
    }

    // ---- REPL ----（依赖 python3 / node 安装；跑测试机器一般有 python3）

    #[tokio::test]
    async fn repl_python_simple_expr() {
        // Skip if python3 not on PATH (CI may not have it)
        let py_check = std::process::Command::new("python3")
            .arg("--version")
            .output();
        if py_check.is_err() || !py_check.unwrap().status.success() {
            return;
        }
        let tool = ReplTool;
        let r = tool
            .call(
                json!({"language": "python", "code": "print(2+2)"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        if !r.is_error {
            match r.content {
                base::tool::ToolResultContent::Text(s) => {
                    assert!(s.contains("4"));
                }
                _ => panic!(),
            }
        }
    }

    // ---- TaskOutput ----

    #[tokio::test]
    async fn task_output_returns_error_for_unknown_id() {
        let tool = TaskOutputTool;
        let r = tool
            .call(
                json!({"task_id": "nonexistent"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        match r.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("no running background task"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn task_output_queries_running_tasks_callback() {
        use base::tool::RunningTasksCallback;
        use std::sync::Mutex;

        struct TestTasks {
            #[allow(clippy::type_complexity)]
            data: Mutex<
                std::collections::HashMap<
                    String,
                    (String, Vec<String>, base::context::RunningStatus),
                >,
            >,
        }
        impl RunningTasksCallback for TestTasks {
            fn find(
                &self,
                task_id: &str,
            ) -> Option<(String, Vec<String>, base::context::RunningStatus)> {
                self.data.lock().unwrap().get(task_id).cloned()
            }
            fn cancel(&self, _task_id: &str) -> bool {
                false
            }
        }
        impl std::fmt::Debug for TestTasks {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("TestTasks").finish()
            }
        }

        let tasks = Arc::new(TestTasks {
            data: Mutex::new(std::collections::HashMap::from([(
                "task-1".into(),
                (
                    "hello world".into(),
                    vec!["event1".into()],
                    base::context::RunningStatus::Running,
                ),
            )])),
        });

        let mut c = ToolContext::for_test(PathBuf::from("/tmp"));
        c.running_tasks = Some(tasks as Arc<dyn RunningTasksCallback>);

        let tool = TaskOutputTool;
        let r = tool
            .call(json!({"task_id": "task-1"}), c, ProgressSender::noop("t"))
            .await
            .unwrap();
        assert!(!r.is_error);
        match r.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("hello world"));
                assert!(s.contains("event1"));
            }
            _ => panic!(),
        }
    }
}
