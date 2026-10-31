//! Sleeve holdings (Phase 53 D3): what each sleeve of the rule owns, persisted in the state
//! file, reconciled against the wallet every tick, and the `sleeve_mismatch` halt.
//!
//! - **Ownership.** Each sleeve owns at most one asset (`asset`, `qty`, `opened_at`). The
//!   rule decides only on what its sleeves own: other wallet balances (a BNB fee balance, a
//!   manual buy) are unowned, never sold, never counted as held or as portfolio value.
//! - **Strict load** ([`crate::state::TraderState::load_strict`]): a missing state file or
//!   one without `sleeves` (written before Phase 53) is migrated once; a state file that
//!   cannot be read or whose `sleeves` is malformed halts (fail closed).
//! - **Migration** ([`migrate`]): a single non-USDT, non-cash-like free balance worth at
//!   least its USDT market's minimum order is adopted by the trading sleeve (today's
//!   behaviour); several such balances: none adopted, listed as `unowned`.
//! - **Reconcile** ([`reconcile`]): per asset, the sleeves' quantities must not exceed the
//!   free balance plus one step of its USDT market (dust tolerance), else halt.
//! - **Halt** ([`sleeve_halt`]): a `sleeve_mismatch` decision row halts the trader (no
//!   decisions, no orders; `execute_route` refuses in both modes) until a later
//!   `sleeves_reviewed` row from `bb bot-spot --mark-sleeves-reviewed`, which re-reads the
//!   balances and is refused unless the state reconciles now. The halt lives in the audit
//!   log, so it survives restarts, and no tick writes a review row.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::audit::{AuditLog, AuditRow, Decision, RowStatus, RowType};
use crate::config::TraderConfig;
use crate::error::{Result, TraderError};
use crate::memory::AuditRows;
use crate::rules::Rule;
use crate::state::TraderState;
use crate::universe::{Balances, MarketView};

/// `sleeve_check.kind` of a halt row.
pub const HALT_KIND: &str = "sleeve_mismatch";
/// `sleeve_check.kind` of the review row that clears it.
pub const REVIEW_KIND: &str = "sleeves_reviewed";
/// Default `--review-reason` of `--mark-sleeves-reviewed`.
pub const DEFAULT_SLEEVE_REVIEW_REASON: &str =
    "operator reviewed the sleeve holdings against the wallet";

/// One sleeve's position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Holding {
    pub asset: String,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub qty: Decimal,
    pub opened_at: String,
}

/// The sleeves' holdings (the `sleeves` object of the state file).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SleeveState {
    /// Holding per sleeve name.
    pub holdings: BTreeMap<String, Holding>,
    /// Balances the migration left alone (several candidates), for the record.
    #[serde(default)]
    pub unowned: Vec<String>,
    /// When the state was first written (migration time).
    pub since: String,
}

impl SleeveState {
    /// Every sleeve's quantity summed per asset.
    pub fn owned(&self) -> BTreeMap<String, Decimal> {
        let mut out: BTreeMap<String, Decimal> = BTreeMap::new();
        for holding in self.holdings.values() {
            *out.entry(holding.asset.clone()).or_default() += holding.qty;
        }
        out
    }

    /// Holdings that do not fit `rule`: an unknown sleeve, a bad asset name or quantity.
    pub fn validate(&self, rule: &Rule) -> std::result::Result<(), String> {
        for (sleeve, holding) in &self.holdings {
            if !rule.sleeves.iter().any(|s| &s.name == sleeve) {
                return Err(format!(
                    "the state holds {} for sleeve {sleeve}, which the rule file does not have",
                    holding.asset
                ));
            }
            let ok_name = !holding.asset.is_empty()
                && holding
                    .asset
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
            if !ok_name || holding.qty <= Decimal::ZERO {
                return Err(format!(
                    "sleeve {sleeve}: bad holding {} {}",
                    holding.asset, holding.qty
                ));
            }
        }
        Ok(())
    }

    /// Book a filled buy: `sleeve` now owns `qty` of `asset` (replacing what it owned).
    pub fn book_buy(&mut self, sleeve: &str, asset: &str, qty: Decimal, at: &str) {
        self.holdings.insert(
            sleeve.to_owned(),
            Holding {
                asset: asset.to_owned(),
                qty,
                opened_at: at.to_owned(),
            },
        );
    }

    /// Book a filled sell of `sleeve`'s whole holding.
    pub fn book_sell(&mut self, sleeve: &str) {
        self.holdings.remove(sleeve);
    }
}

/// Free balances the migration may adopt: not USDT, not cash-like, worth at least the
/// minimum order of their USDT market.
fn positions(balances: &Balances, view: &MarketView) -> Vec<(String, Decimal)> {
    balances
        .free
        .iter()
        .filter(|(asset, _)| *asset != "USDT" && !crate::assets::is_low_upside(asset))
        .filter(|(asset, qty)| {
            let value = view.usdt_value(asset, **qty);
            let min = view
                .markets
                .get(&format!("{asset}/USDT"))
                .map(|m| m.min_notional);
            matches!((value, min), (Some(v), Some(m)) if v > Decimal::ZERO && v >= m)
        })
        .map(|(asset, qty)| (asset.clone(), *qty))
        .collect()
}

