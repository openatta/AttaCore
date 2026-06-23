//! BashTool —— "Bash"。
//!
//! ：
//! - `bash -c <command>` 子进程，cwd = session.cwd
//! - **平台沙盒包装**（macOS sandbox-exec / Linux bwrap，受 `dangerously_disable_sandbox` 控制）
//! - stdout / stderr 行级流式喂给 `ProgressSender`
//! - 超时（默认 120s，上限 600s）+ cancel 通过 kill child 实现
//! - 命令分类用关键字白名单 / 黑名单（read-only / destructive）
//!
//! 见 docs/RUST_ARCHITECTURE.md §8 与 docs/_ACCEPTANCE.md 场景 2 / 6。

mod sandbox;
pub use sandbox::{sandbox_status, SandboxMode};

use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult, ValidationResult};
use futures::StreamExt;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio_util::codec::{FramedRead, LinesCodec};

/// 默认执行超时（120 秒）
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// 执行超时硬上限（10 分钟）
const MAX_TIMEOUT_MS: u64 = 600_000;
/// 输出文本上限（防止一个 yes / cat /dev/random 把内存吃光）
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
// Stall detection (ref `LocalShellTask.startStallWatchdog`) is intentionally NOT
// ported here: the ref's foreground `BashTool` is non-interactive (stdin null —
// see BashTool/prompt.ts "interactive input not supported") and has no stall
// watchdog. That watchdog belongs to background `LocalShellTask` (output to a
// file, polled for growth), which this foreground bash tool doesn't implement.
// Commands can't block on interactive prompts (stdin is null → EOF), and any
// genuine stall hits the per-command `timeout` below.

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BashInput {
    /// The shell command to execute (run via `bash -c`).
    pub command: String,

    /// Optional execution timeout in milliseconds (default 120000, max 600000).
    #[serde(default)]
    pub timeout_ms: Option<u64>,

    /// Brief one-sentence description of what the command does (for UI).
    #[serde(default)]
    pub description: Option<String>,

    /// Run the command as a detached background task instead of blocking.
    /// The model can poll with TaskOutput or check /proc for completion.
    #[serde(default)]
    #[serde(alias = "run_in_background")]
    pub run_in_background: bool}

