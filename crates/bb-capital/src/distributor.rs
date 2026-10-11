//! Advisory cash distribution across the fleet (port of `src/cash_distributor.py`), computed by the
//! shared allocator with lifetime-P&L scoring and the untested-bot rule. Never moves funds.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use rust_decimal::Decimal;
use serde::Serialize;

use crate::allocator::{AllocationUnit, Scoring, build_plan};
use crate::config::CapitalPoolConfig;
use crate::scorecard::{LedgerStats, ModuleScorecard, read_ledger};
use crate::status::{BotKind, Fx, bot_spec, ledger_stats_for, signed};

/// Per-bot share bounds. Defaults reflect the live Binance fleet: trend_filter and
/// listing_sniper_momentum are the live bots (floor 30 %, cap 60 %); funding_arb earns a share only
/// through realised P&L (floor 0, cap 60 %) because it stays inert below its own $50 balance gate.
/// Paper bots are intentionally absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistributorConfig {
    pub bot_min_share_pct: BTreeMap<String, Decimal>,
    pub bot_max_share_pct: BTreeMap<String, Decimal>,
    /// Ignore smaller delta suggestions.
    pub rebalance_threshold_usdt: Decimal,
}

impl Default for DistributorConfig {
    fn default() -> Self {
        Self {
            bot_min_share_pct: BTreeMap::from([
                ("trend_filter".into(), Decimal::from(30)),
                ("listing_sniper_momentum".into(), Decimal::from(30)),
                ("funding_arb".into(), Decimal::ZERO),
            ]),
            bot_max_share_pct: BTreeMap::from([
                ("trend_filter".into(), Decimal::from(60)),
                ("listing_sniper_momentum".into(), Decimal::from(60)),
                ("funding_arb".into(), Decimal::from(60)),
            ]),
            rebalance_threshold_usdt: Decimal::from(5),
        }
    }
}

impl DistributorConfig {
    /// `(min, max)` share of a bot; unknown bots get 0 / 100.
    pub fn shares(&self, name: &str) -> (Decimal, Decimal) {
        (
            self.bot_min_share_pct
                .get(name)
                .copied()
                .unwrap_or_default(),
            self.bot_max_share_pct
                .get(name)
                .copied()
                .unwrap_or(Decimal::ONE_HUNDRED),
        )
    }

    /// Every configured bot's `(min, max)`.
    pub fn all_shares(&self) -> BTreeMap<String, (Decimal, Decimal)> {
        self.bot_min_share_pct
            .keys()
            .chain(self.bot_max_share_pct.keys())
            .map(|name| (name.clone(), self.shares(name)))
            .collect()
    }
}

/// One strategy's contribution to the distribution input.
#[derive(Debug, Clone, PartialEq)]
pub struct BotSnapshot {
    pub name: String,
    pub lifetime_pnl_usdt: Decimal,
    pub trades_lifetime: u64,
    /// Cash + positions attributed to this bot.
    pub current_value_usdt: Decimal,
    pub min_share_pct: Decimal,
    pub max_share_pct: Decimal,
}

impl BotSnapshot {
    /// Allocator unit view.
    pub fn unit(&self) -> AllocationUnit {
        AllocationUnit::new(ModuleScorecard {
            realized_pnl_usdt: self.lifetime_pnl_usdt,
            trades: self.trades_lifetime,
            ..ModuleScorecard::new(self.name.clone())
        })
        .with_shares(self.min_share_pct, self.max_share_pct)
    }
}

/// One bot's recommendation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Allocation {
    pub bot: String,
    pub score: f64,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub raw_share_pct: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub final_share_pct: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub target_value_usdt: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub current_value_usdt: Decimal,
    /// Positive = top up, negative = drain.
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub delta_usdt: Decimal,
}

/// Advisory plan.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DistributionPlan {
    pub timestamp: String,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub total_treasury_usdt: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub free_usdt: Decimal,
    pub allocations: Vec<Allocation>,
    pub notes: Vec<String>,
}

