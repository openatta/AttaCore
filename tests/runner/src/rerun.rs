//! Rerun a recording against the live model and judge whether it still holds.
//!
//! The question this answers is *"is the recording still a faithful sample?"* —
//! feed each recorded request back to the model and see whether the answer
//! still means the same thing. A recording that survives this is one you can
//! keep replaying; one that does not has either gone stale or was never
//! reproducible to begin with.
//!
//! It is deliberately **not** a correctness check. The recording is the
//! baseline, so if the model was wrong when it was recorded, it is wrong here
//! too and this says nothing. Correctness is what the prose expectations in a
//! `.test` file and [`crate::comparator`] are for.
//!
//! ## Two halves, judged differently
//!
//! - **Tool calls** compare exactly (`telemetry::recorder::rerun::ToolDiff`).
//!   A tool call is an act; `Write(path="src/main.c")` and
//!   `Write(path="src/Main.c")` are different acts, and a judge asked to
//!   compare them will call them equivalent.
//! - **Text** goes to a judge, because wording drifts run to run and no
//!   temperature setting removes that.
//!
//! The judge is asked only when the text is not already byte-identical. That
//! filter is what keeps its cost — and its own non-determinism — off the
//! common path.

use base::interface::model::{
    MessageRole, Model, ModelContentBlock, ModelEvent, ModelMessage, StreamParams,
};
use base::interface::settings::ThinkingMode;
use futures::StreamExt;
use std::path::Path;
use std::sync::Arc;
use telemetry::recorder::rerun::{self, ResponseDiff};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallVerdict {
    /// Tool calls identical, text identical or judged equivalent.
    Consistent,
    /// Tool calls identical, text differs but means the same.
    TextDrift,
    /// Tool calls differ, or text no longer means the same.
    Diverged,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CallReport {
    pub index: usize,
    pub session_id: Option<String>,
    pub turn: u32,
    pub step: u32,
    pub purpose: Option<String>,
    pub verdict: CallVerdict,
    /// Whether an earlier call in this recording already diverged.
    ///
    /// Context, not a verdict. Every call is compared against the input the
    /// *recording* holds, and that input is fixed — call `k`'s messages carry
    /// the recorded results of call `k-1`, never this run's. So each call is
    /// an independent question ("given exactly this, is the answer still the
    /// same?") and stays worth asking after a divergence. What a divergence
    /// does change is the story: the conversation that led here would not have
    /// unfolded this way live, which is worth saying without withholding the
    /// answer.
    pub after_divergence: bool,
    /// `Tool.key` entries that differed — what a reader triages from.
    pub offending_keys: Vec<String>,
    /// Why the judge said what it said. Empty when no judge was asked.
    pub judge_reason: String,
    #[serde(skip)]
    pub diff: Option<ResponseDiff>,
    /// The prompt-block sources of the request, so a report says what the model
    /// was given, not just what it answered.
    pub system_sources: Vec<String>,
    pub tool_count: usize,
    pub message_count: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RerunReport {
    pub recording: String,
    pub calls: Vec<CallReport>,
}

impl RerunReport {
    pub fn consistent(&self) -> usize {
        self.count(CallVerdict::Consistent)
    }
    pub fn text_drift(&self) -> usize {
        self.count(CallVerdict::TextDrift)
    }
    pub fn diverged(&self) -> usize {
        self.count(CallVerdict::Diverged)
    }
    fn count(&self, v: CallVerdict) -> usize {
        self.calls.iter().filter(|c| c.verdict == v).count()
    }

    /// True when nothing diverged. Text drift is expected, not a failure.
    pub fn holds(&self) -> bool {
        self.diverged() == 0
    }

    /// The call that opens a session, reported apart from the rest.
    ///
    /// It is the one place the model picks its first move with nothing but the
    /// prompt to go on, and across every recorded case it is where divergence
    /// lands: `Bash` instead of `Write`, `TodoWrite` instead of answering
    /// directly, `ToolSearch` instead of spawning. Later calls carry concrete
    /// tool results and leave far less room. Averaging the two together buries
    /// a real regression in the opening call's noise.
    pub fn opening_calls(&self) -> Vec<&CallReport> {
        let mut seen: Vec<&str> = Vec::new();
        self.calls
            .iter()
            .filter(|c| {
                let session = c.session_id.as_deref().unwrap_or("");
                if seen.contains(&session) {
                    return false;
                }
                seen.push(session);
                true
            })
            .collect()
    }

    /// Divergences outside the opening call — the ones worth acting on first.
    pub fn diverged_after_opening(&self) -> usize {
        let openings: Vec<usize> = self.opening_calls().iter().map(|c| c.index).collect();
        self.calls
            .iter()
            .filter(|c| c.verdict == CallVerdict::Diverged && !openings.contains(&c.index))
            .count()
    }
}

/// Rerun every call of the recording at `dir` and judge the results.
///
/// `model` must be a real provider; a replaying recorder would hand back the
/// recorded answer and make every verdict vacuously `Consistent`.
pub async fn rerun_recording(
    dir: &Path,
    model: &Arc<dyn Model>,
    judge: &dyn Model,
    judge_model: &str,
) -> anyhow::Result<RerunReport> {
    let requests = rerun::load_all(dir)?;
    let mut calls = Vec::new();
    let mut diverged_yet = false;

    for (index, request) in requests.into_iter().enumerate() {
        let meta = request.record.clone();
        let system_sources = request
            .prompt_blocks
            .iter()
            .map(|b| b.name.clone().unwrap_or_else(|| "(unlabelled)".into()))
            .collect();
        let tool_count = request.tools.len();
        let message_count = request.messages.len();

        // Every call is rerun, including ones after a divergence. Skipping
        // them looked like thrift and was a mistake: the input comes from the
        // recording, not from this run, so a later call's question is not made
        // invalid by an earlier answer moving. Skipping meant a recording of
        // twelve calls got one of them checked — and the one checked was the
        // opening call, where the model is choosing its first move freely and
        // is least stable. The eleven skipped are the ones whose input carries
        // concrete tool results, which is where the model has the least room
        // and the signal is worth the most.
        let (diff, _live) = rerun::rerun_one(model, request, CancellationToken::new()).await?;
        let offending_keys = diff.tools.offending_keys();
        let mut judge_reason = String::new();

        let verdict = if !diff.tools.matches() {
            CallVerdict::Diverged
        } else if diff.text_identical() {
            CallVerdict::Consistent
        } else {
            // Only here does a judge get involved: the acts agree, only the
            // wording moved, which is the one question exact comparison cannot
            // answer.
            let (same, reason) =
                judge_text(judge, judge_model, &diff.recorded_text, &diff.live_text).await?;
            judge_reason = reason;
            if same {
                CallVerdict::TextDrift
            } else {
                CallVerdict::Diverged
            }
        };

        calls.push(CallReport {
            index,
            session_id: meta.session_id.clone(),
            turn: meta.turn,
            step: meta.step,
            purpose: meta.purpose.clone(),
            verdict,
            after_divergence: diverged_yet,
            offending_keys,
            judge_reason,
            diff: Some(diff),
            system_sources,
            tool_count,
            message_count,
        });
        diverged_yet |= verdict == CallVerdict::Diverged;
    }

    Ok(RerunReport {
        recording: dir.display().to_string(),
        calls,
    })
}

/// Ask the judge whether two answers mean the same thing.
///
/// Returns `(equivalent, reason)`. A judge that answers in an unparseable
/// shape is treated as "not equivalent" — an unreadable verdict must not pass
/// silently, since passing is the outcome nobody investigates.
async fn judge_text(
    judge: &dyn Model,
    judge_model: &str,
    recorded: &str,
    live: &str,
) -> anyhow::Result<(bool, String)> {
    let prompt = format!(
        "你在判断同一个请求的两次模型回复**含义**是否一致。\n\
         \n\
         用词、语序、格式、长度的差异都不算不一致 —— 模型每次措辞本来就不同。\n\
         只有当两者在**做的事或给出的结论**上不同才算不一致，例如：\n\
         - 一个说能做、另一个说做不了\n\
         - 给出的数值/结论不同\n\
         - 一个提出了某个行动、另一个没有\n\
         \n\
         ## 回复 A（录像里的）\n{recorded}\n\
         \n\
         ## 回复 B（本次重跑的）\n{live}\n\
         \n\
         只输出一行 JSON，不要任何其他文字：\n\
         {{\"equivalent\": true|false, \"reason\": \"一句话理由\"}}"
    );

    let messages = vec![ModelMessage {
        role: MessageRole::User,
        content: vec![ModelContentBlock::Text { text: prompt }],
    }];
    let params = StreamParams {
        model: judge_model.to_string(),
        max_tokens: 256,
        thinking_mode: ThinkingMode::Off,
        fallback_model: None,
        cache_edits: vec![],
        origin: None,
        input_map: None,
    };

    let mut stream = judge
        .stream(vec![], vec![], messages, params, CancellationToken::new())
        .await?;
    let mut text = String::new();
    while let Some(e) = stream.next().await {
        if let Ok(ModelEvent::TextDelta { text: t }) = e {
            text.push_str(&t);
        }
    }
    Ok(parse_verdict(&text))
}

/// Pull the verdict out of the judge's reply.
///
/// Structured rather than prefix-matched: asking for JSON and reading JSON
/// keeps a chatty preamble from flipping the answer, which is what a
/// `starts_with("pass")` style parser does the first time a model says
/// "Looking at these two replies, ... pass".
fn parse_verdict(reply: &str) -> (bool, String) {
    let slice = match (reply.find('{'), reply.rfind('}')) {
        (Some(a), Some(b)) if b > a => &reply[a..=b],
        _ => return (false, format!("判官回复无法解析为 JSON: {}", reply.trim())),
    };
    match serde_json::from_str::<serde_json::Value>(slice) {
        Ok(v) => {
            let equivalent = v.get("equivalent").and_then(|e| e.as_bool());
            let reason = v
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
            match equivalent {
                Some(e) => (e, reason),
                None => (false, format!("判官回复缺少 equivalent 字段: {slice}")),
            }
        }
        Err(e) => (false, format!("判官回复不是合法 JSON ({e}): {slice}")),
    }
}

// ── Reporting ────────────────────────────────────────────────────────────

impl CallVerdict {
    fn mark(self) -> &'static str {
        match self {
            CallVerdict::Consistent => "✓",
            CallVerdict::TextDrift => "~",
            CallVerdict::Diverged => "✗",
        }
    }
}

/// One line per call, plus a tally. Meant to be read while it scrolls past.
pub fn terminal_summary(report: &RerunReport) -> String {
    let mut out = String::new();
    for c in &report.calls {
        let where_ = match &c.purpose {
            Some(p) => p.clone(),
            None => format!("turn {} step {}", c.turn, c.step),
        };
        let detail = match c.verdict {
            CallVerdict::Diverged if !c.offending_keys.is_empty() => {
                format!("  {}", c.offending_keys.join(", "))
            }
            CallVerdict::Diverged => format!("  {}", first_line(&c.judge_reason)),
            CallVerdict::TextDrift => format!("  {}", first_line(&c.judge_reason)),
            _ => String::new(),
        };
        let opening = if report.opening_calls().iter().any(|o| o.index == c.index) {
            "开"
        } else if c.after_divergence {
            "↓ "
        } else {
            "  "
        };
        out.push_str(&format!(
            "  {}{} #{:<3} {:<22} {:<16}{}\n",
            c.verdict.mark(),
            opening,
            c.index,
            c.session_id.as_deref().unwrap_or("(no session)"),
            where_,
            detail
        ));
    }
    out.push_str(&format!(
        "\n  {} 一致  {} 措辞漂移  {} 分歧\n  其中开场调用之外的分歧: {}（开场是模型自由选第一步的位置，抖动本来就大）\n",
        report.consistent(),
        report.text_drift(),
        report.diverged(),
        report.diverged_after_opening(),
    ));
    out
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(90).collect()
}

/// The long form: every call, what it was given, and both answers side by side.
pub fn markdown_report(report: &RerunReport) -> String {
    let mut out = format!(
        "# Rerun 报告\n\n\
         录像：`{}`\n\n\
         一致 {} · 措辞漂移 {} · 分歧 {}（开场调用之外: {}）\n\n\
         > 判定的是**录像是否仍然成立**（同一输入下模型是否给出同义的输出），\n\
         > 不是模型答得对不对——录像本身就是基准。\n\n\
         > 每条调用都独立重跑：发出去的是**录像里存的输入**，不含本次的任何结果，\n\
         > 所以「给定这个确切输入，答案还一样吗」对每一条都成立，前面有没有分歧都一样。\n\
         > 标 `↓` 的表示它之前已经有过分歧——本次这段对话不会这样展开，但这一条的判定仍然有效。\n\n\
         > **开场调用（标 `开`）单列**：那是模型只凭提示词自由选第一步的位置，\n\
         > 抖动本来就大。真正值得先看的是开场之外的分歧。\n\n\
         > 工具调用逐参数精确比对，不经判官：一次工具调用是一个动作，\n\
         > `Write(path=\"a.c\")` 和 `Write(path=\"A.c\")` 不是「差不多」。\n\
         > 代价是生成的文件内容、自由文本的 description 每次都会不同而判失败——\n\
         > 下面的逐 key 明细就是让你一眼分辨良性漂移和真回归的。\n\n",
        report.recording,
        report.consistent(),
        report.text_drift(),
        report.diverged(),
        report.diverged_after_opening(),
    );

    for c in &report.calls {
        out.push_str(&format!(
            "---\n\n## {} call #{} — {}\n\n",
            c.verdict.mark(),
            c.index,
            match c.verdict {
                CallVerdict::Consistent => "一致",
                CallVerdict::TextDrift => "措辞漂移，含义一致",
                CallVerdict::Diverged => "分歧",
            },
        ));
        if report.opening_calls().iter().any(|o| o.index == c.index) {
            out.push_str("> 开场调用：模型在此只凭提示词选第一步。\n\n");
        } else if c.after_divergence {
            out.push_str(
                "> 之前已有分歧——本次对话不会走到这里，但这一条比的是录像里的输入，判定有效。\n\n",
            );
        }
        out.push_str(&format!(
            "| | |\n|---|---|\n\
             | session | `{}` |\n| 坐标 | turn {} step {}{} |\n\
             | system 块 | {} 块：{} |\n| 工具表 | {} 个 |\n| messages | {} 条 |\n\n",
            c.session_id.as_deref().unwrap_or("(none)"),
            c.turn,
            c.step,
            c.purpose
                .as_ref()
                .map(|p| format!("（purpose: {p}）"))
                .unwrap_or_default(),
            c.system_sources.len(),
            c.system_sources.join(", "),
            c.tool_count,
            c.message_count
        ));

        if !c.judge_reason.is_empty() {
            out.push_str(&format!("**判官**：{}\n\n", c.judge_reason));
        }
        if let Some(d) = &c.diff {
            if d.identical() {
                out.push_str("输出逐字节相同。\n\n");
            } else {
                out.push_str("```\n");
                out.push_str(&d.report());
                out.push_str("```\n\n");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_json_verdict_is_read_as_given() {
        let (eq, reason) = parse_verdict(r#"{"equivalent": true, "reason": "都在解释所有权"}"#);
        assert!(eq);
        assert_eq!(reason, "都在解释所有权");
    }

    /// A judge that编造 a preamble must not flip the verdict — the failure mode
    /// a prefix-matching parser has.
    #[test]
    fn a_preamble_before_the_json_does_not_confuse_it() {
        let (eq, _) =
            parse_verdict("看了一下两段回复：\n{\"equivalent\": false, \"reason\": \"结论不同\"}");
        assert!(!eq);
    }

    /// An unreadable verdict is not a pass. Passing is the outcome nobody
    /// investigates, so it must never be the fallback.
    #[test]
    fn an_unparseable_reply_is_not_equivalent() {
        assert!(!parse_verdict("大概差不多吧").0);
        assert!(!parse_verdict(r#"{"verdict": "pass"}"#).0);
        assert!(!parse_verdict("").0);
    }
}