#[derive(Debug, Default, Clone, Copy)]
pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Run a shell command (sandboxed by default)."
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(BashInput))
            .expect("schemars output is valid JSON")
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/bash.prompt.md").to_string()
    }

    fn is_concurrency_safe(&self, input: &Value) -> bool {
        self.is_read_only(input)
    }

    fn interrupt_behavior(&self, _input: &Value) -> base::tool::InterruptBehavior {
        base::tool::InterruptBehavior::Block
    }

    fn is_read_only(&self, input: &Value) -> bool {
        parse_classification(input)
            .map(|c| c.read_only)
            .unwrap_or(false)
    }

    fn is_destructive(&self, input: &Value) -> bool {
        parse_classification(input)
            .map(|c| c.destructive)
            .unwrap_or(true) // 未知命令保守按 destructive 看
    }

    fn permission_match_content(&self, input: &Value) -> Option<String> {
        serde_json::from_value::<BashInput>(input.clone())
            .ok()
            .map(|p| p.command)
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let parsed: Result<BashInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) if p.command.trim().is_empty() => {
                ValidationResult::err("command must not be empty", 1)
            }
            Ok(p) if p.timeout_ms.unwrap_or(0) > MAX_TIMEOUT_MS => {
                ValidationResult::err(format!("timeout_ms exceeds {} ms cap", MAX_TIMEOUT_MS), 3)
            }
            // Block `sleep N` (N >= 2) — use Monitor tool instead for polling.
            Ok(ref p) if is_long_sleep(&p.command) => ValidationResult::err(
                "sleep >= 2s is blocked — use the Monitor tool for polling, or sleep 1 if you must",
                4,
            ),
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2)}
    }

    async fn check_permissions(&self, input: &Value, _: &ToolContext) -> PermissionDecision {
        // 已知只读命令（ls、cat、git status、cargo build 等）自动允许，
        // 不弹权限窗。其余命令走 gate 模式分派（bypass → allow,
        // acceptEdits → allow if readonly, default → ask）。
        if self.is_read_only(input) {
            return PermissionDecision::Allow {
                decision_reason: Some("read_only".into())};
        }
        PermissionDecision::Ask {
            message: "Bash command requires confirmation".into(),
            decision_reason: None}
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: BashInput = serde_json::from_value(input)?;
        let timeout = Duration::from_millis(
            input
                .timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        );

        // 平台沙盒包装：拒写 cwd / additional 之外。
        // dangerously_disable_sandbox=true 时直跑 bash。
        let writable: Vec<PathBuf> = ctx.additional_writable_dirs.clone();
        let wrapped = sandbox::wrap(sandbox::SandboxOptions {
            command: &input.command,
            cwd: &ctx.cwd,
            additional_writable: &writable,
            disable: ctx.dangerously_disable_sandbox,
            policy: sandbox::SandboxPolicy::default()});
        if wrapped.mode == SandboxMode::Unavailable {
            tracing::warn!(
                platform = std::env::consts::OS,
                "BashTool sandbox unavailable on this platform; running unsandboxed. \
                 Install bwrap (Linux) or use --dangerously-disable-sandbox to silence."
            );
        }

        let mut cmd = tokio::process::Command::new(&wrapped.program);
        cmd.args(&wrapped.args);
        cmd.current_dir(&ctx.cwd);
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        // 防止子进程泄露 SIGPIPE / SIGINT 行为；bash -c 自带正常默认
        cmd.kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| ToolError::exec(e.to_string()))?;
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // 流式读 stdout / stderr
        let stdout_progress = progress.clone();
        let stderr_progress = progress.clone();
        let stdout_handle =
            tokio::spawn(async move { drain_stream(stdout, true, stdout_progress).await });
        let stderr_handle =
            tokio::spawn(async move { drain_stream(stderr, false, stderr_progress).await });

        // 等待 + 超时 + cancel
        let wait_result = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => {
                let _ = child.kill().await;
                return Err(ToolError::Cancelled);
            }
            _ = tokio::time::sleep(timeout) => {
                let _ = child.kill().await;
                return Err(ToolError::Timeout(timeout));
            }
            r = child.wait() => r};
        let status = wait_result.map_err(|e| ToolError::exec(e.to_string()))?;

        // exit code 130 (128 + SIGINT) = user pressed Ctrl-C during this tool.
        // Treat as cancellation so the engine stops the turn instead of feeding
        // a "tool error" back to the model, which would fight the user's intent.
        // Child has already exited, so no kill needed.
        if let Some(130) = status.code() {
            return Err(ToolError::Cancelled);
        }

        let stdout_text = stdout_handle.await.unwrap_or_default();
        let stderr_text = stderr_handle.await.unwrap_or_default();
        let mut combined = stdout_text;
        if !stderr_text.is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&stderr_text);
        }

        if combined.is_empty() {
            combined.push_str("(no output)");
        }
        let is_error = !status.success();
        if is_error {
            if let Some(code) = status.code() {
                combined.push_str(&format!("\n[exit code: {code}]"));
            } else {
                combined.push_str("\n[killed by signal]");
            }
        }

        if is_error {
            Ok(ToolResult::error_text(combined))
        } else {
            Ok(ToolResult::text(combined))
        }
    }
}

