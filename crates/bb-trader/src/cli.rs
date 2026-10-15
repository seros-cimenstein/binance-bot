//! `claude-spot` command (port of `python -m src.claude_spot`).

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use bb_binance::{BinanceClient, BinanceConfig, Credentials};
use bb_omp::CancellationToken;
use bb_social::SocialSettings;
use serde_json::json;

use crate::alerts::Alerter;
use crate::audit::{DEFAULT_REVIEW_REASON, mark_failed_route_reviewed};
use crate::config::TraderConfig;
use crate::cycle::{CycleMode, Runtime, Trader};
use crate::error::{Result, TraderError};

/// Trader commands.
#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Run the Claude agentic Spot trader (one cycle, or a daemon with --daemon).
    ClaudeSpot(ClaudeSpotArgs),
    /// Replay the exit-plan menu on Claude Spot's real round trips (public klines, no orders).
    ClaudeSpotExitReplay(crate::exit_replay::ExitReplayArgs),
    /// Paper exit-plan watcher: follows bids between cycles, records would-be sells (no orders).
    ClaudeSpotExitWatch(crate::exit_watch::ExitWatchArgs),
}

#[derive(clap::Args, Debug, Clone)]
pub struct ClaudeSpotArgs {
    /// Place real Spot orders when every live arming flag is set.
    #[arg(long)]
    pub submit_live: bool,
    /// Ask Claude; execute_route only records plans (overrides --submit-live).
    #[arg(long)]
    pub plan_only: bool,
    /// Print the decision context without asking Claude or submitting orders.
    #[arg(long)]
    pub evidence_only: bool,
    /// Append an operator-review audit marker that clears the failed-route halt.
    #[arg(long)]
    pub mark_failed_route_reviewed: bool,
    /// Reason stored with --mark-failed-route-reviewed.
    #[arg(long, default_value = DEFAULT_REVIEW_REASON)]
    pub review_reason: String,
    /// Keep one omp session alive and run a cycle every --interval-seconds.
    #[arg(long)]
    pub daemon: bool,
    #[arg(long, default_value_t = 240)]
    pub interval_seconds: u64,
}

pub async fn run(cmd: Command) -> anyhow::Result<ExitCode> {
    match cmd {
        Command::ClaudeSpot(args) => Ok(match claude_spot(args).await {
            Ok(code) => code,
            Err(error) => {
                eprintln!("Claude Spot failed: {error}");
                ExitCode::FAILURE
            }
        }),
        Command::ClaudeSpotExitReplay(args) => Ok(match crate::exit_replay::run_cli(args).await {
            Ok(code) => code,
            Err(error) => {
                eprintln!("Exit-plan replay failed: {error}");
                ExitCode::FAILURE
            }
        }),
        Command::ClaudeSpotExitWatch(args) => Ok(match crate::exit_watch::run_cli(args).await {
            Ok(code) => code,
            Err(error) => {
                eprintln!("Exit-plan watcher failed: {error}");
                ExitCode::FAILURE
            }
        }),
    }
}

fn binance_client(config: &TraderConfig) -> Result<BinanceClient> {
    let credentials = Credentials::new(config.api_key.clone(), &config.api_secret)?;
    // Default retry policy: transport/5xx failures on idempotent reads retry
    // with backoff; order POSTs are never retried.
    let binance = BinanceConfig::new(config.use_testnet).with_credentials(credentials);
    Ok(BinanceClient::new(binance)?)
}

fn print_json(value: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn stop_guard(config: &TraderConfig) -> Result<()> {
    if bb_core::fsutil::stop_requested(&config.stop_path) {
        return Err(TraderError::safety(format!(
            "Claude Spot emergency stop is active: {}",
            config.stop_path.display()
        )));
    }
    Ok(())
}

async fn claude_spot(args: ClaudeSpotArgs) -> Result<ExitCode> {
    let config = Arc::new(TraderConfig::from_env()?);
    if args.mark_failed_route_reviewed {
        let path = mark_failed_route_reviewed(&config, &args.review_reason)?;
        print_json(&json!({"status": "reviewed", "audit_path": path.display().to_string()}))?;
        return Ok(ExitCode::SUCCESS);
    }
    let mode = if args.submit_live && !args.plan_only {
        CycleMode::Live
    } else {
        CycleMode::PlanOnly
    };
    stop_guard(&config)?;
    if mode == CycleMode::Live {
        // Hard safety: refuse before any LLM call, lock or order.
        config.check_live_arming()?;
    }
    let social = SocialSettings::from_env()?;
    let alerter = Alerter::new(
        bb_core::telegram::Telegram::from_env()
            .filter(|_| !args.evidence_only)
            .map(|telegram| Arc::new(telegram) as Arc<dyn crate::alerts::AlertSink>),
        config.telegram_explain,
        config.telegram_reason_chars,
    );
    let runtime = Arc::new(Runtime::new(
        Arc::clone(&config),
        mode,
        binance_client(&config)?,
        alerter,
        Some(social),
    ));
    if args.evidence_only {
        let snapshot = runtime.build_snapshot().await?;
        print_json(&snapshot.context.as_payload())?;
        return Ok(ExitCode::SUCCESS);
    }

    let _lock = bb_core::fsutil::RunLock::try_acquire(&config.lock_path)?;
    let shutdown = CancellationToken::new();
    spawn_signal_listener(shutdown.clone(), Arc::clone(&runtime));
    let mut trader = Trader::new(Arc::clone(&runtime));
    let mut exit = ExitCode::SUCCESS;
    let mut exit_guard = (args.daemon && mode == CycleMode::Live && config.exit.live).then(|| {
        tracing::info!(
            default_plan = ?config.exit.default_plan.map(|d| d.plan.to_string()),
            poll_seconds = config.exit.poll_seconds,
            "live exit plans enabled"
        );
        crate::exit_guard::ExitGuard::new(Arc::clone(&runtime))
    });
    loop {
        match trader.run_cycle(&shutdown).await {
            Ok(outcome) => {
                print_json(&serde_json::to_value(&outcome.summary)?)?;
                if outcome.failed {
                    exit = ExitCode::FAILURE;
                }
            }
            Err(error) => {
                if !args.daemon {
                    trader.close().await;
                    return Err(error);
                }
                tracing::error!(%error, "Claude Spot cycle failed");
            }
        }
        if !args.daemon || shutdown.is_cancelled() {
            break;
        }
        let next_cycle = tokio::time::Instant::now() + Duration::from_secs(args.interval_seconds);
        match exit_guard.as_mut() {
            // Live exit plans: follow bids and sell when a plan fires until the next cycle.
            Some(guard) => guard.run_until(next_cycle, &shutdown).await,
            None => tokio::select! {
                () = tokio::time::sleep_until(next_cycle) => {}
                () = shutdown.cancelled() => {}
            },
        }
        if shutdown.is_cancelled() {
            break;
        }
    }
    runtime.begin_shutdown();
    trader.close().await;
    Ok(exit)
}

/// SIGTERM / SIGINT: refuse new routes at once and cancel `token` (aborts the agent turn);
/// a route already executing still completes every leg.
fn spawn_signal_listener(token: CancellationToken, runtime: Arc<Runtime>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut terminate =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(signal) => signal,
                    Err(error) => {
                        tracing::warn!(%error, "SIGTERM handler unavailable");
                        let _ = tokio::signal::ctrl_c().await;
                        runtime.begin_shutdown();
                        token.cancel();
                        return;
                    }
                };
            tokio::select! {
                _ = terminate.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        tracing::warn!("shutdown signal received; finishing the current cycle");
        runtime.begin_shutdown();
        token.cancel();
    });
}
