//! AttaCore test runner CLI
//!
//! 用法:
//! ```sh
//! # Agent API 模式（录制）
//! ATTA_RECORD=c_project cargo run -p test-runner -- \
//!   --mode agent --case tests/cases/c_project.test
//!
//! # Agent API 模式（回放）
//! ATTA_REPLAY=c_project cargo run -p test-runner -- \
//!   --mode agent --case tests/cases/c_project.test
//!
//! # Daemon 模式
//! cargo run -p test-runner -- \
//!   --mode daemon --socket /tmp/attacored.sock --case tests/cases/c_project.test
//! ```

use test_runner::{api_runner, cli_runner, comparator, config, mutations, reporter, script};

use base::interface::settings::RecorderMode;
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

    /// Recording name (defaults to case file stem)
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

    /// Round identifier — recorded data is stored under a per-round
    /// subdirectory so re-recording never overwrites/mixes with an earlier
    /// round. Defaults to $ATTA_RECORD_ROUND, or
    /// today's UTC date (YYYY-MM-DD) if that's unset either.
    #[clap(long)]
    round: Option<String>,

    /// Which registered `AgentScene` to run the case against — coding, chat,
    /// research, or demo. `api_runner.rs` used to hard-code `CodingScene`;
    /// chat/research were never exercised end-to-end anywhere in the repo
    /// until this flag existed.
    /// Scene to run under. Omitted, the case's own `scene:` declaration wins,
    /// and failing that `DEFAULT_SCENE`.
    #[clap(long)]
    scene: Option<String>,

    /// Build one `Agent` for the whole case and send every turn to it in
    /// sequence, instead of a fresh `Agent` per turn — see
    /// `api_runner::run_test_case_same_session`'s doc comment for why this
    /// is the only way to actually exercise the Skills/Agent-type live-reload
    /// watchers (a fresh per-turn `Agent` sees current disk state regardless
    /// of whether a watcher exists). Auto-enabled when the case has a
    /// `.mutations.json` sidecar (see `--case`) even without this flag, since
    /// a reload test always needs both, and when the case file itself declares
    /// `session: shared` in its meta block (the preferred way — a case knows
    /// whether its turns depend on each other; a command line doesn't).
    #[clap(long)]
    same_session: bool,

    /// Re-issue this recording's requests against the live model and judge
    /// whether the answers still mean the same thing — i.e. whether the
    /// recording still holds. Costs one real call per recorded call. Not a
    /// correctness check: the recording is the baseline.
    #[clap(long)]
    rerun: bool,
}

/// Mirrors `daemon/src/main.rs::resolve_scene` — kept as a small independent
/// copy rather than a shared dependency, since `test-runner` shouldn't need
/// to depend on the `daemon` binary crate just for this one match.
/// What a case runs under when neither the command line nor the case says.
const DEFAULT_SCENE: &str = "coding";

