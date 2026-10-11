//! Capital-pool leases and quote auto-conversion for runtime buys (port of
//! the refactor orchestrator's `_capital_adjusted_order_amount` and
//! `ensure_quote_for_order` call sites, on top of `bb-capital`).
//!
//! Before each BUY the module (strategy name) requests a lease; the amount is
//! shrunk to the approved notional and a paused/zero lease blocks the order.
//! When enabled, auto-convert then sells an allowed asset into the quote if
//! the free quote cannot fund the buy (`order/test` only unless the runtime
//! is live). Sells bypass both.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use bb_capital::{
    AutoConversionResult, AutoConvertConfig, CapitalError, CapitalPool, CapitalPoolConfig,
    ConvertExchange, ConvertMode, FundingRequest, LeaseRequest, LeaseStatus,
    ensure_quote_for_order,
};
use bb_core::Decimal;
use serde_json::json;
use tokio::sync::Mutex;

use crate::config::Settings;
use crate::orders::models::{Order, OrderSide};
use crate::risk::RiskBreachError;

#[derive(Debug, thiserror::Error)]
pub enum CapitalGateError {
    #[error(transparent)]
    Blocked(#[from] RiskBreachError),
    #[error(transparent)]
    Capital(#[from] CapitalError),
}

struct PoolGate {
    pool: Mutex<CapitalPool>,
    state_path: Option<PathBuf>,
}

struct ConvertGate {
    exchange: Arc<dyn ConvertExchange>,
    config: AutoConvertConfig,
    mode: ConvertMode,
    audit: Option<bb_core::audit::JsonlLog>,
}

/// Lease + auto-convert hooks shared by every strategy client of a runtime.
#[derive(Default)]
pub struct CapitalGate {
    pool: Option<PoolGate>,
    convert: Option<ConvertGate>,
}

impl std::fmt::Debug for CapitalGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapitalGate")
            .field("pool", &self.pool.is_some())
            .field("auto_convert", &self.convert.as_ref().map(|c| c.mode))
            .finish()
    }
}

impl CapitalGate {
    /// From `CAPITAL_POOL_ENABLED` / `AUTO_CONVERT_TO_QUOTE` (and their
    /// companion variables). `None` when both are disabled. With
    /// `CAPITAL_AI_ADVISOR_ENABLED=true` every buy plan is first offered to the
    /// guarded advisor (proposal file, then the Claude allocator over omp); it can
    /// only clamp the deterministic plan. Conversions are submitted only when the
    /// runtime is live (not `DRY_RUN`, not testnet).
    pub fn from_env(
        settings: &Settings,
        client: bb_binance::BinanceClient,
    ) -> Result<Option<Self>, CapitalError> {
        let mut gate = Self::default();
        let pool_config = CapitalPoolConfig::from_env()?;
        if pool_config.enabled {
            let path = bb_capital::config::state_file();
            let mut pool = CapitalPool::new(pool_config);
            if let Err(err) = pool.load(&path) {
                tracing::warn!(path = %path.display(), error = %err, "Capital pool state unreadable; starting fresh");
            }
            let fleet = Arc::new(bb_capital::claude::LiveFleet {
                client: Some(client.clone()),
                data_dir: bb_capital::status::default_data_dir(),
            });
            if let Some(advisor) = bb_capital::claude::guarded_advisor_from_env(fleet)? {
                pool = pool.with_advisor(advisor);
            }
            gate = gate.with_pool(pool, Some(path));
        }
        let convert_exchange: Arc<dyn ConvertExchange> = Arc::new(client);
        let convert_config = AutoConvertConfig::from_env()?;
        if convert_config.enabled {
            let mode = if !settings.dry_run && !settings.use_testnet {
                ConvertMode::Live
            } else {
                ConvertMode::PlanOnly
            };
            gate = gate.with_auto_convert(
                convert_exchange,
                convert_config,
                mode,
                Some(bb_capital::config::convert_audit_path()),
            );
        }
        Ok((gate.pool.is_some() || gate.convert.is_some()).then_some(gate))
    }

    /// Enable leases from `pool` (state saved to `state_path` after each lease).
    pub fn with_pool(mut self, pool: CapitalPool, state_path: Option<PathBuf>) -> Self {
        self.pool = Some(PoolGate {
            pool: Mutex::new(pool),
            state_path,
        });
        self
    }

    /// Enable quote auto-conversion.
    pub fn with_auto_convert(
        mut self,
        exchange: Arc<dyn ConvertExchange>,
        config: AutoConvertConfig,
        mode: ConvertMode,
        audit_path: Option<PathBuf>,
    ) -> Self {
        self.convert = Some(ConvertGate {
            exchange,
            config,
            mode,
            audit: audit_path.map(bb_core::audit::JsonlLog::new),
        });
        self
    }

    pub fn pool_enabled(&self) -> bool {
        self.pool.is_some()
    }

