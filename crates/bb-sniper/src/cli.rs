//! `bb sniper-*` subcommands (former `python -m src.sniper.*` entry points).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive as _;
use serde::Serialize;

use crate::detector::{DEFAULT_RPC_WS_URL, DetectorRun, run_detector};
use crate::error::SniperError;
use crate::jupiter::{DEFAULT_QUOTE_URL, JupiterClient, LAMPORTS_PER_SOL};
use crate::paper::{
    ExecutionFriction, PI_REALISM_V1, PaperRun, SimulationParams, ZERO_PROFILE, run_paper_executor,
};
use crate::pipeline::{live_candidate_stream, run_pipeline};
use crate::scorer::{DEFAULT_RPC_HTTP_URL, RpcScorer, SafetyThresholds};

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Solana pool-creation detector: stream logsNotifications from the
    /// launchpad programs and append detected events to a JSONL audit log.
    SniperDetector(DetectorArgs),
    /// Score the safety of a Solana token mint (exit 0 tradeable, 2 rejected, 1 error).
    SniperScorer(ScorerArgs),
    /// Run detector → decoder → scorer and write evaluated candidates to JSONL.
    SniperPipeline(PipelineArgs),
    /// Forward-paper sniper: detect, decode, score, quote on Jupiter, simulate
    /// hold + exit, record P&L to a JSONL ledger.
    SniperPaper(PaperArgs),
}

#[derive(clap::Args, Debug)]
pub struct DetectorArgs {
    #[arg(long, env = "SNIPER_RPC_WS_URL", default_value = DEFAULT_RPC_WS_URL)]
    pub ws_url: String,
    #[arg(
        long,
        env = "SNIPER_AUDIT_PATH",
        default_value = "data/sniper-events.jsonl"
    )]
    pub audit_path: PathBuf,
    #[arg(
        long,
        env = "SNIPER_EMERGENCY_STOP_PATH",
        default_value = "data/sniper.stop"
    )]
    pub emergency_stop_path: PathBuf,
    /// Stop the detector after this many seconds (useful for smoke tests).
    #[arg(long)]
    pub run_seconds: Option<f64>,
    /// Stop after detecting this many events.
    #[arg(long)]
    pub max_events: Option<usize>,
}

#[derive(clap::Args, Debug)]
pub struct ScorerArgs {
    /// Base58 mint address.
    pub mint: String,
    #[arg(long, default_value = DEFAULT_RPC_HTTP_URL)]
    pub rpc_url: String,
    #[arg(long, default_value_t = 30.0)]
    pub max_top_holder_pct: f64,
    #[arg(long = "max-top-10-pct", default_value_t = 75.0)]
    pub max_top_10_pct: f64,
}

#[derive(clap::Args, Debug)]
pub struct PipelineArgs {
    #[arg(long, env = "SNIPER_RPC_WS_URL", default_value = DEFAULT_RPC_WS_URL)]
    pub ws_url: String,
    #[arg(long, env = "SNIPER_RPC_HTTP_URL", default_value = DEFAULT_RPC_HTTP_URL)]
    pub rpc_url: String,
    #[arg(
        long,
        env = "SNIPER_CANDIDATE_AUDIT_PATH",
        default_value = "data/sniper-candidates.jsonl"
    )]
    pub audit_path: PathBuf,
    /// Stop after evaluating this many candidates (tradeable by default;
    /// --emit-rejected counts both).
    #[arg(long)]
    pub max_events: Option<usize>,
    /// Also write rejected candidates to the audit (useful for tuning the safety filters).
    #[arg(long)]
    pub emit_rejected: bool,
    /// Stop after this many seconds (smoke-test friendly).
    #[arg(long)]
    pub run_seconds: Option<f64>,
}

