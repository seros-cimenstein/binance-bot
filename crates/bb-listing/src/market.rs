//! Exchange helpers shared by the detector and the sniper.

use std::collections::BTreeSet;

use bb_binance::fills::FillSummary;
use bb_binance::spot::{NewOrder, Order};
use bb_binance::{BinanceClient, BinanceError, Side};
use rust_decimal::Decimal;

use crate::error::{ListingError, Result};
use crate::util::describe;

/// Binance `-1121 Invalid symbol.`
const INVALID_SYMBOL_CODE: i64 = -1121;

/// Symbols currently `TRADING` with `quoteAsset == quote_asset` (one `exchangeInfo` call).
pub async fn trading_symbols_for_quote(
    client: &BinanceClient,
    quote_asset: &str,
) -> Result<BTreeSet<String>> {
    let info = client
        .exchange_info(None)
        .await
        .map_err(ListingError::exchange("exchangeInfo failed"))?;
    Ok(info
        .symbols
        .into_iter()
        .filter(|s| s.status == "TRADING" && s.quote_asset == quote_asset && !s.symbol.is_empty())
        .map(|s| s.symbol)
        .collect())
}

/// Last traded price; `None` on any failure (callers treat it as a skipped poll).
pub async fn last_price(client: &BinanceClient, symbol: &str) -> Option<Decimal> {
    match client.price(symbol).await {
        Ok(t) => Some(t.price),
        Err(err) => {
            tracing::debug!(symbol, error = %err, "ticker price unavailable");
            None
        }
    }
}

/// Whether `pair` (`BASE/QUOTE`) is listed and TRADING; fails closed when
/// markets cannot be loaded.
pub async fn exchange_has_pair(client: &BinanceClient, pair: &str) -> bool {
    match client.spot_markets(false).await {
        Ok(markets) => markets.get(pair).is_some_and(|r| r.is_trading()),
        Err(err) => {
            tracing::warn!(pair, error = %err, "cannot load spot markets; treating pair as absent");
            false
        }
    }
}

/// The failure mode of a brand-new listing racing our view of the venue:
/// Binance `-1121 Invalid symbol` or a client-side unknown-symbol rejection.
/// (Port of the ccxt `BadSymbol` stale-market-cache detection.)
pub fn is_unknown_symbol_error(err: &BinanceError) -> bool {
    if matches!(err, BinanceError::InvalidSymbol(_)) || err.code() == Some(INVALID_SYMBOL_CODE) {
        return true;
    }
    let msg = err.message().to_ascii_lowercase();
    msg.contains("invalid symbol") || msg.contains("does not have market symbol")
}

/// MARKET BUY spending `notional` quote. On an unknown-symbol error the
/// exchangeInfo cache is refreshed and the order retried exactly once; a
/// second failure is reported with an `after_market_reload:` prefix.
pub async fn market_buy_quote(
    client: &BinanceClient,
    pair: &str,
    notional: Decimal,
) -> std::result::Result<Order, String> {
    let order = NewOrder::market_quote(pair, Side::Buy, notional);
    match client.create_order(&order).await {
        Ok(filled) => Ok(filled),
        Err(err) if is_unknown_symbol_error(&err) => {
            tracing::warn!(pair, error = %err, "unknown symbol on buy; refreshing exchangeInfo and retrying once");
            let retried = match client.spot_markets(true).await {
                Ok(_) => client.create_order(&order).await,
                Err(reload_err) => Err(reload_err),
            };
            retried.map_err(|e| format!("after_market_reload: {}", describe(&e)))
        }
        Err(err) => Err(describe(&err)),
    }
}

/// Base quantity actually available from `buy`: executed quantity net of any
/// commission charged in the base asset, floored to the MARKET lot step when
/// the symbol rules are known.
pub async fn sellable_quantity(client: &BinanceClient, pair: &str, buy: &Order) -> Decimal {
    let base = pair.split('/').next().unwrap_or(pair);
    let net = FillSummary::from_order(buy).net_base_received(base);
    let rules = match client.symbol_rules(pair).await {
        Ok(rules) => Some(rules),
        Err(_) => match client.spot_markets(true).await {
            Ok(markets) => markets.get(pair).cloned(),
            Err(_) => None,
        },
    };
    match rules {
        Some(rules) => rules.round_market_qty(net),
        None => {
            tracing::warn!(
                pair,
                "symbol rules unavailable; selling unrounded net quantity"
            );
            net
        }
    }
}

/// MARKET SELL of `quantity` base.
pub async fn market_sell(
    client: &BinanceClient,
    pair: &str,
    quantity: Decimal,
) -> std::result::Result<Order, String> {
    client
        .create_order(&NewOrder::market(pair, Side::Sell, quantity))
        .await
        .map_err(|e| describe(&e))
}

/// Free balance of `asset`.
pub async fn free_balance(
    client: &BinanceClient,
    asset: &str,
) -> std::result::Result<Decimal, String> {
    client
        .asset_balance(asset)
        .await
        .map(|b| b.free)
        .map_err(|e| describe(&e))
}

/// Volume-weighted average fill price, zero when nothing executed.
pub fn average_price(order: &Order) -> Decimal {
    order.average_price().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(code: Option<i64>, msg: &str) -> BinanceError {
        BinanceError::Api {
            endpoint: "POST /api/v3/order".into(),
            http_status: 400,
            code,
            msg: msg.into(),
        }
    }

    #[test]
    fn detects_unknown_symbol_by_code_variant_and_message() {
        assert!(is_unknown_symbol_error(&api(
            Some(-1121),
            "Invalid symbol."
        )));
        assert!(is_unknown_symbol_error(&BinanceError::InvalidSymbol(
            "TSLAB/USDT".into()
        )));
        assert!(is_unknown_symbol_error(&api(
            None,
            "binance does not have market symbol TSLAB/USDT"
        )));
    }

    #[test]
    fn ignores_other_failures() {
        assert!(!is_unknown_symbol_error(&api(
            Some(-2010),
            "Account has insufficient balance"
        )));
        assert!(!is_unknown_symbol_error(&api(
            Some(-1013),
            "Filter failure: NOTIONAL"
        )));
    }
}
