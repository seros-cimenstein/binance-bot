//! Forward-paper executor: for each tradeable candidate, quote SOL→token on
//! Jupiter, hold, quote token→SOL, and record the simulated round trip to a
//! JSONL ledger. No signing, no swaps, no SOL spent.
//!
//! [`ExecutionFriction`] models the gap between idealised quote fills and
//! live Solana execution (latency, fill probability, gas).

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Timelike as _, Utc};
use futures::{Stream, StreamExt as _};
use rand::{Rng, SeedableRng as _};
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use tokio::task::JoinSet;

use crate::error::{SniperError, truncate_chars};
use crate::jupiter::{JupiterQuote, LAMPORTS_PER_SOL, QuoteProvider, SOL_MINT, decimal_or_zero};
use crate::pipeline::TradeCandidate;

pub const DEFAULT_LEDGER_PATH: &str = "data/sniper-paper-ledger.jsonl";
pub const DEFAULT_SUMMARY_PATH: &str = "data/sniper-paper-summary.json";
pub const PI_REALISM_V1: &str = "pi_realism_v1";
pub const ZERO_PROFILE: &str = "zero";

/// Execution-friction model.
///
/// * latency — pre-quote sleep so Jupiter prices the post-front-running state;
/// * fill probability — failed entries burn the base fee without a position;
///   failed exits lose the whole position;
/// * gas — base fee on every submitted tx, priority fee only on landed ones.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionFriction {
    pub profile_id: String,
    pub entry_latency_ms: u64,
    pub entry_latency_jitter_ms: u64,
    pub entry_fill_prob: f64,
    pub exit_fill_prob: f64,
    pub base_fee_lamports: u64,
    pub priority_fee_lamports: u64,
}

impl Default for ExecutionFriction {
    /// The `pi_realism_v1` profile.
    fn default() -> Self {
        Self {
            profile_id: PI_REALISM_V1.into(),
            entry_latency_ms: 800,
            entry_latency_jitter_ms: 600,
            entry_fill_prob: 0.70,
            exit_fill_prob: 0.95,
            base_fee_lamports: 5_000,
            priority_fee_lamports: 20_000,
        }
    }
}

impl ExecutionFriction {
    /// Historic idealised paper behaviour.
    pub fn zero() -> Self {
        Self {
            profile_id: ZERO_PROFILE.into(),
            entry_latency_ms: 0,
            entry_latency_jitter_ms: 0,
            entry_fill_prob: 1.0,
            exit_fill_prob: 1.0,
            base_fee_lamports: 0,
            priority_fee_lamports: 0,
        }
    }

    pub fn sampled_entry_latency_ms<R: Rng + ?Sized>(&self, rng: &mut R) -> u64 {
        if self.entry_latency_jitter_ms == 0 {
            return self.entry_latency_ms;
        }
        self.entry_latency_ms + rng.gen_range(0..=self.entry_latency_jitter_ms)
    }
}

/// Exit outcome stored in the ledger `exit_status` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitStatus {
    RoundTrip,
    NoExitRoute,
    ExitError,
    EntryFailedTx,
    ExitFailedTx,
}

fn usd(value: &Decimal) -> Value {
    Value::Number(
        value
            .normalize()
            .to_string()
            .parse()
            .unwrap_or_else(|_| serde_json::Number::from(0)),
    )
}

fn serialize_usd<S: serde::Serializer>(value: &Decimal, s: S) -> Result<S::Ok, S::Error> {
    usd(value).serialize(s)
}

fn deserialize_usd<'de, D: Deserializer<'de>>(d: D) -> Result<Decimal, D::Error> {
    let value = Value::deserialize(d)?;
    Ok(decimal_or_zero(Some(&value)))
}

