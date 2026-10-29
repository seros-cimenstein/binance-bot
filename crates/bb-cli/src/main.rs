//! `bb` — single entry point for the Binance Bot Spot trader and its research commands.
//!
//! Each crate owns its subcommands; this binary only flattens them.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "bb",
    version,
    about = "Binance Bot Spot trader",
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    #[command(flatten)]
    Social(bb_social::cli::Command),
    #[command(flatten)]
    Strategies(bb_strategies::cli::Command),
    #[command(flatten)]
    Trader(bb_trader::cli::Command),
}

async fn dispatch(command: Command) -> anyhow::Result<ExitCode> {
    match command {
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
