//! Local API symbol/asset whitelist scope audit (port of `src/scope_audit.py`).
//!
//! Binance does not expose an API key's symbol whitelist, so the operator mirrors
//! it in `API_SYMBOL_WHITELIST` / `API_ASSET_WHITELIST`; every live-relevant
//! symbol or asset must appear there before a live path may run.

use std::collections::HashSet;

use bb_binance::normalize_symbol;
use serde::Serialize;

use crate::settings::{Settings, is_asset_symbol};

pub const ITEM_SYMBOL: &str = "symbol";
pub const ITEM_ASSET: &str = "asset";
pub const SOURCE_TRADING_SYMBOL: &str = "trading_symbol";
pub const SOURCE_POOL_ROUTE: &str = "pool_route";
pub const SOURCE_CANARY: &str = "canary";
pub const SOURCE_MARGIN: &str = "margin";
pub const SOURCE_FUTURES: &str = "futures";
pub const SOURCE_TRANSFER: &str = "transfer";
pub const SOURCE_WITHDRAWAL: &str = "withdrawal";
pub const SOURCE_PREDICTION: &str = "prediction";

/// Raised when local scope diagnostics cannot be evaluated or a scope is blocked.
#[derive(Debug, thiserror::Error)]
pub enum ScopeAuditError {
    #[error("API scope audit failed: {0}")]
    Blocked(String),
    #[error("invalid asset symbol: {0}")]
    InvalidAsset(String),
    #[error("unknown scope item kind: {0}")]
    UnknownKind(String),
    #[error(transparent)]
    Symbol(#[from] bb_binance::BinanceError),
}

/// One symbol or asset requiring local API scope approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScopeItem {
    pub kind: String,
    pub value: String,
    pub source: String,
    pub feature: String,
    pub live_relevant: bool,
}

impl ScopeItem {
    pub fn symbol(value: &str, source: &str, feature: &str) -> Result<Self, ScopeAuditError> {
        Ok(Self::new(
            ITEM_SYMBOL,
            normalize_symbol(value)?,
            source,
            feature,
        ))
    }

    pub fn asset(value: &str, source: &str, feature: &str) -> Result<Self, ScopeAuditError> {
        Ok(Self::new(ITEM_ASSET, asset_symbol(value)?, source, feature))
    }

    fn new(kind: &str, value: String, source: &str, feature: &str) -> Self {
        Self {
            kind: kind.to_owned(),
            value,
            source: source.to_owned(),
            feature: feature.to_owned(),
            live_relevant: true,
        }
    }
}

/// Evaluation result for one scope item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScopeCheck {
    pub item: ScopeItem,
    pub allowed: bool,
    pub reason: String,
}

/// Full local API scope report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeAuditReport {
    pub configured_symbols: Vec<String>,
    pub configured_assets: Vec<String>,
    pub checks: Vec<ScopeCheck>,
    pub binance_symbol_whitelist_visible: bool,
}

impl ScopeAuditReport {
    pub fn allowed(&self) -> bool {
        self.checks.iter().all(|check| check.allowed)
    }

    pub fn live_ready(&self) -> bool {
        self.allowed() && !self.configured_symbols.is_empty()
    }

    pub fn blocked_checks(&self) -> impl Iterator<Item = &ScopeCheck> {
        self.checks.iter().filter(|check| !check.allowed)
    }

    /// JSON identical to Python `ScopeAuditReport.as_dict()`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "configured_symbols": self.configured_symbols,
            "configured_assets": self.configured_assets,
            "binance_symbol_whitelist_visible": self.binance_symbol_whitelist_visible,
            "allowed": self.allowed(),
            "live_ready": self.live_ready(),
            "checks": self.checks,
        })
    }
}

/// Optional module inputs for [`build_scope_items`].
#[derive(Debug, Clone, Default)]
pub struct ScopeInputs<'a> {
    pub pool_route_symbols: &'a [String],
    pub canary_symbols: &'a [String],
    pub margin_symbols: &'a [String],
    pub futures_symbols: &'a [String],
    pub transfer_assets: &'a [String],
    pub withdrawal_assets: &'a [String],
    pub prediction_markets: &'a [String],
}

