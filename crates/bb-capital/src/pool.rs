//! Shared capital pool (refactor `src/capital/pool.py::CapitalPoolManager`): one plan per buy order,
//! per-order leases capped by the module's remaining allocation, a usage ledger, and a JSON state
//! file (`schema_version` 3) that survives restarts.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::advisor::{AllocationContext, GuardedAdvisor};
use crate::allocator::{AllocationPlan, AllocationStatus, AllocationUnit, Scoring, build_plan};
use crate::config::CapitalPoolConfig;
use crate::error::{CapitalError, Result};
use crate::scorecard::ModuleScorecard;

/// Persisted state layout version (same as the refactor).
pub const STATE_SCHEMA_VERSION: u64 = 3;
/// Leases kept in memory and in the state file.
pub const MAX_LEASE_HISTORY: usize = 200;

/// Lease outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LeaseStatus {
    /// Pool disabled or not a buy: the requested notional passes untouched.
    Bypassed,
    /// No budget: the order must not be placed.
    Paused,
    Approved,
    /// Approved for less than requested.
    Reduced,
}

/// Per-order capital permission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllocationLease {
    pub lease_id: String,
    pub module_id: String,
    pub symbol: String,
    pub side: String,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub requested_notional_usdt: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub approved_notional_usdt: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub module_allocation_usdt: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub used_module_notional_usdt: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub remaining_module_allocation_usdt: Decimal,
    pub status: LeaseStatus,
    pub reason: String,
    pub created_at_ms: i64,
    pub plan: AllocationPlan,
}

impl AllocationLease {
    /// Whether the order may proceed (for `approved_notional_usdt`).
    pub fn approved(&self) -> bool {
        self.approved_notional_usdt > Decimal::ZERO && self.status != LeaseStatus::Paused
    }
}

/// Capital accounting for one module.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModuleLedgerEntry {
    pub module_id: String,
    /// Allocation status, or `unallocated` for usage without a plan entry.
    pub status: String,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub allocated_usdt: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub used_usdt: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub free_usdt: Decimal,
    pub score: f64,
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision")]
    pub realized_pnl_usdt: Decimal,
    #[serde(default)]
    pub trades: u64,
    #[serde(default)]
    pub updated_at_ms: i64,
}

/// One lease request.
#[derive(Debug, Clone)]
pub struct LeaseRequest<'a> {
    pub module_id: &'a str,
    pub symbol: &'a str,
    pub side: &'a str,
    pub requested_notional_usdt: Decimal,
    pub total_equity_usdt: Decimal,
    /// Every module competing for the pool (the requested one is added).
    pub module_ids: &'a [String],
    /// Open notional per module (buys − sells, floored at zero by the caller or here).
    pub module_usage_usdt: &'a BTreeMap<String, Decimal>,
}

/// Shared allocator used before orders reach risk/execution.
pub struct CapitalPool {
    config: CapitalPoolConfig,
    scoring: Scoring,
    unit_shares: BTreeMap<String, (Decimal, Decimal)>,
    advisor: Option<GuardedAdvisor>,
    scorecards: BTreeMap<String, ModuleScorecard>,
    lease_history: VecDeque<AllocationLease>,
    last_plan: Option<AllocationPlan>,
    ledger: BTreeMap<String, ModuleLedgerEntry>,
}

fn normalize(module_id: &str) -> Result<String> {
    let trimmed = module_id.trim();
    if trimmed.is_empty() {
        return Err(CapitalError::invalid("module_id must not be blank"));
    }
    Ok(trimmed.to_owned())
}

fn usage_of(usage: &BTreeMap<String, Decimal>, module_id: &str) -> Decimal {
    usage
        .get(module_id)
        .copied()
        .unwrap_or_default()
        .max(Decimal::ZERO)
}

impl CapitalPool {
    pub fn new(config: CapitalPoolConfig) -> Self {
        Self {
            config,
            scoring: Scoring::Scorecard,
            unit_shares: BTreeMap::new(),
            advisor: None,
            scorecards: BTreeMap::new(),
            lease_history: VecDeque::new(),
            last_plan: None,
            ledger: BTreeMap::new(),
        }
    }

    pub fn with_scoring(mut self, scoring: Scoring) -> Self {
        self.scoring = scoring;
        self
    }

