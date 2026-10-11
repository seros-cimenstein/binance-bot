//! Paper-only portfolio cycle over the dynamically discovered Binance universe.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use bb_core::money::to_f64;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::catalog::{CatalogConfig, MarketCatalog};
use crate::config::PoolSettings;
use crate::error::Result;
use crate::fees::{CommissionService, estimate_bnb_reserve, seconds};
use crate::ledger::{PaperLedger, rate_lookup};
use crate::models::{DecisionAction, PaperDecision, RouteQuote, VirtualPortfolio};
use crate::pyfmt::py_isoformat;
use crate::routes::{RouteConfig, RouteEngine};
use crate::scoring::{AssetScore, Scorer, basket_performance};
use crate::source::PoolClient;

const HUNDRED: Decimal = Decimal::ONE_HUNDRED;

/// Injectable wall clock (replays drive simulated time).
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Observable outcome of one paper-pool evaluation cycle.
#[derive(Debug, Clone, PartialEq)]
pub struct CycleReport {
    pub observed_at: String,
    pub catalog_markets: usize,
    pub eligible_markets: usize,
    pub seeded_balances: BTreeMap<String, Decimal>,
    /// `portfolio_id -> [(benchmark, total)]` in benchmark order.
    pub benchmark_totals: BTreeMap<String, Vec<(String, Decimal)>>,
    pub decisions: Vec<PaperDecision>,
}

/// Runs paper portfolios; owns no order-submission capability.
pub struct PaperPoolRuntime<C: PoolClient + 'static> {
    client: Arc<C>,
    settings: PoolSettings,
    ledger: PaperLedger,
    catalog: MarketCatalog,
    fees: Arc<CommissionService>,
    clock: Clock,
}

impl<C: PoolClient + 'static> std::fmt::Debug for PaperPoolRuntime<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaperPoolRuntime")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

type Rates = Vec<((String, String), Decimal)>;