/// Build scope items from runtime settings and optional module inputs.
pub fn build_scope_items(
    settings: &Settings,
    inputs: &ScopeInputs<'_>,
) -> Result<Vec<ScopeItem>, ScopeAuditError> {
    let mut items = Vec::new();
    for symbol in &settings.trading_symbols {
        items.push(ScopeItem::symbol(
            symbol,
            SOURCE_TRADING_SYMBOL,
            "spot_trading",
        )?);
    }
    items.extend(symbol_items(
        inputs.pool_route_symbols,
        SOURCE_POOL_ROUTE,
        "paper_pool",
    )?);
    items.extend(symbol_items(
        inputs.canary_symbols,
        SOURCE_CANARY,
        "spot_live_canary",
    )?);
    items.extend(symbol_items(
        inputs.margin_symbols,
        SOURCE_MARGIN,
        "margin_trading",
    )?);
    items.extend(symbol_items(
        inputs.futures_symbols,
        SOURCE_FUTURES,
        "futures_trading",
    )?);
    items.extend(asset_items(
        inputs.transfer_assets,
        SOURCE_TRANSFER,
        "universal_transfer",
    )?);
    items.extend(asset_items(
        inputs.withdrawal_assets,
        SOURCE_WITHDRAWAL,
        "withdrawals",
    )?);
    items.extend(symbol_items(
        inputs.prediction_markets,
        SOURCE_PREDICTION,
        "prediction_trading",
    )?);
    Ok(dedupe_items(items))
}

/// Evaluate local symbol/asset scope from settings plus module inputs.
pub fn build_scope_audit_report(
    settings: &Settings,
    inputs: &ScopeInputs<'_>,
) -> Result<ScopeAuditReport, ScopeAuditError> {
    let items = build_scope_items(settings, inputs)?;
    report_for_items(settings, items)
}

/// Evaluate an explicit item list without contacting Binance.
pub fn report_for_items(
    settings: &Settings,
    items: Vec<ScopeItem>,
) -> Result<ScopeAuditReport, ScopeAuditError> {
    let checks = items
        .into_iter()
        .map(|item| evaluate_item(settings, item))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ScopeAuditReport {
        configured_symbols: settings.api_symbol_whitelist.clone(),
        configured_assets: settings.api_asset_whitelist.clone(),
        checks,
        binance_symbol_whitelist_visible: false,
    })
}

/// Fail closed unless every requested symbol and asset is locally allowed.
pub fn ensure_scope_allowed(
    report: &ScopeAuditReport,
) -> Result<&ScopeAuditReport, ScopeAuditError> {
    let details: Vec<String> = report
        .blocked_checks()
        .map(|check| {
            format!(
                "{} {} from {}: {}",
                check.item.kind, check.item.value, check.item.source, check.reason
            )
        })
        .collect();
    if details.is_empty() {
        Ok(report)
    } else {
        Err(ScopeAuditError::Blocked(details.join("; ")))
    }
}

/// Concise local scope report.
pub fn render_scope_report(report: &ScopeAuditReport) -> String {
    let join_or_none = |values: &[String]| {
        if values.is_empty() {
            "none".to_owned()
        } else {
            values.join(", ")
        }
    };
    let mut lines = vec![
        "API scope audit: local/operator whitelist".to_owned(),
        "Binance symbol whitelist visible: no".to_owned(),
        format!(
            "Configured symbols: {}",
            join_or_none(&report.configured_symbols)
        ),
        format!(
            "Configured assets: {}",
            join_or_none(&report.configured_assets)
        ),
        format!(
            "Live scope ready: {}",
            if report.live_ready() { "yes" } else { "no" }
        ),
        "Checks:".to_owned(),
    ];
    if report.checks.is_empty() {
        lines.push("  - none".to_owned());
    }
    for check in &report.checks {
        let verdict = if check.allowed { "allowed" } else { "blocked" };
        lines.push(format!(
            "  - {} {} ({}/{}): {verdict} - {}",
            check.item.kind, check.item.value, check.item.source, check.item.feature, check.reason
        ));
    }
    lines.join("\n")
}

/// `scope-audit` CLI body: settings items plus `--symbol`/`--asset` manual items.
pub fn cli_report(
    settings: &Settings,
    symbols: &[String],
    assets: &[String],
) -> Result<ScopeAuditReport, ScopeAuditError> {
    let mut items = build_scope_items(settings, &ScopeInputs::default())?;
    items.extend(symbol_items(symbols, "cli", "manual")?);
    items.extend(asset_items(assets, "cli", "manual")?);
    report_for_items(settings, dedupe_items(items))
}

fn evaluate_item(settings: &Settings, item: ScopeItem) -> Result<ScopeCheck, ScopeAuditError> {
    let (allowed, reason) = match item.kind.as_str() {
        ITEM_SYMBOL => {
            if settings.api_symbol_whitelist.is_empty() {
                (false, "no local API_SYMBOL_WHITELIST configured")
            } else if settings.api_symbol_whitelist.contains(&item.value) {
                (true, "symbol is in local whitelist")
            } else {
                (false, "symbol is not in local whitelist")
            }
        }
        ITEM_ASSET => {
            if settings.api_asset_whitelist.is_empty() {
                (false, "no local API_ASSET_WHITELIST configured")
            } else if settings.api_asset_whitelist.contains(&item.value) {
                (true, "asset is in local whitelist")
            } else {
                (false, "asset is not in local whitelist")
            }
        }
        other => return Err(ScopeAuditError::UnknownKind(other.to_owned())),
    };
    Ok(ScopeCheck {
        item,
        allowed,
        reason: reason.to_owned(),
    })
}

