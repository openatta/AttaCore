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

mod api_runner;
mod cli_runner;
mod comparator;
mod reporter;
mod script;

use base::interface::settings::VcrMode;
use std::path::PathBuf;

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

    /// DeepSeek config file path (supports .sh format: export KEY=VALUE)
    #[clap(long, default_value = ".deepseek")]
    config: String,
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

    let vcr_dir = args.out_dir.join(&scenario).join(&args.mode);
    let report_dir = args.out_dir.join(&scenario);
    let _ = std::fs::create_dir_all(&vcr_dir);
    let _ = std::fs::create_dir_all(&report_dir);

    match args.mode.as_str() {
        "api" | "agent" => {
            run_api_mode(&args, &case, vcr_mode, &scenario, &vcr_dir, &report_dir, &config_path).await?;
        }
        "cli" | "daemon" => {
            run_cli_mode(&args, &case, &scenario, &report_dir).await?;
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
    vcr_dir: &PathBuf,
    report_dir: &PathBuf,
    config_path: &str,
) -> anyhow::Result<()> {
    // Build DeepSeek model
    let config = load_config(config_path)?;
    let model = build_model(&config)?;

    let telemetry_path = vcr_dir.join(format!("{scenario}.telemetry.md"));

    let runner_config = api_runner::AgentRunnerConfig {
        model: model.clone(),
        vcr_mode: vcr_mode.clone(),
        vcr_scenario: scenario.to_string(),
        vcr_dir: vcr_dir.clone(),
        telemetry_path: Some(telemetry_path),
    };

    let outputs = api_runner::run_test_case(runner_config, case).await?;

    if args.compare {
        let compare_model = build_compare_model(&config)?;
        run_comparison(case, &outputs, compare_model.as_ref(), report_dir).await?;
    } else {
        eprintln!("Skipping comparison (use --compare to enable LLM-based verification)");
    }
    Ok(())
}

async fn run_cli_mode(args: &Args, case: &script::TestCase, scenario: &str, report_dir: &PathBuf) -> anyhow::Result<()> {
    let config_path = shellexpand::tilde(&args.config).to_string();
    let vcr_mode: Option<String> = std::env::var("ATTA_VCR_RECORD").ok().map(|_| "record".into())
        .or_else(|| std::env::var("ATTA_VCR_REPLAY").ok().map(|_| "replay".into()));
    let output_dir = args.out_dir.join(scenario).join("cli");
    let _ = std::fs::create_dir_all(&output_dir);
    let config = cli_runner::CliRunnerConfig {
        socket_path: args.socket.clone(),
        daemon_binary: args.daemon_binary.clone(),
        config_path: config_path.clone().into(),
        scenario: scenario.to_string(),
        vcr_mode,
        output_dir,
    };

    let outputs = cli_runner::run_test_case(config, case).await?;

    if args.compare {
        let cfg = load_config(&config_path)?;
        let compare_model = build_compare_model(&cfg)?;
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

fn load_config(path: &str) -> anyhow::Result<(String, String, String)> {
    let content = std::fs::read_to_string(path)?;
    let (mut url, mut token, mut model) = (String::new(), String::new(), String::new());
    for line in content.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("export ANTHROPIC_BASE_URL=") { url = v.trim().into(); }
        else if let Some(v) = l.strip_prefix("export ANTHROPIC_AUTH_TOKEN=") { token = v.trim().into(); }
        else if let Some(v) = l.strip_prefix("export ANTHROPIC_MODEL=") { model = v.trim().into(); }
    }
    anyhow::ensure!(!url.is_empty(), "ANTHROPIC_BASE_URL not found in config");
    anyhow::ensure!(!token.is_empty(), "ANTHROPIC_AUTH_TOKEN not found in config");
    Ok((url, token, model))
}

fn build_model(config: &(String, String, String)) -> anyhow::Result<std::sync::Arc<dyn base::interface::model::Model>> {
    let (url, token, _) = config;
    let mut url = url.clone();
    if !url.ends_with('/') { url.push('/'); }
    let c = model::client::HttpAnthropicClient::with_base(
        model::client::AuthMode::ApiKey(token.clone()),
        url::Url::parse(&url)?,
    )?
    .with_backoff(vec![100, 200, 500]);
    Ok(std::sync::Arc::new(model::adapter::AnthropicModel::new(std::sync::Arc::new(c))))
}

fn build_compare_model(config: &(String, String, String)) -> anyhow::Result<std::sync::Arc<dyn base::interface::model::Model>> {
    build_model(config)
}
