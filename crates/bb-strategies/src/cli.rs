//! `bb` subcommands for Bot Spot research. Each module owns its `Args` and `run_cli`.

use std::process::ExitCode;

use crate::{bot_spot_backtest, entry_study, spec_snapshot};

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Replay Bot Spot's cost-aware momentum rule against historical klines.
    BotSpotBacktest(bot_spot_backtest::Args),
    /// Public-data Binance speculation snapshot (CSVs + facts-only SUMMARY.md).
    SpecSnapshot(spec_snapshot::Args),
    /// Research: net forward return after each kind of entry over liquid USDT coins.
    EntryStudy(entry_study::Args),
}

pub async fn run(cmd: Command) -> anyhow::Result<ExitCode> {
    match cmd {
        Command::BotSpotBacktest(args) => bot_spot_backtest::run_cli(args).await,
        Command::SpecSnapshot(args) => spec_snapshot::run_cli(args).await,
        Command::EntryStudy(args) => entry_study::run_cli(args).await,
    }
}

/// argparse `parser.error(...)`: message on stderr, exit status 2.
pub(crate) fn usage_error(command: &str, message: &str) -> ExitCode {
    eprintln!("usage: bb {command} [options]\nbb {command}: error: {message}");
    ExitCode::from(2)
}