    /// Per-module floor/ceiling shares (%) of the allocatable pool.
    pub fn with_unit_shares(mut self, shares: BTreeMap<String, (Decimal, Decimal)>) -> Self {
        self.unit_shares = shares;
        self
    }

    /// Guarded AI advisor consulted for every buy-order plan.
    pub fn with_advisor(mut self, advisor: GuardedAdvisor) -> Self {
        self.advisor = Some(advisor);
        self
    }

    pub fn with_scorecards(mut self, scorecards: BTreeMap<String, ModuleScorecard>) -> Self {
        self.update_scorecards(scorecards);
        self
    }

    pub fn config(&self) -> &CapitalPoolConfig {
        &self.config
    }

    pub fn scoring(&self) -> Scoring {
        self.scoring
    }

    pub fn scorecards(&self) -> &BTreeMap<String, ModuleScorecard> {
        &self.scorecards
    }

    /// Replace the scorecards used by the allocator (keys are trimmed; blank keys dropped).
    pub fn update_scorecards(&mut self, scorecards: BTreeMap<String, ModuleScorecard>) {
        self.scorecards = scorecards
            .into_iter()
            .filter_map(|(id, mut card)| {
                let id = normalize(&id).ok()?;
                card.module_id.clone_from(&id);
                Some((id, card))
            })
            .collect();
    }

    pub fn ledger(&self) -> &BTreeMap<String, ModuleLedgerEntry> {
        &self.ledger
    }

    pub fn last_plan(&self) -> Option<&AllocationPlan> {
        self.last_plan.as_ref()
    }

    pub fn lease_history(&self) -> impl DoubleEndedIterator<Item = &AllocationLease> {
        self.lease_history.iter()
    }