#[derive(clap::Args, Debug)]
pub struct PaperArgs {
    /// SOL per simulated buy.
    #[arg(long, default_value = "0.01")]
    pub entry_sol: Decimal,
    #[arg(long, default_value_t = 300)]
    pub hold_seconds: u64,
    #[arg(long, default_value_t = 500)]
    pub slippage_bps: u32,
    #[arg(long, default_value_t = 5)]
    pub max_concurrent: usize,
    #[arg(long, default_value = DEFAULT_QUOTE_URL)]
    pub quote_url: String,
    #[arg(long, env = "SNIPER_PAPER_LEDGER", default_value = crate::paper::DEFAULT_LEDGER_PATH)]
    pub ledger_path: PathBuf,
    #[arg(long, env = "SNIPER_PAPER_SUMMARY", default_value = crate::paper::DEFAULT_SUMMARY_PATH)]
    pub summary_path: PathBuf,
    /// Stop after queuing this many simulations.
    #[arg(long)]
    pub max_candidates: Option<usize>,
    /// Stop accepting new candidates after this many seconds; in-flight
    /// simulations are still allowed to finish.
    #[arg(long)]
    pub duration_seconds: Option<f64>,
    /// Execution friction preset.
    #[arg(long, env = "SNIPER_FRICTION_PROFILE", default_value = PI_REALISM_V1,
          value_parser = [PI_REALISM_V1, ZERO_PROFILE])]
    pub friction_profile: String,
    #[arg(long, env = "SNIPER_ENTRY_LATENCY_MS")]
    pub entry_latency_ms: Option<u64>,
    #[arg(long, env = "SNIPER_ENTRY_LATENCY_JITTER_MS")]
    pub entry_latency_jitter_ms: Option<u64>,
    #[arg(long, env = "SNIPER_ENTRY_FILL_PROB")]
    pub entry_fill_prob: Option<f64>,
    #[arg(long, env = "SNIPER_EXIT_FILL_PROB")]
    pub exit_fill_prob: Option<f64>,
    #[arg(long, env = "SNIPER_BASE_FEE_LAMPORTS")]
    pub base_fee_lamports: Option<u64>,
    #[arg(long, env = "SNIPER_PRIORITY_FEE_LAMPORTS")]
    pub priority_fee_lamports: Option<u64>,
}

impl PaperArgs {
    /// Precedence: CLI flag > env var > preset default.
    pub fn friction(&self) -> ExecutionFriction {
        let base = if self.friction_profile == ZERO_PROFILE {
            ExecutionFriction::zero()
        } else {
            ExecutionFriction::default()
        };
        ExecutionFriction {
            profile_id: self.friction_profile.clone(),
            entry_latency_ms: self.entry_latency_ms.unwrap_or(base.entry_latency_ms),
            entry_latency_jitter_ms: self
                .entry_latency_jitter_ms
                .unwrap_or(base.entry_latency_jitter_ms),
            entry_fill_prob: self.entry_fill_prob.unwrap_or(base.entry_fill_prob),
            exit_fill_prob: self.exit_fill_prob.unwrap_or(base.exit_fill_prob),
            base_fee_lamports: self.base_fee_lamports.unwrap_or(base.base_fee_lamports),
            priority_fee_lamports: self
                .priority_fee_lamports
                .unwrap_or(base.priority_fee_lamports),
        }
    }

    /// `--entry-sol` in lamports (truncated, exact decimal arithmetic).
    pub fn entry_sol_atoms(&self) -> anyhow::Result<u64> {
        (self.entry_sol * Decimal::from(LAMPORTS_PER_SOL))
            .trunc()
            .to_u64()
            .context("--entry-sol must be a non-negative SOL amount")
    }
}

pub async fn run(cmd: Command) -> anyhow::Result<ExitCode> {
    match cmd {
        Command::SniperDetector(args) => detector(args).await,
        Command::SniperScorer(args) => scorer(args).await,
        Command::SniperPipeline(args) => pipeline(args).await,
        Command::SniperPaper(args) => paper(args).await,
    }
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
        .context("building HTTP client")
}

fn seconds(flag: &str, value: Option<f64>) -> anyhow::Result<Option<Duration>> {
    value
        .map(|secs| {
            Duration::try_from_secs_f64(secs.max(0.0))
                .with_context(|| format!("{flag} must be a finite number"))
        })
        .transpose()
}

/// Pretty JSON with sorted keys (Python `json.dumps(indent=2, sort_keys=True)`).
fn print_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    let sorted = serde_json::to_value(value)?;
    println!("{}", serde_json::to_string_pretty(&sorted)?);
    Ok(())
}

async fn shutdown_signal() -> std::io::Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = term.recv() => Ok(()),
    }
}

async fn detector(args: DetectorArgs) -> anyhow::Result<ExitCode> {
    let run = DetectorRun {
        ws_url: &args.ws_url,
        audit_path: &args.audit_path,
        emergency_stop_path: &args.emergency_stop_path,
        run_duration: seconds("--run-seconds", args.run_seconds)?,
        max_events: args.max_events,
    };
    let result = tokio::select! {
        result = run_detector(run) => result,
        // SIGINT/SIGTERM from systemd: clean exit without a summary.
        _ = shutdown_signal() => return Ok(ExitCode::SUCCESS),
    };
    match result {
        Ok(summary) => {
            print_json(&summary)?;
            Ok(ExitCode::SUCCESS)
        }
        Err(SniperError::Detector(msg)) => {
            println!("sniper-detector failed: {msg}");
            Ok(ExitCode::from(1))
        }
        Err(err) => Err(err.into()),
    }
}

