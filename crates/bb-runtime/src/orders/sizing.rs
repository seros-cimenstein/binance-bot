//! Port of `src/orders/sizing.py`: dynamic order sizing from portfolio state
//! and exchange market rules.

use std::collections::HashMap;

use bb_core::Decimal;
use bb_core::money::floor_to_step;
use rust_decimal::RoundingStrategy;
use rust_decimal_macros::dec;

use crate::config::{OrderSizeMode, Settings};
use crate::exchange::{AmountPrecision, Exchange, MarketRules};
use crate::orders::models::OrderSide;
use crate::portfolio::{PortfolioSnapshot, PortfolioSource};

const TOLERANCE: Decimal = dec!(0.000000000001);

/// The order cannot be sized safely.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct OrderSizingError(pub String);

fn err(message: impl Into<String>) -> OrderSizingError {
    OrderSizingError(message.into())
}

/// Runtime configuration used by [`OrderSizer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderSizingConfig {
    pub mode: OrderSizeMode,
    pub order_size_percent: Decimal,
    pub min_order_notional_usd: Decimal,
    pub max_order_notional_usd: Decimal,
    pub min_balance_usdt: Decimal,
    pub max_position_size_usd: Decimal,
}

impl Default for OrderSizingConfig {
    fn default() -> Self {
        Self {
            mode: OrderSizeMode::Fixed,
            order_size_percent: dec!(1.0),
            min_order_notional_usd: Decimal::ZERO,
            max_order_notional_usd: Decimal::ZERO,
            min_balance_usdt: Decimal::ZERO,
            max_position_size_usd: dec!(500.0),
        }
    }
}

impl OrderSizingConfig {
    /// Build from the central settings (already validated there).
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            mode: settings.order_size_mode,
            order_size_percent: settings.order_size_percent,
            min_order_notional_usd: settings.min_order_notional_usd,
            max_order_notional_usd: settings.max_order_notional_usd,
            min_balance_usdt: settings.min_balance_usdt,
            max_position_size_usd: settings.max_position_size_usd,
        }
    }

    /// `__post_init__` constraints.
    pub fn validate(&self) -> Result<(), OrderSizingError> {
        let at_least = |v: Decimal, name: &str| {
            if v < Decimal::ZERO {
                Err(err(format!("{name} must be at least 0")))
            } else {
                Ok(())
            }
        };
        at_least(self.order_size_percent, "ORDER_SIZE_PERCENT")?;
        if self.mode != OrderSizeMode::Fixed && self.order_size_percent <= Decimal::ZERO {
            return Err(err("ORDER_SIZE_PERCENT must be greater than zero"));
        }
        at_least(self.min_order_notional_usd, "MIN_ORDER_NOTIONAL_USD")?;
        at_least(self.max_order_notional_usd, "MAX_ORDER_NOTIONAL_USD")?;
        at_least(self.min_balance_usdt, "MIN_BALANCE_USDT")?;
        if self.max_position_size_usd <= Decimal::ZERO {
            return Err(err("MAX_POSITION_SIZE_USD must be greater than 0"));
        }
        if self.max_order_notional_usd > Decimal::ZERO
            && self.max_order_notional_usd < self.min_order_notional_usd
        {
            return Err(err(
                "MAX_ORDER_NOTIONAL_USD must be greater than or equal to MIN_ORDER_NOTIONAL_USD",
            ));
        }
        Ok(())
    }
}

/// Inputs needed to resolve a concrete base-asset order amount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderSizingRequest {
    pub symbol: String,
    pub side: OrderSide,
    pub requested_amount: Decimal,
    pub price: Option<Decimal>,
}

/// Resolved order amount and supporting sizing context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizedOrder {
    pub amount: Decimal,
    pub notional_usdt: Option<Decimal>,
    pub price: Option<Decimal>,
    pub mode: OrderSizeMode,
    pub original_amount: Decimal,
    pub portfolio: Option<PortfolioSnapshot>,
}

/// Resolves configured sizing modes into Binance-ready base quantities.
#[derive(Debug, Clone, Copy, Default)]
pub struct OrderSizer;

