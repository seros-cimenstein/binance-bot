//! Typed trading rules from `exchangeInfo` filters, with rounding and validation.

use std::fmt;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use bb_core::money::{ceil_to_step, floor_to_step};

/// One raw `exchangeInfo` filter (spot or USD-M futures). Every field is optional
/// because each `filterType` uses a different subset.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SymbolFilter {
    pub filter_type: String,
    pub min_price: Option<Decimal>,
    pub max_price: Option<Decimal>,
    pub tick_size: Option<Decimal>,
    pub min_qty: Option<Decimal>,
    pub max_qty: Option<Decimal>,
    pub step_size: Option<Decimal>,
    pub min_notional: Option<Decimal>,
    /// Futures `MIN_NOTIONAL` uses `notional`.
    pub notional: Option<Decimal>,
    pub max_notional: Option<Decimal>,
    pub apply_to_market: Option<bool>,
    pub apply_min_to_market: Option<bool>,
    pub apply_max_to_market: Option<bool>,
    pub avg_price_mins: Option<u32>,
    pub multiplier_up: Option<Decimal>,
    pub multiplier_down: Option<Decimal>,
    pub bid_multiplier_up: Option<Decimal>,
    pub bid_multiplier_down: Option<Decimal>,
    pub ask_multiplier_up: Option<Decimal>,
    pub ask_multiplier_down: Option<Decimal>,
    pub max_num_orders: Option<u32>,
    pub max_num_algo_orders: Option<u32>,
    pub limit: Option<u32>,
    pub max_position: Option<Decimal>,
}

/// Order side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    /// Binance wire value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "BUY",
            Self::Sell => "SELL",
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which market a rule set belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MarketKind {
    Spot,
    UsdmFutures,
}

/// `PRICE_FILTER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceFilter {
    pub min_price: Decimal,
    pub max_price: Decimal,
    pub tick_size: Decimal,
}

/// `LOT_SIZE` / `MARKET_LOT_SIZE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LotSize {
    pub min_qty: Decimal,
    pub max_qty: Decimal,
    pub step_size: Decimal,
}

/// `MIN_NOTIONAL` / `NOTIONAL` (spot) or futures `MIN_NOTIONAL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotionalFilter {
    pub min_notional: Option<Decimal>,
    pub max_notional: Option<Decimal>,
    pub apply_min_to_market: bool,
    pub apply_max_to_market: bool,
    pub avg_price_mins: Option<u32>,
}

/// `PERCENT_PRICE_BY_SIDE` (spot) — or `PERCENT_PRICE` mapped to both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PercentPriceBySide {
    pub bid_multiplier_up: Decimal,
    pub bid_multiplier_down: Decimal,
    pub ask_multiplier_up: Decimal,
    pub ask_multiplier_down: Decimal,
    pub avg_price_mins: Option<u32>,
}

/// Typed trading rules for one symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolRules {
    pub market: MarketKind,
    /// Exchange id (`BTCUSDT`).
    pub symbol: String,
    /// Unified symbol (`BTC/USDT`, futures `BTC/USDT:USDT`).
    pub unified: String,
    pub status: String,
    pub base_asset: String,
    pub quote_asset: String,
    /// Futures margin asset.
    pub settle_asset: Option<String>,
    /// Futures contract type (`PERPETUAL`, ...).
    pub contract_type: Option<String>,
    /// Flattened, de-duplicated `permissions` ∪ `permissionSets`.
    pub permissions: Vec<String>,
    pub order_types: Vec<String>,
    pub spot_trading_allowed: bool,
    pub margin_trading_allowed: bool,
    pub quote_order_qty_market_allowed: bool,
    pub base_asset_precision: Option<u32>,
    pub quote_asset_precision: Option<u32>,
    pub price: Option<PriceFilter>,
    pub lot: Option<LotSize>,
    pub market_lot: Option<LotSize>,
    pub notional: Option<NotionalFilter>,
    pub percent_price: Option<PercentPriceBySide>,
    pub max_num_orders: Option<u32>,
    /// All raw filters for anything not lifted above.
    pub filters: Vec<SymbolFilter>,
}