/// One simulated round trip (one ledger line).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaperSnipeRecord {
    pub mint: String,
    pub symbol: String,
    pub name: String,
    pub entry_time_ms: i64,
    pub exit_time_ms: i64,
    pub hold_seconds: u64,
    /// SOL spent on the buy (0 when the entry never landed).
    pub entry_in_sol_atoms: u64,
    pub entry_out_token_atoms: u64,
    pub exit_out_sol_atoms: u64,
    /// `exit_out - entry_in - gas_paid_lamports`.
    pub sol_pnl_atoms: i64,
    pub sol_pnl_pct: f64,
    #[serde(serialize_with = "serialize_usd")]
    pub entry_usd_value: Decimal,
    #[serde(serialize_with = "serialize_usd")]
    pub exit_usd_value: Decimal,
    #[serde(serialize_with = "serialize_usd")]
    pub usd_pnl: Decimal,
    pub entry_route_labels: Vec<String>,
    pub exit_route_labels: Vec<String>,
    pub exit_status: ExitStatus,
    pub error: String,
    pub friction_profile_id: String,
    pub entry_landed: bool,
    pub exit_landed: bool,
    pub gas_paid_lamports: u64,
    pub entry_latency_ms_applied: u64,
}

/// SOL→USD rate per lamport implied by a quote's dollar value; 0 when unknown.
pub fn usd_per_sol_atom(quote: &JupiterQuote) -> Decimal {
    if quote.in_amount == 0 || quote.swap_usd_value <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    if quote.input_mint == SOL_MINT {
        return quote.swap_usd_value / Decimal::from(quote.in_amount);
    }
    if quote.output_mint == SOL_MINT && quote.out_amount > 0 {
        return quote.swap_usd_value / Decimal::from(quote.out_amount);
    }
    Decimal::ZERO
}

fn pnl_pct(pnl_atoms: i64, entry_atoms: u64) -> f64 {
    if entry_atoms == 0 {
        0.0
    } else {
        pnl_atoms as f64 / entry_atoms as f64 * 100.0
    }
}

fn signed(atoms: u64) -> i64 {
    i64::try_from(atoms).unwrap_or(i64::MAX)
}

/// Shared, immutable per-run simulation parameters.
#[derive(Debug, Clone)]
pub struct SimulationParams {
    pub entry_sol_atoms: u64,
    pub hold_seconds: u64,
    pub slippage_bps: u32,
    pub friction: ExecutionFriction,
}

/// Append one ledger row with sorted keys (Python `sort_keys=True`).
fn append_ledger(
    ledger: &bb_core::audit::JsonlLog,
    record: &PaperSnipeRecord,
) -> Result<(), SniperError> {
    let row = serde_json::to_value(record).map_err(bb_core::audit::AuditError::from)?;
    ledger.append(&row)?;
    Ok(())
}