async fn scorer(args: ScorerArgs) -> anyhow::Result<ExitCode> {
    let thresholds = SafetyThresholds {
        max_top_holder_pct: args.max_top_holder_pct,
        max_top_10_pct: args.max_top_10_pct,
    };
    let scorer = RpcScorer::new(http_client()?, args.rpc_url, thresholds);
    match scorer.score_token(&args.mint).await {
        Ok(score) => {
            print_json(&score)?;
            Ok(if score.is_tradeable() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            })
        }
        Err(err) => {
            println!("scorer failed: {err}");
            Ok(ExitCode::from(1))
        }
    }
}

async fn pipeline(args: PipelineArgs) -> anyhow::Result<ExitCode> {
    let candidates = live_candidate_stream(
        &args.ws_url,
        &args.rpc_url,
        http_client()?,
        args.emit_rejected,
    )
    .await?;
    let outcome = run_pipeline(
        candidates,
        &args.audit_path,
        args.max_events,
        seconds("--run-seconds", args.run_seconds)?,
    )
    .await?;
    print_json(&outcome)?;
    Ok(ExitCode::SUCCESS)
}

async fn paper(args: PaperArgs) -> anyhow::Result<ExitCode> {
    let client = http_client()?;
    let run = PaperRun {
        params: SimulationParams {
            entry_sol_atoms: args.entry_sol_atoms()?,
            hold_seconds: args.hold_seconds,
            slippage_bps: args.slippage_bps,
            friction: args.friction(),
        },
        max_concurrent: args.max_concurrent,
        ledger_path: args.ledger_path,
        summary_path: args.summary_path,
        max_candidates: args.max_candidates,
        duration: seconds("--duration-seconds", args.duration_seconds)?,
    };
    let candidates = live_candidate_stream(
        DEFAULT_RPC_WS_URL,
        DEFAULT_RPC_HTTP_URL,
        client.clone(),
        false,
    )
    .await?;
    let quotes = Arc::new(JupiterClient::new(client, args.quote_url));
    let summary = run_paper_executor(candidates, quotes, &run).await?;
    print_json(&summary)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[derive(clap::Parser, Debug)]
    struct Cli {
        #[command(subcommand)]
        cmd: Command,
    }

    fn paper_args(argv: &[&str]) -> PaperArgs {
        let mut full = vec!["bb", "sniper-paper"];
        full.extend_from_slice(argv);
        match Cli::try_parse_from(full).unwrap().cmd {
            Command::SniperPaper(args) => args,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn paper_flags_match_python_contract() {
        let args = paper_args(&[
            "--entry-sol",
            "0.005",
            "--hold-seconds",
            "300",
            "--slippage-bps",
            "500",
            "--max-concurrent",
            "5",
            "--ledger-path",
            "/tmp/l.jsonl",
            "--summary-path",
            "/tmp/s.json",
            "--friction-profile",
            "zero",
            "--entry-fill-prob",
            "0.5",
        ]);
        assert_eq!(args.entry_sol_atoms().unwrap(), 5_000_000);
        assert_eq!(args.ledger_path, PathBuf::from("/tmp/l.jsonl"));
        let f = args.friction();
        assert_eq!(f.profile_id, "zero");
        assert_eq!(f.entry_latency_ms, 0);
        assert!((f.entry_fill_prob - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn paper_entry_sol_is_exact() {
        // Python's float math gives int(0.0029 * 1e9) == 2899999.
        assert_eq!(
            paper_args(&["--entry-sol", "0.0029"])
                .entry_sol_atoms()
                .unwrap(),
            2_900_000
        );
        assert!(paper_args(&["--entry-sol=-1"]).entry_sol_atoms().is_err());
    }

    #[test]
    fn paper_rejects_unknown_friction_profile() {
        assert!(
            Cli::try_parse_from(["bb", "sniper-paper", "--friction-profile", "bogus"]).is_err()
        );
    }

    #[test]
    fn subcommand_names() {
        for argv in [
            vec!["bb", "sniper-detector", "--run-seconds", "1"],
            vec!["bb", "sniper-scorer", "MINT", "--max-top-10-pct", "50"],
            vec![
                "bb",
                "sniper-pipeline",
                "--emit-rejected",
                "--max-events",
                "3",
            ],
        ] {
            Cli::try_parse_from(argv).unwrap();
        }
    }
}
