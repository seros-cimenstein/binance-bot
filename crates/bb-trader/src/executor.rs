//! Live route execution — the hard-safety core. Per route: arming, leg count, scope, stop
//! file, account READING + canTrade. Per leg: stop file, market eligibility, live free
//! balance, fee/slippage reserve sizing to exchange filters, Binance `order/test`, stop file
//! again, MARKET order, terminal-status poll, balance reconciliation. Any ambiguity halts.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use bb_binance::spot::{AccountInfo, NewOrder, Order, OrderRef};
use bb_binance::{BinanceClient, BinanceError};
use bb_core::money::{BPS_PER_UNIT, floor_to_step};
use bb_pool::{CommissionService, LegSide, MarketSnapshot, RouteQuote};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::TraderConfig;
use crate::context::decimal_json;

/// The only order-capable exchange surface the trader uses (Spot MARKET orders).
#[async_trait]
pub trait SpotExchange: Send + Sync {
    /// `GET /api/v3/account`.
    async fn account(&self) -> Result<AccountInfo, BinanceError>;
    /// `POST /api/v3/order/test`.
    async fn test_order(&self, order: &NewOrder) -> Result<(), BinanceError>;
    /// `POST /api/v3/order` (LIVE).
    async fn create_order(&self, order: &NewOrder) -> Result<Order, BinanceError>;
    /// `GET /api/v3/order`.
    async fn query_order(&self, symbol: &str, order_id: i64) -> Result<Order, BinanceError>;
}

#[async_trait]
impl SpotExchange for BinanceClient {
    async fn account(&self) -> Result<AccountInfo, BinanceError> {
        BinanceClient::account(self).await
    }

    async fn test_order(&self, order: &NewOrder) -> Result<(), BinanceError> {
        BinanceClient::test_order(self, order).await
    }

    async fn create_order(&self, order: &NewOrder) -> Result<Order, BinanceError> {
        BinanceClient::create_order(self, order).await
    }

    async fn query_order(&self, symbol: &str, order_id: i64) -> Result<Order, BinanceError> {
        BinanceClient::query_order(self, symbol, &OrderRef::Id(order_id)).await
    }
}

/// Auditable result of one submitted leg (Python `BotSpotLegResult`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LegResult {
    pub symbol: String,
    pub side: String,
    pub source_asset: String,
    pub destination_asset: String,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub order_amount: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub source_free_before: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub destination_free_before: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub source_free_after: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub destination_free_after: Decimal,
    pub order: Value,
    pub final_order: Value,
    /// ccxt-style status of the final order (`closed` when filled).
    pub status: String,
}

/// What a completed route delivered: its last leg's destination asset and the quantity that
/// arrived (destination free after − before), net of any fee Binance took in that asset.
/// Sleeves book this, never the quoted or ordered amount.
pub fn arrival(legs: &[LegResult]) -> Option<(String, Decimal)> {
    legs.last().map(|leg| {
        (
            leg.destination_asset.clone(),
            leg.destination_free_after - leg.destination_free_before,
        )
    })
}

/// A route stopped. `legs` holds every leg that reached the exchange.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionFailure {
    pub message: String,
    pub legs: Vec<LegResult>,
    /// True when an order may exist on the exchange (any submitted leg, or an ambiguous
    /// submission failure). Drives the failed-route halt.
    pub submission_attempted: bool,
}

impl ExecutionFailure {
    fn before_orders(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            legs: Vec::new(),
            submission_attempted: false,
        }
    }
}

/// Python `_buy_quote_reserve_bps`: `max(taker*1e4 + slippage + rounding, floor)`.
pub fn buy_quote_reserve_bps(
    taker_fee_rate: Decimal,
    config: &TraderConfig,
) -> Result<Decimal, String> {
    if taker_fee_rate < Decimal::ZERO {
        return Err("Bot Spot dynamic fee rate is invalid".into());
    }
    let dynamic = taker_fee_rate * BPS_PER_UNIT
        + config.slippage_buffer_bps
        + config.leg_rounding_reserve_bps;
    Ok(dynamic.max(config.min_leg_quote_reserve_bps))
}