/// Symbol items for non-blank inputs.
pub fn symbol_items(
    symbols: &[String],
    source: &str,
    feature: &str,
) -> Result<Vec<ScopeItem>, ScopeAuditError> {
    symbols
        .iter()
        .filter(|symbol| !symbol.trim().is_empty())
        .map(|symbol| ScopeItem::symbol(symbol, source, feature))
        .collect()
}

/// Asset items for non-blank inputs.
pub fn asset_items(
    assets: &[String],
    source: &str,
    feature: &str,
) -> Result<Vec<ScopeItem>, ScopeAuditError> {
    assets
        .iter()
        .filter(|asset| !asset.trim().is_empty())
        .map(|asset| ScopeItem::asset(asset, source, feature))
        .collect()
}

/// Drop repeated `(kind, value, source, feature)` items, keeping first occurrence.
pub fn dedupe_items(items: Vec<ScopeItem>) -> Vec<ScopeItem> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|item| {
            seen.insert((
                item.kind.clone(),
                item.value.clone(),
                item.source.clone(),
                item.feature.clone(),
            ))
        })
        .collect()
}

fn asset_symbol(value: &str) -> Result<String, ScopeAuditError> {
    let asset = value.trim().to_uppercase();
    if !is_asset_symbol(&asset) {
        return Err(ScopeAuditError::InvalidAsset(value.to_owned()));
    }
    Ok(asset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SettingsBuilder;

    fn settings(symbols: &[&str], assets: &[&str]) -> Settings {
        let mut builder = SettingsBuilder::new("key", "secret");
        builder.trading_symbols = vec!["BTC/USDT".into()];
        builder.api_symbol_whitelist = symbols.iter().map(|s| (*s).to_owned()).collect();
        builder.api_asset_whitelist = assets.iter().map(|s| (*s).to_owned()).collect();
        builder.build().unwrap()
    }

    #[test]
    fn missing_whitelist_blocks_live_scope() {
        let report =
            build_scope_audit_report(&settings(&[], &[]), &ScopeInputs::default()).unwrap();
        assert!(!report.allowed());
        assert!(!report.live_ready());
        assert_eq!(
            report.checks[0].reason,
            "no local API_SYMBOL_WHITELIST configured"
        );
        let err = ensure_scope_allowed(&report).unwrap_err().to_string();
        assert_eq!(
            err,
            "API scope audit failed: symbol BTC/USDT from trading_symbol: no local API_SYMBOL_WHITELIST configured"
        );
    }

    #[test]
    fn whitelisted_symbols_and_assets_pass() {
        let settings = settings(&["BTC/USDT", "BNB/USDT"], &["USDT"]);
        let canary = vec!["bnbusdt".to_owned()];
        let transfer = vec!["usdt".to_owned()];
        let report = build_scope_audit_report(
            &settings,
            &ScopeInputs {
                canary_symbols: &canary,
                transfer_assets: &transfer,
                ..ScopeInputs::default()
            },
        )
        .unwrap();
        assert!(report.allowed());
        assert!(report.live_ready());
        assert!(ensure_scope_allowed(&report).is_ok());
        assert_eq!(report.checks[1].item.value, "BNB/USDT");
        assert_eq!(report.checks[1].item.feature, "spot_live_canary");
        assert_eq!(report.checks[2].item.kind, "asset");
        let json = report.to_json();
        assert_eq!(json["live_ready"], true);
        assert_eq!(json["checks"][0]["item"]["live_relevant"], true);
    }

    #[test]
    fn non_whitelisted_items_are_blocked_and_rendered() {
        let settings = settings(&["BTC/USDT"], &["BTC"]);
        let report =
            cli_report(&settings, &["ETH/USDT".into(), " ".into()], &["bnb".into()]).unwrap();
        assert_eq!(report.checks.len(), 3);
        assert_eq!(report.checks[1].reason, "symbol is not in local whitelist");
        assert_eq!(report.checks[2].reason, "asset is not in local whitelist");
        let text = render_scope_report(&report);
        assert!(text.contains("Live scope ready: no"));
        assert!(text.contains(
            "  - symbol ETH/USDT (cli/manual): blocked - symbol is not in local whitelist"
        ));
    }

    #[test]
    fn dedupes_and_rejects_invalid_assets() {
        let settings = settings(&[], &[]);
        let report = cli_report(&settings, &["BTC/USDT".into(), "btcusdt".into()], &[]).unwrap();
        assert_eq!(report.checks.len(), 2);
        assert!(matches!(
            cli_report(&settings, &[], &["B$D".into()]),
            Err(ScopeAuditError::InvalidAsset(_))
        ));
    }
}
