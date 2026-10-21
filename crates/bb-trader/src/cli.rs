//! `bot-spot` command (port of `python -m src.claude_spot`).

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
    /// Run the agentic Spot trader (one cycle, or a daemon with --daemon).
    BotSpot(BotSpotArgs),
    /// Replay the exit-plan menu on Bot Spot's real round trips (public klines, no orders).
    BotSpotExitReplay(crate::exit_replay::ExitReplayArgs),
    /// Paper exit-plan watcher: follows bids between cycles, records would-be sells (no orders).
    BotSpotExitWatch(crate::exit_watch::ExitWatchArgs),
    /// Fit the Phase 50 long-term value (Markov) model on cached 1m candles (offline, no orders).
    BotSpotMdpFit(crate::mdp::MdpFitArgs),
    /// Backtest the Phase 48 rule personas offline on cached 1m candles (no network, no orders).
    BotSpotPersonaBacktest(crate::persona_backtest::report::PersonaBacktestArgs),
    /// Download GCP2 network coherence history and colour zones (public data, read-only).
    Gcp2History(crate::gcp2::Gcp2HistoryArgs),
}

#[derive(clap::Args, Debug, Clone)]
pub struct BotSpotArgs {
    /// Place real Spot orders when every live arming flag is set.
    #[arg(long)]
    pub submit_live: bool,
    /// Ask the bot; execute_route only records plans (overrides --submit-live).
    #[arg(long)]
    pub plan_only: bool,
    /// Print the decision context without asking the bot or submitting orders.
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
        Command::BotSpot(args) => Ok(match bot_spot(args).await {
            Ok(code) => code,
            Err(error) => {
                eprintln!("Bot Spot failed: {error}");
                ExitCode::FAILURE
            }
        }),
        Command::BotSpotExitReplay(args) => Ok(match crate::exit_replay::run_cli(args).await {
            Ok(code) => code,
            Err(error) => {
                eprintln!("Exit-plan replay failed: {error}");
                ExitCode::FAILURE
            }
        }),
        Command::BotSpotExitWatch(args) => Ok(match crate::exit_watch::run_cli(args).await {
            Ok(code) => code,
            Err(error) => {
                eprintln!("Exit-plan watcher failed: {error}");
                ExitCode::FAILURE
            }
        }),
        Command::BotSpotMdpFit(args) => Ok(match crate::mdp::run_fit_cli(&args) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("MDP fit failed: {error}");
                ExitCode::FAILURE
            }
        }),
        Command::BotSpotPersonaBacktest(args) => {
            Ok(match crate::persona_backtest::report::run_cli(&args) {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("Persona backtest failed: {error}");
                    ExitCode::FAILURE
                }
            })
        }
        Command::Gcp2History(args) => Ok(match crate::gcp2::run_history_cli(args).await {
            Ok(code) => code,
            Err(error) => {
                eprintln!("GCP2 history download failed: {error}");
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

/// One-shot runs refuse to start under the emergency stop file. The daemon starts anyway and
/// idles (see [`daemon_slot_runs`]), so a paused trader survives restarts and deploys instead
/// of exiting into a `restart: unless-stopped` loop.
fn stop_guard(config: &TraderConfig, daemon: bool) -> Result<()> {
    if !daemon && bb_core::fsutil::stop_requested(&config.stop_path) {
        return Err(TraderError::safety(format!(
            "Bot Spot emergency stop is active: {}",
            config.stop_path.display()
        )));
    }
    Ok(())
}

/// Daemon: whether this cycle slot runs a cycle. Under the stop file it skips the cycle (no
/// LLM call, no order) and logs only when the stop starts or ends, not every interval.
fn daemon_slot_runs(stop_active: bool, idling: &mut bool, stop_path: &std::path::Path) -> bool {
    if stop_active != *idling {
        *idling = stop_active;
        if stop_active {
            tracing::warn!(path = %stop_path.display(), "emergency stop active: no cycles until the stop file is removed");
        } else {
            tracing::info!("emergency stop removed: cycles resume");
        }
    }
    !stop_active
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
    stop_guard(&config, args.daemon)?;
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
    let _ = runtime
        .run_config
        .set(config.run_config(args.daemon.then_some(args.interval_seconds)));
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
    let interval = Duration::from_secs(args.interval_seconds);
    // A restart keeps the cadence: the first cycle waits until one interval after the last
    // one (exit plans keep running meanwhile), so restarts do not add cycles.
    if args.daemon
        && let Some(wait) = cadence_wait(
            &crate::journal::AgentState::load(&config.state_path),
            interval,
            bb_core::time::now(),
        )
    {
        tracing::info!(
            wait_seconds = wait.as_secs(),
            "keeping the cycle cadence: first cycle one interval after the last one"
        );
        pause(tokio::time::Instant::now() + wait, exit_guard.as_mut(), &shutdown).await;
    }
    let mut idling = false;
    while !shutdown.is_cancelled() {
        if args.daemon
            && !daemon_slot_runs(
                bb_core::fsutil::stop_requested(&config.stop_path),
                &mut idling,
                &config.stop_path,
            )
        {
            pause(tokio::time::Instant::now() + interval, exit_guard.as_mut(), &shutdown).await;
            continue;
        }
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
                tracing::error!(%error, "Bot Spot cycle failed");
            }
        }
        if !args.daemon || shutdown.is_cancelled() {
            break;
        }
        let next_cycle = tokio::time::Instant::now() + interval;
        pause(next_cycle, exit_guard.as_mut(), &shutdown).await;
    }
    runtime.begin_shutdown();
    trader.close().await;
    Ok(exit)
}

/// Until `until` or shutdown: follow exit plans when they run live, else sleep.
async fn pause(
    until: tokio::time::Instant,
    guard: Option<&mut crate::exit_guard::ExitGuard>,
    shutdown: &CancellationToken,
) {
    match guard {
        // Live exit plans: follow bids and sell when a plan fires until the next cycle.
        Some(guard) => guard.run_until(until, shutdown).await,
        None => tokio::select! {
            () = tokio::time::sleep_until(until) => {}
            () = shutdown.cancelled() => {}
        },
    }
}

/// How long a restarted daemon waits before its first cycle: the rest of the interval since
/// the last finished cycle, if any is left.
fn cadence_wait(
    state: &crate::journal::AgentState,
    interval: Duration,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<Duration> {
    let finished = state.last_cycle.as_ref()?.finished_at.as_str();
    let finished = chrono::DateTime::parse_from_rfc3339(finished).ok()?;
    let elapsed = (now - finished.with_timezone(&chrono::Utc)).to_std().ok()?;
    interval.checked_sub(elapsed).filter(|wait| !wait.is_zero())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{AgentState, LastCycle};

    #[test]
    fn restart_waits_out_the_rest_of_the_interval_since_the_last_cycle() {
        let interval = Duration::from_secs(1200);
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-17T13:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let state = |finished_at: &str| AgentState {
            last_cycle: Some(LastCycle {
                cycle_id: "c".into(),
                finished_at: finished_at.into(),
                status: "held".into(),
                turn_status: "completed".into(),
                end_cycle: None,
                executions: Vec::new(),
                error: String::new(),
            }),
            ..AgentState::default()
        };
        assert_eq!(
            cadence_wait(&state("2026-10-17T12:55:00+00:00"), interval, now),
            Some(Duration::from_secs(900))
        );
        // A whole interval ago, no record or an unreadable time: cycle at once.
        assert_eq!(cadence_wait(&state("2026-10-17T12:30:00+00:00"), interval, now), None);
        assert_eq!(cadence_wait(&AgentState::default(), interval, now), None);
        assert_eq!(cadence_wait(&state("yesterday"), interval, now), None);
    }

    #[test]
    fn under_the_stop_file_one_shot_refuses_but_the_daemon_starts_and_idles() {
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

        // Stopped slots run no cycle (no LLM call, no order); removing the file resumes.
        let mut idling = false;
        let runs: Vec<bool> = [true, true, false, false, true]
            .into_iter()
            .map(|stopped| daemon_slot_runs(stopped, &mut idling, &stop))
            .collect();
        assert_eq!(runs, [false, false, true, true, false]);
        assert!(idling);
    }
}
