//! Live exit plans (Phase 45 step 3, operator approval 2026-10-15). Between Claude's
//! cycles the trader daemon follows the bid of every position with an exit plan
//! (Claude's from `set_exit_plan`, else the default plan) and, when the plan fires, sells
//! through [`Runtime::exit_sell`] with the same hard safety as `execute_route`. The guard
//! writes the watch state and log itself; the paper watcher must not run alongside it.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use bb_omp::CancellationToken;
use rust_decimal::Decimal;
use serde_json::json;
use tokio::time::Instant;

use crate::cycle::Runtime;
use crate::exit_book::{ExitPaths, PlanBook, WatchState};
use crate::exit_watch::{AuditCache, BinancePrices, FileLog, PlanSignal, WatchLog, Watcher};

/// Slippage assumed only for the plan-vs-Claude comparison of closed positions.
const COMPARISON_SLIPPAGE_BPS: Decimal = Decimal::from_parts(5, 0, 0, false, 0);
/// Sell attempts that failed before any order, before the signal is dropped.
const MAX_SELL_ATTEMPTS: u32 = 3;

struct PendingSell {
    signal: PlanSignal,
    attempts: u32,
}

/// The trader's live exit-plan loop.
pub struct ExitGuard {
    runtime: Arc<Runtime>,
    watcher: Watcher,
    prices: BinancePrices,
    audit: AuditCache,
    log: FileLog,
    paths: ExitPaths,
    poll: Duration,
    pending: Vec<PendingSell>,
}

fn sell_reason(signal: &PlanSignal) -> String {
    format!(
        "exit plan {} fired ({}, {}): sell {}% of the position, signal price {}",
        signal.spec,
        signal.fill.reason.as_str(),
        signal.source,
        (signal.share * Decimal::ONE_HUNDRED)
            .round_dp(1)
            .normalize(),
        signal.fill.price.round_dp(8).normalize()
    )
}

impl ExitGuard {
    pub fn new(runtime: Arc<Runtime>) -> Self {
        let config = Arc::clone(&runtime.config);
        let paths = config.exit_paths.clone();
        Self {
            watcher: Watcher::new(
                WatchState::load(&paths.state),
                COMPARISON_SLIPPAGE_BPS,
                config.exit.default_plan,
            ),
            prices: BinancePrices::new(runtime.client.clone()),
            audit: AuditCache::new(config.audit_path.clone()),
            log: FileLog {
                path: paths.log.clone(),
            },
            poll: Duration::from_secs(config.exit.poll_seconds.max(1)),
            paths,
            pending: Vec::new(),
            runtime,
        }
    }

    /// Follow plans and sell until `until` or shutdown.
    pub async fn run_until(&mut self, until: Instant, shutdown: &CancellationToken) {
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            self.pass().await;
            let wake = (Instant::now() + self.poll).min(until);
            tokio::select! {
                () = tokio::time::sleep_until(wake) => {}
                () = shutdown.cancelled() => return,
            }
            if Instant::now() >= until {
                return;
            }
        }
    }

    /// One pass: update every plan with the current bid, then execute fired sells.
    pub async fn pass(&mut self) {
        if self.runtime.shutting_down() {
            return;
        }
        let book = PlanBook::load(&self.paths.plans).unwrap_or_else(|error| {
            tracing::warn!(%error, "exit plan book unreadable");
            PlanBook::default()
        });
        let trips = self.audit.trips().to_vec();
        let signals = self
            .watcher
            .step(
                &book,
                &trips,
                &self.prices,
                bb_core::time::now_ms(),
                &mut self.log,
            )
            .await;
        self.pending
            .extend(signals.into_iter().map(|signal| PendingSell {
                signal,
                attempts: 0,
            }));
        // A position Claude (or an earlier sell) already closed needs nothing more.
        let open: BTreeSet<&str> = trips
            .iter()
            .filter(|t| t.exit.is_none())
            .map(|t| t.asset.as_str())
            .collect();
        let pending: Vec<PendingSell> = std::mem::take(&mut self.pending)
            .into_iter()
            .filter(|p| open.contains(p.signal.asset.as_str()))
            .collect();
        for mut sell in pending {
            let reason = sell_reason(&sell.signal);
            let signal = sell.signal.clone();
            match self
                .runtime
                .exit_sell(&signal.asset, signal.share, &reason)
                .await
            {
                Ok(result) => {
                    tracing::info!(asset = %signal.asset, spec = %signal.spec, "exit plan sold");
                    self.log.append(json!({
                        "event": "sold",
                        "key": signal.key,
                        "asset": signal.asset,
                        "spec": signal.spec,
                        "reason": signal.fill.reason.as_str(),
                        "share": signal.share,
                        "result": result,
                    }));
                }
                Err(error) => {
                    sell.attempts += 1;
                    let retry = !error.submission_attempted && sell.attempts < MAX_SELL_ATTEMPTS;
                    tracing::warn!(asset = %signal.asset, %error, attempts = sell.attempts, retry, "exit plan sell failed");
                    self.log.append(json!({
                        "event": if retry { "sell_failed" } else { "sell_abandoned" },
                        "key": signal.key,
                        "asset": signal.asset,
                        "spec": signal.spec,
                        "error": error.message,
                        "submission_attempted": error.submission_attempted,
                        "attempts": sell.attempts,
                    }));
                    if retry {
                        self.pending.push(sell);
                    }
                }
            }
        }
        if let Err(error) = self.watcher.state().save(&self.paths.state) {
            tracing::warn!(%error, "exit watch state not saved");
        }
    }
}
