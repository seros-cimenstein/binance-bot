//! Binance product modules (port of the Python `src/` product layer).
//!
//! - [`api_permissions`]: API capability diagnostics and the feature registry
//!   every live path checks before running.
//! - [`scope_audit`]: local symbol/asset whitelist audit.
//! - Product planners: margin, USD-M futures, wallet transfers, withdrawals and
//!   prediction markets — read-only discovery plus local risk modelling; live
//!   mutation is blocked by policy.
//! - Spot live paths: the tiny spot canary, the guarded live spot allocator and
//!   the one-shot live probe, all gated by explicit confirmations.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::float_cmp
    )
)]

pub mod advanced_readiness;
pub mod alerts;
pub mod api_permissions;
pub mod cli;
pub mod commission;
pub mod common;
pub mod futures;
pub mod futures_canary;
pub mod live_probe;
pub mod live_spot_allocator;
pub mod margin;
pub mod margin_canary;
pub mod prediction;
pub mod product_modules;
pub mod scope_audit;
pub mod settings;
pub mod spot_canary;
pub mod spot_common;
#[cfg(test)]
pub(crate) mod testutil;
pub mod transfer_canary;
pub mod transfers;
pub mod withdrawals;

pub use api_permissions::{
    ApiPermissionError, Capability, CapabilityStatus, Feature, InspectOptions, PermissionReport,
    SpotOrderTest, ensure_feature_allowed, ensure_spot_order_test_allowed, inspect_api_permissions,
};
pub use settings::{Settings, SettingsBuilder, SettingsError};