/// 把 stdout/stderr 行级转 String + 流式喂 progress。
/// 软上限到 MAX_OUTPUT_BYTES，超了截断（防止超长输出吃光内存）。
async fn drain_stream<R: AsyncRead + Unpin + Send + 'static>(
    reader: R,
    _is_stdout: bool,
    progress: ProgressSender,
) -> String {
    let mut framed = FramedRead::new(reader, LinesCodec::new_with_max_length(64 * 1024));
    let mut buf = String::new();
    while let Some(line_res) = framed.next().await {
        match line_res {
            Ok(line) => {
                if buf.len() < MAX_OUTPUT_BYTES {
                    buf.push_str(&line);
                    buf.push('\n');
                }
                let line_with_nl = format!("{line}\n");
                progress.send(&line_with_nl);
            }
            Err(_) => break}
    }
    if buf.len() >= MAX_OUTPUT_BYTES {
        buf.push_str("\n[output truncated]\n");
    }
    buf
}

#[derive(Debug, Clone, Copy)]
pub struct CmdClassification {
    pub read_only: bool,
    pub destructive: bool}

/// 命令中不允许出现的字符 — `;` 链式、`&` 后台/AND、`>` `<` 重定向、
/// `` ` `` 命令替换、`$` 变量展开/命令替换。
/// Detect `sleep N` where N >= 2 (blocking poll patterns). Single
/// `sleep 1` is allowed; longer sleeps should use the Monitor tool.
fn is_long_sleep(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    if !trimmed.starts_with("sleep ") && !trimmed.starts_with("sleep\t") {
        return false;
    }
    trimmed
        .split_whitespace()
        .nth(1)
        .and_then(|n| n.parse::<f64>().ok())
        .map(|n| n >= 2.0)
        .unwrap_or(false)
}

/// `|` 不在此列：pipe chain 单独检查每段内的程序是否安全。
/// `DESTRUCTIVE_PREFIXES` 等常量仍在 `classify` 模块中使用。
fn parse_classification(input: &Value) -> Option<CmdClassification> {
    let parsed = serde_json::from_value::<BashInput>(input.clone()).ok()?;
    Some(classify::classify(&parsed.command))
}

pub mod classify {
    use super::CmdClassification;
    use super::{DESTRUCTIVE_PREFIXES, READ_ONLY_COMMANDS, READ_ONLY_PREFIXES};

    /// 命令前缀分类。**保守**：未识别 → read_only=false, destructive=false（不算
    /// 安全也不算危险，走默认 ask 路径）。
    ///
    /// Compound commands with `&&` are split and each segment classified
    /// independently: all segments must be read-only for the whole to be
    /// read-only; any segment destructive → whole is destructive. This allows
    /// `cd dir && git status` to be recognized as read-only.
    pub fn classify(cmd: &str) -> CmdClassification {
        let trimmed = cmd.trim();
        if trimmed.is_empty() {
            return CmdClassification {
                read_only: true,
                destructive: false};
        }

        // Does the command contain quoted strings? If so, don't split on
        // `;` or `|` — those characters may be inside quotes (e.g.,
        // `bash -c 'echo hello; echo world'`). `&&` is safe to split
        // because it's almost never inside quotes.
        let has_quotes = trimmed.contains('\'') || trimmed.contains('\"');
        // Compound command: split on &&, ;, and | to classify each segment.
        // Each segment is classified independently; the overall verdict is
        // "safe only if all segments are safe" and "destructive if any
        // segment is destructive".
        let segments: Vec<&str> = if trimmed.contains("&&") {
            trimmed.split("&&").collect()
        } else if !has_quotes && trimmed.contains(';') {
            trimmed.split(';').collect()
        } else if !has_quotes && trimmed.contains('|') && !trimmed.contains("||") {
            // Only split on single pipe, not || (OR is conditional/unsafe).
            trimmed.split('|').collect()
        } else {
            vec![trimmed]
        };
        if segments.len() > 1 {
            let mut all_read_only = true;
            let mut any_destructive = false;
            for seg in segments {
                let c = classify_single(seg.trim());
                if !c.read_only {
                    all_read_only = false;
                }
                if c.destructive {
                    any_destructive = true;
                }
            }
            return CmdClassification {
                read_only: all_read_only,
                destructive: any_destructive};
        }

        classify_single(trimmed)
    }

