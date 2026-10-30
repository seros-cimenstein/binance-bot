//! `bot-spot` command: one decision, or the nonstop trader with `--nonstop`.

use std::process::ExitCode;
use std::sync::Arc;

use bb_binance::{BinanceClient, BinanceConfig, Credentials};
use tokio_util::sync::CancellationToken;
use serde_json::json;

use crate::audit::{DEFAULT_REVIEW_REASON, mark_failed_route_reviewed};
use crate::config::TraderConfig;
use crate::cycle::{CycleMode, Runtime};
use crate::nonstop::Nonstop;
use crate::error::{Result, TraderError};

/// Trader commands.
#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Run the algorithm Spot trader (one decision, or nonstop with --nonstop).
    BotSpot(BotSpotArgs),
    /// Backtest the algorithm offline on cached 1m candles (no network, no orders).
    AlgoBacktest(crate::algo_backtest::report::AlgoBacktestArgs),
}

#[derive(clap::Args, Debug, Clone)]
pub struct BotSpotArgs {
    /// Place real Spot orders when every live arming flag is set.
    #[arg(long)]
    pub submit_live: bool,
    /// The algorithm decides; execute_route only records plans (overrides --submit-live).
    #[arg(long)]
    pub plan_only: bool,
    /// Print the decision context and the routes quotable now; no row, no order.
    #[arg(long)]
    pub evidence_only: bool,
    /// Append an operator-review audit marker that clears the failed-route halt.
    #[arg(long)]
    pub mark_failed_route_reviewed: bool,
    /// Reason stored with --mark-failed-route-reviewed.
    #[arg(long, default_value = DEFAULT_REVIEW_REASON)]
    pub review_reason: String,
    /// Keep trading: poll prices and decide every 15 s until SIGTERM; a decision row only
    /// when the decided action changes, plus one an hour.
    #[arg(long)]
    pub nonstop: bool,
}

pub async fn run(cmd: Command) -> anyhow::Result<ExitCode> {
    match cmd {
        Command::BotSpot(args) => Ok(match bot_spot(args).await {
            Ok(code) => code,
            Err(error) => {
                eprintln!("Bot Spot failed: {error}");
                ExitCode::FAILURE
            }
        }),
        Command::AlgoBacktest(args) => Ok(match crate::algo_backtest::report::run_cli(&args) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("Algorithm backtest failed: {error}");
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

/// One-shot runs refuse to start under the emergency stop file. The nonstop trader starts
/// anyway and idles until the file is removed, so a paused trader survives restarts and
/// deploys instead of exiting into a `restart: unless-stopped` loop.
fn stop_guard(config: &TraderConfig, nonstop: bool) -> Result<()> {
    if !nonstop && bb_core::fsutil::stop_requested(&config.stop_path) {
        return Err(TraderError::safety(format!(
            "Bot Spot emergency stop is active: {}",
            config.stop_path.display()
        )));
    }
    Ok(())
}

async fn bot_spot(args: BotSpotArgs) -> Result<ExitCode> {
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
    stop_guard(&config, args.nonstop)?;
    if mode == CycleMode::Live {
        // Hard safety: refuse before any lock or order.
        config.check_live_arming()?;
    }
    let runtime = Arc::new(Runtime::new(
        Arc::clone(&config),
        mode,
        binance_client(&config)?,
    ));
    let _ = runtime.run_config.set(config.run_config(args.nonstop));
    let mut trader = Nonstop::new(Arc::clone(&runtime));
    if args.evidence_only {
        print_json(&trader.evidence().await?.as_payload())?;
        return Ok(ExitCode::SUCCESS);
    }

    let _lock = bb_core::fsutil::RunLock::try_acquire(&config.lock_path)?;
    let shutdown = CancellationToken::new();
    spawn_signal_listener(shutdown.clone(), Arc::clone(&runtime));
    let exit = if args.nonstop {
        tracing::info!(mode = mode.as_str(), "nonstop trader started");
        trader.run(&shutdown).await;
        ExitCode::SUCCESS
    } else {
        let row = trader.once(&shutdown).await?;
        print_json(&serde_json::to_value(&row)?)?;
        if mode == CycleMode::Live && row.status == crate::audit::RowStatus::Failed {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        }
    };
    runtime.begin_shutdown();
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
        tracing::warn!("shutdown signal received; finishing the current decision");
        runtime.begin_shutdown();
        token.cancel();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_the_stop_file_one_shot_refuses_but_the_nonstop_trader_starts() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("bot-spot.stop");
        let stop_text = stop.to_string_lossy().into_owned();
        let config = TraderConfig::from_lookup(crate::config::tests::lookup(&[(
            "BOT_SPOT_STOP_PATH",
            &stop_text,
        )]))
        .unwrap();
        assert!(stop_guard(&config, false).is_ok());
        assert!(stop_guard(&config, true).is_ok());

        std::fs::write(&stop, "").unwrap();
        assert!(stop_guard(&config, false).is_err());
        assert!(stop_guard(&config, true).is_ok());
    }
}
