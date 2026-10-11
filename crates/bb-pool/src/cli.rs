//! Command-line entry points (`pool`, `pool-report`, `pool-replay`, `pool-history-replay`,
//! `pool-unified`). Every command is paper-only or read-only.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use bb_binance::history::HistoryFetcher;
use bb_binance::{BinanceClient, BinanceConfig, Credentials};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tracing::instrument::WithSubscriber;
use tracing::subscriber::NoSubscriber;

use crate::config::{PoolSettings, pool_credentials};
use crate::history_replay::{DEFAULT_HISTORY_DB, HistoricalReplayRequest, run_historical_replay};
use crate::ledger::PaperLedger;
use crate::pyfmt::{parse_py_iso, pretty_sorted};
use crate::replay::{DEFAULT_REPLAY_DB, run_replay};
use crate::report::{DEFAULT_PAPER_DB, ReportOptions, build_report, render_report};
use crate::runtime::PaperPoolRuntime;
use crate::unified::{
    UnifiedPoolLimits, build_unified_pool_snapshot, load_holdings, load_prices,
    render_unified_snapshot,
};

/// Pool commands.
#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Run the Binance paper asset pool (POOL_MODE=paper; read-only exchange access).
    Pool {
        /// Evaluate one cycle and exit.
        #[arg(long)]
        once: bool,
    },
    /// Read paper-pool decision evidence.
    PoolReport {
        #[arg(long, default_value = DEFAULT_PAPER_DB)]
        db_path: PathBuf,
        #[arg(long, default_value_t = 96)]
        required_observations: usize,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Render structured JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run isolated Spot research replay from Binance public candles.
    PoolHistoryReplay {
        /// Candidate direct */USDT market (repeatable).
        #[arg(long = "symbol")]
        symbol: Vec<String>,
        /// Initial balance such as BNB=0.016 (repeatable, required).
        #[arg(long = "balance", required = true, value_parser = parse_balance)]
        balance: Vec<(String, Decimal)>,
        #[arg(long, value_parser = parse_timestamp)]
        start: DateTime<Utc>,
        #[arg(long, value_parser = parse_timestamp)]
        end: DateTime<Utc>,
        #[arg(long, default_value = "15m")]
        timeframe: String,
        #[arg(long, default_value_t = 96)]
        required_observations: usize,
        #[arg(long)]
        cycles: Option<usize>,
        #[arg(long, default_value = "0.001")]
        taker_fee: Decimal,
        #[arg(long, default_value = "10.0")]
        spread_bps: Decimal,
        #[arg(long, default_value = "25.0")]
        slippage_buffer_bps: Decimal,
        #[arg(long, default_value = "5.0")]
        min_notional: Decimal,
        #[arg(long, default_value = DEFAULT_HISTORY_DB)]
        db_path: PathBuf,
        /// Show candle fetch and replay logs.
        #[arg(long)]
        verbose: bool,
    },
    /// Run fast paper-pool fixture replay.
    PoolReplay {
        /// Path to replay fixture JSON.
        #[arg(long)]
        fixture: PathBuf,
        /// Number of replay cycles.
        #[arg(long)]
        cycles: Option<usize>,
        /// Isolated replay SQLite path; must not be pool-paper.db.
        #[arg(long, default_value = DEFAULT_REPLAY_DB)]
        db_path: PathBuf,
        /// Append to an existing replay database instead of resetting it.
        #[arg(long)]
        append: bool,
        /// Suppress per-cycle logs and print only the replay summary.
        #[arg(long)]
        quiet: bool,
    },
    /// Value a unified dry-run asset pool from local JSON inputs.
    PoolUnified {
        /// Required.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        holdings_json: Option<PathBuf>,
        #[arg(long)]
        prices_json: Option<PathBuf>,
        #[arg(long, default_value = "USDT,BTC,PAXG,XAUT")]
        benchmarks: String,
        #[arg(long)]
        json: bool,
    },
}

