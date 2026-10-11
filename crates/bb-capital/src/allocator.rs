//! The one deterministic allocator: reserve → share floors → score-weighted performance pool →
//! share caps (per unit and the global USDT ceiling) with overflow redistribution → USDT budgets.
//!
//! Merges the refactor `DeterministicAllocationAdvisor` (reserve %, global min/max USDT, scorecard
//! weights) with `src/cash_distributor.compute_plan` (per-bot floor/ceiling shares, iterative cap
//! redistribution, lifetime-P&L scoring with the untested-bot rule).

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use bb_core::money::from_f64;
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};

use crate::config::CapitalPoolConfig;
use crate::scorecard::ModuleScorecard;

const HUNDRED: Decimal = Decimal::ONE_HUNDRED;
/// Overflow below this share (%) is considered placed.
const OVERFLOW_EPSILON: Decimal = Decimal::from_parts(1, 0, 0, false, 9);
/// Bounded redistribution passes (as in the Python distributor).
const MAX_CAP_PASSES: usize = 10;
/// USDT budgets are truncated to this many decimals so they never exceed their bound.
const BUDGET_DP: u32 = 8;

/// How units compete for the performance pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scoring {
    /// Refactor scorecard multiplier (`max(score, 0.1)`); untested units weigh 1.0.
    #[default]
    Scorecard,
    /// Cash-distributor rule: weight `max(lifetime P&L, 0)`; units with zero lifetime trades get
    /// no share at all unless every unit is untested (bootstrap: configured floors, equal split).
    LifetimePnl,
}

impl Scoring {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scorecard => "scorecard",
            Self::LifetimePnl => "lifetime-pnl",
        }
    }
}

impl fmt::Display for Scoring {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Scoring {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "scorecard" => Ok(Self::Scorecard),
            "lifetime-pnl" => Ok(Self::LifetimePnl),
            other => Err(format!(
                "unknown scoring {other:?} (scorecard or lifetime-pnl)"
            )),
        }
    }
}

/// One allocatable module / bot.
#[derive(Debug, Clone, PartialEq)]
pub struct AllocationUnit {
    pub scorecard: ModuleScorecard,
    /// Floor share of the allocatable pool (%).
    pub min_share_pct: Decimal,
    /// Ceiling share of the allocatable pool (%).
    pub max_share_pct: Decimal,
}

impl AllocationUnit {
    /// Unit without floor (0 %) or per-unit ceiling (100 %).
    pub fn new(scorecard: ModuleScorecard) -> Self {
        Self {
            scorecard,
            min_share_pct: Decimal::ZERO,
            max_share_pct: HUNDRED,
        }
    }

    pub fn with_shares(mut self, min_share_pct: Decimal, max_share_pct: Decimal) -> Self {
        self.min_share_pct = min_share_pct;
        self.max_share_pct = max_share_pct;
        self
    }

    pub fn module_id(&self) -> &str {
        &self.scorecard.module_id
    }
}

/// `active | reduced | paused`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AllocationStatus {
    Active,
    Reduced,
    Paused,
}

impl AllocationStatus {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "active" => Some(Self::Active),
            "reduced" => Some(Self::Reduced),
            "paused" => Some(Self::Paused),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Reduced => "reduced",
            Self::Paused => "paused",
        }
    }
}

/// Budget assigned to one module.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModuleAllocation {
    pub module_id: String,
    pub status: AllocationStatus,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub max_usdt: Decimal,
    pub score: f64,
    pub reason: String,
    /// Share the floors + score weighting asked for (%).
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision")]
    pub raw_share_pct: Decimal,
    /// Share after caps and redistribution (%).
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision")]
    pub final_share_pct: Decimal,
}

/// Full pool budget for one decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllocationPlan {
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub total_equity_usdt: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub reserve_usdt: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub allocatable_usdt: Decimal,
    #[serde(default)]
    pub allocations: Vec<ModuleAllocation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl AllocationPlan {
    pub fn allocation_for(&self, module_id: &str) -> Option<&ModuleAllocation> {
        let wanted = module_id.trim();
        self.allocations.iter().find(|a| a.module_id == wanted)
    }

    fn empty(
        total_equity: Decimal,
        reserve: Decimal,
        allocatable: Decimal,
        note: Option<&str>,
    ) -> Self {
        Self {
            total_equity_usdt: total_equity,
            reserve_usdt: reserve,
            allocatable_usdt: allocatable,
            allocations: Vec::new(),
            notes: note.map(|n| vec![n.to_owned()]).unwrap_or_default(),
        }
    }
}

