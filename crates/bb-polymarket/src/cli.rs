//! `bb polymarket-scanner`: one advisory scan pass printed to stdout.

use std::process::ExitCode;

use clap::{Args, Subcommand};
use rust_decimal::Decimal;

use crate::client::{CLOB_API_BASE, PolymarketClient};
use crate::scanner::{ScanConfig, render_text, scan, summarize};

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Scan Polymarket binary markets for buy-both / sell-both arbitrage.
    PolymarketScanner(ScannerArgs),
}

#[derive(Args, Debug)]
pub struct ScannerArgs {
    #[arg(long, default_value = CLOB_API_BASE)]
    pub base_url: String,
    #[arg(long, default_value_t = 300)]
    pub max_markets: usize,
    #[arg(long, default_value = "10.0")]
    pub taker_fee_bps_per_leg: Decimal,
    #[arg(long, default_value = "0.005")]
    pub min_gross_profit: Decimal,
    #[arg(long, default_value = "5.0")]
    pub min_size_per_leg: Decimal,
    #[arg(long, default_value_t = 20)]
    pub max_rows: usize,
    /// Print the summary as JSON.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(cmd: Command) -> anyhow::Result<ExitCode> {
    match cmd {
        Command::PolymarketScanner(args) => run_scanner(args).await,
    }
}

async fn run_scanner(args: ScannerArgs) -> anyhow::Result<ExitCode> {
    let client = PolymarketClient::new(&args.base_url)?;
    let cfg = ScanConfig {
        max_markets: args.max_markets,
        taker_fee_bps_per_leg: args.taker_fee_bps_per_leg,
        min_gross_profit: args.min_gross_profit,
        min_size_per_leg: args.min_size_per_leg,
        ..ScanConfig::default()
    };
    let opportunities = scan(&client, &cfg).await?;
    let summary = summarize(&opportunities, args.max_rows);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print!("{}", render_text(&summary));
    }
    Ok(ExitCode::SUCCESS)
}