/// Simulate one entry → hold → exit round trip.
///
/// `Ok(None)` when the entry quote itself fails (no route at launch).
pub async fn simulate_snipe<Q, R>(
    candidate: &TradeCandidate,
    params: &SimulationParams,
    quotes: &Q,
    ledger: &bb_core::audit::JsonlLog,
    rng: &mut R,
) -> Result<Option<PaperSnipeRecord>, SniperError>
where
    Q: QuoteProvider,
    R: Rng + ?Sized,
{
    let friction = &params.friction;
    let entry_sol_atoms = params.entry_sol_atoms;
    let create = &candidate.create_event;

    // Faster snipers have already moved the pool by the time we ask.
    let latency = friction.sampled_entry_latency_ms(rng);
    if latency > 0 {
        tokio::time::sleep(Duration::from_millis(latency)).await;
    }

    let buy = match quotes
        .get_quote(SOL_MINT, &create.mint, entry_sol_atoms, params.slippage_bps)
        .await
    {
        Ok(quote) => quote,
        Err(SniperError::Quote(_)) => return Ok(None),
        Err(err) => return Err(err),
    };
    let entry_time_ms = bb_core::time::now_ms();
    let usd_per_lamport = usd_per_sol_atom(&buy);
    let base = |status, error: &str| PaperSnipeRecord {
        mint: create.mint.clone(),
        symbol: create.symbol.clone(),
        name: create.name.clone(),
        entry_time_ms,
        exit_time_ms: entry_time_ms,
        hold_seconds: params.hold_seconds,
        entry_in_sol_atoms: 0,
        entry_out_token_atoms: 0,
        exit_out_sol_atoms: 0,
        sol_pnl_atoms: 0,
        sol_pnl_pct: 0.0,
        entry_usd_value: Decimal::ZERO,
        exit_usd_value: Decimal::ZERO,
        usd_pnl: Decimal::ZERO,
        entry_route_labels: buy.route_labels.clone(),
        exit_route_labels: Vec::new(),
        exit_status: status,
        error: error.to_owned(),
        friction_profile_id: friction.profile_id.clone(),
        entry_landed: true,
        exit_landed: true,
        gas_paid_lamports: 0,
        entry_latency_ms_applied: latency,
    };

    if rng.r#gen::<f64>() >= friction.entry_fill_prob {
        let gas = friction.base_fee_lamports;
        let pnl = -signed(gas);
        let record = PaperSnipeRecord {
            sol_pnl_atoms: pnl,
            sol_pnl_pct: pnl_pct(pnl, entry_sol_atoms),
            usd_pnl: -(Decimal::from(gas) * usd_per_lamport),
            entry_landed: false,
            exit_landed: false,
            gas_paid_lamports: gas,
            ..base(ExitStatus::EntryFailedTx, "entry tx did not land")
        };
        append_ledger(ledger, &record)?;
        return Ok(Some(record));
    }

    let token_atoms = buy.out_amount;
    let mut gas = friction.base_fee_lamports + friction.priority_fee_lamports;
    tokio::time::sleep(Duration::from_secs(params.hold_seconds)).await;

    let mut status = ExitStatus::RoundTrip;
    let mut error = String::new();
    let mut sell = match quotes
        .get_quote(&create.mint, SOL_MINT, token_atoms, params.slippage_bps)
        .await
    {
        Ok(quote) => Some(quote),
        // No liquidity on the other side: no tx submitted, no extra gas.
        Err(SniperError::Quote(msg)) => {
            status = ExitStatus::NoExitRoute;
            error = truncate_chars(&msg, 200);
            None
        }
        Err(err) => {
            status = ExitStatus::ExitError;
            error = format!("{}: {}", err.kind(), truncate_chars(&err.to_string(), 200));
            None
        }
    };

    let mut exit_landed = true;
    if sell.is_some() {
        exit_landed = rng.r#gen::<f64>() < friction.exit_fill_prob;
        if exit_landed {
            gas += friction.base_fee_lamports + friction.priority_fee_lamports;
        } else {
            status = ExitStatus::ExitFailedTx;
            error = "exit tx did not land — position lost".into();
            gas += friction.base_fee_lamports;
            sell = None;
        }
    }

    let exit_out = sell.as_ref().map_or(0, |q| q.out_amount);
    let pnl = signed(exit_out) - signed(entry_sol_atoms) - signed(gas);
    let exit_usd = sell.as_ref().map_or(Decimal::ZERO, |q| q.swap_usd_value);
    let record = PaperSnipeRecord {
        exit_time_ms: bb_core::time::now_ms(),
        entry_in_sol_atoms: entry_sol_atoms,
        entry_out_token_atoms: token_atoms,
        exit_out_sol_atoms: exit_out,
        sol_pnl_atoms: pnl,
        sol_pnl_pct: pnl_pct(pnl, entry_sol_atoms),
        entry_usd_value: buy.swap_usd_value,
        exit_usd_value: exit_usd,
        usd_pnl: exit_usd - buy.swap_usd_value - Decimal::from(gas) * usd_per_lamport,
        exit_route_labels: sell.map(|q| q.route_labels).unwrap_or_default(),
        exit_landed,
        gas_paid_lamports: gas,
        ..base(status, &error)
    };
    append_ledger(ledger, &record)?;
    Ok(Some(record))
}

/// Paper run configuration.
#[derive(Debug, Clone)]
pub struct PaperRun {
    pub params: SimulationParams,
    pub max_concurrent: usize,
    pub ledger_path: std::path::PathBuf,
    pub summary_path: std::path::PathBuf,
    pub max_candidates: Option<usize>,
    /// Stop accepting candidates after this long; in-flight simulations still finish.
    pub duration: Option<Duration>,
}

