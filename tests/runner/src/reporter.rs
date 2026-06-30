//! TestReporter — 输出测试报告 (JSON + Markdown)。

use crate::comparator::{ComparisonResult, Verdict};
use crate::script::TestCase;
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
struct JsonReport {
    case: String,
    meta: String,
    turns: Vec<JsonTurn>,
    summary: JsonSummary,
}

#[derive(Serialize)]
struct JsonTurn {
    index: usize,
    input: String,
    expected: String,
    verdict: String,
    reasoning: String,
}

#[derive(Serialize)]
struct JsonSummary {
    total: usize,
    passed: usize,
    failed: usize,
    partial: usize,
    skipped: usize,
}

pub fn write_reports(
    case: &TestCase,
    comparisons: &[ComparisonResult],
    out_dir: &Path,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(out_dir)?;
    let stem = Path::new(&case.source_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("report");

    let summary = JsonSummary {
        total: comparisons.len(),
        passed: comparisons
            .iter()
            .filter(|c| c.verdict == Verdict::Pass)
            .count(),
        failed: comparisons
            .iter()
            .filter(|c| c.verdict == Verdict::Fail)
            .count(),
        partial: comparisons
            .iter()
            .filter(|c| c.verdict == Verdict::Partial)
            .count(),
        skipped: comparisons
            .iter()
            .filter(|c| c.verdict == Verdict::Skipped)
            .count(),
    };

    // JSON report
    let json = JsonReport {
        case: case.source_path.clone(),
        meta: case.meta.clone(),
        turns: comparisons
            .iter()
            .map(|c| JsonTurn {
                index: c.turn_index,
                input: case
                    .turns
                    .get(c.turn_index)
                    .map(|t| t.input.clone())
                    .unwrap_or_default(),
                expected: case
                    .turns
                    .get(c.turn_index)
                    .map(|t| t.expected.clone())
                    .unwrap_or_default(),
                verdict: format!("{:?}", c.verdict).to_lowercase(),
                reasoning: c.reasoning.clone(),
            })
            .collect(),
        summary,
    };
    let json_path = out_dir.join(format!("{stem}.report.json"));
    std::fs::write(&json_path, serde_json::to_string_pretty(&json)?)?;

    // Markdown report
    let mut md = String::new();
    md.push_str(&format!("# Test Report: {}\n\n", stem));
    md.push_str(&format!("{}\n\n", case.meta));
    md.push_str("## Summary\n\n");
    md.push_str(&format!(
        "| total | passed | failed | partial | skipped |\n"
    ));
    md.push_str(&format!(
        "|-------|--------|--------|---------|---------|\n"
    ));
    md.push_str(&format!(
        "| {} | {} | {} | {} | {} |\n\n",
        json.summary.total,
        json.summary.passed,
        json.summary.failed,
        json.summary.partial,
        json.summary.skipped
    ));

    for c in comparisons {
        let turn = case.turns.get(c.turn_index);
        let emoji = match c.verdict {
            Verdict::Pass => "✅",
            Verdict::Fail => "❌",
            Verdict::Partial => "⚠️",
            Verdict::Skipped => "⏭️",
        };
        md.push_str(&format!("---\n\n### Turn {} {}\n\n", c.turn_index, emoji));
        md.push_str("**Input:**\n\n```text\n");
        if let Some(t) = turn {
            md.push_str(&t.input);
        }
        md.push_str("\n```\n\n");
        md.push_str("**Expected:**\n\n```text\n");
        if let Some(t) = turn {
            md.push_str(&t.expected);
        }
        md.push_str("\n```\n\n");
        md.push_str("**Verdict:**\n\n");
        md.push_str(&c.reasoning);
        md.push_str("\n\n");
    }

    let md_path = out_dir.join(format!("{stem}.report.md"));
    std::fs::write(&md_path, md)?;

    eprintln!("Reports written to {:?}", out_dir);
    Ok(())
}