impl OrderSizer {
    /// Sized amount after clamps, rounding and checks.
    pub async fn size_order(
        &self,
        request: &OrderSizingRequest,
        config: &OrderSizingConfig,
        portfolio_service: Option<&dyn PortfolioSource>,
        exchange: Option<&dyn Exchange>,
    ) -> Result<SizedOrder, OrderSizingError> {
        config.validate()?;
        let symbol = request.symbol.trim();
        if symbol.is_empty() {
            return Err(err("symbol must not be blank"));
        }
        let side = request.side;
        let requested = request.requested_amount;
        if requested <= Decimal::ZERO {
            return Err(err("amount must be greater than 0"));
        }
        if request.price.is_some_and(|p| p <= Decimal::ZERO) {
            return Err(err("price must be greater than 0"));
        }
        let price = request.price;
        let rules = match exchange {
            Some(exchange) => exchange.market_rules(symbol).await.unwrap_or_default(),
            None => MarketRules::default(),
        };

        if config.mode != OrderSizeMode::Fixed && price.is_none() {
            return Err(err(format!(
                "ORDER_SIZE_MODE={} requires a current market price",
                config.mode.as_str()
            )));
        }

        let mut portfolio = None;
        if requires_portfolio(config, side) {
            let Some(price) = price else {
                return Err(err("balance-aware sizing requires a market price"));
            };
            let Some(service) = portfolio_service else {
                return Err(err("portfolio snapshot service is required"));
            };
            let marks = HashMap::from([(symbol.to_owned(), price)]);
            portfolio = Some(
                service
                    .snapshot(&marks)
                    .await
                    .map_err(|e| err(e.to_string()))?,
            );
        }

        let mut amount = match (config.mode, price, &portfolio) {
            (OrderSizeMode::Fixed, _, _) => requested,
            (_, Some(price), Some(portfolio)) => {
                let mut target = target_notional(config, portfolio);
                if side == OrderSide::Buy {
                    target = target.min(spendable(config, portfolio)?);
                }
                target / price
            }
            _ => return Err(err("balance-aware sizing requires a market price")),
        };

        if let Some(price) = price
            && config.max_order_notional_usd > Decimal::ZERO
            && amount * price > config.max_order_notional_usd
        {
            amount = config.max_order_notional_usd / price;
        }

        let amount = round_amount_down(amount, rules.amount_precision)?;
        validate_min_amount(amount, &rules)?;
        let notional = price.map(|p| amount * p);
        validate_notional(notional, config, &rules)?;
        if side == OrderSide::Buy
            && let (Some(portfolio), Some(notional)) = (&portfolio, notional)
        {
            let spendable = spendable(config, portfolio)?;
            if notional > spendable + TOLERANCE {
                return Err(err(format!(
                    "insufficient available balance: {} USDT exceeds spendable {} USDT",
                    f8(notional),
                    f8(spendable)
                )));
            }
        }

        Ok(SizedOrder {
            amount,
            notional_usdt: notional,
            price,
            mode: config.mode,
            original_amount: requested,
            portfolio,
        })
    }
}

fn requires_portfolio(config: &OrderSizingConfig, side: OrderSide) -> bool {
    config.mode != OrderSizeMode::Fixed
        || (side == OrderSide::Buy && config.min_balance_usdt > Decimal::ZERO)
}

fn target_notional(config: &OrderSizingConfig, portfolio: &PortfolioSnapshot) -> Decimal {
    let percent = config.order_size_percent / dec!(100);
    match config.mode {
        OrderSizeMode::QuotePercent => portfolio.available_balance_usdt * percent,
        OrderSizeMode::EquityPercent => portfolio.total_equity_usdt * percent,
        OrderSizeMode::RiskPercent => config.max_position_size_usd * percent,
        OrderSizeMode::Fixed => Decimal::ZERO,
    }
}

fn spendable(
    config: &OrderSizingConfig,
    portfolio: &PortfolioSnapshot,
) -> Result<Decimal, OrderSizingError> {
    let spendable = portfolio.available_balance_usdt - config.min_balance_usdt;
    if spendable <= Decimal::ZERO {
        return Err(err(format!(
            "insufficient available balance: {} USDT is at or below MIN_BALANCE_USDT={}",
            f8(portfolio.available_balance_usdt),
            f8(config.min_balance_usdt)
        )));
    }
    Ok(spendable)
}