    /// Safe wrappers that don't change the semantics of the wrapped command
    /// for classification purposes.
    const SAFE_WRAPPERS: &[&str] = &["timeout", "time", "nice", "nohup"];

    /// Strip safe wrappers from a command before classifying. E.g.,
    /// `timeout 5 rm -rf /` → `rm -rf /` for classification purposes.
    /// Also handles `nice -n 5 cmd`, `timeout 30s cmd`, etc.
    fn strip_safe_wrappers(cmd: &str) -> &str {
        let trimmed = cmd.trim();
        for wrapper in SAFE_WRAPPERS {
            if let Some(rest) = trimmed.strip_prefix(wrapper) {
                let rest = rest.trim_start();
                if rest.is_empty() {
                    return trimmed;
                }
                // Skip tokens that look like flags or numeric arguments:
                // -n, --adjustment, --signal=TERM, 5, 30s, -10
                let mut tokens = rest.split_whitespace();
                for token in tokens.by_ref() {
                    let is_flag = token.starts_with('-');
                    let is_numeric = token
                        .trim_end_matches(|c: char| c.is_alphabetic()) // "30s" → "30"
                        .parse::<f64>()
                        .is_ok();
                    if !is_flag && !is_numeric {
                        // Found the real command — return from this token onward
                        if let Some(pos) = rest.find(token) {
                            return &rest[pos..];
                        }
                    }
                }
                // All tokens were flags/numbers — nothing left to classify
                return "";
            }
        }
        trimmed
    }

    /// Return the first "real" command token in a shell command, skipping
    /// variable assignments (`VAR=val`) and flag-like tokens (`-n`, `--flag`).
    ///
    /// E.g., `VAR=val rm -rf /` → `rm`, `nice -n 5 cmd` → `cmd`.
    fn first_command_token(cmd: &str) -> &str {
        for token in cmd.split_whitespace() {
            // Skip variable assignments: FOO=bar, PATH=/usr/bin, etc.
            if let Some(eq_pos) = token.find('=') {
                if eq_pos > 0 {
                    continue;
                }
            }
            // Skip flag-like tokens
            if token.starts_with('-') {
                continue;
            }
            return token;
        }
        // Fall back to first whitespace token if everything was assignments/flags
        cmd.split_whitespace().next().unwrap_or("")
    }

    /// Classify a single (non-compound) command.
    fn classify_single(cmd: &str) -> CmdClassification {
        let inner = strip_safe_wrappers(cmd);
        let trimmed = inner.trim();
        if trimmed.is_empty() {
            return CmdClassification {
                read_only: true,
                destructive: false};
        }

        // 1. destructive 优先 — match against the first real command token.
        // Use tokenization so `VAR=val rm -rf /` still catches `rm`.
        {
            let first = first_command_token(trimmed);
            for &p in DESTRUCTIVE_PREFIXES {
                if first == p {
                    return CmdClassification {
                        read_only: false,
                        destructive: true};
                }
            }
        }

        // 2. read-only — match against the first real command token.
        let first = first_command_token(trimmed);
        if READ_ONLY_COMMANDS.contains(&first) {
            return CmdClassification {
                read_only: true,
                destructive: false};
        }
        for &p in READ_ONLY_PREFIXES {
            if trimmed == p
                || (trimmed.starts_with(p) && trimmed.as_bytes().get(p.len()) == Some(&b' '))
            {
                return CmdClassification {
                    read_only: true,
                    destructive: false};
            }
        }

        // 未识别 → 走默认 ask 路径（不标 destructive，由 gate/mode 分派决定）
        CmdClassification {
            read_only: false,
            destructive: false}
    }

