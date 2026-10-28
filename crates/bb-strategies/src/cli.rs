//! `bb` subcommands for the former `python -m src.<module>` strategy entry points.
//! Each module owns its `Args` (identical argparse flags) and `run_cli`.

use std::process::ExitCode;

use crate::{
    basis_arb_backtest, biz_lead_study, bot_spot_backtest, entry_study, funding_arb,
    funding_arb_backtest, grid_backtest, mean_reversion, mean_reversion_backtest,
    news_event_backtest, spec_snapshot, stat_arb_backtest, trend_filter, trend_filter_backtest,
};

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Funding-arb runtime. Plan-only by default; paper and live are opt-in.
    FundingArb(funding_arb::Args),
    /// Replay delta-neutral funding-rate harvesting against historical funding events.
    FundingArbBacktest(funding_arb_backtest::Args),
    /// Trend-filter runtime. Plan-only by default; --submit-live is gated.
    TrendFilter(trend_filter::Args),
    /// Replay the top-K trend filter against historical klines.
    TrendFilterBacktest(trend_filter_backtest::Args),
    /// Mean-reversion runtime (plan-only, --paper, --daemon).
    MeanReversion(mean_reversion::Args),
    /// Replay intraday mean reversion against historical klines.
    MeanReversionBacktest(mean_reversion_backtest::Args),
    /// Replay spot/perp basis arbitrage against historical klines.
    BasisArbBacktest(basis_arb_backtest::Args),
    /// Replay a static spot grid against historical klines.
    GridBacktest(grid_backtest::Args),
    /// Replay macro news-event trades against historical klines.
    NewsEventBacktest(news_event_backtest::Args),
    /// Replay pair statistical arbitrage against historical klines.
    StatArbBacktest(stat_arb_backtest::Args),
    /// Replay Bot Spot's cost-aware momentum rule against historical klines.
    BotSpotBacktest(bot_spot_backtest::Args),
    /// Public-data Binance speculation snapshot (CSVs + facts-only SUMMARY.md).
    SpecSnapshot(spec_snapshot::Args),
    /// Research: net forward return after each kind of entry over liquid USDT coins.
    EntryStudy(entry_study::Args),
    /// Research: does /biz/ talk about a coin (biz-app feeds) come before its price moves?
    BizLeadStudy(biz_lead_study::Args),
}

pub async fn run(cmd: Command) -> anyhow::Result<ExitCode> {
    match cmd {
        Command::FundingArb(args) => funding_arb::run_cli(args).await,
        Command::FundingArbBacktest(args) => funding_arb_backtest::run_cli(args).await,
        Command::TrendFilter(args) => trend_filter::run_cli(args).await,
        Command::TrendFilterBacktest(args) => trend_filter_backtest::run_cli(args).await,
        Command::MeanReversion(args) => mean_reversion::run_cli(args).await,
        Command::MeanReversionBacktest(args) => mean_reversion_backtest::run_cli(args).await,
        Command::BasisArbBacktest(args) => basis_arb_backtest::run_cli(args).await,
        Command::GridBacktest(args) => grid_backtest::run_cli(args).await,
        Command::NewsEventBacktest(args) => news_event_backtest::run_cli(args).await,
        Command::StatArbBacktest(args) => stat_arb_backtest::run_cli(args).await,
        Command::BotSpotBacktest(args) => bot_spot_backtest::run_cli(args).await,
        Command::SpecSnapshot(args) => spec_snapshot::run_cli(args).await,
        Command::EntryStudy(args) => entry_study::run_cli(args).await,
        Command::BizLeadStudy(args) => biz_lead_study::run_cli(args).await,
    }
}

/// argparse `parser.error(...)`: message on stderr, exit status 2.
pub(crate) fn usage_error(command: &str, message: &str) -> ExitCode {
    eprintln!("usage: bb {command} [options]\nbb {command}: error: {message}");
    ExitCode::from(2)
}
