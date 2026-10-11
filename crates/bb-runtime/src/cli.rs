//! `bb main` (`python -m src.main` / `python main.py`) and `bb ops-check`.

use std::process::ExitCode;
use std::sync::Arc;

use bb_binance::{BinanceClient, BinanceConfig, Credentials};

use crate::alerts::TelegramAlerter;
use crate::config::Settings;
use crate::exchange::BinanceExchange;
use crate::ops_check::run_checks;
use crate::runtime::{BinanceComponentFactory, main};

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Run the strategy runtime until SIGINT/SIGTERM (SIGUSR1 closes all positions).
    Main,
    /// Run read-only Binance bot operational checks.
    OpsCheck {
        /// Send one operational verification message to the configured chat.
        #[arg(long)]
        telegram_test: bool,
    },
}

pub async fn run(cmd: Command) -> anyhow::Result<ExitCode> {
    match cmd {
        Command::Main => {
            let settings = Settings::from_env()?;
            match main(settings, Arc::new(BinanceComponentFactory)).await {
                Ok(()) => Ok(ExitCode::SUCCESS),
                Err(err) if err.is_fatal() => Ok(ExitCode::FAILURE),
                Err(err) => Err(err.into()),
            }
        }
        Command::OpsCheck { telegram_test } => {
            let result = async {
                let settings = Settings::from_env()?;
                let credentials = Credentials::new(settings.api_key.clone(), &settings.api_secret)?;
                let client = BinanceClient::new(
                    BinanceConfig::new(settings.use_testnet).with_credentials(credentials),
                )?;
                let alerter = telegram_test.then(|| {
                    TelegramAlerter::new(&settings.telegram_bot_token, &settings.telegram_chat_id)
                });
                anyhow::Ok(
                    run_checks(&settings, &BinanceExchange::new(client), alerter.as_ref()).await?,
                )
            }
            .await;
            match result {
                Ok(lines) => {
                    for line in lines {
                        println!("{line}");
                    }
                    Ok(ExitCode::SUCCESS)
                }
                Err(err) => {
                    println!("Operational check failed: {err}");
                    Ok(ExitCode::FAILURE)
                }
            }
        }
    }
}