fn round_amount_down(
    amount: Decimal,
    precision: Option<AmountPrecision>,
) -> Result<Decimal, OrderSizingError> {
    if amount <= Decimal::ZERO {
        return Err(err("sized amount must be greater than zero"));
    }
    Ok(match precision {
        None => amount,
        Some(AmountPrecision::Places(places)) => {
            amount.round_dp_with_strategy(places, RoundingStrategy::ToZero)
        }
        Some(AmountPrecision::Step(step)) => floor_to_step(amount, step),
    })
}

fn validate_min_amount(amount: Decimal, rules: &MarketRules) -> Result<(), OrderSizingError> {
    if amount <= Decimal::ZERO {
        return Err(err("sized amount must be greater than zero"));
    }
    if rules.min_amount > Decimal::ZERO && amount + TOLERANCE < rules.min_amount {
        return Err(err(format!(
            "order amount below exchange lot size: {} < {}",
            amount.normalize(),
            rules.min_amount.normalize()
        )));
    }
    Ok(())
}

fn validate_notional(
    notional: Option<Decimal>,
    config: &OrderSizingConfig,
    rules: &MarketRules,
) -> Result<(), OrderSizingError> {
    let Some(notional) = notional else {
        return Ok(());
    };
    let min = config.min_order_notional_usd.max(rules.min_notional);
    if min > Decimal::ZERO && notional + TOLERANCE < min {
        return Err(err(format!(
            "order notional below minimum: {} USDT < {} USDT",
            f8(notional),
            f8(min)
        )));
    }
    if config.max_order_notional_usd > Decimal::ZERO
        && notional > config.max_order_notional_usd + TOLERANCE
    {
        return Err(err(format!(
            "order notional above maximum: {} USDT > {} USDT",
            f8(notional),
            f8(config.max_order_notional_usd)
        )));
    }
    Ok(())
}