/// Python `_leg_order_amount`: base quantity for a MARKET order spending `input` of the
/// leg's source asset, rounded down to the lot step and checked against exchange minimums.
pub fn leg_order_amount(
    market: &MarketSnapshot,
    side: LegSide,
    input: Decimal,
    slippage_bps: Decimal,
) -> Result<Decimal, String> {
    let slip = slippage_bps / BPS_PER_UNIT;
    let round = |value: Decimal| {
        market
            .order_step()
            .map_or(value, |step| floor_to_step(value, step))
    };
    let (amount, notional) = match side {
        LegSide::Sell => {
            let amount = round(input);
            (amount, amount * market.bid * (Decimal::ONE - slip))
        }
        LegSide::Buy => {
            let price = market.ask * (Decimal::ONE + slip);
            if price <= Decimal::ZERO {
                return Err(format!("{} has no usable ask price", market.symbol));
            }
            let amount = round(input / price);
            (amount, amount * price)
        }
    };
    if amount <= Decimal::ZERO {
        return Err("Bot Spot leg order amount rounded to zero".into());
    }
    if amount < market.min_amount || notional < market.min_notional {
        return Err("Bot Spot leg is below Binance minimum amount or notional".into());
    }
    Ok(amount)
}

/// Python `_live_leg_order_amount`: buys reserve the dynamic taker fee + slippage + rounding.
pub async fn live_leg_order_amount(
    fees: &CommissionService,
    market: &MarketSnapshot,
    side: LegSide,
    input: Decimal,
    config: &TraderConfig,
) -> Result<Decimal, String> {
    let mut spendable = input;
    if side == LegSide::Buy {
        let commission = fees
            .get(&market.symbol)
            .await
            .map_err(|error| format!("commission lookup failed for {}: {error}", market.symbol))?;
        let reserve = buy_quote_reserve_bps(commission.taker_rate(side, false), config)?;
        if reserve >= BPS_PER_UNIT {
            return Err("Bot Spot buy reserve consumes full quote balance".into());
        }
        spendable = input * (Decimal::ONE - reserve / BPS_PER_UNIT);
    }
    leg_order_amount(market, side, spendable, config.slippage_buffer_bps)
}

/// Hard safety: the key can read the account and the account can trade.
pub async fn ensure_trading_allowed(exchange: &dyn SpotExchange) -> Result<(), String> {
    let account = exchange
        .account()
        .await
        .map_err(|error| format!("account reading is not allowed: {error}"))?;
    if !account.can_trade {
        return Err("Binance account canTrade is not true".into());
    }
    Ok(())
}

/// ccxt-shaped order summary (Python `_sanitize_order` field set).
pub fn sanitize_order(order: &Order) -> Value {
    let mut fees: BTreeMap<String, Decimal> = BTreeMap::new();
    for fill in &order.fills {
        *fees.entry(fill.commission_asset.clone()).or_default() += fill.commission;
    }
    let timestamp = order.transact_time.or(order.update_time).or(order.time);
    json!({
        "id": order.order_id.to_string(),
        "clientOrderId": order.client_order_id,
        "symbol": order.symbol,
        "type": order.order_type.to_lowercase(),
        "side": order.side.to_lowercase(),
        "status": order.status.ccxt(),
        "amount": decimal_json(order.orig_qty),
        "filled": decimal_json(order.executed_qty),
        "remaining": decimal_json(order.orig_qty - order.executed_qty),
        "average": order.average_price().map_or(Value::Null, decimal_json),
        "price": order.average_price().map_or_else(|| decimal_json(order.price), decimal_json),
        "cost": decimal_json(order.cummulative_quote_qty),
        "fees": fees.iter().map(|(asset, cost)| json!({"currency": asset, "cost": decimal_json(*cost)})).collect::<Vec<_>>(),
        "timestamp": timestamp,
        "datetime": timestamp.and_then(bb_core::time::from_ms).map(bb_core::time::iso),
    })
}

