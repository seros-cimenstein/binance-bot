//! Paper and live round trips plus the live-fire Telegram text.

use bb_binance::BinanceClient;
use bb_binance::spot::Order;
use rust_decimal::Decimal;
use serde::Serialize;

use super::exit::hold;
use super::settings::{HoldMode, SniperSettings};
use crate::market;

/// Approximate round-trip taker fees (10 bps each side), in percent.
const PAPER_FEE_DRAG_PCT: Decimal = Decimal::from_parts(2, 0, 0, false, 1);

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PaperOutcome {
    BuySkippedNoPrice {
        symbol: String,
        hold_mode: HoldMode,
    },
    RoundTripComplete {
        symbol: String,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        entry_price: Decimal,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        exit_price: Decimal,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        gross_pnl_pct: Decimal,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        net_pnl_pct: Decimal,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        pnl_usdt: Decimal,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        notional_usdt: Decimal,
        hold_mode: HoldMode,
        exit_reason: String,
        elapsed_seconds: f64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum LiveOutcome {
    BuyFailed {
        symbol: String,
        reason: String,
    },
    BuyZeroFill {
        symbol: String,
        buy_order: Box<Order>,
    },
    /// Bought but could not sell: the position is stranded.
    SellFailed {
        symbol: String,
        buy_order: Box<Order>,
        reason: String,
        elapsed_seconds: f64,
        exit_reason: String,
    },
    RoundTripComplete {
        symbol: String,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        filled_qty: Decimal,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        avg_buy: Decimal,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        avg_sell: Decimal,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        sold_qty: Decimal,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        pnl_quote: Decimal,
        hold_mode: HoldMode,
        elapsed_seconds: f64,
        exit_reason: String,
        buy_order_id: String,
        sell_order_id: String,
    },
}

/// One fire, tagged with `"mode": "paper" | "live"`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum FireResult {
    Paper(PaperOutcome),
    Live(LiveOutcome),
}

/// Paper round trip on public prices: entry = ticker at detection, exit =
/// ticker when the hold mode triggers, net = gross minus 0.2 % fee drag.
pub async fn fire_paper(
    settings: &SniperSettings,
    public: &BinanceClient,
    symbol: &str,
) -> PaperOutcome {
    let entry_price = match market::last_price(public, symbol).await {
        Some(p) if p > Decimal::ZERO => p,
        _ => {
            return PaperOutcome::BuySkippedNoPrice {
                symbol: symbol.to_owned(),
                hold_mode: settings.hold_mode,
            };
        }
    };
    let outcome = hold(settings, entry_price, || market::last_price(public, symbol)).await;
    let gross_pnl_pct = (outcome.exit_price / entry_price - Decimal::ONE) * Decimal::ONE_HUNDRED;
    let net_pnl_pct = gross_pnl_pct - PAPER_FEE_DRAG_PCT;
    PaperOutcome::RoundTripComplete {
        symbol: symbol.to_owned(),
        entry_price,
        exit_price: outcome.exit_price,
        gross_pnl_pct,
        net_pnl_pct,
        pnl_usdt: settings.notional_usdt * net_pnl_pct / Decimal::ONE_HUNDRED,
        notional_usdt: settings.notional_usdt,
        hold_mode: settings.hold_mode,
        exit_reason: outcome.exit_reason,
        elapsed_seconds: outcome.elapsed_seconds,
    }
}

/// `FOOUSDT` → `FOO/USDT`.
pub fn unified_pair(symbol: &str, quote_asset: &str) -> String {
    let base = symbol.strip_suffix(quote_asset).unwrap_or(symbol);
    format!("{base}/{quote_asset}")
}

/// Live round trip: MARKET BUY by quote (refresh exchangeInfo + retry once on
/// an unknown-symbol error), unified hold loop, MARKET SELL of the net base.
pub async fn fire_live(
    settings: &SniperSettings,
    client: &BinanceClient,
    symbol: &str,
) -> LiveOutcome {
    let pair = unified_pair(symbol, &settings.quote_asset);
    let buy = match market::market_buy_quote(client, &pair, settings.notional_usdt).await {
        Ok(order) => order,
        Err(reason) => {
            return LiveOutcome::BuyFailed {
                symbol: pair,
                reason,
            };
        }
    };
    let filled_qty = buy.executed_qty;
    let avg_buy = market::average_price(&buy);
    if filled_qty <= Decimal::ZERO {
        return LiveOutcome::BuyZeroFill {
            symbol: pair,
            buy_order: Box::new(buy),
        };
    }
    let outcome = hold(settings, avg_buy, || market::last_price(client, &pair)).await;
    let sell_qty = market::sellable_quantity(client, &pair, &buy).await;
    let sell = match market::market_sell(client, &pair, sell_qty).await {
        Ok(order) => order,
        Err(reason) => {
            return LiveOutcome::SellFailed {
                symbol: pair,
                buy_order: Box::new(buy),
                reason,
                elapsed_seconds: outcome.elapsed_seconds,
                exit_reason: outcome.exit_reason,
            };
        }
    };
    let avg_sell = market::average_price(&sell);
    LiveOutcome::RoundTripComplete {
        symbol: pair,
        filled_qty,
        avg_buy,
        avg_sell,
        sold_qty: sell.executed_qty,
        pnl_quote: (avg_sell - avg_buy) * filled_qty,
        hold_mode: settings.hold_mode,
        elapsed_seconds: outcome.elapsed_seconds,
        exit_reason: outcome.exit_reason,
        buy_order_id: buy.order_id.to_string(),
        sell_order_id: sell.order_id.to_string(),
    }
}

/// Telegram one-liner for a live fire.
pub fn format_live_fire_alert(result: &LiveOutcome, hold_mode: HoldMode) -> String {
    let mode = hold_mode.as_str();
    match result {
        LiveOutcome::RoundTripComplete {
            symbol,
            avg_buy,
            avg_sell,
            pnl_quote,
            exit_reason,
            ..
        } => {
            let pct = if *avg_buy > Decimal::ZERO {
                (*avg_sell / *avg_buy - Decimal::ONE) * Decimal::ONE_HUNDRED
            } else {
                Decimal::ZERO
            };
            let emoji = if *pnl_quote > Decimal::ZERO {
                "💰"
            } else if *pnl_quote < Decimal::ZERO {
                "📉"
            } else {
                "➖"
            };
            format!(
                "{emoji} [LIVE {mode}] {symbol} closed: ${:.6} → ${:.6} ({:+.2}% / {:+.4} USDT) — {exit_reason}",
                avg_buy.round_dp(6),
                avg_sell.round_dp(6),
                pct.round_dp(2),
                pnl_quote.round_dp(4)
            )
        }
        LiveOutcome::SellFailed { symbol, reason, .. } => {
            format!("⚠️ [LIVE {mode}] {symbol} STRANDED — bought but sell failed: {reason}")
        }
        LiveOutcome::BuyFailed { symbol, reason } => {
            format!("❌ [LIVE {mode}] {symbol} buy_failed: {reason}")
        }
        LiveOutcome::BuyZeroFill { symbol, .. } => {
            format!("❌ [LIVE {mode}] {symbol} buy_zero_fill: (zero fill)")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bb_binance::{BinanceConfig, Credentials};
    use rust_decimal_macros::dec;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn client(server: &MockServer) -> BinanceClient {
        let cfg = BinanceConfig::new(false)
            .with_spot_rest_url(server.uri())
            .with_retries(0, Duration::ZERO, Duration::ZERO)
            .with_credentials(Credentials::new("k", "s").unwrap());
        BinanceClient::new(cfg).unwrap()
    }

    /// Scripted `/ticker/price` sequence, then HTTP 503.
    async fn price_server(prices: &[&str]) -> MockServer {
        let server = MockServer::start().await;
        for p in prices {
            Mock::given(method("GET"))
                .and(path("/api/v3/ticker/price"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"symbol": "FOOUSDT", "price": p})),
                )
                .up_to_n_times(1)
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/api/v3/ticker/price"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        server
    }

    fn paper(
        mode: HoldMode,
        hold_seconds: u64,
        interval: u64,
        hard_stop: Decimal,
    ) -> SniperSettings {
        SniperSettings {
            notional_usdt: dec!(10),
            hold_mode: mode,
            hold_seconds,
            trailing_poll_interval_seconds: interval,
            max_hold_seconds: 600,
            hard_stop_loss_pct: hard_stop,
            ..SniperSettings::default()
        }
    }

    fn round_trip(outcome: PaperOutcome) -> (Decimal, Decimal, Decimal, Decimal, String) {
        match outcome {
            PaperOutcome::RoundTripComplete {
                exit_price,
                gross_pnl_pct,
                net_pnl_pct,
                pnl_usdt,
                exit_reason,
                ..
            } => (
                exit_price,
                gross_pnl_pct,
                net_pnl_pct,
                pnl_usdt,
                exit_reason,
            ),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn paper_flat_records_entry_and_exit() {
        let server = price_server(&["1.0", "1.10"]).await;
        let s = paper(HoldMode::Flat, 0, 0, dec!(-25));
        let out = fire_paper(&s, &client(&server), "FOOUSDT").await;
        let json = serde_json::to_value(FireResult::Paper(out.clone())).unwrap();
        assert_eq!(json["mode"], "paper");
        assert_eq!(json["action"], "round_trip_complete");
        assert_eq!(json["hold_mode"], "flat");
        assert_eq!(json["entry_price"], serde_json::json!(1.0));
        let (exit, gross, _, pnl, reason) = round_trip(out);
        assert_eq!((exit, gross, pnl), (dec!(1.10), dec!(10), dec!(0.98)));
        assert_eq!(reason, "flat_hold_elapsed");
    }

    #[tokio::test]
    async fn paper_skips_without_entry_price() {
        let server = price_server(&[]).await;
        let out = fire_paper(
            &paper(HoldMode::Flat, 0, 0, dec!(-25)),
            &client(&server),
            "FOOUSDT",
        )
        .await;
        assert!(matches!(out, PaperOutcome::BuySkippedNoPrice { .. }));
    }

    #[tokio::test]
    async fn hard_stop_fires_inside_floor_for_flat_and_momentum() {
        let server = price_server(&["1.0", "0.70"]).await;
        let (_, gross, net, _, reason) = round_trip(
            fire_paper(
                &paper(HoldMode::Flat, 30, 0, dec!(-25)),
                &client(&server),
                "RUGUSDT",
            )
            .await,
        );
        assert!(reason.contains("hard_stop_loss_hit"), "{reason}");
        assert_eq!(
            reason,
            "hard_stop_loss_hit: last=0.70000000 <= stop_price=0.75000000 (-25.0% from entry)"
        );
        assert_eq!((gross, net), (dec!(-30), dec!(-30.2)));

        let server = price_server(&["1.0", "0.50"]).await;
        let (_, _, _, _, reason) = round_trip(
            fire_paper(
                &paper(HoldMode::Momentum, 30, 0, dec!(-25)),
                &client(&server),
                "RUGUSDT",
            )
            .await,
        );
        assert!(reason.contains("hard_stop_loss_hit"), "{reason}");
    }

    #[tokio::test]
    async fn trailing_exits_on_retrace_and_max_hold_uses_last_price() {
        let server = price_server(&["1.0", "1.5", "2.0", "1.6"]).await;
        let s = paper(HoldMode::Trailing, 0, 0, dec!(-25));
        let (exit, _, _, _, reason) = round_trip(fire_paper(&s, &client(&server), "FOOUSDT").await);
        assert_eq!(exit, dec!(1.6));
        assert_eq!(
            reason,
            "trailing_stop_hit: last=1.60000000 <= peak*0.8500=1.70000000"
        );

        let server = price_server(&["1.0", "1.1"]).await;
        let s = SniperSettings {
            trailing_poll_interval_seconds: 1,
            max_hold_seconds: 2,
            ..s
        };
        let out = fire_paper(&s, &client(&server), "FOOUSDT").await;
        let PaperOutcome::RoundTripComplete {
            exit_price,
            exit_reason,
            elapsed_seconds,
            ..
        } = out
        else {
            panic!("{out:?}")
        };
        assert_eq!(
            (exit_price, exit_reason.as_str(), elapsed_seconds),
            (dec!(1.1), "max_hold_seconds_reached", 2.0)
        );
    }

    async fn mount_time(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/v3/time"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"serverTime": bb_core::time::now_ms()})),
            )
            .mount(server)
            .await;
    }

    async fn mount_exchange_info(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/v3/exchangeInfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"symbols": [
                {"symbol": "TSLABUSDT", "status": "TRADING", "baseAsset": "TSLAB", "quoteAsset": "USDT"}]})))
            .mount(server)
            .await;
    }

    fn invalid_symbol() -> ResponseTemplate {
        ResponseTemplate::new(400)
            .set_body_json(serde_json::json!({"code": -1121, "msg": "Invalid symbol."}))
    }

    fn zero_fill() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "symbol": "TSLABUSDT", "orderId": 1, "status": "EXPIRED", "executedQty": "0", "cummulativeQuoteQty": "0"}))
    }

    async fn order_calls(server: &MockServer) -> (usize, usize) {
        let reqs = server.received_requests().await.unwrap();
        let orders = reqs
            .iter()
            .filter(|r| r.url.path() == "/api/v3/order")
            .count();
        let infos = reqs
            .iter()
            .filter(|r| r.url.path() == "/api/v3/exchangeInfo")
            .count();
        (orders, infos)
    }

    fn live(mode: HoldMode) -> SniperSettings {
        SniperSettings {
            hold_mode: mode,
            hold_seconds: 0,
            trailing_poll_interval_seconds: 0,
            max_hold_seconds: 0,
            ..SniperSettings::default()
        }
    }

    #[tokio::test]
    async fn live_buy_refreshes_exchange_info_and_retries_once_on_unknown_symbol() {
        let server = MockServer::start().await;
        mount_time(&server).await;
        mount_exchange_info(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/v3/order"))
            .respond_with(invalid_symbol())
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v3/order"))
            .respond_with(zero_fill())
            .mount(&server)
            .await;
        let out = fire_live(&live(HoldMode::Flat), &client(&server), "TSLABUSDT").await;
        assert_eq!(order_calls(&server).await, (2, 1));
        let LiveOutcome::BuyZeroFill { symbol, .. } = out else {
            panic!("{out:?}")
        };
        assert_eq!(symbol, "TSLAB/USDT");
    }

    #[tokio::test]
    async fn live_buy_does_not_retry_other_errors() {
        let server = MockServer::start().await;
        mount_time(&server).await;
        mount_exchange_info(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/v3/order"))
            .respond_with(ResponseTemplate::new(400).set_body_json(
                serde_json::json!({"code": -1013, "msg": "Filter failure: NOTIONAL"}),
            ))
            .mount(&server)
            .await;
        let out = fire_live(&live(HoldMode::Flat), &client(&server), "MUBUSDT").await;
        assert_eq!(order_calls(&server).await, (1, 0));
        let LiveOutcome::BuyFailed { reason, .. } = out else {
            panic!("{out:?}")
        };
        assert!(reason.contains("NOTIONAL"), "{reason}");
    }

    #[tokio::test]
    async fn live_buy_reports_post_reload_failure_distinctly() {
        let server = MockServer::start().await;
        mount_time(&server).await;
        mount_exchange_info(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/v3/order"))
            .respond_with(invalid_symbol())
            .mount(&server)
            .await;
        let out = fire_live(&live(HoldMode::Flat), &client(&server), "GHOSTUSDT").await;
        assert_eq!(order_calls(&server).await, (2, 1));
        let LiveOutcome::BuyFailed { reason, .. } = out else {
            panic!("{out:?}")
        };
        assert!(reason.starts_with("after_market_reload:"), "{reason}");
    }

    #[tokio::test]
    async fn live_round_trip_sells_net_quantity_rounded_to_step() {
        let server = MockServer::start().await;
        mount_time(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v3/exchangeInfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"symbols": [{
                "symbol": "FOOUSDT", "status": "TRADING", "baseAsset": "FOO", "quoteAsset": "USDT",
                "filters": [{"filterType": "LOT_SIZE", "minQty": "0.01", "maxQty": "1000000", "stepSize": "0.01"}]}]})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v3/order"))
            .and(query_param("side", "BUY"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "symbol": "FOOUSDT", "orderId": 7, "status": "FILLED", "executedQty": "5", "cummulativeQuoteQty": "5",
                "fills": [{"price": "1", "qty": "5", "commission": "0.005", "commissionAsset": "FOO"}]})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v3/order"))
            .and(query_param("side", "SELL"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "symbol": "FOOUSDT", "orderId": 8, "status": "FILLED", "executedQty": "4.99", "cummulativeQuoteQty": "5.489"})))
            .mount(&server)
            .await;
        let out = fire_live(&live(HoldMode::Flat), &client(&server), "FOOUSDT").await;
        let LiveOutcome::RoundTripComplete {
            symbol,
            sold_qty,
            exit_reason,
            buy_order_id,
            sell_order_id,
            ..
        } = &out
        else {
            panic!("{out:?}")
        };
        assert_eq!(symbol, "FOO/USDT");
        assert_eq!(
            (*sold_qty, exit_reason.as_str()),
            (dec!(4.99), "max_hold_seconds_reached")
        );
        assert_eq!((buy_order_id.as_str(), sell_order_id.as_str()), ("7", "8"));
        let sells: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter_map(|r| r.url.query().map(str::to_owned))
            .filter(|q| q.contains("side=SELL"))
            .collect();
        assert!(sells[0].contains("quantity=4.99"), "{}", sells[0]);
    }

    #[test]
    fn alert_texts() {
        let win = LiveOutcome::RoundTripComplete {
            symbol: "FOO/USDT".into(),
            filled_qty: dec!(5),
            avg_buy: dec!(1.0),
            avg_sell: dec!(1.10),
            sold_qty: dec!(5),
            pnl_quote: dec!(0.50),
            hold_mode: HoldMode::Momentum,
            elapsed_seconds: 10.0,
            exit_reason: "trailing_stop_hit".into(),
            buy_order_id: "1".into(),
            sell_order_id: "2".into(),
        };
        assert_eq!(
            format_live_fire_alert(&win, HoldMode::Momentum),
            "💰 [LIVE momentum] FOO/USDT closed: $1.000000 → $1.100000 (+10.00% / +0.5000 USDT) — trailing_stop_hit"
        );
        let failed = LiveOutcome::BuyFailed {
            symbol: "TSLAB/USDT".into(),
            reason: "BadRequest: market not found".into(),
        };
        let msg = format_live_fire_alert(&failed, HoldMode::Momentum);
        assert!(
            msg.contains("❌") && msg.contains("TSLAB/USDT") && msg.contains("market not found"),
            "{msg}"
        );
        let stranded = LiveOutcome::SellFailed {
            symbol: "X/USDT".into(),
            buy_order: Box::default(),
            reason: "network timeout".into(),
            elapsed_seconds: 0.0,
            exit_reason: String::new(),
        };
        let msg = format_live_fire_alert(&stranded, HoldMode::Trailing);
        assert!(msg.contains("⚠️") && msg.contains("STRANDED"), "{msg}");
    }

    #[test]
    fn unified_pair_strips_quote_suffix() {
        assert_eq!(unified_pair("TSLABUSDT", "USDT"), "TSLAB/USDT");
    }
}