    /// True if read only.
    pub fn is_read_only(cmd: &str) -> bool {
        classify(cmd).read_only
    }
    /// True if destructive.
    pub fn is_destructive(cmd: &str) -> bool {
        classify(cmd).destructive
    }

    // ── T1.2: Shell metacharacter tokenization & injection detection ──

    /// Split a shell command on metacharacters (`;`, `&&`, `||`, `|`, backticks, `$()`).
    /// Returns individual command segments for independent classification.
    ///
    /// Respects quoted regions — metacharacters inside single/double quotes
    /// or after backslash escapes are NOT treated as splitters.
    pub fn tokenize_shell(cmd: &str) -> Vec<&str> {
        let mut segments = Vec::new();
        let mut start = 0;
        let bytes = cmd.as_bytes();
        let mut in_single = false;
        let mut in_double = false;
        let mut i = 0;

        while i < bytes.len() {
            let b = bytes[i];

            if !in_double && !in_single && b == b'\\' && i + 1 < bytes.len() {
                i += 2; // skip escaped character
                continue;
            }
            if !in_double && b == b'\'' {
                in_single = !in_single;
                i += 1;
                continue;
            }
            if !in_single && b == b'"' {
                in_double = !in_double;
                i += 1;
                continue;
            }

            if !in_single && !in_double {
                // Check for separators
                let remaining = &bytes[i..];
                let sep_len = if remaining.starts_with(b"&&") || remaining.starts_with(b"||") {
                    2
                } else if b == b';' || b == b'|' {
                    1
                } else {
                    0
                };

                if sep_len > 0 {
                    if i > start {
                        let seg = cmd[start..i].trim();
                        if !seg.is_empty() { segments.push(seg); }
                    }
                    start = i + sep_len;
                    i = start;
                    continue;
                }
            }
            i += 1;
        }
        if start < bytes.len() {
            let seg = cmd[start..].trim();
            if !seg.is_empty() { segments.push(seg); }
        }
        if segments.is_empty() && !cmd.trim().is_empty() {
            segments.push(cmd.trim());
        }
        segments
    }

    /// Detect potential command injection patterns: backtick substitution,
    /// `$()` substitution, or redirects to sensitive paths.
    pub fn detect_injection_risks(cmd: &str) -> Vec<String> {
        let mut risks = Vec::new();

        // Backtick injection: `cmd `injected` arg`
        if cmd.contains('`') {
            risks.push("contains backtick substitution".to_string());
        }
        // $() substitution inside non-obvious contexts
        if let Some(dollar_paren) = cmd.find("$(") {
            // Allow simple cases like `echo "$(date)"` or `VAR=$(cmd)`
            let before = &cmd[..dollar_paren];
            if !before.trim().is_empty() && !before.ends_with('=') && !before.ends_with("echo ") {
                risks.push("contains $() substitution in non-assignment context".to_string());
            }
        }
        // Redirect writing to system paths
        for pattern in &["> /etc/", ">> /etc/", "> /usr/", ">> /usr/", "> /boot/"] {
            if cmd.contains(pattern) {
                risks.push(format!("redirect writing to system path: {pattern}"));
            }
        }
        // sudo/doas with redirects
        if (cmd.starts_with("sudo ") || cmd.starts_with("doas ")) && (cmd.contains('>') || cmd.contains("&&")) {
            risks.push("sudo/doas with redirect or compound command".to_string());
        }

        risks
    }
}

