//! `bb listing-detector` / `bb listing-sniper` (former `python -m src.listing_detector`
//! and `python -m src.listing_sniper`).

use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context as _;
use bb_binance::{BinanceClient, BinanceConfig, Credentials};
use serde::Serialize;

use crate::alerts::Alerter;
use crate::detector::{self, DetectorMode, DetectorSettings};
use crate::error::{ListingError, Result};
use crate::sniper::{self, SniperMode, SniperSettings, SniperState, Venue};

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Detect Binance new-listing announcements. Default mode is detect-only.
    ListingDetector(DetectorArgs),
    /// Listing sniper: poll exchangeInfo, fire on new TRADING symbols.
    ListingSniper(SniperArgs),
}

#[derive(clap::Args, Debug)]
pub struct DetectorArgs {
    /// Audit and alert only (default).
    #[arg(long, conflicts_with_all = ["paper", "submit_live"])]
    pub detect_only: bool,
    /// Record hypothetical round trips for pairs already live.
    #[arg(long, conflicts_with = "submit_live")]
    pub paper: bool,
    /// Buy/hold/sell live (gated).
    #[arg(long)]
    pub submit_live: bool,
}

impl DetectorArgs {
    fn mode(&self) -> DetectorMode {
        if self.submit_live {
            DetectorMode::Live
        } else if self.paper {
            DetectorMode::Paper
        } else {
            DetectorMode::DetectOnly
        }
    }
}

#[derive(clap::Args, Debug)]
pub struct SniperArgs {
    /// Detect and audit only (default).
    #[arg(long, conflicts_with_all = ["paper", "submit_live"])]
    pub plan_only: bool,
    /// Simulate round trips on public prices.
    #[arg(long, conflicts_with = "submit_live")]
    pub paper: bool,
    /// Fire real orders (gated).
    #[arg(long)]
    pub submit_live: bool,
    /// Loop forever every poll_interval_seconds.
    #[arg(long)]
    pub daemon: bool,
}

impl SniperArgs {
    fn mode(&self) -> SniperMode {
        if self.submit_live {
            SniperMode::Live
        } else if self.paper {
            SniperMode::Paper
        } else {
            SniperMode::PlanOnly
        }
    }
}

pub async fn run(cmd: Command) -> anyhow::Result<ExitCode> {
    match cmd {
        Command::ListingDetector(args) => {
            finish("listing-detector", run_detector(args.mode()).await)
        }
        Command::ListingSniper(args) => {
            let mode = args.mode();
            if args.daemon {
                finish(
                    "listing-sniper",
                    run_sniper_daemon(mode).await.map(|()| None::<()>),
                )
            } else {
                finish("listing-sniper", run_sniper_once(mode).await.map(Some))
            }
        }
    }
}

/// Print the report as pretty JSON (exit 0) or the failure line (exit 1).
fn finish<T: Serialize>(name: &str, outcome: Result<T>) -> anyhow::Result<ExitCode> {
    match outcome {
        Ok(report) => {
            let json = serde_json::to_value(&report).context("serialising report")?;
            if !json.is_null() {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json).context("serialising report")?
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(err) => {
            eprintln!("{name} failed: {err}");
            Ok(ExitCode::FAILURE)
        }
    }
}

/// Mainnet client (the Python runtimes always used mainnet ccxt / api.binance.com),
/// signed when `credentials` is given.
fn mainnet_client(credentials: Option<Credentials>) -> Result<BinanceClient> {
    let mut config = BinanceConfig::new(false);
    if let Some(credentials) = credentials {
        config = config.with_credentials(credentials);
    }
    BinanceClient::new(config).map_err(ListingError::exchange("cannot build Binance client"))
}

fn required_credentials() -> Result<Credentials> {
    Credentials::required_from_env().map_err(ListingError::exchange("Binance credentials"))
}

async fn run_detector(mode: DetectorMode) -> Result<detector::DetectorReport> {
    let settings = DetectorSettings::from_env()?;
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| ListingError::Announcements(format!("cannot build HTTP client: {e}")))?;
    let client = match mode {
        DetectorMode::DetectOnly => None,
        DetectorMode::Paper => Some(mainnet_client(Credentials::from_env().ok().flatten())?),
        DetectorMode::Live => Some(mainnet_client(Some(required_credentials()?))?),
    };
    detector::run_once(
        &settings,
        mode,
        &http,
        client.as_ref(),
        &Alerter::from_env(),
    )
    .await
}

fn sniper_client(mode: SniperMode) -> Result<BinanceClient> {
    match mode {
        SniperMode::Live => mainnet_client(Some(required_credentials()?)),
        SniperMode::Paper | SniperMode::PlanOnly => mainnet_client(None),
    }
}

fn venue(mode: SniperMode, client: &BinanceClient) -> Venue<'_> {
    Venue {
        public: client,
        trading: (mode == SniperMode::Live).then_some(client),
    }
}

async fn run_sniper_once(mode: SniperMode) -> Result<sniper::CycleReport> {
    let settings = SniperSettings::from_env()?;
    let mut state = SniperState::load(&settings.state_path)?;
    let client = sniper_client(mode)?;
    sniper::run_cycle(
        &settings,
        &mut state,
        mode,
        venue(mode, &client),
        &Alerter::from_env(),
    )
    .await
}

async fn run_sniper_daemon(mode: SniperMode) -> Result<()> {
    let settings = SniperSettings::from_env()?;
    let client = sniper_client(mode)?;
    tracing::info!(
        mode = ?mode,
        hold_mode = settings.hold_mode.as_str(),
        state = %settings.state_path.display(),
        "listing sniper daemon starting"
    );
    sniper::run_daemon(&settings, mode, venue(mode, &client), &Alerter::from_env()).await
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser, Debug)]
    struct Bb {
        #[command(subcommand)]
        cmd: Command,
    }

    fn parse(args: &[&str]) -> std::result::Result<Command, clap::Error> {
        Bb::try_parse_from(std::iter::once("bb").chain(args.iter().copied())).map(|b| b.cmd)
    }

    #[test]
    fn flags_match_python() {
        let Command::ListingDetector(d) = parse(&["listing-detector"]).unwrap() else {
            panic!()
        };
        assert_eq!(d.mode(), DetectorMode::DetectOnly);
        let Command::ListingDetector(d) = parse(&["listing-detector", "--submit-live"]).unwrap()
        else {
            panic!()
        };
        assert_eq!(d.mode(), DetectorMode::Live);
        assert!(parse(&["listing-detector", "--paper", "--detect-only"]).is_err());

        let Command::ListingSniper(s) = parse(&["listing-sniper", "--paper", "--daemon"]).unwrap()
        else {
            panic!()
        };
        assert_eq!((s.mode(), s.daemon), (SniperMode::Paper, true));
        let Command::ListingSniper(s) = parse(&["listing-sniper"]).unwrap() else {
            panic!()
        };
        assert_eq!((s.mode(), s.daemon), (SniperMode::PlanOnly, false));
        assert!(parse(&["listing-sniper", "--plan-only", "--submit-live"]).is_err());
    }
}