/// Distribute 100 % of the treasury: floors, P&L-weighted pool, caps with redistribution.
pub fn compute_plan(
    snapshots: &[BotSnapshot],
    total_treasury_usdt: Decimal,
    free_usdt: Decimal,
) -> DistributionPlan {
    let config = CapitalPoolConfig {
        enabled: true,
        reserve_pct: Decimal::ZERO,
        min_module_allocation_usdt: Decimal::ZERO,
        max_module_allocation_usdt: None,
    };
    let units: Vec<AllocationUnit> = snapshots.iter().map(BotSnapshot::unit).collect();
    let plan = build_plan(&config, total_treasury_usdt, &units, Scoring::LifetimePnl);
    let allocations = snapshots
        .iter()
        .filter_map(|snapshot| {
            let allocation = plan.allocation_for(&snapshot.name)?;
            let target = total_treasury_usdt * allocation.final_share_pct / Decimal::ONE_HUNDRED;
            Some(Allocation {
                bot: snapshot.name.clone(),
                score: allocation.score,
                raw_share_pct: allocation.raw_share_pct,
                final_share_pct: allocation.final_share_pct,
                target_value_usdt: target,
                current_value_usdt: snapshot.current_value_usdt,
                delta_usdt: target - snapshot.current_value_usdt,
            })
        })
        .collect();
    DistributionPlan {
        timestamp: bb_core::time::now().to_rfc3339(),
        total_treasury_usdt,
        free_usdt,
        allocations,
        notes: plan.notes,
    }
}

/// The live fleet the distributor plans for (pre-sunset defaults restored with bb-listing).
pub const DEFAULT_FLEET: [&str; 3] = ["trend_filter", "listing_sniper_momentum", "funding_arb"];

/// Ledger statistics of a fleet bot: `net_pnl_usdt` ledgers (trend-filter fill rows carry no
/// realised P&L), listing snipers through `bb-listing` (paper `pnl_usdt` + live `pnl_quote`).
pub fn fleet_ledger_stats(name: &str, data_dir: &Path, now_ms: i64) -> LedgerStats {
    match bot_spec(name) {
        Some(spec) if spec.kind == BotKind::ListingSniper => {
            ledger_stats_for(spec, data_dir, now_ms)
        }
        Some(spec) if !spec.ledger.is_empty() => {
            read_ledger(&data_dir.join(spec.ledger), &["net_pnl_usdt"], now_ms)
        }
        _ => LedgerStats::default(),
    }
}

/// Snapshots of [`DEFAULT_FLEET`].
pub fn build_default_snapshots(
    data_dir: &Path,
    current_values: &BTreeMap<String, Decimal>,
    config: &DistributorConfig,
) -> Vec<BotSnapshot> {
    let now = bb_core::time::now_ms();
    DEFAULT_FLEET
        .into_iter()
        .map(|name| {
            let stats = fleet_ledger_stats(name, data_dir, now);
            let (min_share_pct, max_share_pct) = config.shares(name);
            BotSnapshot {
                name: name.into(),
                lifetime_pnl_usdt: stats.pnl_usdt,
                trades_lifetime: stats.trades,
                current_value_usdt: current_values.get(name).copied().unwrap_or_default(),
                min_share_pct,
                max_share_pct,
            }
        })
        .collect()
}

/// Ledger-derived scorecards of [`DEFAULT_FLEET`] (for the capital pool).
pub fn fleet_scorecards(data_dir: &Path) -> BTreeMap<String, ModuleScorecard> {
    let now = bb_core::time::now_ms();
    DEFAULT_FLEET
        .into_iter()
        .map(|name| {
            let stats = fleet_ledger_stats(name, data_dir, now);
            (name.to_owned(), ModuleScorecard::from_ledger(name, &stats))
        })
        .collect()
}