/// Ledger row fields the summary needs (old rows may lack friction fields).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LedgerRow {
    entry_time_ms: i64,
    exit_status: String,
    sol_pnl_atoms: i64,
    #[serde(deserialize_with = "deserialize_usd")]
    usd_pnl: Decimal,
    gas_paid_lamports: u64,
}

/// `data/sniper-paper-summary.json`; fields in alphabetical order to match
/// Python's `sort_keys=True` output.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaperSummary {
    pub candidates_queued: usize,
    pub candidates_skipped_at_capacity: usize,
    pub entry_failed_tx_count: usize,
    pub entry_sol_atoms: u64,
    pub exit_failed_tx_count: usize,
    pub finished_at: String,
    pub friction_profile_id: String,
    pub hold_seconds: u64,
    pub losses: usize,
    #[serde(serialize_with = "serialize_usd")]
    pub net_sol_pnl: Decimal,
    pub net_sol_pnl_atoms: i64,
    #[serde(serialize_with = "serialize_usd")]
    pub net_usd_pnl: Decimal,
    #[serde(serialize_with = "serialize_usd")]
    pub round_trip_sol_pnl: Decimal,
    pub round_trip_sol_pnl_atoms: i64,
    #[serde(serialize_with = "serialize_usd")]
    pub round_trip_usd_pnl: Decimal,
    pub round_trips_completed: usize,
    pub simulations_recorded: usize,
    pub started_at: String,
    pub total_gas_lamports: u64,
    #[serde(serialize_with = "serialize_usd")]
    pub total_gas_sol: Decimal,
    pub win_rate_pct: f64,
    pub wins: usize,
}

/// Python `datetime.isoformat()` for an aware UTC timestamp
/// (microseconds only when non-zero, `+00:00` offset).
pub fn py_isoformat(ts: DateTime<Utc>) -> String {
    if ts.nanosecond() / 1000 == 0 {
        ts.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    } else {
        ts.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
    }
}

fn lamports_to_sol(atoms: i64) -> Decimal {
    (Decimal::from(atoms) / Decimal::from(LAMPORTS_PER_SOL)).normalize()
}

fn summarize(
    run: &PaperRun,
    started_at_ms: i64,
    queued: usize,
    skipped_at_capacity: usize,
) -> Result<PaperSummary, SniperError> {
    let rows: Vec<LedgerRow> = bb_core::audit::JsonlLog::new(&run.ledger_path).read_all()?;
    let relevant: Vec<&LedgerRow> = rows
        .iter()
        .filter(|r| r.entry_time_ms >= started_at_ms)
        .collect();
    let with_status = |status: &str| relevant.iter().filter(|r| r.exit_status == status).count();
    let completed: Vec<&&LedgerRow> = relevant
        .iter()
        .filter(|r| r.exit_status == "round_trip")
        .collect();
    let wins = completed.iter().filter(|r| r.sol_pnl_atoms > 0).count();
    let losses = completed.iter().filter(|r| r.sol_pnl_atoms < 0).count();
    let round_trip_sol: i64 = completed.iter().map(|r| r.sol_pnl_atoms).sum();
    let round_trip_usd: Decimal = completed.iter().map(|r| r.usd_pnl).sum();
    let net_sol: i64 = relevant.iter().map(|r| r.sol_pnl_atoms).sum();
    let net_usd: Decimal = relevant.iter().map(|r| r.usd_pnl).sum();
    let gas: u64 = relevant.iter().map(|r| r.gas_paid_lamports).sum();
    Ok(PaperSummary {
        started_at: py_isoformat(bb_core::time::from_ms(started_at_ms).unwrap_or_default()),
        finished_at: py_isoformat(bb_core::time::now()),
        candidates_queued: queued,
        candidates_skipped_at_capacity: skipped_at_capacity,
        simulations_recorded: relevant.len(),
        round_trips_completed: completed.len(),
        entry_failed_tx_count: with_status("entry_failed_tx"),
        exit_failed_tx_count: with_status("exit_failed_tx"),
        wins,
        losses,
        win_rate_pct: wins as f64 / (wins + losses).max(1) as f64 * 100.0,
        round_trip_sol_pnl_atoms: round_trip_sol,
        round_trip_sol_pnl: lamports_to_sol(round_trip_sol),
        round_trip_usd_pnl: round_trip_usd.round_dp(6).normalize(),
        net_sol_pnl_atoms: net_sol,
        net_sol_pnl: lamports_to_sol(net_sol),
        net_usd_pnl: net_usd.round_dp(6).normalize(),
        total_gas_lamports: gas,
        total_gas_sol: lamports_to_sol(signed(gas)),
        entry_sol_atoms: run.params.entry_sol_atoms,
        hold_seconds: run.params.hold_seconds,
        friction_profile_id: run.params.friction.profile_id.clone(),
    })
}

