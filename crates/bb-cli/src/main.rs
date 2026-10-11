//! `bb` — single entry point for the Binance bot fleet.
//!
//! Every former `python -m src.<module>` entry point is a subcommand named after
//! the module path in kebab-case (`src.funding_arb` → `bb funding-arb`). Each
//! crate owns its subcommands; this binary only flattens them.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "bb",
    version,
    about = "Binance bot fleet",
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    #[command(flatten)]
    Airdrop(bb_airdrop::cli::Command),
    #[command(flatten)]
    Capital(bb_capital::cli::Command),
    #[command(flatten)]
    Listing(bb_listing::cli::Command),
    #[command(flatten)]
    Polymarket(bb_polymarket::cli::Command),
    #[command(flatten)]
    Pool(bb_pool::cli::Command),
    #[command(flatten)]
    Products(bb_products::cli::Command),
    #[command(flatten)]
    Runtime(bb_runtime::cli::Command),
    #[command(flatten)]
    Signals(bb_signals::cli::Command),
    #[command(flatten)]
    Sniper(bb_sniper::cli::Command),
    #[command(flatten)]
    Social(bb_social::cli::Command),
    #[command(flatten)]
    Strategies(bb_strategies::cli::Command),
    #[command(flatten)]
    Trader(bb_trader::cli::Command),
}

async fn dispatch(command: Command) -> anyhow::Result<ExitCode> {
    match command {
        Command::Airdrop(cmd) => bb_airdrop::cli::run(cmd).await,
        Command::Capital(cmd) => bb_capital::cli::run(cmd).await,
        Command::Listing(cmd) => bb_listing::cli::run(cmd).await,
        Command::Polymarket(cmd) => bb_polymarket::cli::run(cmd).await,
        Command::Pool(cmd) => bb_pool::cli::run(cmd).await,
        Command::Products(cmd) => bb_products::cli::run(cmd).await,
        Command::Runtime(cmd) => bb_runtime::cli::run(cmd).await,
        Command::Signals(cmd) => bb_signals::cli::run(cmd).await,
        Command::Sniper(cmd) => bb_sniper::cli::run(cmd).await,
        Command::Social(cmd) => bb_social::cli::run(cmd).await,
        Command::Strategies(cmd) => bb_strategies::cli::run(cmd).await,
        Command::Trader(cmd) => bb_trader::cli::run(cmd).await,
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    bb_core::env::load_dotenv();
    bb_core::logging::init();
    let cli = Cli::parse();
    match dispatch(cli.command).await {
        Ok(code) => code,
        Err(err) => {
            tracing::error!(error = format!("{err:#}"), "command failed");
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn subcommand_tree_is_consistent() {
        // Catches duplicate subcommand names across crates and invalid clap attributes.
        Cli::command().debug_assert();
    }
}