fn f8(value: Decimal) -> String {
    format!("{:.8}", value.round_dp(8))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::{Balances, ExchangeError, ExchangeOrderRequest};
    use crate::orders::payload::OrderPayload;
    use crate::portfolio::PortfolioError;
    use async_trait::async_trait;
    use parking_lot::Mutex;

    struct RulesExchange(MarketRules);

    #[async_trait]
    impl Exchange for RulesExchange {
        async fn create_order(
            &self,
            _: &ExchangeOrderRequest,
        ) -> Result<OrderPayload, ExchangeError> {
            Err(ExchangeError::Other("unused".into()))
        }
        async fn cancel_order(&self, _: &str, _: &str) -> Result<OrderPayload, ExchangeError> {
            Err(ExchangeError::Other("unused".into()))
        }
        async fn fetch_order(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> Result<OrderPayload, ExchangeError> {
            Err(ExchangeError::Other("unused".into()))
        }
        async fn balances(&self) -> Result<Balances, ExchangeError> {
            Ok(Balances::default())
        }
        async fn last_price(&self, _: &str) -> Result<Option<Decimal>, ExchangeError> {
            Ok(None)
        }
        async fn market_rules(&self, _: &str) -> Option<MarketRules> {
            Some(self.0)
        }
    }

    struct StaticPortfolio {
        snapshot: PortfolioSnapshot,
        marks: Mutex<Vec<HashMap<String, Decimal>>>,
    }

    #[async_trait]
    impl PortfolioSource for StaticPortfolio {
        async fn snapshot(
            &self,
            marks: &HashMap<String, Decimal>,
        ) -> Result<PortfolioSnapshot, PortfolioError> {
            self.marks.lock().push(marks.clone());
            Ok(self.snapshot.clone())
        }
    }

    fn portfolio(available: Decimal, equity: Decimal) -> StaticPortfolio {
        StaticPortfolio {
            snapshot: PortfolioSnapshot {
                available_balance_usdt: available,
                total_balance_usdt: available,
                total_equity_usdt: equity,
                ..PortfolioSnapshot::default()
            },
            marks: Mutex::new(Vec::new()),
        }
    }

    fn request(amount: Decimal, price: Option<Decimal>) -> OrderSizingRequest {
        OrderSizingRequest {
            symbol: "BTC/USDT".into(),
            side: OrderSide::Buy,
            requested_amount: amount,
            price,
        }
    }

    #[tokio::test]
    async fn fixed_sizing_preserves_amount_and_rounds_to_exchange_precision() {
        let exchange = RulesExchange(MarketRules {
            min_amount: dec!(0.001),
            min_notional: dec!(10),
            amount_precision: Some(AmountPrecision::Places(3)),
        });
        let config = OrderSizingConfig {
            min_order_notional_usd: dec!(10),
            ..OrderSizingConfig::default()
        };
        let sized = OrderSizer
            .size_order(
                &request(dec!(0.123456), Some(dec!(1000))),
                &config,
                None,
                Some(&exchange),
            )
            .await
            .unwrap();
        assert_eq!(sized.amount, dec!(0.123));
        assert_eq!(sized.notional_usdt, Some(dec!(123)));
        assert!(sized.portfolio.is_none());

        let step = RulesExchange(MarketRules {
            amount_precision: Some(AmountPrecision::Step(dec!(0.00001))),
            ..MarketRules::default()
        });
        let sized = OrderSizer
            .size_order(
                &request(dec!(0.1234567), Some(dec!(1000))),
                &OrderSizingConfig::default(),
                None,
                Some(&step),
            )
            .await
            .unwrap();
        assert_eq!(sized.amount, dec!(0.12345));
    }

    #[tokio::test]
    async fn percent_sizing_modes_resolve_target_notional() {
        for (mode, percent) in [
            (OrderSizeMode::QuotePercent, dec!(10)),
            (OrderSizeMode::EquityPercent, dec!(5)),
            (OrderSizeMode::RiskPercent, dec!(10)),
        ] {
            let service = portfolio(dec!(1000), dec!(2000));
            let config = OrderSizingConfig {
                mode,
                order_size_percent: percent,
                max_position_size_usd: dec!(1000),
                ..OrderSizingConfig::default()
            };
            let sized = OrderSizer
                .size_order(
                    &request(dec!(0.5), Some(dec!(100))),
                    &config,
                    Some(&service),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(sized.amount, dec!(1), "{mode:?}");
            assert_eq!(sized.notional_usdt, Some(dec!(100)));
            assert_eq!(
                *service.marks.lock(),
                vec![HashMap::from([("BTC/USDT".to_owned(), dec!(100))])]
            );
        }
    }

    #[tokio::test]
    async fn max_notional_clamps_sized_amount() {
        let config = OrderSizingConfig {
            max_order_notional_usd: dec!(120),
            ..OrderSizingConfig::default()
        };
        let sized = OrderSizer
            .size_order(&request(dec!(5), Some(dec!(100))), &config, None, None)
            .await
            .unwrap();
        assert_eq!(sized.amount, dec!(1.2));
        assert_eq!(sized.notional_usdt, Some(dec!(120)));
    }

    #[tokio::test]
    async fn minimum_notional_and_lot_size_are_rejected() {
        let config = OrderSizingConfig {
            min_order_notional_usd: dec!(1),
            ..OrderSizingConfig::default()
        };
        let error = OrderSizer
            .size_order(&request(dec!(0.001), Some(dec!(100))), &config, None, None)
            .await
            .unwrap_err();
        assert!(error.0.contains("order notional below minimum"), "{error}");

        let exchange = RulesExchange(MarketRules {
            min_amount: dec!(0.01),
            ..MarketRules::default()
        });
        let error = OrderSizer
            .size_order(
                &request(dec!(0.009), Some(dec!(100))),
                &OrderSizingConfig::default(),
                None,
                Some(&exchange),
            )
            .await
            .unwrap_err();
        assert!(error.0.contains("below exchange lot size"), "{error}");
    }

    #[tokio::test]
    async fn insufficient_balance_is_rejected_before_order_placement() {
        let service = portfolio(dec!(40), dec!(40));
        let config = OrderSizingConfig {
            mode: OrderSizeMode::QuotePercent,
            order_size_percent: dec!(10),
            min_balance_usdt: dec!(50),
            ..OrderSizingConfig::default()
        };
        let error = OrderSizer
            .size_order(
                &request(dec!(0.5), Some(dec!(100))),
                &config,
                Some(&service),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            error.0.contains("insufficient available balance"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn percent_mode_requires_price() {
        let config = OrderSizingConfig {
            mode: OrderSizeMode::QuotePercent,
            ..OrderSizingConfig::default()
        };
        let error = OrderSizer
            .size_order(&request(dec!(1), None), &config, None, None)
            .await
            .unwrap_err();
        assert_eq!(
            error.0,
            "ORDER_SIZE_MODE=quote_percent requires a current market price"
        );
    }
}