fn reap(in_flight: &mut JoinSet<()>) {
    while in_flight.try_join_next().is_some() {}
}

/// Drive the candidate stream, simulate each pick with bounded concurrency,
/// then write and return the summary of rows recorded during this run.
pub async fn run_paper_executor<C, Q>(
    candidates: C,
    quotes: Arc<Q>,
    run: &PaperRun,
) -> Result<PaperSummary, SniperError>
where
    C: Stream<Item = TradeCandidate> + Unpin,
    Q: QuoteProvider + 'static,
{
    let started_at_ms = bb_core::time::now_ms();
    let ledger = bb_core::audit::JsonlLog::new(&run.ledger_path);
    let params = Arc::new(run.params.clone());
    let capacity = run.max_concurrent.max(1);
    let mut queued = 0usize;
    let mut skipped_at_capacity = 0usize;
    let mut in_flight: JoinSet<()> = JoinSet::new();

    let accept = async {
        let mut candidates = candidates;
        while let Some(candidate) = candidates.next().await {
            reap(&mut in_flight);
            if in_flight.len() >= capacity {
                skipped_at_capacity += 1;
                continue;
            }
            queued += 1;
            let (quotes, params, ledger) = (quotes.clone(), params.clone(), ledger.clone());
            in_flight.spawn(async move {
                let mut rng = rand::rngs::StdRng::from_entropy();
                match simulate_snipe(&candidate, &params, quotes.as_ref(), &ledger, &mut rng).await {
                    Ok(Some(record)) => tracing::info!(
                        mint = %record.mint, symbol = %record.symbol, status = ?record.exit_status,
                        sol_pnl_atoms = record.sol_pnl_atoms, "paper snipe recorded"
                    ),
                    Ok(None) => tracing::info!(mint = %candidate.create_event.mint, "no entry route; skipped"),
                    Err(err) => tracing::warn!(mint = %candidate.create_event.mint, error = %err, "paper snipe failed"),
                }
            });
            if run.max_candidates.is_some_and(|max| queued >= max) {
                break;
            }
        }
    };
    match run.duration {
        Some(duration) => {
            let _ = tokio::time::timeout(duration, accept).await;
        }
        None => accept.await,
    }
    // Let in-flight simulations finish so their exits are recorded.
    while in_flight.join_next().await.is_some() {}

    let summary = summarize(run, started_at_ms, queued, skipped_at_capacity)?;
    bb_core::fsutil::write_json_atomic(&run.summary_path, &summary)
        .map_err(|e| SniperError::io(&run.summary_path, e))?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::decoder::PumpFunCreateEvent;
    use crate::detector::{PUMP_FUN_PROGRAM_ID, PoolCreationEvent};
    use crate::pipeline::tests::safe_score;
    use rust_decimal_macros::dec;

    fn candidate(mint: &str) -> TradeCandidate {
        TradeCandidate {
            pool_event: PoolCreationEvent {
                program_label: "pump_fun".into(),
                program_id: PUMP_FUN_PROGRAM_ID.into(),
                signature: "sig".into(),
                slot: 1,
                detected_at_ms: 0,
                matched_markers: Vec::new(),
                log_excerpt: Vec::new(),
            },
            create_event: PumpFunCreateEvent {
                name: "Test".into(),
                symbol: "TST".into(),
                uri: "https://x/m.json".into(),
                mint: mint.into(),
                bonding_curve: "bc".into(),
                user: "u".into(),
            },
            safety: safe_score(mint),
            evaluated_at_ms: 0,
        }
    }

    fn quote(input: &str, in_amount: u64, out_amount: u64, usd: Decimal) -> JupiterQuote {
        JupiterQuote {
            input_mint: input.into(),
            output_mint: "X".into(),
            in_amount,
            out_amount,
            price_impact_pct: 0.0,
            swap_usd_value: usd,
            route_labels: vec!["Pump.fun".into()],
            context_slot: 1,
            raw: Value::Null,
        }
    }

    /// Buy at 10M lamports → 100M tokens ($1); sell 100M tokens → 12M lamports ($1.2).
    #[derive(Clone, Copy)]
    enum Exit {
        Profit,
        NoRoute,
        Transport,
    }

    struct FakeQuotes {
        entry_fails: bool,
        exit: Exit,
    }

    impl FakeQuotes {
        fn new(exit: Exit) -> Arc<Self> {
            Arc::new(Self {
                entry_fails: false,
                exit,
            })
        }
    }

    impl QuoteProvider for FakeQuotes {
        async fn get_quote(
            &self,
            input: &str,
            _output: &str,
            _amount: u64,
            _bps: u32,
        ) -> Result<JupiterQuote, SniperError> {
            if input == SOL_MINT {
                if self.entry_fails {
                    return Err(SniperError::Quote("no route".into()));
                }
                return Ok(quote(SOL_MINT, 10_000_000, 100_000_000, dec!(1.0)));
            }
            match self.exit {
                Exit::Profit => Ok(quote("MINT", 100_000_000, 12_000_000, dec!(1.2))),
                Exit::NoRoute => Err(SniperError::Quote("liquidity rug-pulled".into())),
                Exit::Transport => Err(SniperError::Detector("boom".into())),
            }
        }
    }

    fn friction(entry: f64, exit: f64, base: u64, prio: u64, latency: u64) -> ExecutionFriction {
        ExecutionFriction {
            profile_id: "custom".into(),
            entry_latency_ms: latency,
            entry_latency_jitter_ms: 0,
            entry_fill_prob: entry,
            exit_fill_prob: exit,
            base_fee_lamports: base,
            priority_fee_lamports: prio,
        }
    }

    fn params(friction: ExecutionFriction, hold: u64) -> SimulationParams {
        SimulationParams {
            entry_sol_atoms: 10_000_000,
            hold_seconds: hold,
            slippage_bps: 500,
            friction,
        }
    }

    async fn simulate(
        q: &FakeQuotes,
        p: &SimulationParams,
        ledger: &Path,
    ) -> Option<PaperSnipeRecord> {
        let log = bb_core::audit::JsonlLog::new(ledger);
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        simulate_snipe(&candidate("TESTMINT"), p, q, &log, &mut rng)
            .await
            .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn records_profitable_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let rec = simulate(
            &FakeQuotes::new(Exit::Profit),
            &params(ExecutionFriction::zero(), 1),
            &ledger,
        )
        .await
        .unwrap();
        assert_eq!(rec.exit_status, ExitStatus::RoundTrip);
        assert_eq!(rec.sol_pnl_atoms, 2_000_000);
        assert!((rec.sol_pnl_pct - 20.0).abs() < 1e-9);
        assert_eq!(rec.usd_pnl, dec!(0.2));
        assert_eq!(rec.gas_paid_lamports, 0);
        assert_eq!(rec.friction_profile_id, "zero");
        let text = std::fs::read_to_string(&ledger).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1);
        let row: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(row["sol_pnl_atoms"], 2_000_000);
        assert_eq!(row["exit_status"], "round_trip");
        assert_eq!(row["usd_pnl"].to_string(), "0.2");
        assert!(
            lines[0].starts_with("{\"entry_in_sol_atoms\""),
            "keys sorted: {}",
            lines[0]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn returns_none_when_entry_quote_fails() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let q = FakeQuotes {
            entry_fails: true,
            exit: Exit::Profit,
        };
        assert!(
            simulate(&q, &params(ExecutionFriction::zero(), 1), &ledger)
                .await
                .is_none()
        );
        assert!(!ledger.exists());
    }

    #[tokio::test(start_paused = true)]
    async fn records_no_exit_route_and_exit_error() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let rec = simulate(
            &FakeQuotes::new(Exit::NoRoute),
            &params(ExecutionFriction::zero(), 1),
            &ledger,
        )
        .await
        .unwrap();
        assert_eq!(rec.exit_status, ExitStatus::NoExitRoute);
        assert_eq!(rec.sol_pnl_atoms, -10_000_000);
        assert!(rec.error.contains("liquidity"));

        let rec = simulate(
            &FakeQuotes::new(Exit::Transport),
            &params(ExecutionFriction::zero(), 1),
            &ledger,
        )
        .await
        .unwrap();
        assert_eq!(rec.exit_status, ExitStatus::ExitError);
        assert_eq!(rec.error, "SniperDetectorError: boom");
    }

    #[tokio::test(start_paused = true)]
    async fn entry_failed_tx_burns_base_fee_only() {
        let dir = tempfile::tempdir().unwrap();
        let p = params(
            ExecutionFriction {
                profile_id: "entry_always_fails".into(),
                ..friction(0.0, 1.0, 5_000, 20_000, 0)
            },
            1,
        );
        let rec = simulate(
            &FakeQuotes::new(Exit::Profit),
            &p,
            &dir.path().join("l.jsonl"),
        )
        .await
        .unwrap();
        assert_eq!(rec.exit_status, ExitStatus::EntryFailedTx);
        assert!(!rec.entry_landed && !rec.exit_landed);
        assert_eq!(rec.entry_in_sol_atoms, 0);
        assert_eq!(rec.gas_paid_lamports, 5_000);
        assert_eq!(rec.sol_pnl_atoms, -5_000);
        // $1 / 10M lamports × 5k lamports.
        assert_eq!(rec.usd_pnl, dec!(-0.0005));
        assert_eq!(rec.friction_profile_id, "entry_always_fails");
    }

    #[tokio::test(start_paused = true)]
    async fn exit_failed_tx_is_full_position_loss() {
        let dir = tempfile::tempdir().unwrap();
        let p = params(friction(1.0, 0.0, 5_000, 20_000, 0), 1);
        let rec = simulate(
            &FakeQuotes::new(Exit::Profit),
            &p,
            &dir.path().join("l.jsonl"),
        )
        .await
        .unwrap();
        assert_eq!(rec.exit_status, ExitStatus::ExitFailedTx);
        assert!(rec.entry_landed && !rec.exit_landed);
        assert_eq!(rec.exit_out_sol_atoms, 0);
        assert_eq!(rec.gas_paid_lamports, 30_000);
        assert_eq!(rec.sol_pnl_atoms, -10_030_000);
    }

    #[tokio::test(start_paused = true)]
    async fn landed_round_trip_subtracts_gas() {
        let dir = tempfile::tempdir().unwrap();
        let p = params(friction(1.0, 1.0, 5_000, 20_000, 0), 1);
        let rec = simulate(
            &FakeQuotes::new(Exit::Profit),
            &p,
            &dir.path().join("l.jsonl"),
        )
        .await
        .unwrap();
        assert_eq!(rec.exit_status, ExitStatus::RoundTrip);
        assert_eq!(rec.gas_paid_lamports, 50_000);
        assert_eq!(rec.sol_pnl_atoms, 1_950_000);
    }

    #[tokio::test(start_paused = true)]
    async fn applies_entry_latency_then_hold() {
        let dir = tempfile::tempdir().unwrap();
        let p = params(friction(1.0, 1.0, 0, 0, 500), 7);
        let started = tokio::time::Instant::now();
        let rec = simulate(
            &FakeQuotes::new(Exit::Profit),
            &p,
            &dir.path().join("l.jsonl"),
        )
        .await
        .unwrap();
        assert_eq!(rec.entry_latency_ms_applied, 500);
        assert_eq!(started.elapsed(), Duration::from_millis(7_500));
    }

    #[test]
    fn jitter_sampling_stays_in_range() {
        let f = ExecutionFriction::default();
        let mut rng = rand::rngs::StdRng::seed_from_u64(1);
        for _ in 0..200 {
            let ms = f.sampled_entry_latency_ms(&mut rng);
            assert!((800..=1400).contains(&ms));
        }
        assert_eq!(
            ExecutionFriction::zero().sampled_entry_latency_ms(&mut rng),
            0
        );
    }

    #[test]
    fn py_isoformat_matches_python() {
        let ts = bb_core::time::from_ms(1_760_184_000_123).unwrap();
        assert_eq!(py_isoformat(ts), "2025-10-11T12:00:00.123000+00:00");
        let ts = bb_core::time::from_ms(1_760_184_000_000).unwrap();
        assert_eq!(py_isoformat(ts), "2025-10-11T12:00:00+00:00");
    }

    #[tokio::test(start_paused = true)]
    async fn run_paper_executor_bounds_concurrency_and_summarises() {
        let dir = tempfile::tempdir().unwrap();
        // A stale row from an earlier run must not count.
        let ledger_path = dir.path().join("ledger.jsonl");
        std::fs::write(&ledger_path, "{\"entry_time_ms\": 1, \"exit_status\": \"round_trip\", \"sol_pnl_atoms\": 99}\nnot json\n").unwrap();
        let run = PaperRun {
            params: params(friction(1.0, 1.0, 5_000, 20_000, 0), 10),
            max_concurrent: 2,
            ledger_path: ledger_path.clone(),
            summary_path: dir.path().join("summary.json"),
            max_candidates: None,
            duration: None,
        };
        let candidates = futures::stream::iter((0..5).map(|i| candidate(&format!("M{i}"))));
        let summary = run_paper_executor(candidates, FakeQuotes::new(Exit::Profit), &run)
            .await
            .unwrap();
        assert_eq!(summary.candidates_queued, 2);
        assert_eq!(summary.candidates_skipped_at_capacity, 3);
        assert_eq!(summary.simulations_recorded, 2);
        assert_eq!(summary.round_trips_completed, 2);
        assert_eq!(summary.wins, 2);
        assert_eq!(summary.losses, 0);
        assert!((summary.win_rate_pct - 100.0).abs() < 1e-9);
        assert_eq!(summary.round_trip_sol_pnl_atoms, 3_900_000);
        assert_eq!(summary.round_trip_sol_pnl, dec!(0.0039));
        assert_eq!(summary.total_gas_lamports, 100_000);
        assert_eq!(summary.total_gas_sol, dec!(0.0001));
        // 2 × (0.2 - 50k × 1e-7) = 2 × 0.195
        assert_eq!(summary.net_usd_pnl, dec!(0.39));
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&run.summary_path).unwrap()).unwrap();
        assert_eq!(written["friction_profile_id"], "custom");
        assert_eq!(written["net_usd_pnl"].to_string(), "0.39");
        assert_eq!(written["candidates_queued"], 2);
    }

    #[tokio::test(start_paused = true)]
    async fn run_paper_executor_honours_max_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let run = PaperRun {
            params: params(ExecutionFriction::zero(), 1),
            max_concurrent: 5,
            ledger_path: dir.path().join("l.jsonl"),
            summary_path: dir.path().join("s.json"),
            max_candidates: Some(1),
            duration: None,
        };
        let candidates = futures::stream::iter((0..5).map(|i| candidate(&format!("M{i}"))));
        let summary = run_paper_executor(candidates, FakeQuotes::new(Exit::NoRoute), &run)
            .await
            .unwrap();
        assert_eq!(summary.candidates_queued, 1);
        assert_eq!(summary.simulations_recorded, 1);
        assert_eq!(summary.round_trips_completed, 0);
        assert_eq!(summary.net_sol_pnl_atoms, -10_000_000);
        assert_eq!(summary.net_sol_pnl, dec!(-0.01));
    }
}
