//! AttaCore test runner CLI
//!
//! 用法:
//! ```sh
//! # Agent API 模式（录制）
//! ATTA_VCR_RECORD=c_project cargo run -p test-runner -- \
//!   --mode agent --case tests/cases/c_project.test
//!
//! # Agent API 模式（回放）
//! ATTA_VCR_REPLAY=c_project cargo run -p test-runner -- \
//!   --mode agent --case tests/cases/c_project.test
//!
//! # Daemon 模式
//! cargo run -p test-runner -- \
//!   --mode daemon --socket /tmp/attacored.sock --case tests/cases/c_project.test
//! ```

use test_runner::{api_runner, cli_runner, comparator, config, reporter, script};

use base::interface::settings::VcrMode;
use std::path::{Path, PathBuf};

#[derive(Debug, clap::Parser)]
#[clap(name = "attacore-test", about = "AttaCore test runner")]
struct Args {
    /// Test mode: agent (API) or daemon (JSON-RPC)
    #[clap(long, default_value = "agent")]
    mode: String,

    /// Path to .test case file
    #[clap(long)]
    case: PathBuf,

    /// Daemon socket path (cli mode only)
    #[clap(long, default_value = "/tmp/attacore-test.sock")]
    socket: PathBuf,

    /// Path to attacored binary (cli mode only)
    #[clap(long, default_value = "target/debug/attacored")]
    daemon_binary: PathBuf,

    /// Enable LLM-based output comparison (slow, requires API calls)
    #[clap(long)]
    compare: bool,

    /// Output directory for reports
    #[clap(long, default_value = "tests/output")]
    out_dir: PathBuf,

    /// VCR scenario name (defaults to case file stem)
    #[clap(long)]
    scenario: Option<String>,

    /// Model config file path (.env, export KEY=VALUE format)
    #[clap(long, default_value = ".env")]
    config: String,

    /// Template project fixture to run the case against (e.g.
    /// tests/fixtures/template_project). Copied to a fresh tmp dir before
    /// each run; the fixture itself is never mutated. Omit for cases that
    /// don't need project-level config (hooks/mcp/agents/rules).
    #[clap(long)]
    fixture: Option<PathBuf>,

    /// VCR round identifier — cassette data is stored under a per-round
    /// subdirectory so re-recording never overwrites/mixes with an earlier
    /// round (see docs/TESTING_GUIDE.md). Defaults to $ATTA_VCR_ROUND, or
    /// today's UTC date (YYYY-MM-DD) if that's unset either.
    #[clap(long)]
    round: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = <Args as clap::Parser>::parse();

    // Resolve config path
    let config_path = shellexpand::tilde(&args.config).to_string();

    // Parse test case
    let case = script::parse_test_file(&args.case)?;
    eprintln!("Loaded: {} ({} turns)", case.source_path, case.turns.len());
    eprintln!("Meta: {}", &case.meta.lines().next().unwrap_or("(none)"));

    // Determine VCR mode from env
    let vcr_mode = if std::env::var("ATTA_VCR_RECORD").is_ok() {
        VcrMode::Record
    } else if std::env::var("ATTA_VCR_REPLAY").is_ok() {
        VcrMode::Replay
    } else {
        VcrMode::Replay // default: try replay, fallback if no fixture
    };

    let scenario = args.scenario.clone().unwrap_or_else(|| {
        args.case.file_stem()
            .and_then(|s| s.to_str())
            .map(|s| {
                // Extract numeric prefix: "000.c_project" → "000"
                s.split('.').next().unwrap_or(s).to_string()
            })
            .unwrap_or_else(|| "unknown".to_string())
    });

    let round = config::resolve_vcr_round(args.round.clone());
    eprintln!("VCR round: {round}");

    // cassette_dir: 本地/CI 缓存的 VCR 录制数据（tests/fixtures/cassettes/，已 gitignore），
    // 按 round 分目录存放——重新录制开新一轮，旧轮次原样留在磁盘上，互不覆盖。
    // output_dir: 纯生成物（人读日志/报告/遥测），继续留在 tests/output/，被 .gitignore 排除。
    let cassette_dir = PathBuf::from("tests/fixtures/cassettes").join(&scenario).join(&args.mode).join(&round);
    let output_dir = args.out_dir.join(&scenario).join(&args.mode);
    let report_dir = args.out_dir.join(&scenario);
    let _ = std::fs::create_dir_all(&cassette_dir);
    let _ = std::fs::create_dir_all(&output_dir);
    let _ = std::fs::create_dir_all(&report_dir);

    match args.mode.as_str() {
        "api" | "agent" => {
            run_api_mode(&args, &case, vcr_mode, &scenario, &cassette_dir, &output_dir, &report_dir, &config_path).await?;
        }
        "cli" | "daemon" => {
            run_cli_mode(&args, &case, &scenario, &cassette_dir, &output_dir, &report_dir).await?;
        }
        _ => anyhow::bail!("Unknown mode: {}. Use 'api' or 'cli'.", args.mode),
    }