/// Operator table.
pub fn render_plan(plan: &DistributionPlan, threshold_usdt: Decimal) -> String {
    let rule = "=".repeat(72);
    let mut out = String::new();
    let _ = writeln!(out, "{rule}");
    let _ = writeln!(out, "  CASH DISTRIBUTION PLAN  ({})", plan.timestamp);
    let _ = writeln!(out, "{rule}");
    let _ = writeln!(
        out,
        "  Treasury value:  ${:.4}",
        Fx(plan.total_treasury_usdt)
    );
    let _ = writeln!(out, "  Free USDT:       ${:.4}", Fx(plan.free_usdt));
    out.push('\n');
    if plan.allocations.is_empty() {
        out.push_str("  (no allocations)\n");
    } else {
        let header = format!(
            "  {:18}  {:>7}  {:>10}  {:>10}  {:>10}  ACTION",
            "BOT", "SHARE", "TARGET", "CURRENT", "DELTA"
        );
        let _ = writeln!(out, "{header}");
        let _ = writeln!(out, "  {}", "-".repeat(header.chars().count() - 2));
        for a in &plan.allocations {
            let action = if a.delta_usdt > threshold_usdt {
                format!("TOP UP +${:.2}", Fx(a.delta_usdt))
            } else if a.delta_usdt < -threshold_usdt {
                format!("DRAIN ${}", signed(a.delta_usdt, 2))
            } else {
                "hold".into()
            };
            let _ = writeln!(
                out,
                "  {:18}  {:>6}%  ${:>9}  ${:>9}  ${:>9}  {action}",
                a.bot,
                format!("{:.2}", Fx(a.final_share_pct)),
                format!("{:.4}", Fx(a.target_value_usdt)),
                format!("{:.4}", Fx(a.current_value_usdt)),
                signed(a.delta_usdt, 4),
            );
        }
    }
    if !plan.notes.is_empty() {
        out.push_str("\nNotes:\n");
        for note in &plan.notes {
            let _ = writeln!(out, "  - {note}");
        }
    }
    out.trim_end_matches('\n').to_owned()
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;
    use crate::allocator::tests::approx;

    fn snap(name: &str, pnl: Decimal, current: Decimal, min: Decimal, max: Decimal) -> BotSnapshot {
        BotSnapshot {
            name: name.into(),
            lifetime_pnl_usdt: pnl,
            trades_lifetime: 10,
            current_value_usdt: current,
            min_share_pct: min,
            max_share_pct: max,
        }
    }

    #[test]
    fn delta_computes_top_up_or_drain() {
        let snaps = [
            snap("a", dec!(50), dec!(10), dec!(20), dec!(80)),
            snap("b", dec!(50), dec!(90), dec!(20), dec!(80)),
        ];
        let plan = compute_plan(&snaps, dec!(100), dec!(100));
        approx(plan.allocations[0].delta_usdt, dec!(40));
        approx(plan.allocations[1].delta_usdt, dec!(-40));
    }

    #[test]
    fn plan_serialises_as_json_and_renders_actions() {
        let snaps = [snap("a", dec!(50), Decimal::ZERO, dec!(10), dec!(60))];
        let plan = compute_plan(&snaps, dec!(100), dec!(100));
        let decoded = serde_json::to_value(&plan).unwrap();
        assert_eq!(decoded["total_treasury_usdt"], serde_json::json!(100));
        assert_eq!(decoded["allocations"].as_array().unwrap().len(), 1);
        let text = render_plan(&plan, dec!(1));
        assert!(text.contains("BOT") && text.contains("SHARE"));
        assert!(text.contains("TOP UP +$60.00"));
        assert!(text.contains("Notes:"));
        let empty = compute_plan(&snaps, Decimal::ZERO, Decimal::ZERO);
        assert!(render_plan(&empty, dec!(5)).contains("(no allocations)"));
    }

    #[test]
    fn default_snapshots_read_live_fleet_ledgers_with_configured_shares() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("funding-arb-ledger.jsonl"),
            "{\"net_pnl_usdt\": 2}\n{\"net_pnl_usdt\": -1}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("trend-filter-ledger.jsonl"),
            "{\"action\": \"buy\"}\n",
        )
        .unwrap();
        let config = DistributorConfig::default();
        let snaps = build_default_snapshots(dir.path(), &BTreeMap::new(), &config);
        let names: Vec<&str> = snaps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, DEFAULT_FLEET);
        assert_eq!(
            (snaps[0].trades_lifetime, snaps[0].min_share_pct),
            (1, dec!(30))
        );
        assert_eq!(
            (snaps[1].trades_lifetime, snaps[1].min_share_pct),
            (0, dec!(30))
        );
        assert_eq!(
            (snaps[2].lifetime_pnl_usdt, snaps[2].max_share_pct),
            (dec!(1), dec!(60))
        );
        assert_eq!(config.shares("unknown"), (Decimal::ZERO, dec!(100)));
        // The momentum sniper never fired, so it is excluded; trend 30 % floor + 10 % overflow.
        let plan = compute_plan(&snaps, dec!(100), dec!(100));
        approx(plan.allocations[0].final_share_pct, dec!(40));
        approx(plan.allocations[1].final_share_pct, Decimal::ZERO);
        approx(plan.allocations[2].final_share_pct, dec!(60));
        assert_eq!(fleet_scorecards(dir.path())["funding_arb"].trades, 2);
    }
}