fn parse_balance(value: &str) -> Result<(String, Decimal), String> {
    let invalid = || "balance must look like BNB=0.016".to_owned();
    let (asset, amount) = value.split_once('=').ok_or_else(invalid)?;
    let amount = amount.trim();
    let amount = Decimal::from_str_exact(amount)
        .or_else(|_| Decimal::from_scientific(amount))
        .map_err(|_| invalid())?;
    Ok((asset.to_owned(), amount))
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>, String> {
    parse_py_iso(value).ok_or_else(|| format!("invalid ISO timestamp: {value}"))
}

/// Run one pool command.
pub async fn run(cmd: Command) -> anyhow::Result<ExitCode> {
    match cmd {
        Command::Pool { once } => run_pool(once).await,
        Command::PoolReport {
            db_path,
            required_observations,
            limit,
            json,
        } => {
            let options = ReportOptions {
                required_observations,
                limit,
                ..ReportOptions::default()
            };
            match build_report(&db_path, &options) {
                Ok(report) => {
                    println!(
                        "{}",
                        if json {
                            report.to_json()?
                        } else {
                            render_report(&report)
                        }
                    );
                    Ok(ExitCode::SUCCESS)
                }
                Err(err) => {
                    eprintln!("paper pool report failed: {err}");
                    Ok(ExitCode::FAILURE)
                }
            }
        }
        Command::PoolHistoryReplay {
            symbol,
            balance,
            start,
            end,
            timeframe,
            required_observations,
            cycles,
            taker_fee,
            spread_bps,
            slippage_buffer_bps,
            min_notional,
            db_path,
            verbose,
        } => {
            let request = HistoricalReplayRequest {
                timeframe,
                required_observations,
                cycles,
                taker_fee,
                spread_bps,
                slippage_buffer_bps,
                min_notional,
                ..HistoricalReplayRequest::new(symbol, balance, start, end)
            };
            let fetcher = HistoryFetcher::spot(BinanceClient::new(BinanceConfig::new(false))?);
            let replay = run_historical_replay(&request, &db_path, &fetcher);
            let result = if verbose {
                replay.await
            } else {
                replay.with_subscriber(NoSubscriber::default()).await
            };
            match result {
                Ok(summary) => {
                    println!("{}", pretty_sorted(&summary)?);
                    Ok(ExitCode::SUCCESS)
                }
                Err(err) => {
                    eprintln!("historical replay failed: {err}");
                    Ok(ExitCode::FAILURE)
                }
            }
        }
        Command::PoolReplay {
            fixture,
            cycles,
            db_path,
            append,
            quiet,
        } => {
            let replay = run_replay(&fixture, cycles, &db_path, !append);
            let summary = if quiet {
                replay.with_subscriber(NoSubscriber::default()).await
            } else {
                replay.await
            }?;
            println!("{}", pretty_sorted(&summary)?);
            Ok(ExitCode::SUCCESS)
        }
        Command::PoolUnified {
            dry_run,
            holdings_json,
            prices_json,
            benchmarks,
            json,
        } => {
            if !dry_run {
                println!("unified pool check failed: --dry-run is required");
                return Ok(ExitCode::FAILURE);
            }
            match unified(holdings_json, prices_json, &benchmarks, json) {
                Ok(output) => {
                    println!("{output}");
                    Ok(ExitCode::SUCCESS)
                }
                Err(err) => {
                    println!("unified pool check failed: {err}");
                    Ok(ExitCode::FAILURE)
                }
            }
        }
    }
}

fn unified(
    holdings_json: Option<PathBuf>,
    prices_json: Option<PathBuf>,
    benchmarks: &str,
    json: bool,
) -> crate::Result<String> {
    let holdings = holdings_json
        .map(|p| load_holdings(&p))
        .transpose()?
        .unwrap_or_default();
    let prices = match prices_json {
        Some(path) => load_prices(&path)?,
        None => [("USDT".to_owned(), Decimal::ONE)].into(),
    };
    let benchmarks: Vec<String> = benchmarks
        .split(',')
        .map(|b| b.trim().to_uppercase())
        .filter(|b| !b.is_empty())
        .collect();
    let snapshot = build_unified_pool_snapshot(
        &holdings,
        &prices,
        &benchmarks,
        None,
        &UnifiedPoolLimits::default(),
    )?;
    Ok(if json {
        pretty_sorted(&snapshot)?
    } else {
        render_unified_snapshot(&snapshot)
    })
}

async fn run_pool(once: bool) -> anyhow::Result<ExitCode> {
    let settings = PoolSettings::from_env()?;
    let (api_key, api_secret) = pool_credentials(|name| std::env::var(name).ok())?;
    let config = BinanceConfig::new(settings.use_testnet)
        .with_credentials(Credentials::new(api_key, &api_secret)?);
    let client = Arc::new(BinanceClient::new(config)?);
    let ledger = PaperLedger::open(&settings.db_path)?;
    let cycle_seconds = Duration::from_secs_f64(settings.cycle_seconds);
    let mut runtime = PaperPoolRuntime::new(client, settings, ledger, None);

    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        wait_for_shutdown().await;
        let _ = stop_tx.send(true);
    });
    while !*stop_rx.borrow() {
        let report = runtime.cycle().await?;
        for (portfolio_id, totals) in &report.benchmark_totals {
            let totals: Vec<String> = totals.iter().map(|(b, v)| format!("{b}={v}")).collect();
            tracing::info!(portfolio_id = %portfolio_id, totals = %totals.join(","), "Paper portfolio benchmark valuation");
        }
        if once {
            break;
        }
        tokio::select! {
            _ = stop_rx.changed() => {}
            () = tokio::time::sleep(cycle_seconds) => {}
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use clap::{CommandFactory, Parser};
    use rust_decimal_macros::dec;

    use super::*;

    #[derive(Parser, Debug)]
    struct Cli {
        #[command(subcommand)]
        command: Command,
    }

    #[test]
    fn parses_python_flags() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from([
            "bb",
            "pool-history-replay",
            "--balance",
            "BNB=0.016",
            "--symbol",
            "ETH/USDT",
            "--start",
            "2026-05-01T00:00:00Z",
            "--end",
            "2026-05-02",
        ])
        .unwrap();
        let Command::PoolHistoryReplay {
            balance,
            db_path,
            required_observations,
            ..
        } = cli.command
        else {
            unreachable!("wrong variant")
        };
        assert_eq!(balance, vec![("BNB".to_owned(), dec!(0.016))]);
        assert_eq!(db_path, PathBuf::from(DEFAULT_HISTORY_DB));
        assert_eq!(required_observations, 96);
        assert!(Cli::try_parse_from(["bb", "pool-history-replay", "--balance", "BNB"]).is_err());
        let cli =
            Cli::try_parse_from(["bb", "pool-replay", "--fixture", "f.json", "--quiet"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::PoolReplay {
                quiet: true,
                append: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn unified_cli_requires_dry_run_and_reads_local_json() {
        let dir = tempfile::tempdir().unwrap();
        let holdings = dir.path().join("holdings.json");
        let prices = dir.path().join("prices.json");
        std::fs::write(
            &holdings,
            r#"[{"account_type": "spot", "wallet": "spot", "asset": "USDT", "total": 10, "free": 10}]"#,
        )
        .unwrap();
        std::fs::write(&prices, r#"{"USDT": 1, "BTC": 50000}"#).unwrap();
        let missing = Command::PoolUnified {
            dry_run: false,
            holdings_json: None,
            prices_json: None,
            benchmarks: "USDT".into(),
            json: false,
        };
        assert_eq!(run(missing).await.unwrap(), ExitCode::FAILURE);
        let output = unified(Some(holdings), Some(prices), "USDT,BTC", true).unwrap();
        assert!(output.contains(r#""total_net_usdt": 10.0"#), "{output}");
    }
}