    Ok(())
}

async fn run_api_mode(
    args: &Args,
    case: &script::TestCase,
    vcr_mode: VcrMode,
    scenario: &str,
    cassette_dir: &PathBuf,
    output_dir: &PathBuf,
    report_dir: &PathBuf,
    config_path: &str,
) -> anyhow::Result<()> {
    let model_config = config::load_env_config(Path::new(config_path))?;
    eprintln!(
        "Config: model={} (record + VCR fallback-on-miss — must match to keep cassette hashes valid) / fast_model={} (LLM comparator only)",
        model_config.model, model_config.fast_model
    );
    let model = build_model(&model_config)?;

    let telemetry_path = output_dir.join(format!("{scenario}.telemetry.md"));

    let runner_config = api_runner::AgentRunnerConfig {
        model: model.clone(),
        vcr_mode: vcr_mode.clone(),
        vcr_scenario: scenario.to_string(),
        vcr_dir: cassette_dir.clone(),
        telemetry_path: Some(telemetry_path),
        fixture_dir: args.fixture.clone(),
    };

    let outputs = api_runner::run_test_case(runner_config, case).await?;

    if args.compare {
        let compare_model = build_model(&model_config)?;
        run_comparison(case, &outputs, compare_model.as_ref(), report_dir).await?;
    } else {
        eprintln!("Skipping comparison (use --compare to enable LLM-based verification)");
    }
    Ok(())
}

async fn run_cli_mode(
    args: &Args,
    case: &script::TestCase,
    scenario: &str,
    cassette_dir: &PathBuf,
    output_dir: &PathBuf,
    report_dir: &PathBuf,
) -> anyhow::Result<()> {
    let config_path = shellexpand::tilde(&args.config).to_string();
    let vcr_mode: Option<String> = std::env::var("ATTA_VCR_RECORD").ok().map(|_| "record".into())
        .or_else(|| std::env::var("ATTA_VCR_REPLAY").ok().map(|_| "replay".into()));
    let config = cli_runner::CliRunnerConfig {
        socket_path: args.socket.clone(),
        daemon_binary: args.daemon_binary.clone(),
        config_path: config_path.clone().into(),
        scenario: scenario.to_string(),
        vcr_mode,
        cassette_dir: cassette_dir.clone(),
        output_dir: output_dir.clone(),
        fixture_dir: args.fixture.clone(),
    };

    let outputs = cli_runner::run_test_case(config, case).await?;

    if args.compare {
        let model_config = config::load_env_config(Path::new(&config_path))?;
        let compare_model = build_model(&model_config)?;
        run_comparison(case, &outputs, compare_model.as_ref(), report_dir).await?;
    }
    Ok(())
}

async fn run_comparison(
    case: &script::TestCase,
    outputs: &[api_runner::TurnOutput],
    compare_model: &dyn base::interface::model::Model,
    report_dir: &PathBuf,
) -> anyhow::Result<()> {
    let mut comparisons = Vec::new();
    for (i, turn) in case.turns.iter().enumerate() {
        if let Some(out) = outputs.get(i) {
            let cmp = comparator::compare_output(compare_model, turn, out)
                .await
                .unwrap_or_else(|e| comparator::ComparisonResult {
                    turn_index: i,
                    verdict: comparator::Verdict::Fail,
                    reasoning: format!("比对失败: {e}"),
                });
            eprintln!("Turn {}: {:?} — {}", i, cmp.verdict, cmp.reasoning.chars().take(100).collect::<String>());
            comparisons.push(cmp);
        }
    }
    reporter::write_reports(case, &comparisons, report_dir)?;
    let passed = comparisons.iter().filter(|c| c.verdict == comparator::Verdict::Pass).count();
    let failed = comparisons.iter().filter(|c| c.verdict == comparator::Verdict::Fail).count();
    eprintln!("\n=== Comparison Complete ===");
    eprintln!("  {} passed, {} failed, {}/{} total", passed, failed, comparisons.len(), case.turns.len());
    if failed > 0 { std::process::exit(1); }
    Ok(())
}

fn build_model(config: &config::TestModelConfig) -> anyhow::Result<std::sync::Arc<dyn base::interface::model::Model>> {
    let mut url = config.base_url.clone();
    if !url.ends_with('/') { url.push('/'); }
    let c = model::client::HttpAnthropicClient::with_base(
        model::client::AuthMode::ApiKey(config.auth_token.clone()),
        url::Url::parse(&url)?,
    )?
    .with_backoff(vec![100, 200, 500]);
    Ok(std::sync::Arc::new(model::adapter::AnthropicModel::new(std::sync::Arc::new(c))))
}