impl<C: PoolClient + 'static> PaperPoolRuntime<C> {
    /// Runtime over `client`, persisting into `ledger`; `clock` defaults to UTC now.
    pub fn new(
        client: Arc<C>,
        settings: PoolSettings,
        ledger: PaperLedger,
        clock: Option<Clock>,
    ) -> Self {
        let fees = Arc::new(CommissionService::new(
            client.clone(),
            settings.fee_cache_seconds,
        ));
        Self {
            catalog: MarketCatalog::new(CatalogConfig::from(&settings)),
            client,
            settings,
            ledger,
            fees,
            clock: clock.unwrap_or_else(|| Arc::new(bb_core::time::now)),
        }
    }

    /// Settings in use.
    pub fn settings(&self) -> &PoolSettings {
        &self.settings
    }

    /// Paper ledger.
    pub fn ledger(&self) -> &PaperLedger {
        &self.ledger
    }

    /// Most recent market catalog.
    pub fn catalog(&self) -> &MarketCatalog {
        &self.catalog
    }

    /// Discover, value, score, and optionally simulate one reallocation per portfolio.
    pub async fn cycle(&mut self) -> Result<CycleReport> {
        let now = (self.clock)();
        let observed_at = py_isoformat(now);
        let raw_markets = self.client.load_markets().await?;
        let tickers = self.client.fetch_tickers().await?;
        self.catalog
            .classify(raw_markets, &tickers, now.timestamp_millis());
        let markets = self.catalog.markets();
        self.ledger.record_catalog(&observed_at, markets)?;
        let routes = RouteEngine::new(
            markets,
            self.fees.clone(),
            RouteConfig::from(&self.settings),
        );
        let initial_balances = self.client.nonzero_balances().await?;
        let portfolios = self
            .settings
            .scorers
            .iter()
            .map(|scorer| {
                self.ledger
                    .seed_if_absent(scorer, scorer, &initial_balances)
            })
            .collect::<Result<Vec<_>>>()?;

        let scoring_assets = self.catalog.scoring_assets();
        let mut observation_assets: BTreeSet<String> = scoring_assets.iter().cloned().collect();
        observation_assets.extend(initial_balances.keys().cloned());
        observation_assets.extend(self.settings.benchmarks.iter().cloned());
        let rates = self.indicative_rates(&routes, &observation_assets);
        self.ledger.record_observations(
            &observed_at,
            &rates,
            self.settings.momentum_observations,
        )?;

        let weights = self.settings.weights();
        let mut totals_by_portfolio = BTreeMap::new();
        let mut decisions = Vec::with_capacity(portfolios.len());
        for portfolio in &portfolios {
            let totals = self.value_balances(&portfolio.balances, &routes);
            let totals_f64: Vec<(String, f64)> = totals
                .iter()
                .map(|(b, v)| (b.clone(), to_f64(*v)))
                .collect();
            let initial_totals = self.ledger.initial_totals(&portfolio.portfolio_id)?;
            let mut score = basket_performance(&totals_f64, &initial_totals, &weights);
            self.ledger
                .record_snapshot(&observed_at, &portfolio.portfolio_id, &totals, score)?;
            if initial_totals.is_empty() {
                score = Some(0.0);
            }
            let decision = self
                .decide(
                    portfolio,
                    &scoring_assets,
                    &routes,
                    &rates,
                    &totals,
                    score,
                    &weights,
                )
                .await?;
            self.ledger.record_decision(&decision)?;
            if decision.action == DecisionAction::Rebalance
                && let Some(route) = &decision.route
            {
                self.ledger.apply_route(&portfolio.portfolio_id, route)?;
            }
            totals_by_portfolio.insert(portfolio.portfolio_id.clone(), totals);
            decisions.push(decision);
        }

        let report = CycleReport {
            observed_at,
            catalog_markets: markets.len(),
            eligible_markets: markets.iter().filter(|m| m.eligible).count(),
            seeded_balances: initial_balances,
            benchmark_totals: totals_by_portfolio,
            decisions,
        };
        tracing::info!(
            catalog_markets = report.catalog_markets,
            eligible_markets = report.eligible_markets,
            seeded_assets = ?report.seeded_balances.keys().collect::<Vec<_>>(),
            decisions = ?report.decisions.iter().map(|d| d.action.as_str()).collect::<Vec<_>>(),
            "Paper pool evaluation cycle completed"
        );
        Ok(report)
    }

    fn rate(routes: &RouteEngine, asset: &str, benchmark: &str) -> Option<Decimal> {
        if asset == benchmark {
            Some(Decimal::ONE)
        } else {
            routes.indicative_rate(asset, benchmark)
        }
    }

    fn indicative_rates(&self, routes: &RouteEngine, assets: &BTreeSet<String>) -> Rates {
        let mut rates = Vec::new();
        for asset in assets {
            for benchmark in &self.settings.benchmarks {
                if let Some(rate) =
                    Self::rate(routes, asset, benchmark).filter(|r| *r > Decimal::ZERO)
                {
                    rates.push(((asset.clone(), benchmark.clone()), rate));
                }
            }
        }
        rates
    }

    fn value_balances(
        &self,
        balances: &BTreeMap<String, Decimal>,
        routes: &RouteEngine,
    ) -> Vec<(String, Decimal)> {
        let mut totals: Vec<(String, Decimal)> = self
            .settings
            .benchmarks
            .iter()
            .map(|b| (b.clone(), Decimal::ZERO))
            .collect();
        for (asset, amount) in balances {
            if *amount <= Decimal::ZERO {
                continue;
            }
            for (benchmark, total) in &mut totals {
                if let Some(rate) = Self::rate(routes, asset, benchmark) {
                    *total += *amount * rate;
                }
            }
        }
        totals
    }

    #[allow(clippy::too_many_arguments)]
    async fn decide(
        &self,
        portfolio: &VirtualPortfolio,
        scoring_assets: &[String],
        routes: &RouteEngine,
        rates: &Rates,
        totals: &[(String, Decimal)],
        basket_score: Option<f64>,
        weights: &[(String, f64)],
    ) -> Result<PaperDecision> {
        let settings = &self.settings;
        let scorer = Scorer::from_name(&portfolio.scorer_name)?;
        let mut scored: Vec<AssetScore> = Vec::new();
        for asset in scoring_assets {
            let history = self.ledger.observation_history(
                asset,
                &settings.benchmarks,
                settings.momentum_observations,
            )?;
            if history
                .iter()
                .any(|(_, values)| values.len() < settings.momentum_observations)
            {
                continue;
            }
            if let Some(result) = scorer.score(asset, &history, weights) {
                scored.push(result);
            }
        }
        if scored.is_empty() {
            return Ok(self.hold(
                portfolio,
                format!(
                    "insufficient benchmark history for paper ranking (requires {} cycles)",
                    settings.momentum_observations
                ),
                None,
            ));
        }
        if basket_score.is_some_and(|score| score < -(settings.max_drawdown_percent / 100.0)) {
            return Ok(self.hold(
                portfolio,
                "paper portfolio drawdown gate is active".into(),
                None,
            ));
        }
        scored.sort_by(|a, b| b.score.total_cmp(&a.score));
        let target = &scored[0];
        let hold = |reason: &str| Ok(self.hold(portfolio, reason.into(), Some(target.score)));
        let lookup = rate_lookup(rates);
        let source = largest_holding(&portfolio.balances, &lookup);
        let Some(source) = source.filter(|s| *s != target.asset) else {
            return hold("highest-ranked asset is already the largest holding");
        };
        let source_score = scored
            .iter()
            .find(|item| item.asset == source)
            .map(|item| item.score);
        if source_score.is_some_and(|s| target.score - s <= settings.min_improvement_bps / 10_000.0)
        {
            return hold("rank advantage does not clear turnover threshold");
        }
        if let Some(last) = self.ledger.last_rebalance_at(&portfolio.portfolio_id)?
            && (self.clock)() - last < seconds(settings.decision_cooldown_seconds)
        {
            return hold("reallocation cooldown is active");
        }

        let source_balance = portfolio.balances.get(&source).copied().unwrap_or_default();
        let bnb_balance = portfolio.balances.get("BNB").copied().unwrap_or_default();
        let amount = source_balance * settings.max_reallocation_percent / HUNDRED;
        let Some(mut route) = routes
            .quote(
                &source,
                &target.asset,
                amount,
                bnb_balance >= settings.min_bnb_fee_reserve,
            )
            .await?
        else {
            return hold("candidate reallocation does not satisfy routed market minimums or costs");
        };
        let destination_to_bnb: HashMap<String, Decimal> = route
            .legs
            .iter()
            .filter_map(|leg| {
                routes
                    .indicative_rate(&leg.destination_asset, "BNB")
                    .map(|rate| (leg.destination_asset.clone(), rate))
            })
            .collect();
        let reserve = estimate_bnb_reserve(
            [&route],
            &destination_to_bnb,
            settings.bnb_fee_buffer_multiplier,
            settings.min_bnb_fee_reserve,
        );
        let remaining_bnb = bnb_balance
            - if source == "BNB" {
                route.input_amount
            } else {
                Decimal::ZERO
            };
        if uses_bnb_discount(&route) && remaining_bnb < reserve {
            match routes.quote(&source, &target.asset, amount, false).await? {
                Some(undiscounted) => route = undiscounted,
                None => return hold("route fails without available BNB fee reserve"),
            }
        }
        let usdt_rate = lookup.get(&(target.asset.as_str(), "USDT")).copied();
        let total_usdt = totals
            .iter()
            .find(|(b, _)| b == "USDT")
            .map_or(Decimal::ZERO, |(_, v)| *v);
        if let Some(usdt_rate) = usdt_rate.filter(|r| !r.is_zero())
            && total_usdt > Decimal::ZERO
        {
            let destination_after = (portfolio
                .balances
                .get(&target.asset)
                .copied()
                .unwrap_or_default()
                + route.output_amount)
                * usdt_rate;
            if destination_after > total_usdt * settings.max_allocation_percent / HUNDRED {
                return hold("destination allocation cap would be exceeded");
            }
        }
        Ok(PaperDecision {
            portfolio_id: portfolio.portfolio_id.clone(),
            scorer_name: portfolio.scorer_name.clone(),
            source_asset: Some(source),
            destination_asset: Some(target.asset.clone()),
            input_amount: Some(amount),
            route: Some(route),
            action: DecisionAction::Rebalance,
            reason: "simulated reallocation passed route, cost, reserve, and allocation gates"
                .into(),
            score: Some(target.score),
            decided_at: (self.clock)(),
        })
    }

    fn hold(
        &self,
        portfolio: &VirtualPortfolio,
        reason: String,
        score: Option<f64>,
    ) -> PaperDecision {
        PaperDecision {
            portfolio_id: portfolio.portfolio_id.clone(),
            scorer_name: portfolio.scorer_name.clone(),
            source_asset: None,
            destination_asset: None,
            input_amount: None,
            route: None,
            action: DecisionAction::Hold,
            reason,
            score,
            decided_at: (self.clock)(),
        }
    }
}