/// First start without sleeve state (see the module docs).
pub fn migrate(rule: &Rule, balances: &Balances, view: &MarketView, at: &str) -> SleeveState {
    let mut state = SleeveState {
        since: at.to_owned(),
        ..SleeveState::default()
    };
    match positions(balances, view).as_slice() {
        [] => {}
        [(asset, qty)] => state.book_buy(&rule.trading_sleeve().name, asset, *qty, at),
        several => state.unowned = several.iter().map(|(a, _)| a.clone()).collect(),
    }
    state
}

/// The sleeves' quantities against the free balances (see the module docs); the reason
/// when an asset is short.
pub fn reconcile(
    state: &SleeveState,
    balances: &Balances,
    view: &MarketView,
) -> std::result::Result<(), String> {
    for (asset, owned) in state.owned() {
        let free = balances.free(&asset);
        let step = view
            .markets
            .get(&format!("{asset}/USDT"))
            .and_then(|m| m.order_step())
            .unwrap_or_default();
        if owned > free + step {
            return Err(format!(
                "sleeves own {owned} {asset} but the wallet has {free} free (dust tolerance {step})"
            ));
        }
    }
    Ok(())
}

fn kind(row: &Value) -> Option<&str> {
    row.get("sleeve_check")?.get("kind")?.as_str()
}

