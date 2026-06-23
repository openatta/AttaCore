//! Hook execution methods: command, prompt, HTTP.
//!
//! These are `impl HookRunner` blocks extending the runner with execution logic.
//! Separated from `mod.rs` to keep the struct definition and dispatch clean.

use super::{HookOutcome, HookRunner};
use super::ssrf::ssrf_check_url;
use crate::payload::{HookDecision, HookInput, HookResponse};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

impl HookRunner {
    /// Prompt hook：调注入的 PromptHookExecutor，把模型返回的 JSON 解析为
    /// HookResponse。无 executor 时 Skipped + actionable 错误信息。
    pub(super) async fn exec_prompt(
        &self,
        prompt: &str,
        model: Option<&str>,
        input: &HookInput,
        timeout_ms: Option<u64>,
    ) -> HookOutcome {
        let Some(executor) = &self.prompt_executor else {
            return HookOutcome::Skipped(
                "prompt-type hook needs a PromptHookExecutor injected (CLI does this for Auto mode)",
            );
        };
        let timeout_dur = timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.default_timeout);
        let result =
            match tokio::time::timeout(timeout_dur, executor.execute(prompt, model, input)).await {
                Ok(Ok(text)) => text,
                Ok(Err(e)) => {
                    return HookOutcome::Error(format!("prompt hook executor: {e}"));
                }
                Err(_) => {
                    return HookOutcome::Error(format!(
                        "prompt hook timed out after {}ms",
                        timeout_dur.as_millis()
                    ));
                }
            };
        // 模型可能用 markdown 包 JSON；尽力解
        let response = parse_hook_response_lenient(&result);
        HookOutcome::Ran {
            response,
            stdout: result,
            stderr: String::new(),
            exit_code: Some(0),
        }
    }

    /// HTTP hook：POST JSON payload 到 url，期望返回 JSON HookResponse。
    /// 自定义 headers 加进 request；status >= 400 → Error；body 解析失败 → 默认空 response。
    ///
    /// **SSRF guard**: rejects requests to private/reserved IP ranges (127.0.0.0/8,
    /// 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, ::1) before connecting.
    pub(super) async fn exec_http(
        &self,
        url: &str,
        headers: &std::collections::HashMap<String, String>,
        input: &HookInput,
        timeout_ms: Option<u64>,
    ) -> HookOutcome {
        // SSRF guard: block private/reserved IPs before connecting
        if let Err(e) = ssrf_check_url(url).await {
            return HookOutcome::Error(e.to_string());
        }

        let timeout_dur = timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.default_timeout);
        let mut req = self.http().post(url).timeout(timeout_dur).json(input);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return HookOutcome::Error(format!("http hook send: {e}")),
        };
        let status = resp.status();
        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => return HookOutcome::Error(format!("http hook body: {e}")),
        };
        if !status.is_success() {
            return HookOutcome::Error(format!("http hook {status}: {}", truncate(&body, 500)));
        }
        let response = parse_hook_response_lenient(&body);
        HookOutcome::Ran {
            response,
            stdout: body,
            stderr: String::new(),
            exit_code: Some(0),
        }
    }

    pub(super) async fn exec_command(
        &self,
        shell: &str,
        command: &str,
        input: &HookInput,
        timeout: Duration,
    ) -> HookOutcome {
        let mut cmd = tokio::process::Command::new(shell);
        cmd.arg("-c").arg(command);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return HookOutcome::Error(format!("hook spawn failed: {e}")),
        };

        // 喂 stdin
        if let Some(mut stdin) = child.stdin.take() {
            match serde_json::to_vec(input) {
                Ok(mut payload) => {
                    payload.push(b'\n');
                    let _ = stdin.write_all(&payload).await;
                    let _ = stdin.flush().await;
                }
                Err(e) => warn!("hook input serialize failed: {e}"),
            }
            // 关 stdin → hook 端读到 EOF
            drop(stdin);
        }

        let wait = child.wait_with_output();
        tokio::pin!(wait);
        let output = tokio::select! {
            _ = tokio::time::sleep(timeout) => {
                return HookOutcome::Error(format!("hook timed out after {:?}", timeout));
            }
            r = &mut wait => match r {
                Ok(o) => o,
                Err(e) => return HookOutcome::Error(format!("hook wait failed: {e}")),
            },
        };

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let exit_code = output.status.code();

        // 解析 stdout
        let response = if stdout.trim().is_empty() {
            // exit != 0 + 空 stdout → 视为 block，message 用 stderr
            if !output.status.success() {
                debug!(
                    ?exit_code,
                    %stderr,
                    "hook exited non-zero with empty stdout; treating as block"
                );
                HookResponse {
                    decision: Some(HookDecision::Block),
                    message: Some(if stderr.is_empty() {
                        format!("hook exited {exit_code:?}")
                    } else {
                        stderr.trim().to_string()
                    }),
                    ..Default::default()
                }
            } else {
                HookResponse::default()
            }
        } else {
            match serde_json::from_str::<HookResponse>(stdout.trim()) {
                Ok(r) => r,
                Err(e) => {
                    warn!(stdout = %stdout, error = %e, "hook stdout not valid JSON; default response");
                    HookResponse::default()
                }
            }
        };

        HookOutcome::Ran {
            response,
            stdout,
            stderr,
            exit_code,
        }
    }
}

/// 解析 hook 响应文本为 HookResponse。模型可能 wrap 在 ``` ```json``` 里；
/// 解析失败时返回 default response（continue=None, decision=None）。
fn parse_hook_response_lenient(s: &str) -> HookResponse {
    let trimmed = s.trim();
    let candidate = if let Some(rest) = trimmed.strip_prefix("```json") {
        rest.trim_start_matches('\n')
            .trim_end_matches("```")
            .trim()
    } else if let Some(rest) = trimmed.strip_prefix("```") {
        rest.trim_start_matches('\n')
            .trim_end_matches("```")
            .trim()
    } else {
        trimmed
    };
    serde_json::from_str(candidate).unwrap_or_default()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

// ---- Tests for parse_hook_response_lenient ----

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hook_response_handles_plain_json() {
        let r = parse_hook_response_lenient(r#"{"decision":"block"}"#);
        assert_eq!(r.decision, Some(HookDecision::Block));
    }

    #[test]
    fn parse_hook_response_handles_markdown_block() {
        let r = parse_hook_response_lenient("```json\n{\"continue\":false}\n```");
        assert_eq!(r.r#continue, Some(false));
    }

    #[test]
    fn parse_hook_response_garbage_returns_default() {
        let r = parse_hook_response_lenient("not json at all{{{");
        assert_eq!(r.decision, None);
        assert_eq!(r.r#continue, None);
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let result = truncate("hello world this is a long string", 10);
        assert!(result.starts_with("hello worl"));
        assert!(result.ends_with('…'));
    }

    #[test]
    fn parse_hook_response_handles_markdown_without_lang() {
        let r = parse_hook_response_lenient("```\n{\"decision\":\"approve\"}\n```");
        assert_eq!(r.decision, Some(HookDecision::Approve));
    }

    #[test]
    fn parse_hook_response_empty_string_returns_default() {
        let r = parse_hook_response_lenient("");
        assert_eq!(r.decision, None);
    }

    #[test]
    fn parse_hook_response_whitespace_only() {
        let r = parse_hook_response_lenient("   \n  ");
        assert_eq!(r.decision, None);
    }
}