/// Truncated USDT budget.
pub(crate) fn budget(value: Decimal) -> Decimal {
    value.round_dp_with_strategy(BUDGET_DP, RoundingStrategy::ToZero)
}

fn ratio(numerator: f64, denominator: f64) -> Decimal {
    if denominator > 0.0 {
        from_f64(numerator / denominator).unwrap_or_default()
    } else {
        Decimal::ZERO
    }
}

/// Deterministic plan. `config.enabled` is ignored (leases check it).
pub fn build_plan(
    config: &CapitalPoolConfig,
    total_equity_usdt: Decimal,
    units: &[AllocationUnit],
    scoring: Scoring,
) -> AllocationPlan {
    let equity = total_equity_usdt.max(Decimal::ZERO);
    let reserve = equity * config.reserve_pct.clamp(Decimal::ZERO, HUNDRED) / HUNDRED;
    let allocatable = (equity - reserve).max(Decimal::ZERO);

    let mut seen = BTreeSet::new();
    let units: Vec<&AllocationUnit> = units
        .iter()
        .filter(|u| {
            !u.module_id().trim().is_empty() && seen.insert(u.module_id().trim().to_owned())
        })
        .collect();
    if equity <= Decimal::ZERO {
        return AllocationPlan::empty(
            equity,
            reserve,
            allocatable,
            Some("treasury is empty; no allocations possible"),
        );
    }
    if units.is_empty() {
        return AllocationPlan::empty(equity, reserve, allocatable, Some("no bots configured"));
    }
    if allocatable <= Decimal::ZERO {
        return AllocationPlan::empty(
            equity,
            reserve,
            allocatable,
            Some("reserve leaves nothing to allocate; no allocations possible"),
        );
    }

    let mut notes = Vec::new();
    let bootstrap = units.iter().all(|u| u.scorecard.trades == 0);
    let untested: Vec<bool> = units
        .iter()
        .map(|u| scoring == Scoring::LifetimePnl && !bootstrap && u.scorecard.trades == 0)
        .collect();
    let excluded: Vec<&str> = units
        .iter()
        .zip(&untested)
        .filter(|(_, untested)| **untested)
        .map(|(u, _)| u.module_id())
        .collect();
    if !excluded.is_empty() {
        notes.push(format!(
            "untested bots excluded (0 lifetime trades): {}",
            excluded.join(", ")
        ));
    }

    let eff_min: Vec<Decimal> = units
        .iter()
        .zip(&untested)
        .map(|(u, untested)| {
            if *untested {
                Decimal::ZERO
            } else {
                u.min_share_pct.max(Decimal::ZERO)
            }
        })
        .collect();
    let usd_cap_share = config
        .max_module_allocation_usdt
        .map(|max| max.max(Decimal::ZERO) / allocatable * HUNDRED);
    let caps: Vec<Decimal> = units
        .iter()
        .zip(&untested)
        .map(|(u, untested)| {
            let unit_cap = if *untested {
                Decimal::ZERO
            } else {
                u.max_share_pct.max(Decimal::ZERO)
            };
            usd_cap_share.map_or(unit_cap, |usd| unit_cap.min(usd))
        })
        .collect();
    let weights: Vec<f64> = units
        .iter()
        .zip(&untested)
        .map(|(u, untested)| match scoring {
            Scoring::Scorecard => u.scorecard.score().max(0.1),
            Scoring::LifetimePnl if *untested => 0.0,
            Scoring::LifetimePnl => bb_core::money::to_f64(u.scorecard.realized_pnl_usdt).max(0.0),
        })
        .collect();

    // Floors: the performance pool is what the floors leave over.
    let sum_min: Decimal = eff_min.iter().sum();
    let (mins, pool_pct) = if sum_min > HUNDRED {
        let scale = HUNDRED / sum_min;
        notes.push(format!(
            "configured min shares sum to {:.1}% > 100; min shares renormalised by {:.3} and performance pool removed",
            crate::status::Fx(sum_min),
            crate::status::Fx(scale)
        ));
        (
            eff_min.iter().map(|m| *m * scale).collect::<Vec<_>>(),
            Decimal::ZERO,
        )
    } else {
        (eff_min.clone(), HUNDRED - sum_min)
    };

    // Score-weighted performance pool; equal split among eligible units when nobody scores.
    let total_weight: f64 = weights.iter().sum();
    let eligible = untested.iter().filter(|u| !**u).count();
    let n_eligible = Decimal::from(if eligible == 0 { units.len() } else { eligible });
    let raw: Vec<Decimal> = (0..units.len())
        .map(|i| {
            let bonus = if pool_pct <= Decimal::ZERO || untested[i] {
                Decimal::ZERO
            } else if total_weight <= 0.0 {
                pool_pct / n_eligible
            } else {
                pool_pct * ratio(weights[i], total_weight)
            };
            mins[i] + bonus
        })
        .collect();

    // Caps with bounded redistribution of the overflow by remaining headroom.
    let mut shares = raw.clone();
    for _ in 0..MAX_CAP_PASSES {
        let mut overflow = Decimal::ZERO;
        let mut capped = vec![false; shares.len()];
        for i in 0..shares.len() {
            if shares[i] > caps[i] {
                overflow += shares[i] - caps[i];
                shares[i] = caps[i];
                capped[i] = true;
            }
        }
        if overflow < OVERFLOW_EPSILON {
            break;
        }
        let open: Vec<usize> = (0..shares.len())
            .filter(|i| !capped[*i] && shares[*i] < caps[*i])
            .collect();
        if open.is_empty() {
            notes.push(format!(
                "overflow {:.2}% could not be redistributed; all eligible bots already at max",
                crate::status::Fx(overflow)
            ));
            break;
        }
        let total_room: Decimal = open.iter().map(|i| caps[*i] - shares[*i]).sum();
        if total_room > Decimal::ZERO {
            for i in open {
                let room = caps[i] - shares[i];
                shares[i] += overflow * room / total_room;
            }
        }
    }

    let min_usdt = config.min_module_allocation_usdt.max(Decimal::ZERO);
    let allocations = units
        .iter()
        .enumerate()
        .map(|(i, unit)| {
            let mut max_usdt = budget(allocatable * shares[i] / HUNDRED);
            if let Some(cap) = config.max_module_allocation_usdt {
                max_usdt = max_usdt.min(cap);
            }
            let (status, reason) = if untested[i] {
                max_usdt = Decimal::ZERO;
                (AllocationStatus::Paused, "untested_bot_excluded")
            } else if max_usdt <= Decimal::ZERO || (min_usdt > Decimal::ZERO && max_usdt < min_usdt)
            {
                max_usdt = Decimal::ZERO;
                (AllocationStatus::Paused, "below_minimum_allocation")
            } else if shares[i] + OVERFLOW_EPSILON < raw[i] {
                (AllocationStatus::Reduced, "module_cap")
            } else {
                (AllocationStatus::Active, "score_weighted")
            };
            ModuleAllocation {
                module_id: unit.module_id().trim().to_owned(),
                status,
                max_usdt,
                score: weights[i],
                reason: reason.into(),
                raw_share_pct: raw[i],
                final_share_pct: shares[i],
            }
        })
        .collect();

    AllocationPlan {
        total_equity_usdt: equity,
        reserve_usdt: reserve,
        allocatable_usdt: allocatable,
        allocations,
        notes,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    pub(crate) fn approx(actual: Decimal, expected: Decimal) {
        assert!(
            (actual - expected).abs() < dec!(0.000001),
            "{actual} != {expected}"
        );
    }

    fn tested(name: &str, pnl: Decimal, trades: u64, min: Decimal, max: Decimal) -> AllocationUnit {
        AllocationUnit::new(ModuleScorecard {
            realized_pnl_usdt: pnl,
            trades,
            ..ModuleScorecard::new(name)
        })
        .with_shares(min, max)
    }

    fn distributor() -> CapitalPoolConfig {
        CapitalPoolConfig {
            enabled: true,
            reserve_pct: Decimal::ZERO,
            min_module_allocation_usdt: Decimal::ZERO,
            max_module_allocation_usdt: None,
        }
    }

    fn shares(plan: &AllocationPlan, id: &str) -> Decimal {
        plan.allocation_for(id).unwrap().final_share_pct
    }

    #[test]
    fn reserve_is_held_back_and_global_cap_reduces_modules() {
        let config = CapitalPoolConfig {
            enabled: true,
            reserve_pct: dec!(40),
            min_module_allocation_usdt: Decimal::ZERO,
            max_module_allocation_usdt: Some(dec!(25)),
        };
        let units = [
            AllocationUnit::new(ModuleScorecard {
                realized_pnl_usdt: dec!(12),
                trades: 10,
                wins: 7,
                losses: 3,
                ..ModuleScorecard::new("demo-sma:BTC/USDT")
            }),
            AllocationUnit::new(ModuleScorecard::new("demo-sma:ETH/USDT")),
        ];
        let plan = build_plan(&config, dec!(100), &units, Scoring::Scorecard);
        assert_eq!(plan.reserve_usdt, dec!(40));
        assert_eq!(plan.allocatable_usdt, dec!(60));
        for allocation in &plan.allocations {
            assert_eq!(allocation.max_usdt, dec!(25));
            assert_eq!(allocation.status, AllocationStatus::Reduced);
            assert_eq!(allocation.reason, "module_cap");
        }
        assert!(
            plan.notes
                .iter()
                .any(|n| n.contains("could not be redistributed"))
        );
    }

    #[test]
    fn capped_overflow_is_redistributed_to_modules_with_room() {
        let config = CapitalPoolConfig {
            reserve_pct: Decimal::ZERO,
            max_module_allocation_usdt: None,
            ..CapitalPoolConfig::default()
        };
        let units = [
            AllocationUnit::new(ModuleScorecard::new("a")).with_shares(Decimal::ZERO, dec!(20)),
            AllocationUnit::new(ModuleScorecard::new("b")),
        ];
        let plan = build_plan(&config, dec!(100), &units, Scoring::Scorecard);
        approx(plan.allocation_for("a").unwrap().max_usdt, dec!(20));
        approx(plan.allocation_for("b").unwrap().max_usdt, dec!(80));
        assert_eq!(
            plan.allocation_for("b").unwrap().status,
            AllocationStatus::Active
        );
    }

    #[test]
    fn minimum_allocation_pauses_small_modules_and_duplicates_collapse() {
        let config = CapitalPoolConfig {
            reserve_pct: Decimal::ZERO,
            min_module_allocation_usdt: dec!(40),
            max_module_allocation_usdt: None,
            ..CapitalPoolConfig::default()
        };
        let units = [
            AllocationUnit::new(ModuleScorecard::new("a")),
            AllocationUnit::new(ModuleScorecard::new("b")),
            AllocationUnit::new(ModuleScorecard::new("c")),
            AllocationUnit::new(ModuleScorecard::new(" a ")),
        ];
        let plan = build_plan(&config, dec!(100), &units, Scoring::Scorecard);
        assert_eq!(plan.allocations.len(), 3);
        assert!(
            plan.allocations
                .iter()
                .all(|a| a.status == AllocationStatus::Paused
                    && a.reason == "below_minimum_allocation"
                    && a.max_usdt.is_zero())
        );
    }

    #[test]
    fn empty_treasury_or_no_units_yield_no_allocations() {
        let plan = build_plan(
            &distributor(),
            Decimal::ZERO,
            &[tested("a", dec!(10), 10, dec!(10), dec!(60))],
            Scoring::LifetimePnl,
        );
        assert!(plan.allocations.is_empty());
        assert!(plan.notes.iter().any(|n| n.contains("treasury")));
        assert!(
            build_plan(&distributor(), dec!(100), &[], Scoring::LifetimePnl)
                .allocations
                .is_empty()
        );
        let all_reserved = CapitalPoolConfig {
            reserve_pct: dec!(150),
            ..distributor()
        };
        let plan = build_plan(
            &all_reserved,
            dec!(100),
            &[tested("a", dec!(1), 1, dec!(0), dec!(100))],
            Scoring::Scorecard,
        );
        assert_eq!(plan.reserve_usdt, dec!(100));
        assert!(plan.allocations.is_empty());
    }

    #[test]
    fn floor_applies_when_all_bots_unprofitable() {
        let units = [
            tested("a", dec!(-5), 10, dec!(10), dec!(60)),
            tested("b", dec!(-3), 10, dec!(10), dec!(60)),
        ];
        let plan = build_plan(&distributor(), dec!(100), &units, Scoring::LifetimePnl);
        approx(shares(&plan, "a"), dec!(50));
        approx(shares(&plan, "b"), dec!(50));
    }

    #[test]
    fn winner_takes_performance_pool_up_to_its_cap() {
        let units = [
            tested("winner", dec!(80), 10, dec!(10), dec!(60)),
            tested("loser", dec!(-20), 10, dec!(10), dec!(60)),
        ];
        let plan = build_plan(&distributor(), dec!(100), &units, Scoring::LifetimePnl);
        approx(shares(&plan, "winner"), dec!(60));
        approx(shares(&plan, "loser"), dec!(40));
    }

    #[test]
    fn runaway_winner_is_capped() {
        let units = [
            tested("a", dec!(100), 10, dec!(10), dec!(40)),
            tested("b", dec!(0), 10, dec!(10), dec!(80)),
        ];
        let plan = build_plan(&distributor(), dec!(200), &units, Scoring::LifetimePnl);
        approx(shares(&plan, "a"), dec!(40));
        approx(shares(&plan, "a") + shares(&plan, "b"), dec!(100));
    }

    #[test]
    fn min_shares_over_100_are_renormalised() {
        let units = [
            tested("a", dec!(10), 10, dec!(60), dec!(70)),
            tested("b", dec!(10), 10, dec!(60), dec!(70)),
        ];
        let plan = build_plan(&distributor(), dec!(100), &units, Scoring::LifetimePnl);
        approx(shares(&plan, "a"), dec!(50));
        approx(shares(&plan, "b"), dec!(50));
        assert!(plan.notes.iter().any(|n| n.contains("renormalised")));
    }

    #[test]
    fn untested_bot_is_excluded_once_others_have_traded() {
        let units = [
            tested("active", dec!(0), 5, dec!(30), dec!(80)),
            tested("untested", dec!(0), 0, dec!(30), dec!(80)),
        ];
        let plan = build_plan(&distributor(), dec!(100), &units, Scoring::LifetimePnl);
        approx(shares(&plan, "untested"), Decimal::ZERO);
        approx(shares(&plan, "active"), dec!(80));
        assert_eq!(
            plan.allocation_for("untested").unwrap().reason,
            "untested_bot_excluded"
        );
        assert!(
            plan.notes
                .iter()
                .any(|n| n.contains("untested bots excluded"))
        );
        // The scorecard policy keeps untested units at full weight (refactor semantics).
        let plan = build_plan(&distributor(), dec!(100), &units, Scoring::Scorecard);
        assert!(shares(&plan, "untested") > Decimal::ZERO);
    }

    #[test]
    fn bootstrap_keeps_equal_floors_when_nobody_traded() {
        let units = [
            tested("a", dec!(0), 0, dec!(20), dec!(60)),
            tested("b", dec!(0), 0, dec!(20), dec!(60)),
        ];
        let plan = build_plan(&distributor(), dec!(100), &units, Scoring::LifetimePnl);
        approx(shares(&plan, "a"), dec!(50));
        approx(shares(&plan, "b"), dec!(50));
    }

    #[test]
    fn scoring_parses_from_cli_spellings() {
        assert_eq!(
            "lifetime_pnl".parse::<Scoring>().unwrap(),
            Scoring::LifetimePnl
        );
        assert_eq!("Scorecard".parse::<Scoring>().unwrap(), Scoring::Scorecard);
        assert!("sharpe".parse::<Scoring>().is_err());
    }
}
