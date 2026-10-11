//! Port of `src/ops_check.py`: read-only operational verification.

use crate::alerts::TelegramAlerter;
use crate::config::Settings;
use crate::exchange::{Exchange, ExchangeError};

#[derive(Debug, thiserror::Error)]
pub enum OperationsCheckError {
    #[error("live configuration requires CONFIRM_LIVE_TRADING=yes")]
    LiveNotConfirmed,
    #[error("ticker response did not include a price")]
    MissingPrice,
    #[error("Telegram verification delivery failed")]
    TelegramDelivery,
    #[error(transparent)]
    Exchange(#[from] ExchangeError),
}

/// Ticker + balance checks, plus an optional Telegram delivery check.
/// Returns the report lines (`Mode: ...` first).
pub async fn run_checks(
    settings: &Settings,
    exchange: &dyn Exchange,
    alerter: Option<&TelegramAlerter>,
) -> Result<Vec<String>, OperationsCheckError> {
    if !settings.use_testnet && settings.confirm_live_trading != "yes" {
        return Err(OperationsCheckError::LiveNotConfirmed);
    }
    let mode = settings.mode();
    let result = checks(settings, exchange, alerter, mode).await;
    exchange.close().await;
    let mut lines = vec![format!("Mode: {mode}")];
    lines.extend(result?);
    Ok(lines)
}

async fn checks(
    settings: &Settings,
    exchange: &dyn Exchange,
    alerter: Option<&TelegramAlerter>,
    mode: &str,
) -> Result<Vec<String>, OperationsCheckError> {
    let mut results = Vec::new();
    let price = exchange
        .last_price(&settings.trading_symbol)
        .await?
        .ok_or(OperationsCheckError::MissingPrice)?;
    results.push(format!(
        "Exchange ticker OK: {} last={price}",
        settings.trading_symbol
    ));

    exchange.balances().await?;
    results.push("Account access OK: balance request succeeded".to_owned());

    if let Some(alerter) = alerter {
        if !alerter
            .alert_operations_test(mode, &settings.trading_symbol)
            .await
        {
            return Err(OperationsCheckError::TelegramDelivery);
        }
        results.push("Telegram delivery OK: verification message sent".to_owned());
    }
    Ok(results)
}