/// The reason of the last `sleeve_mismatch` row no review row follows (an unreadable
/// history is [`sleeve_halt`]'s business).
pub fn standing_reason(rows: &AuditRows) -> Option<String> {
    let last = rows.rows.iter().rposition(|r| kind(r) == Some(HALT_KIND))?;
    if rows.rows[last + 1..]
        .iter()
        .any(|r| kind(r) == Some(REVIEW_KIND))
    {
        return None;
    }
    Some(
        rows.rows[last]["sleeve_check"]["reason"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    )
}

/// Hard safety: why the trader is halted on its sleeves, if it is (see the module docs). An
/// unreadable audit history halts too.
pub fn sleeve_halt(rows: &AuditRows) -> Option<String> {
    if let Some(error) = &rows.error {
        return Some(format!(
            "could not read Bot Spot audit history: {error}; sleeve holdings cannot be checked"
        ));
    }
    standing_reason(rows).map(|reason| {
        format!("{HALT_KIND}: {reason}; holding until `bb bot-spot --mark-sleeves-reviewed`")
    })
}

/// The halt row: a blocked decision row carrying `sleeve_check` (kind [`HALT_KIND`]).
pub fn halt_row(config: &TraderConfig, reason: &str, detail: Value) -> AuditRow {
    let mut row = AuditRow::new(
        config,
        RowType::CycleSummary,
        RowStatus::Blocked,
        Decision::hold(format!(
            "{HALT_KIND}: {reason}; no decisions or orders until `bb bot-spot --mark-sleeves-reviewed`"
        )),
    );
    row.error = format!("{HALT_KIND}: {reason}");
    let mut check = json!({"kind": HALT_KIND, "reason": reason});
    if let (Some(check), Value::Object(detail)) = (check.as_object_mut(), detail) {
        check.extend(detail);
    }
    row.sleeve_check = Some(check);
    row
}

/// `bb bot-spot --mark-sleeves-reviewed`: with a sleeve halt standing, `balances` (just
/// re-read) and `view`, append the review row when the state file loads and reconciles;
/// refuse otherwise (the state is never rewritten here).
pub fn mark_sleeves_reviewed(
    config: &TraderConfig,
    balances: &Balances,
    view: &MarketView,
    reason: &str,
) -> Result<AuditRow> {
    let log = AuditLog::new(&config.audit_path);
    let rows = log.rows();
    if let Some(error) = &rows.error {
        return Err(TraderError::safety(format!(
            "could not read Bot Spot audit history: {error}"
        )));
    }
    let halt = sleeve_halt(&rows)
        .ok_or_else(|| TraderError::safety("no sleeve_mismatch halt needs review"))?;
    let state = TraderState::load_strict(&config.state_path).map_err(|e| {
        TraderError::safety(format!(
            "sleeve state unreadable: {e}; fix or restore the state file first"
        ))
    })?;
    let sleeves = state.sleeves.unwrap_or_default();
    sleeves
        .validate(&config.algo_rule)
        .and_then(|()| reconcile(&sleeves, balances, view))
        .map_err(|e| {
            TraderError::safety(format!(
                "the sleeves still do not reconcile: {e}; fix the wallet or the state file first"
            ))
        })?;
    let free: BTreeMap<String, String> = sleeves
        .owned()
        .keys()
        .map(|asset| (asset.clone(), balances.free(asset).normalize().to_string()))
        .collect();
    let mut row = AuditRow::new(
        config,
        RowType::Review,
        RowStatus::Reviewed,
        Decision::hold(reason),
    );
    row.sleeve_check = Some(json!({
        "kind": REVIEW_KIND,
        "reason": reason,
        "halt": halt,
        "holdings": sleeves.holdings,
        "free": free,
    }));
    log.append(&row)?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;
    use crate::universe::tests::{config, view};

    fn balances(free: &[(&str, Decimal)]) -> Balances {
        Balances {
            free: free.iter().map(|(a, q)| ((*a).to_owned(), *q)).collect(),
            used: Default::default(),
            total: free.iter().map(|(a, q)| ((*a).to_owned(), *q)).collect(),
        }
    }

    fn holding(asset: &str, qty: Decimal) -> Holding {
        Holding {
            asset: asset.into(),
            qty,
            opened_at: "t".into(),
        }
    }

    #[test]
    fn migration_adopts_one_position_and_leaves_several_unowned() {
        let config = config(&[]);
        let view = view(&config);
        let rule = &config.algo_rule;
        // ETH worth 150 USDT; USDT and a cash-like coin are never positions; dust under the
        // minimum order is not one either.
        let one = balances(&[
            ("USDT", dec!(100)),
            ("ETH", dec!(0.05)),
            ("USDC", dec!(50)),
            ("BTC", dec!(0.000000000001)),
        ]);
        let state = migrate(rule, &one, &view, "2026-10-31T13:00:00Z");
        assert_eq!(
            state.holdings["fast"],
            Holding {
                asset: "ETH".into(),
                qty: dec!(0.05),
                opened_at: "2026-10-31T13:00:00Z".into()
            }
        );
        assert!(state.unowned.is_empty());
        // Two positions: none adopted, both listed.
        let two = balances(&[("ETH", dec!(0.05)), ("BTC", dec!(0.01))]);
        let state = migrate(rule, &two, &view, "t");
        assert!(state.holdings.is_empty());
        assert_eq!(state.unowned, ["BTC", "ETH"]);
        // Nothing: an empty, valid state.
        assert_eq!(
            migrate(rule, &balances(&[("USDT", dec!(17.4))]), &view, "t")
                .holdings
                .len(),
            0
        );
    }

    #[test]
    fn reconcile_sums_sleeves_per_asset_within_one_step_of_dust() {
        let config = config(&[]);
        let view = view(&config);
        let step = view.markets["ETH/USDT"].order_step().unwrap();
        assert!(step > Decimal::ZERO);
        // Two sleeves own ETH: 0.03 + 0.02 against 0.05 free.
        let mut state = SleeveState::default();
        state
            .holdings
            .insert("fast".into(), holding("ETH", dec!(0.03)));
        state
            .holdings
            .insert("core".into(), holding("ETH", dec!(0.02)));
        assert!(reconcile(&state, &balances(&[("ETH", dec!(0.05))]), &view).is_ok());
        // Short by exactly one step (fees in the base asset): still fine; one unit more: halt.
        let at_edge = dec!(0.05) - step;
        assert!(reconcile(&state, &balances(&[("ETH", at_edge)]), &view).is_ok());
        let err = reconcile(
            &state,
            &balances(&[("ETH", at_edge - dec!(0.00000001))]),
            &view,
        )
        .unwrap_err();
        assert!(
            err.contains("sleeves own 0.05 ETH") && err.contains(&format!("dust tolerance {step}")),
            "{err}"
        );
        // An unowned balance (more ETH, a BNB fee balance) never makes a mismatch.
        assert!(
            reconcile(
                &state,
                &balances(&[("ETH", dec!(3)), ("BNB", dec!(1))]),
                &view
            )
            .is_ok()
        );
        // Both sleeves are in the shipped rule; a sleeve the rule file lacks is invalid.
        assert!(state.validate(&config.algo_rule).is_ok());
        state
            .holdings
            .insert("slow".into(), holding("ETH", dec!(0.01)));
        let err = state.validate(&config.algo_rule).unwrap_err();
        assert!(err.contains("sleeve slow"), "{err}");
    }

    #[test]
    fn a_halt_stands_until_a_review_row_follows_it() {
        let row =
            |kind: &str| json!({"sleeve_check": {"kind": kind, "reason": "sleeves own 1 ETH"}});
        let rows = |r: Vec<Value>| AuditRows {
            rows: r,
            error: None,
        };
        assert_eq!(sleeve_halt(&rows(vec![json!({"status": "planned"})])), None);
        let halted = sleeve_halt(&rows(vec![row(HALT_KIND), json!({"status": "held"})])).unwrap();
        assert!(
            halted.contains("sleeves own 1 ETH") && halted.contains("--mark-sleeves-reviewed"),
            "{halted}"
        );
        assert_eq!(
            sleeve_halt(&rows(vec![row(HALT_KIND), row(REVIEW_KIND)])),
            None
        );
        // A new mismatch after a review halts again.
        assert!(
            sleeve_halt(&rows(vec![
                row(HALT_KIND),
                row(REVIEW_KIND),
                row(HALT_KIND)
            ]))
            .is_some()
        );
        let unreadable = AuditRows {
            rows: Vec::new(),
            error: Some("bad line".into()),
        };
        assert!(sleeve_halt(&unreadable).unwrap().contains("could not read"));
    }
}