    /// Shrink a buy to its capital lease; a paused or empty lease blocks it.
    /// Sells and a disabled pool return `amount` unchanged.
    pub async fn adjust_buy(&self, lease: &BuyLease<'_>) -> Result<Decimal, CapitalGateError> {
        let Some(gate) = &self.pool else {
            return Ok(lease.amount);
        };
        if lease.side != OrderSide::Buy {
            return Ok(lease.amount);
        }
        let requested = lease.amount * lease.price;
        let granted = {
            let mut pool = gate.pool.lock().await;
            let granted = pool
                .request_lease(&LeaseRequest {
                    module_id: lease.module_id,
                    symbol: lease.symbol,
                    side: "buy",
                    requested_notional_usdt: requested,
                    total_equity_usdt: lease.total_equity_usdt,
                    module_ids: lease.module_ids,
                    module_usage_usdt: lease.module_usage_usdt,
                })
                .await?;
            if let Some(path) = &gate.state_path
                && let Err(err) = pool.save(path)
            {
                tracing::warn!(path = %path.display(), error = %err, "Capital pool state save failed");
            }
            granted
        };
        let context = json!({
            "symbol": lease.symbol,
            "side": "buy",
            "module_id": lease.module_id,
            "requested_notional_usdt": requested,
            "approved_notional_usdt": granted.approved_notional_usdt,
            "reason": granted.reason,
        });
        if !granted.approved() {
            return Err(RiskBreachError {
                message: "capital pool allocation blocked buy order".into(),
                context,
            }
            .into());
        }
        let adjusted = lease
            .amount
            .min(granted.approved_notional_usdt / lease.price);
        if adjusted <= Decimal::ZERO {
            return Err(RiskBreachError {
                message: "capital pool allocation is too small for a buy order".into(),
                context,
            }
            .into());
        }
        if granted.status == LeaseStatus::Reduced || adjusted < lease.amount {
            tracing::info!(
                symbol = lease.symbol,
                module_id = lease.module_id,
                requested_amount = %lease.amount,
                adjusted_amount = %adjusted,
                requested_notional_usdt = %requested,
                approved_notional_usdt = %granted.approved_notional_usdt,
                reason = %granted.reason,
                "Adjusted buy amount to capital pool allocation"
            );
        }
        Ok(adjusted)
    }

    /// Sell an allowed asset into the quote when free quote cannot fund the
    /// buy. `None` when auto-convert is disabled or the order is a sell.
    pub async fn ensure_quote(
        &self,
        symbol: &str,
        side: OrderSide,
        amount: Decimal,
        price: Decimal,
        min_quote_balance: Decimal,
    ) -> Result<Option<AutoConversionResult>, CapitalGateError> {
        let Some(gate) = &self.convert else {
            return Ok(None);
        };
        if side != OrderSide::Buy {
            return Ok(None);
        }
        let mut request = FundingRequest::for_order(symbol, "buy", amount, price)?;
        request.min_quote_balance = min_quote_balance;
        let result =
            ensure_quote_for_order(gate.exchange.as_ref(), &gate.config, &request, gate.mode)
                .await?;
        if result.attempted
            && let Some(audit) = &gate.audit
            && let Err(err) = audit.append(&bb_capital::convert::audit_row(
                &request, &result, gate.mode,
            ))
        {
            tracing::warn!(error = %err, "Auto-convert audit append failed");
        }
        tracing::info!(
            symbol,
            reason = %result.reason,
            attempted = result.attempted,
            executed = result.executed,
            converted_quote_value = %result.converted_quote_value,
            "Auto-convert quote funding check"
        );
        Ok(Some(result))
    }
}

/// One buy lease request.
#[derive(Debug, Clone)]
pub struct BuyLease<'a> {
    pub module_id: &'a str,
    pub symbol: &'a str,
    pub side: OrderSide,
    pub amount: Decimal,
    pub price: Decimal,
    pub total_equity_usdt: Decimal,
    pub module_ids: &'a [String],
    pub module_usage_usdt: &'a BTreeMap<String, Decimal>,
}

/// Open notional of `module_id` from order history: filled buys minus filled
/// sells (pending/open orders count their full amount), floored at zero.
pub fn module_usage(orders: &[Order], module_id: &str) -> BTreeMap<String, Decimal> {
    let mut net = Decimal::ZERO;
    for order in orders {
        let amount = if order.filled_amount > Decimal::ZERO {
            order.filled_amount
        } else if order.status.is_active() {
            order.amount
        } else {
            continue;
        };
        let Some(price) = order
            .avg_fill_price
            .or(order.price)
            .filter(|p| *p > Decimal::ZERO)
        else {
            continue;
        };
        match order.side {
            OrderSide::Buy => net += amount * price,
            OrderSide::Sell => net -= amount * price,
        }
    }
    if net > Decimal::ZERO {
        BTreeMap::from([(module_id.to_owned(), net)])
    } else {
        BTreeMap::new()
    }
}