fn resolve_scene(
    name: &str,
) -> anyhow::Result<std::sync::Arc<dyn base::interface::scene::AgentScene>> {
    match name {
        "coding" => Ok(std::sync::Arc::new(scene::scene::coding::CodingScene)),
        "chat" => Ok(std::sync::Arc::new(scene::scene::chat::ChatScene)),
        "research" => Ok(std::sync::Arc::new(scene::scene::research::ResearchScene)),
        "demo" => Ok(std::sync::Arc::new(scene::scene::demo::DemoScene)),
        other => anyhow::bail!(
            "unsupported --scene `{other}` — supported scenes: coding, chat, research, demo"
        ),
    }
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
    eprintln!("Meta: {}", case.meta.lines().next().unwrap_or("(none)"));

    // Determine recorder mode from env. `ATTA_REPLAY` (if set) and the
    // no-env-var default both mean the same thing: replay what was recorded —
    // only `ATTA_RECORD` picks a different mode.
    let recorder_mode = if std::env::var("ATTA_RECORD").is_ok() {
        RecorderMode::Record
    } else {
        RecorderMode::Replay
    };

    let scenario = args.scenario.clone().unwrap_or_else(|| {
        let short = args
            .case
            .file_stem()
            .and_then(|s| s.to_str())
            // Extract numeric prefix: "000.c_project" → "000" (legacy flat
            // naming under tests/cases/ directly; new mechanism-focused cases
            // under tests/cases/{skills,agents,mcp,rules,hooks}/ don't use
            // dots in the stem, so this is a no-op for them).
            .map(|s| s.split('.').next().unwrap_or(s).to_string())
            .unwrap_or_else(|| "unknown".to_string());
        // Cases living in a subdirectory of tests/cases/ (e.g.
        // tests/cases/skills/001_startup.test) get that subdirectory folded
        // into the scenario name ("skills/001_startup") so the cassette tree
        // mirrors the case tree (tests/fixtures/cassettes/skills/001_startup/...)
        // and two mechanisms can both have a "001_startup" case without their
        // cassettes colliding. Cases directly under tests/cases/ (parent name
        // "cases") keep the old bare scenario name unchanged.
        match args
            .case
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
        {
            Some(parent) if parent != "cases" => format!("{parent}/{short}"),
            _ => short,
        }
    });

    let round = config::resolve_record_round(args.round.clone());
    eprintln!("Recording round: {round}");

    // cassette_dir: 本地/CI 缓存的录制数据（tests/fixtures/cassettes/，已 gitignore），
    // 按 round 分目录存放——重新录制开新一轮，旧轮次原样留在磁盘上，互不覆盖。
    // output_dir: 纯生成物（人读日志/报告/遥测），继续留在 tests/output/，被 .gitignore 排除。
    let cassette_dir = PathBuf::from("tests/fixtures/cassettes")
        .join(&scenario)
        .join(&args.mode)
        .join(&round);
    let output_dir = args.out_dir.join(&scenario).join(&args.mode);
    let report_dir = args.out_dir.join(&scenario);
    let _ = std::fs::create_dir_all(&cassette_dir);
    let _ = std::fs::create_dir_all(&output_dir);
    let _ = std::fs::create_dir_all(&report_dir);

    if args.rerun {
        return run_rerun_mode(&args, &scenario, &cassette_dir, &output_dir, &config_path).await;
    }

    match args.mode.as_str() {
        "api" | "agent" => {
            run_api_mode(
                &args,
                &case,
                recorder_mode,
                &scenario,
                &cassette_dir,
                &output_dir,
                &report_dir,
                &config_path,
            )
            .await?;
        }
        "cli" | "daemon" => {
            run_cli_mode(
                &args,
                &case,
                &scenario,
                &cassette_dir,
                &output_dir,
                &report_dir,
            )
            .await?;
        }
        _ => anyhow::bail!("Unknown mode: {}. Use 'api' or 'cli'.", args.mode),
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_api_mode(
    args: &Args,
    case: &script::TestCase,
    recorder_mode: RecorderMode,
    scenario: &str,
    cassette_dir: &Path,
    output_dir: &Path,
    report_dir: &Path,
    config_path: &str,
) -> anyhow::Result<()> {
    let model_config = config::load_env_config(Path::new(config_path))?;
    eprintln!(
        "Config: model={} (recording and replay must agree on it, or replay reports a params divergence) / fast_model={} (LLM comparator only)",
        model_config.model, model_config.fast_model
    );
    let model = build_model(&model_config)?;

    // `cassette_dir`/`output_dir` (built by `main()`) are already scoped by the
    // full scenario path (e.g. `.../skills/001_startup/agent/<round>/`), so the
    // recording *name* inside them only needs the leaf component — a name with
    // a `/` in it would nest a second copy of the scenario path under an
    // already-scoped directory.
    let scenario_leaf = scenario.rsplit('/').next().unwrap_or(scenario);
    let telemetry_path = output_dir.join(format!("{scenario_leaf}.telemetry.md"));

    // A case knows what it needs; the command line does not. Falling back to
    // the case's own declaration is what stops a run from silently recording
    // `003.fixture_full` with no fixture at all — which is what every recording
    // of it did before, MCP server and hooks and all simply absent.
    let fixture_dir = args
        .fixture
        .clone()
        .or_else(|| case.fixture.as_ref().map(PathBuf::from));
    let scene_name = args
        .scene
        .clone()
        .or_else(|| case.scene.clone())
        .unwrap_or_else(|| DEFAULT_SCENE.to_string());
    if let (None, Some(from_case)) = (&args.fixture, &fixture_dir) {
        eprintln!("Fixture from the case: {}", from_case.display());
    }
    if args.scene.is_none() && case.scene.is_some() {
        eprintln!("Scene from the case: {scene_name}");
    }

    let runner_config = api_runner::AgentRunnerConfig {
        model: model.clone(),
        recorder_mode,
        recorder_name: scenario_leaf.to_string(),
        recordings_dir: cassette_dir.to_path_buf(),
        telemetry_path: Some(telemetry_path),
        fixture_dir: fixture_dir.clone(),
        scene: resolve_scene(&scene_name)?,
        recorder: telemetry::recorder::Recorder::new(),
    };

    let case_mutations = mutations::load_for_case(&args.case)?;
    let outputs = if shared_session(args, case, case_mutations.is_some()) {
        eprintln!(
            "Shared-session mode: one Agent/session for the whole case — conversation state \
             carries across turns"
        );
        api_runner::run_test_case_same_session(runner_config, case, case_mutations.as_ref()).await?
    } else {
        api_runner::run_test_case(runner_config, case).await?
    };

    check_expectations(case, &outputs)?;

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
    cassette_dir: &Path,
    output_dir: &Path,
    report_dir: &Path,
) -> anyhow::Result<()> {
    let config_path = shellexpand::tilde(&args.config).to_string();
    let recorder_mode: Option<String> = std::env::var("ATTA_RECORD")
        .ok()
        .map(|_| "record".into())
        .or_else(|| std::env::var("ATTA_REPLAY").ok().map(|_| "replay".into()));
    // Same fallback as the api path: the case declares what it needs.
    let fixture_dir = args
        .fixture
        .clone()
        .or_else(|| case.fixture.as_ref().map(PathBuf::from));
    let scene_name = args
        .scene
        .clone()
        .or_else(|| case.scene.clone())
        .unwrap_or_else(|| DEFAULT_SCENE.to_string());
    let config = cli_runner::CliRunnerConfig {
        socket_path: args.socket.clone(),
        daemon_binary: args.daemon_binary.clone(),
        config_path: config_path.clone().into(),
        scenario: scenario.to_string(),
        recorder_mode,
        cassette_dir: cassette_dir.to_path_buf(),
        output_dir: output_dir.to_path_buf(),
        fixture_dir,
        scene: scene_name,
        same_session: shared_session(args, case, false),
    };

    let outputs = cli_runner::run_test_case(config, case).await?;

    check_expectations(case, &outputs)?;

    if args.compare {
        let model_config = config::load_env_config(Path::new(&config_path))?;
        let compare_model = build_model(&model_config)?;
        run_comparison(case, &outputs, compare_model.as_ref(), report_dir).await?;
    }
    Ok(())
}

/// Does this run share one session (conversation history) across the case's
/// turns? Three independent ways to say yes, in order of who knows best:
/// the case file itself (`session: shared` in its meta — the case is the only
/// thing that knows whether its turn 2 depends on turn 1), a `.mutations.json`
/// sidecar (a live-reload test is meaningless without a shared session), or
/// the `--same-session` CLI flag (kept for ad-hoc runs of a case that doesn't
/// declare it).
fn shared_session(args: &Args, case: &script::TestCase, has_mutations: bool) -> bool {
    args.same_session || has_mutations || case.session_mode == script::SessionMode::Shared
}

/// Enforce the `@tools:` / `@no-tools` / `@contains:` assertions a case
/// declares, without an LLM in the loop. "Which tools were called" and "does
/// the reply contain this string" are deterministic facts; before this they
/// could only be checked by writing the requirement into the prose expectation
/// and hoping the `--compare` judge (which is off by default) noticed.
fn check_expectations(
    case: &script::TestCase,
    outputs: &[api_runner::TurnOutput],
) -> anyhow::Result<()> {
    let mut failures = Vec::new();
    for (i, turn) in case.turns.iter().enumerate() {
        if let Some(out) = outputs.get(i) {
            failures.extend(turn.check_expectations(&out.text, &out.tool_uses));
        }
    }
    if failures.is_empty() {
        return Ok(());
    }
    for f in &failures {
        eprintln!("ASSERTION FAILED — {f}");
    }
    anyhow::bail!("{} declared assertion(s) failed", failures.len())
}

async fn run_comparison(
    case: &script::TestCase,
    outputs: &[api_runner::TurnOutput],
    compare_model: &dyn base::interface::model::Model,
    report_dir: &Path,
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
            eprintln!(
                "Turn {}: {:?} — {}",
                i,
                cmp.verdict,
                cmp.reasoning.chars().take(100).collect::<String>()
            );
            comparisons.push(cmp);
        }
    }
    reporter::write_reports(case, &comparisons, report_dir)?;
    let passed = comparisons
        .iter()
        .filter(|c| c.verdict == comparator::Verdict::Pass)
        .count();
    let failed = comparisons
        .iter()
        .filter(|c| c.verdict == comparator::Verdict::Fail)
        .count();
    eprintln!("\n=== Comparison Complete ===");
    eprintln!(
        "  {} passed, {} failed, {}/{} total",
        passed,
        failed,
        comparisons.len(),
        case.turns.len()
    );
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Rerun a recording and report whether it still holds.
///
/// The model here is the real provider, deliberately unwrapped by any recorder:
/// a replaying decorator would answer with the recorded response and make every
/// verdict vacuously "consistent".
async fn run_rerun_mode(
    args: &Args,
    scenario: &str,
    cassette_dir: &Path,
    output_dir: &Path,
    config_path: &str,
) -> anyhow::Result<()> {
    let model_config = config::load_env_config(Path::new(config_path))?;
    let model = build_model(&model_config)?;
    let scenario_leaf = scenario.rsplit('/').next().unwrap_or(scenario);
    // `--mode` selects how a case is *driven*; a recording is a recording
    // whichever mode produced it. Looking under the sibling mode rather than
    // failing keeps `--rerun` from needing a `--mode` that has no meaning here.
    let mut dir = cassette_dir.join(scenario_leaf);
    if !dir.join("calls.jsonl").exists() {
        let alternatives = ["api", "agent", "cli"];
        let found =
            alternatives.iter().find_map(|mode| {
                let candidate = cassette_dir.parent().and_then(|round| round.parent()).map(
                    |scenario_root| {
                        scenario_root
                            .join(mode)
                            .join(cassette_dir.file_name().unwrap_or_default())
                            .join(scenario_leaf)
                    },
                )?;
                candidate.join("calls.jsonl").exists().then_some(candidate)
            });
        match found {
            Some(c) => dir = c,
            None => anyhow::bail!(
                "no recording at {} — record it first (tests/run_api.sh {scenario})",
                dir.display()
            ),
        }
    }
    eprintln!(
        "Rerun: {} (judge={})",
        dir.display(),
        model_config.fast_model
    );

    let report =
        test_runner::rerun::rerun_recording(&dir, &model, model.as_ref(), &model_config.fast_model)
            .await?;

    eprintln!("\n{}", test_runner::rerun::terminal_summary(&report));

    let path = output_dir.join("rerun.md");
    std::fs::create_dir_all(output_dir)?;
    std::fs::write(&path, test_runner::rerun::markdown_report(&report))?;
    eprintln!("报告: {}", path.display());

    if !report.holds() {
        // A divergence is the finding, not a crash — but the exit code has to
        // say so, or a script wrapping this reports success on a broken
        // recording.
        anyhow::bail!("{} 条调用与录像分歧（详见报告）", report.diverged());
    }
    let _ = args;
    Ok(())
}

fn build_model(
    config: &config::TestModelConfig,
) -> anyhow::Result<std::sync::Arc<dyn base::interface::model::Model>> {
    let mut url = config.base_url.clone();
    if !url.ends_with('/') {
        url.push('/');
    }
    let c = model::client::HttpAnthropicClient::with_base(
        model::client::AuthMode::ApiKey(config.auth_token.clone()),
        url::Url::parse(&url)?,
    )?
    .with_backoff(vec![100, 200, 500]);
    Ok(std::sync::Arc::new(model::adapter::AnthropicModel::new(
        std::sync::Arc::new(c),
    )))
}