// 父模块 OBVIOUSLY_SAFE_PROGRAMS 中已有的程序应同步加到这里。
const READ_ONLY_COMMANDS: &[&str] = &[
    "ls",
    "find",
    "grep",
    "rg",
    "fgrep",
    "egrep",
    "fd",
    "ag",
    "ack",
    "cat",
    "head",
    "tail",
    "wc",
    "echo",
    "printf",
    "pwd",
    "which",
    "type",
    "true",
    "false",
    "uname",
    "hostname",
    "date",
    "whoami",
    "id",
    "ps",
    "df",
    "du",
    "stat",
    "file",
    "tree",
    "less",
    "more",
    "tr",
    "cut",
    "awk",
    "sort",
    "uniq",
    "test",
    "[",
    "env",
    "realpath",
    "basename",
    "dirname",
    "free",
    "uptime",
    "groups",
    // 文档 / 查阅
    "man",
    "info",
    "whatis",
    "apropos",
    // 网络诊断（只读查询）
    "dig",
    "ping",
    "nslookup",
    "traceroute",
    "netstat",
    "ss",
    // 文件比较
    "diff",
    "cmp",
    "comm",
    // 目录/文件列示（ls 替代品）
    "exa",
    "eza",
    "bat",
    // 二进制分析
    "nm",
    "objdump",
    "readelf",
    "strings",
    "xxd",
    "hexdump",
    "od",
    // 哈希校验
    "sha256sum",
    "shasum",
    "md5",
    "cksum",
    "sum",
    // 硬件 / 系统信息
    "lscpu",
    "lshw",
    "lsblk",
    "lspci",
    "lsusb",
    "system_profiler",
    "sw_vers",
    // 日期 / 计算
    "cal",
    "ncal",
    "time",
    // 数据生成（无副作用）
    "yes",
    "seq",
    "shuf",
    // 其它只读
    "namei",
    "jq",
    "yq",
    // 目录导航（只读，常与其它命令用 && 组合）
    "cd",
    "pushd",
    "popd",
    "dirs",
];

/// "<word> <word>" 级 read-only：前两个 token 命中视为只读。
const READ_ONLY_PREFIXES: &[&str] = &[
    "git status",
    "git log",
    "git diff",
    "git show",
    "git rev-parse",
    "git branch",
    "git remote",
    "git ls-files",
    "git config",
    "git blame",
    "git describe",
    "git grep",
    "git shortlog",
    "git stash list",
    "git tag",
    "git cherry",
    "cargo check",
    "cargo build",
    "cargo metadata",
    "cargo tree",
    "cargo --version",
    "cargo version",
    "rustc --version",
    "node --version",
    // 包管理查询
    "pip list",
    "pip show",
    "npm list",
    "npm view",
    "brew list",
    "brew info",
    "brew search",
    // Rustup 信息
    "rustup show",
    "rustup toolchain list",
    // Docker 只读
    "docker ps",
    "docker images",
    "docker inspect",
    "docker stats",
    // K8s 只读
    "kubectl get",
    "kubectl describe",
    "kubectl logs",
    "kubectl top",
    // macOS 系统信息
    "diskutil list",
    "diskutil info",
    "sysctl -a",
    "sysctl -n",
];

