//! Port of `src/strategies/loader.py` and the plugin registry of
//! `src/strategies/base.py`.
//!
//! Python kept two registries (strategy classes and settings→config
//! builders). Here one factory per plugin name owns both steps, so a
//! "registered plugin without a config builder" cannot exist.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::base::{OrderClient, Strategy, StrategyError};
use super::demo_sma::{self, DemoSmaStrategy, SmaSettings};
use crate::config::Settings;

/// Builds a configured strategy from runtime settings.
pub type StrategyFactory = Arc<
    dyn Fn(Arc<dyn OrderClient>, &Settings) -> Result<Arc<dyn Strategy>, StrategyError>
        + Send
        + Sync,
>;

#[derive(Clone, Default)]
pub struct StrategyRegistry {
    factories: BTreeMap<String, StrategyFactory>,
}

impl std::fmt::Debug for StrategyRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.factories.keys()).finish()
    }
}

impl StrategyRegistry {
    /// Registry with the built-in `demo-sma` plugin.
    pub fn with_builtin() -> Self {
        let mut registry = Self::default();
        let factory: StrategyFactory = Arc::new(|client, settings| {
            let strategy: Arc<dyn Strategy> = Arc::new(DemoSmaStrategy::new(
                client,
                build_demo_sma_config(settings)?,
            ));
            Ok(strategy)
        });
        // The registry is empty, so registration cannot collide.
        registry
            .factories
            .insert(demo_sma::PLUGIN_NAME.to_owned(), factory);
        registry
    }

    /// Register a plugin; duplicate names are rejected.
    pub fn register(&mut self, name: &str, factory: StrategyFactory) -> Result<(), StrategyError> {
        let name = normalize_name(name)?;
        if self.factories.contains_key(&name) {
            return Err(StrategyError::Invalid(format!(
                "strategy plugin already registered: {name:?}"
            )));
        }
        self.factories.insert(name, factory);
        Ok(())
    }

    pub fn unregister(&mut self, name: &str) {
        self.factories.remove(name.trim());
    }

    pub fn names(&self) -> Vec<String> {
        self.factories.keys().cloned().collect()
    }

    /// Create a configured strategy instance (`create_strategy`).
    pub fn create(
        &self,
        name: &str,
        client: Arc<dyn OrderClient>,
        settings: &Settings,
    ) -> Result<Arc<dyn Strategy>, StrategyError> {
        let name = normalize_name(name)?;
        let factory = self.factories.get(&name).ok_or_else(|| {
            StrategyError::Invalid(format!("strategy plugin is not registered: {name:?}"))
        })?;
        factory(client, settings)
    }
}

/// Demo SMA settings from runtime settings.
pub fn build_demo_sma_config(settings: &Settings) -> Result<SmaSettings, StrategyError> {
    let period = |value: i64, name: &str| {
        usize::try_from(value)
            .map_err(|_| StrategyError::Invalid(format!("{name} must be greater than 0")))
    };
    SmaSettings {
        symbol: settings.trading_symbol.clone(),
        fast_period: period(settings.fast_period, "fast_period")?,
        slow_period: period(settings.slow_period, "slow_period")?,
        timeframe: settings.timeframe.clone(),
        order_size: settings.order_size,
    }
    .validate()
}

fn normalize_name(name: &str) -> Result<String, StrategyError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(StrategyError::Invalid(
            "strategy name must not be blank".into(),
        ));
    }
    Ok(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orders::models::OrderStatus;
    use crate::strategies::base::{OrderResult, StrategyOrder};
    use async_trait::async_trait;
    use bb_core::Decimal;
    use rust_decimal_macros::dec;

    struct NullClient;

    #[async_trait]
    impl OrderClient for NullClient {
        async fn create_order(&self, order: StrategyOrder) -> Result<OrderResult, StrategyError> {
            Ok(OrderResult {
                id: "x".into(),
                symbol: order.symbol,
                order_type: order.order_type,
                side: order.side,
                amount: order.amount,
                price: None,
                status: OrderStatus::Closed,
                filled: order.amount,
                remaining: Decimal::ZERO,
                average: None,
            })
        }
        fn update_last_price(&self, _: Decimal) {}
    }

    fn settings() -> Settings {
        let mut s = Settings::defaults("key", "secret");
        s.trading_symbol = "ethusdt".into();
        s.fast_period = 5;
        s.slow_period = 15;
        s.timeframe = "5m".into();
        s.order_size = dec!(0.25);
        s.validate().unwrap()
    }

    #[test]
    fn creates_demo_sma_from_registry_with_runtime_settings() {
        let registry = StrategyRegistry::with_builtin();
        let strategy = registry
            .create("demo-sma", Arc::new(NullClient), &settings())
            .unwrap();
        assert_eq!(strategy.name(), "demo-sma");
        let config = build_demo_sma_config(&settings()).unwrap();
        assert_eq!(config.symbol, "ETH/USDT");
        assert_eq!((config.fast_period, config.slow_period), (5, 15));
        assert_eq!(config.timeframe, "5m");
        assert_eq!(config.order_size, dec!(0.25));
    }

    #[test]
    fn rejects_unknown_and_duplicate_plugins() {
        let mut registry = StrategyRegistry::with_builtin();
        let err = registry
            .create("missing", Arc::new(NullClient), &settings())
            .err()
            .unwrap();
        assert!(
            err.to_string()
                .contains("strategy plugin is not registered"),
            "{err}"
        );
        let factory: StrategyFactory = Arc::new(|client, s| {
            Ok(
                Arc::new(DemoSmaStrategy::new(client, build_demo_sma_config(s)?))
                    as Arc<dyn Strategy>,
            )
        });
        let err = registry.register("demo-sma", factory.clone()).unwrap_err();
        assert!(err.to_string().contains("already registered"), "{err}");
        registry.register("external", factory).unwrap();
        assert!(
            registry
                .create(" external ", Arc::new(NullClient), &settings())
                .is_ok()
        );
        registry.unregister("external");
        assert_eq!(registry.names(), vec!["demo-sma".to_owned()]);
    }
}