/// Order type for validation (`exchangeInfo.orderTypes` spelling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderType {
    Market,
    Limit,
    LimitMaker,
    StopLoss,
    StopLossLimit,
    TakeProfit,
    TakeProfitLimit,
    Stop,
    StopMarket,
    TakeProfitMarket,
    TrailingStopMarket,
}

impl OrderType {
    /// Binance wire value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Market => "MARKET",
            Self::Limit => "LIMIT",
            Self::LimitMaker => "LIMIT_MAKER",
            Self::StopLoss => "STOP_LOSS",
            Self::StopLossLimit => "STOP_LOSS_LIMIT",
            Self::TakeProfit => "TAKE_PROFIT",
            Self::TakeProfitLimit => "TAKE_PROFIT_LIMIT",
            Self::Stop => "STOP",
            Self::StopMarket => "STOP_MARKET",
            Self::TakeProfitMarket => "TAKE_PROFIT_MARKET",
            Self::TrailingStopMarket => "TRAILING_STOP_MARKET",
        }
    }

    /// Whether the order executes at market (no limit price).
    pub fn is_market(self) -> bool {
        matches!(
            self,
            Self::Market | Self::StopMarket | Self::TakeProfitMarket | Self::TrailingStopMarket
        )
    }
}

impl fmt::Display for OrderType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An order to check against [`SymbolRules::validate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderCheck {
    pub side: Side,
    pub order_type: OrderType,
    /// Base quantity (`quantity`).
    pub quantity: Option<Decimal>,
    /// Quote quantity (`quoteOrderQty`, MARKET only).
    pub quote_quantity: Option<Decimal>,
    /// Limit price.
    pub price: Option<Decimal>,
    /// Reference price (last/mid/avg) for MARKET notional and percent-price bands.
    pub reference_price: Option<Decimal>,
}

/// A single rule the order breaks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "violation", rename_all = "snake_case")]
pub enum RuleViolation {
    NotTrading {
        status: String,
    },
    OrderTypeNotAllowed {
        order_type: String,
    },
    QuoteOrderQtyNotAllowed,
    MissingQuantity,
    MissingPrice,
    PriceBelowMin {
        price: Decimal,
        min: Decimal,
    },
    PriceAboveMax {
        price: Decimal,
        max: Decimal,
    },
    PriceNotOnTick {
        price: Decimal,
        tick: Decimal,
    },
    QtyBelowMin {
        qty: Decimal,
        min: Decimal,
    },
    QtyAboveMax {
        qty: Decimal,
        max: Decimal,
    },
    QtyNotOnStep {
        qty: Decimal,
        step: Decimal,
    },
    NotionalBelowMin {
        notional: Decimal,
        min: Decimal,
    },
    NotionalAboveMax {
        notional: Decimal,
        max: Decimal,
    },
    PriceOutsideBand {
        price: Decimal,
        low: Decimal,
        high: Decimal,
    },
}

impl fmt::Display for RuleViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotTrading { status } => write!(f, "symbol status is {status}, not TRADING"),
            Self::OrderTypeNotAllowed { order_type } => {
                write!(f, "order type {order_type} is not allowed")
            }
            Self::QuoteOrderQtyNotAllowed => {
                f.write_str("quoteOrderQty market orders are not allowed")
            }
            Self::MissingQuantity => f.write_str("quantity or quoteOrderQty is required"),
            Self::MissingPrice => f.write_str("price is required for this order type"),
            Self::PriceBelowMin { price, min } => write!(f, "price {price} below minPrice {min}"),
            Self::PriceAboveMax { price, max } => write!(f, "price {price} above maxPrice {max}"),
            Self::PriceNotOnTick { price, tick } => {
                write!(f, "price {price} is not a multiple of tickSize {tick}")
            }
            Self::QtyBelowMin { qty, min } => write!(f, "quantity {qty} below minQty {min}"),
            Self::QtyAboveMax { qty, max } => write!(f, "quantity {qty} above maxQty {max}"),
            Self::QtyNotOnStep { qty, step } => {
                write!(f, "quantity {qty} is not a multiple of stepSize {step}")
            }
            Self::NotionalBelowMin { notional, min } => {
                write!(f, "notional {notional} below minNotional {min}")
            }
            Self::NotionalAboveMax { notional, max } => {
                write!(f, "notional {notional} above maxNotional {max}")
            }
            Self::PriceOutsideBand { price, low, high } => {
                write!(f, "price {price} outside allowed band [{low}, {high}]")
            }
        }
    }
}

fn on_step(value: Decimal, base: Decimal, step: Decimal) -> bool {
    step <= Decimal::ZERO || ((value - base) % step).is_zero()
}

impl SymbolRules {
    /// Build rules from raw filter list; precision/permission fields are filled by the caller.
    pub(crate) fn apply_filters(&mut self, filters: Vec<SymbolFilter>) {
        for filter in &filters {
            match filter.filter_type.as_str() {
                "PRICE_FILTER" => {
                    self.price = Some(PriceFilter {
                        min_price: filter.min_price.unwrap_or_default(),
                        max_price: filter.max_price.unwrap_or_default(),
                        tick_size: filter.tick_size.unwrap_or_default(),
                    });
                }
                "LOT_SIZE" | "MARKET_LOT_SIZE" => {
                    let lot = LotSize {
                        min_qty: filter.min_qty.unwrap_or_default(),
                        max_qty: filter.max_qty.unwrap_or_default(),
                        step_size: filter.step_size.unwrap_or_default(),
                    };
                    if filter.filter_type == "LOT_SIZE" {
                        self.lot = Some(lot);
                    } else {
                        self.market_lot = Some(lot);
                    }
                }
                "MIN_NOTIONAL" => {
                    let entry = self.notional.get_or_insert(NotionalFilter {
                        min_notional: None,
                        max_notional: None,
                        apply_min_to_market: true,
                        apply_max_to_market: false,
                        avg_price_mins: None,
                    });
                    entry.min_notional = filter.min_notional.or(filter.notional);
                    entry.apply_min_to_market = filter.apply_to_market.unwrap_or(true);
                    entry.avg_price_mins = filter.avg_price_mins;
                }
                "NOTIONAL" => {
                    self.notional = Some(NotionalFilter {
                        min_notional: filter.min_notional,
                        max_notional: filter.max_notional,
                        apply_min_to_market: filter.apply_min_to_market.unwrap_or(true),
                        apply_max_to_market: filter.apply_max_to_market.unwrap_or(false),
                        avg_price_mins: filter.avg_price_mins,
                    });
                }
                "PERCENT_PRICE_BY_SIDE" => {
                    self.percent_price = Some(PercentPriceBySide {
                        bid_multiplier_up: filter.bid_multiplier_up.unwrap_or_default(),
                        bid_multiplier_down: filter.bid_multiplier_down.unwrap_or_default(),
                        ask_multiplier_up: filter.ask_multiplier_up.unwrap_or_default(),
                        ask_multiplier_down: filter.ask_multiplier_down.unwrap_or_default(),
                        avg_price_mins: filter.avg_price_mins,
                    });
                }
                "PERCENT_PRICE" if self.percent_price.is_none() => {
                    let up = filter.multiplier_up.unwrap_or_default();
                    let down = filter.multiplier_down.unwrap_or_default();
                    self.percent_price = Some(PercentPriceBySide {
                        bid_multiplier_up: up,
                        bid_multiplier_down: down,
                        ask_multiplier_up: up,
                        ask_multiplier_down: down,
                        avg_price_mins: filter.avg_price_mins,
                    });
                }
                "MAX_NUM_ORDERS" => {
                    self.max_num_orders = filter.max_num_orders.or(filter.limit);
                }
                _ => {}
            }
        }
        self.filters = filters;
    }

    /// `status == "TRADING"`.
    pub fn is_trading(&self) -> bool {
        self.status == "TRADING"
    }

    /// Whether `permission` (e.g. `SPOT`, `MARGIN`) is granted.
    pub fn has_permission(&self, permission: &str) -> bool {
        self.permissions
            .iter()
            .any(|p| p.eq_ignore_ascii_case(permission))
    }

    /// Whether `order_type` is listed in `orderTypes`.
    pub fn supports_order_type(&self, order_type: OrderType) -> bool {
        self.order_types.is_empty() || self.order_types.iter().any(|t| t == order_type.as_str())
    }

    /// `tickSize` (0 when unknown).
    pub fn tick_size(&self) -> Decimal {
        self.price.map(|p| p.tick_size).unwrap_or_default()
    }

    /// `LOT_SIZE.stepSize` (0 when unknown).
    pub fn step_size(&self) -> Decimal {
        self.lot.map(|l| l.step_size).unwrap_or_default()
    }

    /// Step size for MARKET orders: `MARKET_LOT_SIZE.stepSize` when positive, else `LOT_SIZE`.
    pub fn market_step_size(&self) -> Decimal {
        self.market_lot
            .map(|l| l.step_size)
            .filter(|s| *s > Decimal::ZERO)
            .unwrap_or_else(|| self.step_size())
    }

    /// Minimum notional (`NOTIONAL.minNotional` / `MIN_NOTIONAL`), if any.
    pub fn min_notional(&self) -> Option<Decimal> {
        self.notional.and_then(|n| n.min_notional)
    }

    /// Minimum base quantity (`LOT_SIZE.minQty`), if any.
    pub fn min_qty(&self) -> Option<Decimal> {
        self.lot.map(|l| l.min_qty)
    }

    /// Round a price down to the tick size.
    pub fn round_price(&self, price: Decimal) -> Decimal {
        floor_to_step(price, self.tick_size())
    }

    /// Round a price up to the tick size.
    pub fn round_price_up(&self, price: Decimal) -> Decimal {
        ceil_to_step(price, self.tick_size())
    }

    /// Round a LIMIT quantity down to `LOT_SIZE.stepSize`.
    pub fn round_qty(&self, qty: Decimal) -> Decimal {
        floor_to_step(qty, self.step_size())
    }

    /// Round a MARKET quantity down to [`Self::market_step_size`].
    pub fn round_market_qty(&self, qty: Decimal) -> Decimal {
        floor_to_step(qty, self.market_step_size())
    }

    /// Truncate a quote amount to `quoteAssetPrecision` decimals (for `quoteOrderQty`).
    pub fn round_quote_qty(&self, quote: Decimal) -> Decimal {
        match self.quote_asset_precision {
            Some(dp) => quote
                .round_dp_with_strategy(dp, rust_decimal::RoundingStrategy::ToZero)
                .normalize(),
            None => quote,
        }
    }

    /// Check an order against status, order types and every lifted filter.
    /// Returns all violations (empty = passes).
    pub fn validate(&self, order: &OrderCheck) -> Vec<RuleViolation> {
        let mut out = Vec::new();
        if !self.is_trading() {
            out.push(RuleViolation::NotTrading {
                status: self.status.clone(),
            });
        }
        if !self.supports_order_type(order.order_type) {
            out.push(RuleViolation::OrderTypeNotAllowed {
                order_type: order.order_type.as_str().to_owned(),
            });
        }
        let is_market = order.order_type.is_market();
        if order.quote_quantity.is_some()
            && is_market
            && self.market == MarketKind::Spot
            && !self.quote_order_qty_market_allowed
        {
            out.push(RuleViolation::QuoteOrderQtyNotAllowed);
        }
        if order.quantity.is_none() && order.quote_quantity.is_none() {
            out.push(RuleViolation::MissingQuantity);
        }
        if !is_market && order.price.is_none() {
            out.push(RuleViolation::MissingPrice);
        }

        if let (Some(price), Some(filter), false) = (order.price, self.price, is_market) {
            if filter.min_price > Decimal::ZERO && price < filter.min_price {
                out.push(RuleViolation::PriceBelowMin {
                    price,
                    min: filter.min_price,
                });
            }
            if filter.max_price > Decimal::ZERO && price > filter.max_price {
                out.push(RuleViolation::PriceAboveMax {
                    price,
                    max: filter.max_price,
                });
            }
            if !on_step(price, filter.min_price, filter.tick_size) {
                out.push(RuleViolation::PriceNotOnTick {
                    price,
                    tick: filter.tick_size,
                });
            }
        }

        if let Some(qty) = order.quantity {
            let lots = if is_market {
                vec![
                    self.lot,
                    self.market_lot.filter(|l| l.max_qty > Decimal::ZERO),
                ]
            } else {
                vec![self.lot]
            };
            for lot in lots.into_iter().flatten() {
                if qty < lot.min_qty {
                    out.push(RuleViolation::QtyBelowMin {
                        qty,
                        min: lot.min_qty,
                    });
                }
                if lot.max_qty > Decimal::ZERO && qty > lot.max_qty {
                    out.push(RuleViolation::QtyAboveMax {
                        qty,
                        max: lot.max_qty,
                    });
                }
                if !on_step(qty, lot.min_qty, lot.step_size) {
                    out.push(RuleViolation::QtyNotOnStep {
                        qty,
                        step: lot.step_size,
                    });
                }
            }
        }

        let effective_price = if is_market {
            order.reference_price
        } else {
            order.price
        };
        let notional = match (order.quote_quantity, order.quantity, effective_price) {
            (Some(quote), _, _) => Some(quote),
            (None, Some(qty), Some(price)) => Some(qty * price),
            _ => None,
        };
        if let (Some(notional), Some(filter)) = (notional, self.notional) {
            if let Some(min) = filter.min_notional
                && (!is_market || filter.apply_min_to_market)
                && notional < min
            {
                out.push(RuleViolation::NotionalBelowMin { notional, min });
            }
            if let Some(max) = filter.max_notional
                && max > Decimal::ZERO
                && (!is_market || filter.apply_max_to_market)
                && notional > max
            {
                out.push(RuleViolation::NotionalAboveMax { notional, max });
            }
        }

        if let (false, Some(price), Some(reference), Some(band)) = (
            is_market,
            order.price,
            order.reference_price,
            self.percent_price,
        ) {
            let (up, down) = match order.side {
                Side::Buy => (band.bid_multiplier_up, band.bid_multiplier_down),
                Side::Sell => (band.ask_multiplier_up, band.ask_multiplier_down),
            };
            let (low, high) = (reference * down, reference * up);
            if up > Decimal::ZERO && (price < low || price > high) {
                out.push(RuleViolation::PriceOutsideBand { price, low, high });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    pub(crate) fn btc_rules() -> SymbolRules {
        let filters: Vec<SymbolFilter> = serde_json::from_str(
            r#"[
            {"filterType":"PRICE_FILTER","minPrice":"0.01000000","maxPrice":"1000000.00000000","tickSize":"0.01000000"},
            {"filterType":"LOT_SIZE","minQty":"0.00001000","maxQty":"9000.00000000","stepSize":"0.00001000"},
            {"filterType":"ICEBERG_PARTS","limit":10},
            {"filterType":"MARKET_LOT_SIZE","minQty":"0.00000000","maxQty":"83.27","stepSize":"0.00000000"},
            {"filterType":"PERCENT_PRICE_BY_SIDE","bidMultiplierUp":"5","bidMultiplierDown":"0.2","askMultiplierUp":"5","askMultiplierDown":"0.2","avgPriceMins":5},
            {"filterType":"NOTIONAL","minNotional":"5.00000000","applyMinToMarket":true,"maxNotional":"9000000.00000000","applyMaxToMarket":false,"avgPriceMins":5},
            {"filterType":"MAX_NUM_ORDERS","maxNumOrders":200}
        ]"#,
        )
        .unwrap_or_default();
        let mut rules = SymbolRules {
            market: MarketKind::Spot,
            symbol: "BTCUSDT".into(),
            unified: "BTC/USDT".into(),
            status: "TRADING".into(),
            base_asset: "BTC".into(),
            quote_asset: "USDT".into(),
            settle_asset: None,
            contract_type: None,
            permissions: vec!["SPOT".into()],
            order_types: vec!["LIMIT".into(), "LIMIT_MAKER".into(), "MARKET".into()],
            spot_trading_allowed: true,
            margin_trading_allowed: true,
            quote_order_qty_market_allowed: true,
            base_asset_precision: Some(8),
            quote_asset_precision: Some(8),
            price: None,
            lot: None,
            market_lot: None,
            notional: None,
            percent_price: None,
            max_num_orders: None,
            filters: Vec::new(),
        };
        rules.apply_filters(filters);
        rules
    }

    #[test]
    fn filters_are_lifted() {
        let rules = btc_rules();
        assert_eq!(rules.filters.len(), 7);
        assert_eq!(rules.tick_size(), dec!(0.01));
        assert_eq!(rules.step_size(), dec!(0.00001));
        assert_eq!(rules.market_step_size(), dec!(0.00001));
        assert_eq!(rules.min_notional(), Some(dec!(5)));
        assert_eq!(rules.max_num_orders, Some(200));
        assert_eq!(
            rules.percent_price.map(|p| p.bid_multiplier_down),
            Some(dec!(0.2))
        );
    }

    #[test]
    fn rounding_floors_to_tick_and_step() {
        let rules = btc_rules();
        assert_eq!(rules.round_price(dec!(62000.129)), dec!(62000.12));
        assert_eq!(rules.round_price_up(dec!(62000.121)), dec!(62000.13));
        assert_eq!(rules.round_qty(dec!(0.000123456)), dec!(0.00012));
        assert_eq!(rules.round_market_qty(dec!(1.000019)), dec!(1.00001));
        assert_eq!(rules.round_quote_qty(dec!(6.123456789)), dec!(6.12345678));
    }

    #[test]
    fn validate_reports_typed_violations() {
        let rules = btc_rules();
        let ok = OrderCheck {
            side: Side::Buy,
            order_type: OrderType::Limit,
            quantity: Some(dec!(0.001)),
            quote_quantity: None,
            price: Some(dec!(60000)),
            reference_price: Some(dec!(61000)),
        };
        assert!(rules.validate(&ok).is_empty());

        let bad = OrderCheck {
            quantity: Some(dec!(0.0000155)),
            price: Some(dec!(60000.005)),
            ..ok
        };
        let violations = rules.validate(&bad);
        assert!(violations.contains(&RuleViolation::PriceNotOnTick {
            price: dec!(60000.005),
            tick: dec!(0.01)
        }));
        assert!(violations.contains(&RuleViolation::QtyNotOnStep {
            qty: dec!(0.0000155),
            step: dec!(0.00001)
        }));
        assert!(
            violations
                .iter()
                .any(|v| matches!(v, RuleViolation::NotionalBelowMin { .. }))
        );

        let band = OrderCheck {
            price: Some(dec!(1000)),
            ..ok
        };
        assert!(
            rules
                .validate(&band)
                .iter()
                .any(|v| matches!(v, RuleViolation::PriceOutsideBand { .. }))
        );

        let market_quote = OrderCheck {
            order_type: OrderType::Market,
            quantity: None,
            quote_quantity: Some(dec!(4)),
            price: None,
            ..ok
        };
        assert_eq!(
            rules.validate(&market_quote),
            vec![RuleViolation::NotionalBelowMin {
                notional: dec!(4),
                min: dec!(5)
            }]
        );

        let halted = SymbolRules {
            status: "BREAK".into(),
            ..rules
        };
        assert_eq!(
            halted.validate(&ok),
            vec![RuleViolation::NotTrading {
                status: "BREAK".into()
            }]
        );
    }
}