/// A definitive exchange rejection (4xx with a Binance error body): no order exists.
fn definitive_rejection(error: &BinanceError) -> bool {
    matches!(error, BinanceError::Api { http_status, .. } if (400..500).contains(http_status))
        || matches!(
            error,
            BinanceError::InvalidRequest(_) | BinanceError::InvalidSymbol(_)
        )
}

/// Executes routes against a [`SpotExchange`].
pub struct Executor<'a> {
    pub exchange: &'a dyn SpotExchange,
    pub fees: &'a CommissionService,
    pub config: &'a TraderConfig,
    /// Delay between final-status polls (Python: 1 s, 3 polls).
    pub poll_interval: Duration,
}

impl Executor<'_> {
    fn stopped(&self) -> bool {
        bb_core::fsutil::stop_requested(&self.config.stop_path)
    }

    /// Run every leg of `route` serially. `spend_amount` caps the first leg's input (the
    /// whole free balance otherwise); later legs spend only what the previous leg delivered.
    pub async fn execute(
        &self,
        route: &RouteQuote,
        markets: &BTreeMap<String, MarketSnapshot>,
        spend_amount: Option<Decimal>,
    ) -> Result<Vec<LegResult>, ExecutionFailure> {
        self.config
            .check_live_arming()
            .map_err(|error| ExecutionFailure::before_orders(error.to_string()))?;
        if route.legs.is_empty() {
            return Err(ExecutionFailure::before_orders(
                "selected route has no executable legs",
            ));
        }
        if route.legs.len() > self.config.max_route_legs {
            return Err(ExecutionFailure::before_orders(
                "selected route exceeds configured max_route_legs",
            ));
        }
        if let Some(reason) = self.config.scope.route_block_reason(route) {
            return Err(ExecutionFailure::before_orders(reason));
        }
        if self.stopped() {
            return Err(ExecutionFailure::before_orders(format!(
                "Bot Spot emergency stop is active: {}",
                self.config.stop_path.display()
            )));
        }
        ensure_trading_allowed(self.exchange)
            .await
            .map_err(ExecutionFailure::before_orders)?;

        let mut legs: Vec<LegResult> = Vec::new();
        for (index, leg) in route.legs.iter().enumerate() {
            let fail = |legs: &Vec<LegResult>, message: String| ExecutionFailure {
                message,
                submission_attempted: !legs.is_empty(),
                legs: legs.clone(),
            };
            if self.stopped() {
                return Err(fail(
                    &legs,
                    format!(
                        "Bot Spot emergency stop is active: {}; route halted before {}",
                        self.config.stop_path.display(),
                        leg.symbol
                    ),
                ));
            }
            let Some(market) = markets.get(&leg.symbol).filter(|m| m.eligible) else {
                return Err(fail(
                    &legs,
                    format!("selected route leg is no longer eligible: {}", leg.symbol),
                ));
            };
            let before = match self.exchange.account().await {
                Ok(account) => account,
                Err(error) => {
                    return Err(fail(
                        &legs,
                        format!("balance read failed before {}: {error}", leg.symbol),
                    ));
                }
            };
            let source_before = before.balance(&leg.source_asset).free;
            let destination_before = before.balance(&leg.destination_asset).free;
            if source_before <= Decimal::ZERO {
                return Err(fail(
                    &legs,
                    format!("no free {} balance for route leg", leg.source_asset),
                ));
            }
            let input = match (index, legs.last()) {
                (0, _) => match spend_amount {
                    Some(spend) if spend > source_before => {
                        return Err(fail(
                            &legs,
                            format!(
                                "spend_amount {spend} exceeds free {} balance {source_before}",
                                leg.source_asset
                            ),
                        ));
                    }
                    Some(spend) => spend,
                    None => source_before,
                },
                (_, Some(previous)) => {
                    let received =
                        previous.destination_free_after - previous.destination_free_before;
                    received.min(source_before)
                }
                (_, None) => source_before,
            };
            let order_amount = match live_leg_order_amount(
                self.fees,
                market,
                leg.side,
                input,
                self.config,
            )
            .await
            {
                Ok(amount) => amount,
                Err(message) => {
                    return Err(fail(
                        &legs,
                        format!("Bot Spot preflight failed for {}: {message}", leg.symbol),
                    ));
                }
            };
            let order = NewOrder::market(&leg.symbol, leg.side.order_side(), order_amount);
            if let Err(error) = self.exchange.test_order(&order).await {
                return Err(fail(
                    &legs,
                    format!(
                        "Bot Spot preflight failed for {}; route halted: order test rejected: {error}",
                        leg.symbol
                    ),
                ));
            }
            if self.stopped() {
                return Err(fail(
                    &legs,
                    format!(
                        "Bot Spot emergency stop is active: {}; route halted before {}",
                        self.config.stop_path.display(),
                        leg.symbol
                    ),
                ));
            }
            let placed = match self.exchange.create_order(&order).await {
                Ok(placed) => placed,
                Err(error) => {
                    return Err(ExecutionFailure {
                        message: format!(
                            "Bot Spot order failed for {}; route halted: {error}",
                            leg.symbol
                        ),
                        submission_attempted: !legs.is_empty() || !definitive_rejection(&error),
                        legs,
                    });
                }
            };
            let (final_order, poll_error) = self.final_order(&leg.symbol, &placed).await;
            let status = poll_error.as_ref().map_or_else(
                || final_order.status.ccxt().to_owned(),
                |_| "unknown".to_owned(),
            );
            let after = self.exchange.account().await;
            let (source_after, destination_after) = match &after {
                Ok(account) => (
                    account.balance(&leg.source_asset).free,
                    account.balance(&leg.destination_asset).free,
                ),
                Err(_) => (source_before, destination_before),
            };
            legs.push(LegResult {
                symbol: leg.symbol.clone(),
                side: leg.side.as_str().into(),
                source_asset: leg.source_asset.clone(),
                destination_asset: leg.destination_asset.clone(),
                order_amount,
                source_free_before: source_before,
                destination_free_before: destination_before,
                source_free_after: source_after,
                destination_free_after: destination_after,
                order: sanitize_order(&placed),
                final_order: sanitize_order(&final_order),
                status: status.clone(),
            });
            if let Some(error) = poll_error {
                return Err(fail(
                    &legs,
                    format!(
                        "Bot Spot route halted after {}: order status unknown: {error}",
                        leg.symbol
                    ),
                ));
            }
            if let Err(error) = after {
                return Err(fail(
                    &legs,
                    format!(
                        "Bot Spot route halted after {}: balance read failed: {error}",
                        leg.symbol
                    ),
                ));
            }
            if status != "closed" && status != "filled" {
                return Err(fail(
                    &legs,
                    format!("Bot Spot route halted after {}: {status}", leg.symbol),
                ));
            }
            if destination_after <= destination_before {
                return Err(fail(
                    &legs,
                    format!(
                        "Bot Spot route did not increase {} after {}",
                        leg.destination_asset, leg.symbol
                    ),
                ));
            }
        }
        Ok(legs)
    }

    /// Poll a non-terminal order up to three times.
    async fn final_order(&self, symbol: &str, placed: &Order) -> (Order, Option<BinanceError>) {
        if placed.status.is_terminal() || placed.order_id <= 0 {
            return (placed.clone(), None);
        }
        let mut latest = placed.clone();
        for _ in 0..3 {
            tokio::time::sleep(self.poll_interval).await;
            match self.exchange.query_order(symbol, placed.order_id).await {
                Ok(order) => {
                    latest = order;
                    if latest.status.is_terminal() {
                        break;
                    }
                }
                Err(error) => return (latest, Some(error)),
            }
        }
        (latest, None)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;

    use parking_lot::Mutex;

    use bb_binance::spot::{Balance, OrderStatus};
    use rust_decimal_macros::dec;

    use super::*;
    use crate::scope::tests::{leg, route};
    use crate::universe::tests::{config, fees, market};

    /// In-memory exchange: fills market orders at fixed prices and moves balances.
    pub(crate) struct FakeExchange {
        pub balances: Mutex<BTreeMap<String, Decimal>>,
        pub prices: BTreeMap<String, Decimal>,
        pub orders: Mutex<Vec<NewOrder>>,
        pub tests: Mutex<Vec<NewOrder>>,
        pub status: OrderStatus,
        pub fail_order_on_call: Option<(usize, BinanceError)>,
        pub stop_file_after_first_order: Option<PathBuf>,
        pub can_trade: bool,
        /// Commission Binance takes from the bought asset on a buy (fraction of the fill).
        pub buy_fee_in_base: Decimal,
    }

    impl FakeExchange {
        pub(crate) fn new(balances: &[(&str, Decimal)], prices: &[(&str, Decimal)]) -> Self {
            Self {
                balances: Mutex::new(
                    balances
                        .iter()
                        .map(|(a, v)| ((*a).to_owned(), *v))
                        .collect(),
                ),
                prices: prices.iter().map(|(s, p)| ((*s).to_owned(), *p)).collect(),
                orders: Mutex::new(Vec::new()),
                tests: Mutex::new(Vec::new()),
                status: OrderStatus::Filled,
                fail_order_on_call: None,
                stop_file_after_first_order: None,
                can_trade: true,
                buy_fee_in_base: Decimal::ZERO,
            }
        }
    }

    #[async_trait]
    impl SpotExchange for FakeExchange {
        async fn account(&self) -> Result<AccountInfo, BinanceError> {
            Ok(AccountInfo {
                can_trade: self.can_trade,
                balances: self
                    .balances
                    .lock()
                    .iter()
                    .map(|(asset, free)| Balance {
                        asset: asset.clone(),
                        free: *free,
                        locked: Decimal::ZERO,
                    })
                    .collect(),
                ..AccountInfo::default()
            })
        }

        async fn test_order(&self, order: &NewOrder) -> Result<(), BinanceError> {
            self.tests.lock().push(order.clone());
            Ok(())
        }

        async fn create_order(&self, order: &NewOrder) -> Result<Order, BinanceError> {
            let call = {
                let mut orders = self.orders.lock();
                orders.push(order.clone());
                orders.len()
            };
            if let Some((fail_call, error)) = &self.fail_order_on_call
                && *fail_call == call
            {
                return Err(match error {
                    BinanceError::Api {
                        endpoint,
                        http_status,
                        code,
                        msg,
                    } => BinanceError::Api {
                        endpoint: endpoint.clone(),
                        http_status: *http_status,
                        code: *code,
                        msg: msg.clone(),
                    },
                    other => BinanceError::Network {
                        endpoint: "POST /api/v3/order".into(),
                        message: other.to_string(),
                        timeout: true,
                    },
                });
            }
            if let Some(path) = &self.stop_file_after_first_order {
                std::fs::write(path, "stop").unwrap();
            }
            let qty = order.quantity.unwrap();
            let (base, quote) = order.symbol.split_once('/').unwrap();
            let price = self.prices[&order.symbol];
            if self.status == OrderStatus::Filled {
                let mut balances = self.balances.lock();
                let (spend_asset, spend, get_asset, get) = match order.side {
                    bb_binance::Side::Buy => (quote, qty * price, base, qty * (Decimal::ONE - self.buy_fee_in_base)),
                    bb_binance::Side::Sell => (base, qty, quote, qty * price),
                };
                *balances.entry(spend_asset.to_owned()).or_default() -= spend;
                *balances.entry(get_asset.to_owned()).or_default() += get;
            }
            Ok(Order {
                symbol: order.symbol.replace('/', ""),
                order_id: i64::try_from(call).unwrap(),
                status: self.status,
                orig_qty: qty,
                executed_qty: if self.status == OrderStatus::Filled {
                    qty
                } else {
                    Decimal::ZERO
                },
                order_type: "MARKET".into(),
                side: order.side.as_str().into(),
                ..Order::default()
            })
        }

        async fn query_order(&self, _symbol: &str, order_id: i64) -> Result<Order, BinanceError> {
            Ok(Order {
                order_id,
                status: self.status,
                ..Order::default()
            })
        }
    }

    fn two_leg_route() -> RouteQuote {
        route(
            "USDT",
            "SOL",
            vec![
                leg("ETH/USDT", LegSide::Buy, "USDT", "ETH"),
                leg("SOL/ETH", LegSide::Buy, "ETH", "SOL"),
            ],
        )
    }

    fn markets() -> BTreeMap<String, MarketSnapshot> {
        [
            market("ETH/USDT", dec!(2000), dec!(2000), dec!(5000000)),
            market("SOL/ETH", dec!(0.05), dec!(0.05), dec!(5000000)),
        ]
        .into_iter()
        .map(|m| (m.symbol.clone(), m))
        .collect()
    }

    fn exchange() -> FakeExchange {
        FakeExchange::new(
            &[("USDT", dec!(100)), ("ETH", dec!(1))],
            &[("ETH/USDT", dec!(2000)), ("SOL/ETH", dec!(0.05))],
        )
    }

    fn armed(stop: &std::path::Path) -> TraderConfig {
        let stop = stop.to_string_lossy().into_owned();
        let mut config = config(&[
            ("BOT_SPOT_ENABLED", "true"),
            ("BOT_SPOT_DYNAMIC_SCOPE", "all_eligible_spot"),
            ("USE_TESTNET", "false"),
            ("DRY_RUN", "false"),
            ("CONFIRM_LIVE_TRADING", "yes"),
        ]);
        config.stop_path = PathBuf::from(stop);
        config
    }

    async fn run(
        exchange: &FakeExchange,
        config: &TraderConfig,
        spend: Option<Decimal>,
    ) -> Result<Vec<LegResult>, ExecutionFailure> {
        let fees = fees(&["ETH/USDT", "SOL/ETH"]);
        Executor {
            exchange,
            fees: &fees,
            config,
            poll_interval: Duration::ZERO,
        }
        .execute(&two_leg_route(), &markets(), spend)
        .await
    }

    #[test]
    fn buy_quote_reserve_uses_dynamic_fee_and_floor() {
        let config = config(&[]);
        assert_eq!(
            buy_quote_reserve_bps(dec!(0.001), &config).unwrap(),
            dec!(40)
        );
        assert_eq!(buy_quote_reserve_bps(dec!(0), &config).unwrap(), dec!(30));
        let floor =
            crate::universe::tests::config(&[("BOT_SPOT_MIN_LEG_QUOTE_RESERVE_BPS", "75")]);
        assert_eq!(
            buy_quote_reserve_bps(dec!(0.001), &floor).unwrap(),
            dec!(75)
        );
        assert!(buy_quote_reserve_bps(dec!(-0.1), &config).is_err());
    }

    #[test]
    fn order_amount_uses_lot_step_over_precision_and_minimums() {
        let mut hbar = market("HBAR/BNB", dec!(0.00012), dec!(0.00012), dec!(1000000));
        hbar.amount_step = Some(dec!(1));
        hbar.amount_precision = Some(dec!(0.01));
        hbar.min_amount = dec!(1);
        hbar.min_notional = dec!(0.001);
        assert_eq!(
            leg_order_amount(&hbar, LegSide::Sell, dec!(123.99), dec!(0)).unwrap(),
            dec!(123)
        );
        let buy = leg_order_amount(&hbar, LegSide::Buy, dec!(0.0123), dec!(25)).unwrap();
        assert_eq!(buy, dec!(102));
        assert!(leg_order_amount(&hbar, LegSide::Sell, dec!(0.5), dec!(0)).is_err());
        assert!(
            leg_order_amount(&hbar, LegSide::Sell, dec!(5), dec!(0)).is_err(),
            "below min notional"
        );
    }

    #[tokio::test]
    async fn executes_full_multi_hop_route_serially_with_reserve() {
        let dir = tempfile::tempdir().unwrap();
        let exchange = exchange();
        let config = armed(&dir.path().join("stop"));
        let legs = run(&exchange, &config, None).await.unwrap();
        assert_eq!(legs.len(), 2);
        let orders = exchange.orders.lock().clone();
        assert_eq!(
            exchange.tests.lock().len(),
            2,
            "order/test before every leg"
        );
        // Leg 1: 100 USDT * (1 - 40 bps) / (2000 * (1 + 25 bps)) = 0.049675 -> lot 0.0001.
        assert_eq!(orders[0].quantity, Some(dec!(0.0496)));
        // Leg 2 spends only the ETH received by leg 1, not the pre-existing 1 ETH.
        let received = legs[0].destination_free_after - legs[0].destination_free_before;
        assert_eq!(received, dec!(0.0496));
        assert!(orders[1].quantity.unwrap() * dec!(0.05) <= received);
        assert!(legs.iter().all(|leg| leg.status == "closed"));
        assert_eq!(legs[0].order["status"], "closed");
    }

    #[tokio::test]
    async fn stop_file_created_between_legs_halts_before_leg_two() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("stop");
        let mut exchange = exchange();
        exchange.stop_file_after_first_order = Some(stop.clone());
        let failure = run(&exchange, &armed(&stop), None).await.unwrap_err();
        assert!(
            failure.message.contains("emergency stop"),
            "{}",
            failure.message
        );
        assert_eq!(failure.legs.len(), 1);
        assert!(failure.submission_attempted);
        assert_eq!(exchange.orders.lock().len(), 1);
        assert_eq!(
            exchange.tests.lock().len(),
            1,
            "leg 2 never reached order/test"
        );
    }

    #[tokio::test]
    async fn stop_file_before_route_places_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("stop");
        std::fs::write(&stop, "").unwrap();
        let exchange = exchange();
        let failure = run(&exchange, &armed(&stop), None).await.unwrap_err();
        assert!(!failure.submission_attempted);
        assert!(exchange.orders.lock().is_empty());
    }

    #[tokio::test]
    async fn unclosed_leg_halts_after_polling() {
        let dir = tempfile::tempdir().unwrap();
        let mut exchange = exchange();
        exchange.status = OrderStatus::New;
        let failure = run(&exchange, &armed(&dir.path().join("stop")), None)
            .await
            .unwrap_err();
        assert!(
            failure
                .message
                .contains("route halted after ETH/USDT: open"),
            "{}",
            failure.message
        );
        assert_eq!(failure.legs.len(), 1);
        assert!(failure.submission_attempted);
    }

    #[tokio::test]
    async fn second_leg_failure_preserves_first_leg() {
        let dir = tempfile::tempdir().unwrap();
        let mut exchange = exchange();
        exchange.fail_order_on_call = Some((
            2,
            BinanceError::Api {
                endpoint: "POST /api/v3/order".into(),
                http_status: 400,
                code: Some(-2010),
                msg: "insufficient balance".into(),
            },
        ));
        let failure = run(&exchange, &armed(&dir.path().join("stop")), None)
            .await
            .unwrap_err();
        assert!(
            failure.message.contains("order failed for SOL/ETH"),
            "{}",
            failure.message
        );
        assert_eq!(failure.legs.len(), 1);
        assert!(failure.submission_attempted);
    }

    #[tokio::test]
    async fn ambiguous_first_order_failure_counts_as_attempted() {
        let dir = tempfile::tempdir().unwrap();
        let mut exchange = exchange();
        exchange.fail_order_on_call = Some((
            1,
            BinanceError::Network {
                endpoint: "POST /api/v3/order".into(),
                message: "timeout".into(),
                timeout: true,
            },
        ));
        let failure = run(&exchange, &armed(&dir.path().join("stop")), None)
            .await
            .unwrap_err();
        assert!(failure.legs.is_empty());
        assert!(
            failure.submission_attempted,
            "a timed-out order may exist: halt"
        );

        let mut rejected = super::tests::exchange();
        rejected.fail_order_on_call = Some((
            1,
            BinanceError::Api {
                endpoint: "POST /api/v3/order".into(),
                http_status: 400,
                code: Some(-1013),
                msg: "filter failure".into(),
            },
        ));
        let failure = run(&rejected, &armed(&dir.path().join("stop")), None)
            .await
            .unwrap_err();
        assert!(
            !failure.submission_attempted,
            "a definitive rejection placed nothing"
        );
    }

    #[tokio::test]
    async fn refuses_without_arming_trading_permission_or_enough_balance() {
        let dir = tempfile::tempdir().unwrap();
        let exchange = exchange();
        let unarmed = config(&[]);
        assert!(
            run(&exchange, &unarmed, None)
                .await
                .unwrap_err()
                .message
                .contains("BOT_SPOT_ENABLED")
        );
        let mut locked = super::tests::exchange();
        locked.can_trade = false;
        let failure = run(&locked, &armed(&dir.path().join("stop")), None)
            .await
            .unwrap_err();
        assert!(failure.message.contains("canTrade"));
        let failure = run(&exchange, &armed(&dir.path().join("stop")), Some(dec!(500)))
            .await
            .unwrap_err();
        assert!(
            failure.message.contains("exceeds free USDT"),
            "{}",
            failure.message
        );
        assert!(exchange.orders.lock().is_empty());
    }

    #[tokio::test]
    async fn partial_spend_sizes_first_leg_from_spend_amount() {
        let dir = tempfile::tempdir().unwrap();
        let exchange = exchange();
        run(&exchange, &armed(&dir.path().join("stop")), Some(dec!(50)))
            .await
            .unwrap();
        assert_eq!(exchange.orders.lock()[0].quantity, Some(dec!(0.0248)));
    }

    #[tokio::test]
    async fn the_arrival_is_the_fill_net_of_a_base_asset_fee() {
        let dir = tempfile::tempdir().unwrap();
        let mut exchange = exchange();
        exchange.buy_fee_in_base = dec!(0.001); // 0.1% of the bought ETH
        let fees = fees(&["ETH/USDT"]);
        let config = armed(&dir.path().join("stop"));
        let legs = Executor {
            exchange: &exchange,
            fees: &fees,
            config: &config,
            poll_interval: Duration::ZERO,
        }
        .execute(
            &route("USDT", "ETH", vec![leg("ETH/USDT", LegSide::Buy, "USDT", "ETH")]),
            &markets(),
            Some(dec!(50)),
        )
        .await
        .unwrap();
        // 50 USDT ordered 0.0248 ETH; 0.0000248 went to the fee, and the wallet already held
        // 1 ETH, which is not part of the arrival.
        assert_eq!(exchange.orders.lock()[0].quantity, Some(dec!(0.0248)));
        assert_eq!(arrival(&legs), Some(("ETH".to_owned(), dec!(0.0247752))));
        assert_eq!(legs[0].destination_free_after, dec!(1.0247752));
        assert_eq!(arrival(&[]), None);
    }
}
