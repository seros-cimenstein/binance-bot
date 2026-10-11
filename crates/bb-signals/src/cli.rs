//! `bb signals-*` subcommands (former `python -m src.signals.*` entry points).

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use crate::alerts::{Alerter, TelegramAlerter};
use crate::common::{Clock, SystemClock, http_client, secs, shutdown_signal};
use crate::dexscreener::{DexScreenerDetector, DexScreenerSettings};
use crate::dexscreener_api::DexScreenerApi;
use crate::dexscreener_launches::{LaunchDetector, LaunchDetectorSettings};
use crate::smart_money::{HeliusRpc, SmartMoneySettings, SmartMoneyTracker};
use crate::snapshotter::{SnapshotterSettings, run_snapshotter_once};
use crate::telegram_ingester::{TelegramConfigError, TelegramSettings, run_ingester};
use crate::v2_report::ReportSettings;
use crate::wallet_bootstrap::BirdeyeSource;

/// Polling-daemon flags shared by the detectors. Without `--once` they run
/// continuously (`--daemon` is accepted for compose/cron readability).
#[derive(clap::Args, Debug, Clone, Copy)]
pub struct DaemonArgs {
    /// Run one polling cycle and exit.
    #[arg(long)]
    pub once: bool,
    /// Run continuously (default).
    #[arg(long)]
    pub daemon: bool,
}

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Run the DexScreener signal detector.
    SignalsDexscreener(DaemonArgs),
    /// Run the V2 DexScreener launch detector.
    SignalsDexscreenerLaunches(DaemonArgs),
    /// Run the smart-money signal tracker (requires HELIUS_API_KEY).
    SignalsSmartMoney(DaemonArgs),
    /// Capture due V2 signal snapshots.
    SignalsSnapshotter {
        /// Accepted for cron readability (the command always runs once).
        #[arg(long)]
        once: bool,
    },
    /// Run the Telegram signal ingester.
    SignalsTelegramIngester {
        /// Process pending Telegram events and exit.
        #[arg(long)]
        once: bool,
        /// Run continuously (default).
        #[arg(long)]
        daemon: bool,
    },
    /// Interactive first-time Telegram user-account login.
    SignalsTelegramSetup,
    /// Write the V2 signal evaluation report.
    SignalsV2Report {
        /// Report date, defaults to UTC today.
        #[arg(long)]
        date: Option<String>,
    },
}

const DEXSCREENER_TIMEOUT: Duration = Duration::from_secs(10);
const HELIUS_TIMEOUT: Duration = Duration::from_secs(15);

fn clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock)
}

fn alerter() -> Arc<dyn Alerter> {
    Arc::new(TelegramAlerter)
}

/// Run `fut` until it completes or SIGINT/SIGTERM arrives.
async fn until_shutdown(name: &str, fut: impl Future<Output = ()>) {
    tokio::select! {
        () = fut => {}
        () = shutdown_signal() => tracing::info!("{name} stopped by signal"),
    }
}

pub async fn run(cmd: Command) -> anyhow::Result<ExitCode> {
    match cmd {
        Command::SignalsDexscreener(args) => {
            let settings = DexScreenerSettings::from_env()?;
            let api = DexScreenerApi::new(http_client(DEXSCREENER_TIMEOUT)?);
            let mut detector = DexScreenerDetector::new(settings, api, clock(), alerter());
            until_shutdown("DexScreener detector", detector.run(args.once)).await;
        }
        Command::SignalsDexscreenerLaunches(args) => {
            let settings = LaunchDetectorSettings::from_env()?;
            let api = DexScreenerApi::new(http_client(DEXSCREENER_TIMEOUT)?);
            let mut detector = LaunchDetector::new(settings, api, clock());
            until_shutdown("V2 DexScreener detector", detector.run(args.once)).await;
        }
        Command::SignalsSmartMoney(args) => {
            let settings = SmartMoneySettings::from_env()?;
            if let Err(err) = settings.validate() {
                tracing::error!("{err}");
                return Ok(ExitCode::from(2));
            }
            let http = http_client(HELIUS_TIMEOUT)?;
            let rpc = HeliusRpc::new(http.clone(), &settings.helius_api_key);
            let birdeye = BirdeyeSource::new(http, settings.birdeye_api_key.clone());
            let mut tracker = SmartMoneyTracker::new(settings, rpc, birdeye, clock(), alerter());
            until_shutdown("Smart-money signal tracker", tracker.run(args.once)).await;
        }
        Command::SignalsSnapshotter { once: _ } => {
            let settings = SnapshotterSettings::from_env()?;
            let api = DexScreenerApi::new(http_client(secs(settings.request_timeout_seconds))?);
            let clock = clock();
            let now_ms = clock.now_ms();
            run_snapshotter_once(&settings, &api, clock, now_ms).await?;
        }
        Command::SignalsTelegramIngester { once, daemon: _ } => {
            let settings = TelegramSettings::from_env()?;
            if let Err(err) = Box::pin(run_ingester(settings, once, clock(), alerter())).await {
                if let Some(config) = err.downcast_ref::<TelegramConfigError>() {
                    tracing::error!("{config}");
                    return Ok(ExitCode::from(2));
                }
                return Err(err);
            }
        }
        Command::SignalsTelegramSetup => {
            return Ok(ExitCode::from(crate::telegram_setup::run().await));
        }
        Command::SignalsV2Report { date } => {
            let settings = ReportSettings::from_env();
            let now_ms = SystemClock.now_ms();
            let report_date = date.unwrap_or_else(|| bb_core::time::utc_date(bb_core::time::now()));
            let path = crate::v2_report::generate(&settings, &report_date, now_ms)?;
            tracing::info!(path = %path.display(), "Wrote V2 signal report");
        }
    }
    Ok(ExitCode::SUCCESS)
}