fn uses_bnb_discount(route: &RouteQuote) -> bool {
    route
        .legs
        .iter()
        .any(|leg| leg.commission_discount_asset == "BNB")
}

/// Largest positive holding by USDT value (ties broken by the greater asset name).
fn largest_holding(
    balances: &BTreeMap<String, Decimal>,
    rates: &HashMap<(&str, &str), Decimal>,
) -> Option<String> {
    balances
        .iter()
        .filter(|(_, amount)| **amount > Decimal::ZERO)
        .map(|(asset, amount)| {
            let rate = rates
                .get(&(asset.as_str(), "USDT"))
                .copied()
                .unwrap_or_default();
            (*amount * rate, asset)
        })
        .max()
        .map(|(_, asset)| asset.clone())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use bb_binance::spot::{AccountCommission, CommissionRates};
    use rust_decimal_macros::dec;

    use super::*;
    use crate::source::{BalanceSource, CommissionSource, MarketRules, MarketSource, TickerQuote};

    #[derive(Default)]
    struct ReadOnlyPoolClient {
        balance_reads: AtomicUsize,
        commission_reads: AtomicUsize,
    }

    const SYMBOLS: [(&str, &str, Decimal); 4] = [
        ("BNB/USDT", "BNB", dec!(660)),
        ("BTC/USDT", "BTC", dec!(77000)),
        ("PAXG/USDT", "PAXG", dec!(4500)),
        ("XAUT/USDT", "XAUT", dec!(4490)),
    ];

    #[async_trait]
    impl MarketSource for ReadOnlyPoolClient {
        async fn load_markets(&self) -> Result<Vec<MarketRules>> {
            Ok(SYMBOLS
                .iter()
                .map(|(symbol, base, _)| MarketRules {
                    symbol: (*symbol).into(),
                    base: (*base).into(),
                    quote: "USDT".into(),
                    spot: true,
                    active: true,
                    min_amount: Some(dec!(0.00001)),
                    min_notional: Some(dec!(5)),
                    amount_precision: Some(dec!(0.00001)),
                    amount_step: None,
                })
                .collect())
        }

        async fn fetch_tickers(&self) -> Result<HashMap<String, TickerQuote>> {
            let now = bb_core::time::now_ms();
            Ok(SYMBOLS
                .iter()
                .map(|(symbol, _, price)| {
                    let ticker = TickerQuote {
                        bid: Some(*price),
                        ask: Some(*price + dec!(0.01)),
                        last: Some(*price),
                        quote_volume: Some(dec!(5000000)),
                        timestamp_ms: Some(now),
                    };
                    ((*symbol).to_owned(), ticker)
                })
                .collect())
        }
    }

    #[async_trait]
    impl CommissionSource for ReadOnlyPoolClient {
        async fn account_commission(&self, symbol: &str) -> Result<AccountCommission> {
            self.commission_reads.fetch_add(1, Ordering::SeqCst);
            Ok(AccountCommission {
                symbol: symbol.into(),
                standard_commission: CommissionRates {
                    taker: dec!(0.001),
                    ..CommissionRates::default()
                },
                ..AccountCommission::default()
            })
        }
    }

    #[async_trait]
    impl BalanceSource for ReadOnlyPoolClient {
        async fn nonzero_balances(&self) -> Result<BTreeMap<String, Decimal>> {
            self.balance_reads.fetch_add(1, Ordering::SeqCst);
            Ok(BTreeMap::from([("BNB".to_owned(), dec!(0.0169745))]))
        }
    }

    fn runtime(
        dir: &tempfile::TempDir,
        momentum_observations: usize,
    ) -> (
        Arc<ReadOnlyPoolClient>,
        PaperPoolRuntime<ReadOnlyPoolClient>,
    ) {
        let settings = PoolSettings {
            db_path: dir.path().join("paper.db").display().to_string(),
            min_quote_volume_usdt: Decimal::ZERO,
            max_spread_bps: dec!(50),
            slippage_buffer_bps: Decimal::ZERO,
            max_scoring_assets: 4,
            momentum_observations,
            ..PoolSettings::default()
        }
        .validated()
        .unwrap();
        let client = Arc::new(ReadOnlyPoolClient::default());
        let ledger = PaperLedger::open(&settings.db_path).unwrap();
        (
            client.clone(),
            PaperPoolRuntime::new(client, settings, ledger, None),
        )
    }

    #[tokio::test]
    async fn seeds_real_snapshot_and_never_places_orders() {
        let dir = tempfile::tempdir().unwrap();
        let (client, mut runtime) = runtime(&dir, 96);
        let report = runtime.cycle().await.unwrap();
        assert_eq!(report.catalog_markets, 4);
        assert_eq!(report.eligible_markets, 4);
        assert_eq!(
            report.seeded_balances,
            BTreeMap::from([("BNB".to_owned(), dec!(0.0169745))])
        );
        assert!(report.benchmark_totals["simple_momentum"][0].1 > Decimal::ZERO);
        assert!(
            report
                .decisions
                .iter()
                .all(|d| d.action == DecisionAction::Hold
                    && d.reason.contains("insufficient benchmark history"))
        );
        assert_eq!(client.balance_reads.load(Ordering::SeqCst), 1);
        assert_eq!(client.commission_reads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn requires_configured_observation_warmup() {
        let dir = tempfile::tempdir().unwrap();
        let (client, mut runtime) = runtime(&dir, 4);
        for _ in 0..3 {
            let report = runtime.cycle().await.unwrap();
            assert!(report.decisions.iter().all(
                |d| d.action == DecisionAction::Hold && d.reason.contains("requires 4 cycles")
            ));
        }
        assert_eq!(
            runtime
                .ledger()
                .load_portfolio("simple_momentum")
                .unwrap()
                .balances,
            BTreeMap::from([("BNB".to_owned(), dec!(0.0169745))])
        );
        assert_eq!(client.commission_reads.load(Ordering::SeqCst), 0);
    }
}