    /// Allocation units for `module_ids` (deduplicated, blank ids dropped).
    pub fn units(&self, module_ids: &[String]) -> Vec<AllocationUnit> {
        let mut seen = std::collections::BTreeSet::new();
        module_ids
            .iter()
            .filter_map(|id| normalize(id).ok())
            .filter(|id| seen.insert(id.clone()))
            .map(|id| {
                let card = self
                    .scorecards
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| ModuleScorecard::new(id.clone()));
                let unit = AllocationUnit::new(card);
                match self.unit_shares.get(&id) {
                    Some((min, max)) => unit.with_shares(*min, *max),
                    None => unit,
                }
            })
            .collect()
    }

    /// Deterministic plan only (no advisor call).
    pub fn deterministic_plan(
        &self,
        total_equity_usdt: Decimal,
        module_ids: &[String],
    ) -> AllocationPlan {
        build_plan(
            &self.config,
            total_equity_usdt,
            &self.units(module_ids),
            self.scoring,
        )
    }

    /// Advisor context for `module_ids` around a deterministic fallback plan.
    pub fn context(&self, total_equity_usdt: Decimal, module_ids: &[String]) -> AllocationContext {
        let fallback_plan = self.deterministic_plan(total_equity_usdt, module_ids);
        AllocationContext {
            config: self.config.clone(),
            scoring: self.scoring,
            total_equity_usdt,
            module_ids: fallback_plan
                .allocations
                .iter()
                .map(|a| a.module_id.clone())
                .collect(),
            scorecards: self.scorecards.clone(),
            fallback_plan,
        }
    }

    /// Plan through the guarded advisor when attached, else deterministic; remembered as last plan.
    pub async fn build_plan(
        &mut self,
        total_equity_usdt: Decimal,
        module_ids: &[String],
    ) -> AllocationPlan {
        let plan = match &self.advisor {
            Some(advisor) => {
                advisor
                    .build_plan(&self.context(total_equity_usdt, module_ids))
                    .await
                    .plan
            }
            None => self.deterministic_plan(total_equity_usdt, module_ids),
        };
        self.last_plan = Some(plan.clone());
        plan
    }

    /// Grant (or refuse) capital for one order.
    pub async fn request_lease(&mut self, request: &LeaseRequest<'_>) -> Result<AllocationLease> {
        let module_id = normalize(request.module_id)?;
        let requested = request.requested_notional_usdt;
        if requested <= Decimal::ZERO {
            return Err(CapitalError::invalid(
                "requested_notional_usdt must be greater than zero",
            ));
        }
        let side = request.side.trim().to_ascii_lowercase();
        let mut module_ids = request.module_ids.to_vec();
        module_ids.push(module_id.clone());
        let bypass = side != "buy" || !self.config.enabled;
        let plan = if bypass {
            // Sells and a disabled pool never wait for the advisor.
            let plan = self.deterministic_plan(request.total_equity_usdt, &module_ids);
            self.last_plan = Some(plan.clone());
            plan
        } else {
            self.build_plan(request.total_equity_usdt, &module_ids)
                .await
        };
        let created_at_ms = bb_core::time::now_ms();
        self.sync_ledger_usage(request.module_usage_usdt, created_at_ms);
        let used = usage_of(request.module_usage_usdt, &module_id);

        let lease = |approved, allocation, remaining, status, reason: &str| AllocationLease {
            lease_id: uuid::Uuid::new_v4().simple().to_string(),
            module_id: module_id.clone(),
            symbol: request.symbol.to_owned(),
            side: side.clone(),
            requested_notional_usdt: requested,
            approved_notional_usdt: approved,
            module_allocation_usdt: allocation,
            used_module_notional_usdt: used,
            remaining_module_allocation_usdt: remaining,
            status,
            reason: reason.to_owned(),
            created_at_ms,
            plan: plan.clone(),
        };

        let lease = if bypass {
            lease(
                requested,
                requested,
                requested,
                LeaseStatus::Bypassed,
                "capital_pool_disabled_or_not_buy",
            )
        } else {
            match plan.allocation_for(&module_id) {
                Some(allocation)
                    if allocation.status != AllocationStatus::Paused
                        && allocation.max_usdt > Decimal::ZERO =>
                {
                    let remaining = (allocation.max_usdt - used).max(Decimal::ZERO);
                    if remaining <= Decimal::ZERO {
                        lease(
                            Decimal::ZERO,
                            allocation.max_usdt,
                            Decimal::ZERO,
                            LeaseStatus::Paused,
                            "module_allocation_exhausted",
                        )
                    } else if requested <= remaining {
                        lease(
                            requested,
                            allocation.max_usdt,
                            remaining,
                            LeaseStatus::Approved,
                            "within_allocation",
                        )
                    } else {
                        lease(
                            remaining,
                            allocation.max_usdt,
                            remaining,
                            LeaseStatus::Reduced,
                            "remaining_module_allocation_cap",
                        )
                    }
                }
                _ => lease(
                    Decimal::ZERO,
                    Decimal::ZERO,
                    Decimal::ZERO,
                    LeaseStatus::Paused,
                    "module_has_no_allocation",
                ),
            }
        };
        if self.lease_history.len() >= MAX_LEASE_HISTORY {
            self.lease_history.pop_front();
        }
        self.lease_history.push_back(lease.clone());
        Ok(lease)
    }

    /// Refresh the ledger from the latest plan and an open-usage snapshot.
    pub fn sync_ledger_usage(&mut self, usage: &BTreeMap<String, Decimal>, updated_at_ms: i64) {
        let Some(plan) = &self.last_plan else {
            return;
        };
        let mut next = BTreeMap::new();
        for allocation in &plan.allocations {
            let card = self.scorecards.get(&allocation.module_id);
            let used = usage_of(usage, &allocation.module_id);
            let allocated = allocation.max_usdt.max(Decimal::ZERO);
            next.insert(
                allocation.module_id.clone(),
                ModuleLedgerEntry {
                    module_id: allocation.module_id.clone(),
                    status: allocation.status.as_str().into(),
                    allocated_usdt: allocated,
                    used_usdt: used,
                    free_usdt: (allocated - used).max(Decimal::ZERO),
                    score: allocation.score,
                    realized_pnl_usdt: card.map(|c| c.realized_pnl_usdt).unwrap_or_default(),
                    trades: card.map_or(0, |c| c.trades),
                    updated_at_ms,
                },
            );
        }
        for (module_id, used) in usage {
            let Ok(id) = normalize(module_id) else {
                continue;
            };
            if next.contains_key(&id) {
                continue;
            }
            let card = self
                .scorecards
                .get(&id)
                .cloned()
                .unwrap_or_else(|| ModuleScorecard::new(id.clone()));
            next.insert(
                id.clone(),
                ModuleLedgerEntry {
                    module_id: id,
                    status: "unallocated".into(),
                    allocated_usdt: Decimal::ZERO,
                    used_usdt: (*used).max(Decimal::ZERO),
                    free_usdt: Decimal::ZERO,
                    score: card.score(),
                    realized_pnl_usdt: card.realized_pnl_usdt,
                    trades: card.trades,
                    updated_at_ms,
                },
            );
        }
        self.ledger = next;
    }

    /// Persistent decision state.
    pub fn state_snapshot(&self) -> Value {
        json!({
            "schema_version": STATE_SCHEMA_VERSION,
            "last_plan": self.last_plan,
            "scorecards": self.scorecards,
            "ledger": self.ledger,
            "lease_history": self.lease_history,
        })
    }

    /// Restore scorecards, lease history, ledger and last plan; malformed entries are skipped.
    pub fn restore_state(&mut self, snapshot: &Value) {
        let Some(snapshot) = snapshot.as_object() else {
            return;
        };
        if let Some(cards) = snapshot.get("scorecards").and_then(Value::as_object) {
            self.scorecards = cards
                .iter()
                .filter(|(_, payload)| payload.is_object())
                .filter_map(|(id, payload)| {
                    let id = normalize(id).ok()?;
                    Some((id.clone(), ModuleScorecard::from_value(&id, payload)))
                })
                .collect();
        }
        if let Some(leases) = snapshot.get("lease_history").and_then(Value::as_array) {
            self.lease_history = leases
                .iter()
                .filter_map(|lease| serde_json::from_value::<AllocationLease>(lease.clone()).ok())
                .filter(|lease| !lease.module_id.trim().is_empty())
                .collect();
            while self.lease_history.len() > MAX_LEASE_HISTORY {
                self.lease_history.pop_front();
            }
        }
        if let Some(ledger) = snapshot.get("ledger").and_then(Value::as_object) {
            self.ledger = ledger
                .iter()
                .filter_map(|(id, payload)| {
                    let id = normalize(id).ok()?;
                    let entry =
                        serde_json::from_value::<ModuleLedgerEntry>(payload.clone()).ok()?;
                    (entry.allocated_usdt >= Decimal::ZERO && entry.used_usdt >= Decimal::ZERO)
                        .then_some((id, entry))
                })
                .collect();
        }
        if let Some(plan) = snapshot.get("last_plan").filter(|p| p.is_object()) {
            self.last_plan = serde_json::from_value(plan.clone()).ok();
        }
    }

    /// Restore from a state file (missing file = fresh state).
    pub fn load(&mut self, path: &Path) -> Result<bool> {
        match bb_core::fsutil::read_json::<Value>(path)? {
            Some(state) => {
                self.restore_state(&state);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Atomically write the state file.
    pub fn save(&self, path: &Path) -> Result<()> {
        bb_core::fsutil::write_json_atomic(path, &self.state_snapshot())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use rust_decimal_macros::dec;

    use super::*;
    use crate::advisor::{FileProposalProvider, GuardedAdvisor};
    use crate::allocator::tests::approx;

    fn pool(reserve: Decimal, cap: Decimal) -> CapitalPool {
        CapitalPool::new(CapitalPoolConfig {
            enabled: true,
            reserve_pct: reserve,
            min_module_allocation_usdt: Decimal::ZERO,
            max_module_allocation_usdt: Some(cap),
        })
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[tokio::test]
    async fn capital_pool_reserves_equity_and_caps_module_allocation() {
        let mut cards = BTreeMap::new();
        cards.insert(
            "demo-sma:BTC/USDT".to_owned(),
            ModuleScorecard {
                trades: 10,
                wins: 7,
                losses: 3,
                realized_pnl_usdt: dec!(12),
                ..ModuleScorecard::new("demo-sma:BTC/USDT")
            },
        );
        let mut pool = pool(dec!(40), dec!(25)).with_scorecards(cards);
        let usage = BTreeMap::new();
        let modules = ids(&["demo-sma:BTC/USDT", "demo-sma:ETH/USDT"]);
        let lease = pool
            .request_lease(&LeaseRequest {
                module_id: "demo-sma:BTC/USDT",
                symbol: "BTC/USDT",
                side: "buy",
                requested_notional_usdt: dec!(80),
                total_equity_usdt: dec!(100),
                module_ids: &modules,
                module_usage_usdt: &usage,
            })
            .await
            .unwrap();
        assert_eq!(lease.status, LeaseStatus::Reduced);
        approx(lease.approved_notional_usdt, dec!(25));
        approx(lease.remaining_module_allocation_usdt, dec!(25));
        approx(lease.plan.reserve_usdt, dec!(40));
        assert!(lease.approved());
    }

    #[tokio::test]
    async fn capital_pool_subtracts_existing_module_usage() {
        let mut pool = pool(Decimal::ZERO, dec!(25));
        let usage = BTreeMap::from([("spot-sma-crossover:BTC/USDT".to_owned(), dec!(22))]);
        let modules = ids(&["spot-sma-crossover:BTC/USDT"]);
        let lease = pool
            .request_lease(&LeaseRequest {
                module_id: "spot-sma-crossover:BTC/USDT",
                symbol: "BTC/USDT",
                side: "buy",
                requested_notional_usdt: dec!(20),
                total_equity_usdt: dec!(100),
                module_ids: &modules,
                module_usage_usdt: &usage,
            })
            .await
            .unwrap();
        assert_eq!(lease.status, LeaseStatus::Reduced);
        approx(lease.approved_notional_usdt, dec!(3));
        approx(lease.used_module_notional_usdt, dec!(22));
        approx(lease.remaining_module_allocation_usdt, dec!(3));
        let ledger = &pool.ledger()["spot-sma-crossover:BTC/USDT"];
        approx(ledger.allocated_usdt, dec!(25));
        approx(ledger.used_usdt, dec!(22));
        approx(ledger.free_usdt, dec!(3));
    }

    #[tokio::test]
    async fn exhausted_unknown_and_sell_leases() {
        let mut pool = pool(Decimal::ZERO, dec!(25));
        let modules = ids(&["m:BTC/USDT"]);
        let usage = BTreeMap::from([
            ("m:BTC/USDT".to_owned(), dec!(30)),
            ("orphan".to_owned(), dec!(4)),
        ]);
        let request = LeaseRequest {
            module_id: "m:BTC/USDT",
            symbol: "BTC/USDT",
            side: "BUY",
            requested_notional_usdt: dec!(5),
            total_equity_usdt: dec!(100),
            module_ids: &modules,
            module_usage_usdt: &usage,
        };
        let exhausted = pool.request_lease(&request).await.unwrap();
        assert_eq!(
            (exhausted.status, exhausted.reason.as_str()),
            (LeaseStatus::Paused, "module_allocation_exhausted")
        );
        assert!(!exhausted.approved());
        assert_eq!(pool.ledger()["orphan"].status, "unallocated");

        let sell = pool
            .request_lease(&LeaseRequest {
                side: "sell",
                ..request.clone()
            })
            .await
            .unwrap();
        assert_eq!(sell.status, LeaseStatus::Bypassed);
        assert_eq!(sell.approved_notional_usdt, dec!(5));

        let mut tiny = CapitalPool::new(CapitalPoolConfig {
            enabled: true,
            reserve_pct: dec!(100),
            ..CapitalPoolConfig::default()
        });
        let none = tiny.request_lease(&request).await.unwrap();
        assert_eq!(
            (none.status, none.reason.as_str()),
            (LeaseStatus::Paused, "module_has_no_allocation")
        );

        assert!(
            pool.request_lease(&LeaseRequest {
                requested_notional_usdt: Decimal::ZERO,
                ..request.clone()
            })
            .await
            .is_err()
        );
        assert!(
            pool.request_lease(&LeaseRequest {
                module_id: "  ",
                ..request
            })
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn disabled_pool_bypasses_buys() {
        let mut pool = CapitalPool::new(CapitalPoolConfig::default());
        let usage = BTreeMap::new();
        let lease = pool
            .request_lease(&LeaseRequest {
                module_id: "m",
                symbol: "BTC/USDT",
                side: "buy",
                requested_notional_usdt: dec!(500),
                total_equity_usdt: dec!(100),
                module_ids: &[],
                module_usage_usdt: &usage,
            })
            .await
            .unwrap();
        assert_eq!(lease.status, LeaseStatus::Bypassed);
        assert_eq!(lease.approved_notional_usdt, dec!(500));
    }

    #[tokio::test]
    async fn capital_pool_restores_persistent_ledger_through_a_state_file() {
        let mut pool = pool(Decimal::ZERO, dec!(25));
        let usage = BTreeMap::from([("spot-sma-crossover:BTC/USDT".to_owned(), dec!(7))]);
        let modules = ids(&["spot-sma-crossover:BTC/USDT"]);
        pool.request_lease(&LeaseRequest {
            module_id: "spot-sma-crossover:BTC/USDT",
            symbol: "BTC/USDT",
            side: "buy",
            requested_notional_usdt: dec!(10),
            total_equity_usdt: dec!(100),
            module_ids: &modules,
            module_usage_usdt: &usage,
        })
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capital-pool-state.json");
        pool.save(&path).unwrap();

        let mut restored = CapitalPool::new(CapitalPoolConfig::default());
        assert!(restored.load(&path).unwrap());
        let ledger = &restored.ledger()["spot-sma-crossover:BTC/USDT"];
        approx(ledger.allocated_usdt, dec!(25));
        approx(ledger.used_usdt, dec!(7));
        assert_eq!(restored.lease_history().count(), 1);
        assert!(restored.last_plan().is_some());
        assert_eq!(restored.state_snapshot()["schema_version"], 3);
        assert!(
            !CapitalPool::new(CapitalPoolConfig::default())
                .load(&dir.path().join("missing.json"))
                .unwrap()
        );
    }

    #[test]
    fn restore_accepts_refactor_float_state_and_skips_garbage() {
        let state = json!({
            "schema_version": 3,
            "scorecards": {"a:B": {"realized_pnl_usdt": 1.5, "trades": 2.0, "wins": 1, "losses": 1}, "bad": 7},
            "ledger": {
                "a:B": {"module_id": "a:B", "status": "active", "allocated_usdt": 25.0, "used_usdt": 7.0,
                        "free_usdt": 18.0, "score": 1.0, "realized_pnl_usdt": 0.0, "trades": 0, "updated_at_ms": 1},
                "neg": {"module_id": "neg", "status": "active", "allocated_usdt": -1.0, "used_usdt": 0.0,
                        "free_usdt": 0.0, "score": 1.0}
            },
            "lease_history": [{"nonsense": true}]
        });
        let mut pool = CapitalPool::new(CapitalPoolConfig::default());
        pool.restore_state(&state);
        assert_eq!(pool.scorecards()["a:B"].trades, 2);
        assert_eq!(pool.scorecards().len(), 1);
        assert_eq!(pool.ledger().len(), 1);
        assert_eq!(pool.ledger()["a:B"].allocated_usdt, dec!(25.0));
        assert_eq!(pool.lease_history().count(), 0);
    }

    #[tokio::test]
    async fn advisor_proposal_shapes_the_lease_plan() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("proposal.json");
        std::fs::write(
            &file,
            r#"{"allocations": [{"module_id": "m:BTC/USDT", "status": "active", "max_usdt": 10, "reason": "ai_keep_fallback"}]}"#,
        )
        .unwrap();
        let advisor = GuardedAdvisor::new(
            Arc::new(FileProposalProvider::new(&file)),
            Duration::from_secs(1),
        );
        let mut pool = pool(Decimal::ZERO, dec!(25)).with_advisor(advisor);
        let usage = BTreeMap::new();
        let modules = ids(&["m:BTC/USDT", "other:ETH/USDT"]);
        let lease = pool
            .request_lease(&LeaseRequest {
                module_id: "m:BTC/USDT",
                symbol: "BTC/USDT",
                side: "buy",
                requested_notional_usdt: dec!(50),
                total_equity_usdt: dec!(100),
                module_ids: &modules,
                module_usage_usdt: &usage,
            })
            .await
            .unwrap();
        assert_eq!(lease.approved_notional_usdt, dec!(10));
        // Modules the proposal omits are absent, so their buys pause.
        let other = pool
            .request_lease(&LeaseRequest {
                module_id: "other:ETH/USDT",
                symbol: "ETH/USDT",
                side: "buy",
                requested_notional_usdt: dec!(5),
                total_equity_usdt: dec!(100),
                module_ids: &modules,
                module_usage_usdt: &usage,
            })
            .await
            .unwrap();
        assert_eq!(other.status, LeaseStatus::Paused);
    }
}