/// destructive 前缀：要么整命令以这些开头，要么前两 token 等于这些。
const DESTRUCTIVE_PREFIXES: &[&str] = &[
    "rm",
    "rmdir",
    "mv",
    "cp",
    "sudo",
    "su",
    "chmod",
    "chown",
    "chgrp",
    "dd",
    "mkfs",
    "mount",
    "umount",
    "shred",
    "shutdown",
    "reboot",
    "halt",
    "git reset",
    "git push",
    "git clean",
    "git rebase",
    "git checkout",
    "kubectl delete",
    "docker rm",
    "docker rmi",
    "docker system prune",
    "npm publish",
    "npm uninstall",
    "cargo publish",
];

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use base::tool::ToolResultContent;
    use serde_json::json;
    use tempfile::TempDir;

    fn ctx_in(cwd: &std::path::Path) -> ToolContext {
        ToolContext::for_test(cwd.to_path_buf())
    }

    // ---- safe-bash allow-list ----

    #[tokio::test]
    async fn echo_returns_text() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool;
        let r = tool
            .call(
                json!({"command": "echo hello"}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        match r.content {
            ToolResultContent::Text(t) => {
                assert!(t.contains("hello"));
            }
            _ => panic!()}
    }

    #[tokio::test]
    async fn nonzero_exit_marks_is_error_and_appends_code() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool;
        let r = tool
            .call(
                json!({"command": "exit 7"}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        match r.content {
            ToolResultContent::Text(t) => {
                assert!(
                    t.contains("[exit code: 7]"),
                    "expected exit code marker: {t}"
                );
            }
            _ => panic!()}
    }

    #[tokio::test]
    async fn stderr_is_captured() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool;
        let r = tool
            .call(
                json!({"command": "echo to-stdout && >&2 echo to-stderr"}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            ToolResultContent::Text(t) => {
                assert!(t.contains("to-stdout"));
                assert!(t.contains("to-stderr"));
            }
            _ => panic!()}
    }

    #[tokio::test]
    async fn cwd_is_set_to_session_cwd() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool;
        let r = tool
            .call(
                json!({"command": "pwd"}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            ToolResultContent::Text(t) => {
                let canonical = std::fs::canonicalize(dir.path()).unwrap();
                assert!(
                    t.contains(canonical.to_str().unwrap())
                        || t.contains(dir.path().to_str().unwrap()),
                    "expected cwd in output: {t}"
                );
            }
            _ => panic!()}
    }

    #[tokio::test]
    async fn cancel_kills_child_quickly() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool;
        let ctx = ToolContext::for_test(dir.path().to_path_buf());
        let cancel = ctx.cancel.clone();
        let task = tokio::spawn(async move {
            tool.call(
                json!({"command": "sleep 30"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
        });
        // 等子进程起来再 cancel
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
        let started = std::time::Instant::now();
        let r = task.await.unwrap();
        assert!(matches!(r, Err(ToolError::Cancelled)));
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(30),
            "cancel should be near-instant; took {elapsed:?} (CI runner may be slow)"
        );
    }

    #[tokio::test]
    async fn timeout_fires() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool;
        let r = tool
            .call(
                json!({"command": "sleep 5", "timeout_ms": 200}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap_err();
        assert!(matches!(r, ToolError::Timeout(_)));
    }

    #[tokio::test]
    async fn empty_command_validates_err() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool;
        let v = tool
            .validate_input(&json!({"command": "   "}), &ctx_in(dir.path()))
            .await;
        assert!(!matches!(v, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn timeout_over_cap_validates_err() {
        let dir = TempDir::new().unwrap();
        let tool = BashTool;
        let v = tool
            .validate_input(
                &json!({"command": "ls", "timeout_ms": 99999999}),
                &ctx_in(dir.path()),
            )
            .await;
        assert!(!matches!(v, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn flags_classify_correctly() {
        let tool = BashTool;
        assert!(tool.is_read_only(&json!({"command": "ls -la"})));
        assert!(tool.is_concurrency_safe(&json!({"command": "git status"})));
        assert!(tool.is_destructive(&json!({"command": "rm -rf /tmp/x"})));
        assert!(!tool.is_read_only(&json!({"command": "rm -rf /tmp/x"})));
        // unknown command → not safe to parallelize, not destructive
        assert!(!tool.is_concurrency_safe(&json!({"command": "./build.sh"})));
        // unknown 不再标 destructive（走默认 ask 路径由 gate/mode 分派）
        assert!(!tool.is_destructive(&json!({"command": "./build.sh"})));
    }

    #[tokio::test]
    async fn is_enabled_defaults_true() {
        assert!(BashTool.is_enabled());
    }

    #[tokio::test]
    async fn name_is_bash() {
        assert_eq!(BashTool.name(), "Bash");
    }

    #[test]
    fn input_schema_is_object_with_command_field() {
        let schema = BashTool.input_schema();
        let s = serde_json::to_value(&schema).unwrap();
        assert_eq!(s["type"], "object");
        // "command" should be in properties
        assert!(s["properties"].get("command").is_some());
    }
}
