//! One poll cycle and the daemon loop.

use std::collections::BTreeSet;
use std::path::Path;

use bb_binance::BinanceClient;
use rust_decimal::Decimal;
use serde::Serialize;

use super::execute::{FireResult, fire_live, fire_paper, format_live_fire_alert};
use super::settings::SniperSettings;
use super::state::SniperState;
use crate::alerts::Alerter;
use crate::error::{ListingError, Result};
use crate::market;
use crate::util::{append_jsonl, seconds, timestamp};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SniperMode {
    /// Detect and audit only.
    PlanOnly,
    Paper,
    Live,
}

/// Fire skipped before any order or simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SkipAction {
    SkippedDailyCap {
        symbol: String,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        spent_usdt: Decimal,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        cap_usdt: Decimal,
    },
    /// Live: free quote balance below `LISTING_SNIPER_MIN_BALANCE_USDT`.
    SkippedLowBalance {
        symbol: String,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        free_usdt: Decimal,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        min_balance_usdt: Decimal,
    },
    /// Live: the balance query failed, so the fire was not attempted.
    SkippedBalanceUnavailable { symbol: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum CycleAction {
    Fire(FireResult),
    Skip(SkipAction),
}

/// One audit row in the sniper events JSONL (also printed by the CLI).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CycleReport {
    pub actions: Vec<CycleAction>,
    pub current_universe_size: usize,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub daily_cap_usdt: Decimal,
    #[serde(with = "rust_decimal::serde::arbitrary_precision")]
    pub daily_spent_usdt: Decimal,
    pub mode: SniperMode,
    pub new_symbols: Vec<String>,
    pub newly_armed_this_cycle: Vec<String>,
    pub quote_asset: String,
    pub sniped_lifetime: usize,
    pub timestamp: String,
    pub watched_pending: BTreeSet<String>,
}

#[derive(Debug, Serialize)]
struct LedgerRow<'a> {
    timestamp: String,
    #[serde(flatten)]
    result: &'a FireResult,
}

#[derive(Debug, Serialize)]
struct ErrorRow {
    timestamp: String,
    error: String,
}

pub fn ensure_no_emergency_stop(path: &Path) -> Result<()> {
    if bb_core::fsutil::stop_requested(path) {
        return Err(ListingError::EmergencyStop(path.to_owned()));
    }
    Ok(())
}

/// Emergency stop always; the full live gate ladder when `submit_order`.
pub fn preflight(settings: &SniperSettings, submit_order: bool) -> Result<()> {
    ensure_no_emergency_stop(&settings.emergency_stop_path)?;
    if !submit_order {
        return Ok(());
    }
    let gate = |msg: &str| Err(ListingError::Gate(msg.to_owned()));
    if !settings.enabled {
        return gate("LISTING_SNIPER_ENABLED=true is required for live submission");
    }
    if settings.use_testnet {
        return gate("listing sniper live submission requires USE_TESTNET=false");
    }
    if settings.dry_run {
        return gate("listing sniper live submission requires DRY_RUN=false");
    }
    if settings.confirm_live != "yes" {
        return gate("listing sniper live submission requires CONFIRM_LIVE_LISTING_SNIPER=yes");
    }
    if !settings.proven_edge {
        return gate(
            "listing sniper live submission requires LISTING_SNIPER_PROVEN_EDGE=yes. \
             Run --paper for at least a handful of detected listings first; only \
             flip this when paper ledger expectancy is positive.",
        );
    }
    Ok(())
}

/// Exchange handles for a cycle: `public` serves exchangeInfo and paper
/// prices; `trading` (signed) is required for live fires.
#[derive(Debug, Clone, Copy)]
pub struct Venue<'a> {
    pub public: &'a BinanceClient,
    pub trading: Option<&'a BinanceClient>,
}

/// Live balance gate; `None` when the fire may proceed.
async fn balance_gate(
    settings: &SniperSettings,
    client: &BinanceClient,
    symbol: &str,
) -> Option<SkipAction> {
    if !settings.dynamic_balance {
        return None;
    }
    match market::free_balance(client, &settings.quote_asset).await {
        Ok(free) if free < settings.min_balance_usdt => Some(SkipAction::SkippedLowBalance {
            symbol: symbol.to_owned(),
            free_usdt: free,
            min_balance_usdt: settings.min_balance_usdt,
        }),
        Ok(_) => None,
        Err(reason) => Some(SkipAction::SkippedBalanceUnavailable {
            symbol: symbol.to_owned(),
            reason,
        }),
    }
}

/// One cycle: arm watchlist entries, fetch the TRADING universe, diff, fire
/// (paper/live), then snapshot the universe, audit and persist state.
pub async fn run_cycle(
    settings: &SniperSettings,
    state: &mut SniperState,
    mode: SniperMode,
    venue: Venue<'_>,
    alerter: &Alerter,
) -> Result<CycleReport> {
    preflight(settings, mode == SniperMode::Live)?;
    let newly_armed = state.arm_from_watchlist(&settings.watchlist_path, &settings.quote_asset);
    let current = market::trading_symbols_for_quote(venue.public, &settings.quote_asset).await?;
    let new_symbols = state.compute_new_listings(&current);

    let mut actions = Vec::new();
    for symbol in &new_symbols {
        alerter
            .send(&format!(
                "Listing sniper: new {} pair detected → {symbol}",
                settings.quote_asset
            ))
            .await;
        if mode == SniperMode::PlanOnly {
            continue;
        }
        if state
            .daily_spend
            .would_exceed(settings.notional_usdt, settings.daily_spend_cap_usdt)
        {
            actions.push(CycleAction::Skip(SkipAction::SkippedDailyCap {
                symbol: symbol.clone(),
                spent_usdt: state.daily_spend.spent_usdt,
                cap_usdt: settings.daily_spend_cap_usdt,
            }));
            continue;
        }
        let result = match (mode, venue.trading) {
            (SniperMode::Live, Some(client)) => {
                if let Some(skip) = balance_gate(settings, client, symbol).await {
                    actions.push(CycleAction::Skip(skip));
                    continue;
                }
                let outcome = fire_live(settings, client, symbol).await;
                alerter
                    .send(&format_live_fire_alert(&outcome, settings.hold_mode))
                    .await;
                FireResult::Live(outcome)
            }
            (SniperMode::Paper, _) => {
                FireResult::Paper(fire_paper(settings, venue.public, symbol).await)
            }
            _ => continue,
        };
        state.daily_spend.add(settings.notional_usdt);
        state.sniped_symbols.insert(symbol.clone());
        state.watched_pending.remove(symbol);
        append_jsonl(
            &settings.ledger_path,
            &LedgerRow {
                timestamp: timestamp(),
                result: &result,
            },
        )?;
        actions.push(CycleAction::Fire(result));
    }

    let report = CycleReport {
        actions,
        current_universe_size: current.len(),
        daily_cap_usdt: settings.daily_spend_cap_usdt,
        daily_spent_usdt: state.daily_spend.spent_usdt,
        mode,
        new_symbols,
        newly_armed_this_cycle: newly_armed,
        quote_asset: settings.quote_asset.clone(),
        sniped_lifetime: state.sniped_symbols.len(),
        timestamp: timestamp(),
        watched_pending: state.watched_pending.clone(),
    };
    state.known_trading_symbols = current;
    state.bootstrapped = true;
    append_jsonl(&settings.audit_path, &report)?;
    state.save(&settings.state_path)?;
    Ok(report)
}

/// Loop forever every `poll_interval_seconds`; cycle errors are written to
/// the audit log (`{"timestamp", "error"}`) and never stop the daemon.
pub async fn run_daemon(
    settings: &SniperSettings,
    mode: SniperMode,
    venue: Venue<'_>,
    alerter: &Alerter,
) -> Result<()> {
    let mut state = SniperState::load(&settings.state_path)?;
    loop {
        match run_cycle(settings, &mut state, mode, venue, alerter).await {
            Ok(report) => tracing::debug!(
                universe = report.current_universe_size,
                new = report.new_symbols.len(),
                "listing sniper cycle"
            ),
            Err(err) => {
                tracing::warn!(error = %err, "listing sniper cycle failed");
                let row = ErrorRow {
                    timestamp: timestamp(),
                    error: err.to_string(),
                };
                if let Err(audit_err) = append_jsonl(&settings.audit_path, &row) {
                    tracing::error!(error = %audit_err, "cannot append listing sniper error row");
                }
            }
        }
        tokio::time::sleep(seconds(settings.poll_interval_seconds)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bb_binance::{BinanceConfig, Credentials};
    use rust_decimal_macros::dec;
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::sniper::settings::HoldMode;

    fn client(server: &MockServer) -> BinanceClient {
        let cfg = BinanceConfig::new(false)
            .with_spot_rest_url(server.uri())
            .with_retries(0, Duration::ZERO, Duration::ZERO)
            .with_credentials(Credentials::new("k", "s").unwrap());
        BinanceClient::new(cfg).unwrap()
    }

    fn settings(dir: &Path) -> SniperSettings {
        SniperSettings {
            use_testnet: false,
            dry_run: false,
            enabled: true,
            confirm_live: "yes".into(),
            proven_edge: true,
            hold_mode: HoldMode::Flat,
            hold_seconds: 0,
            trailing_poll_interval_seconds: 0,
            state_path: dir.join("state.json"),
            audit_path: dir.join("audit.jsonl"),
            ledger_path: dir.join("ledger.jsonl"),
            emergency_stop_path: dir.join("stop"),
            watchlist_path: dir.join("wl.json"),
            ..SniperSettings::default()
        }
    }

    #[test]
    fn preflight_gate_ladder() {
        let dir = tempfile::tempdir().unwrap();
        let ready = settings(dir.path());
        preflight(
            &SniperSettings {
                enabled: false,
                ..ready.clone()
            },
            false,
        )
        .unwrap();
        preflight(&ready, true).unwrap();
        let blocked = |s: &SniperSettings, needle: &str| {
            let err = preflight(s, true).unwrap_err().to_string();
            assert!(err.contains(needle), "{err}");
        };
        blocked(
            &SniperSettings {
                enabled: false,
                ..ready.clone()
            },
            "ENABLED",
        );
        blocked(
            &SniperSettings {
                use_testnet: true,
                ..ready.clone()
            },
            "USE_TESTNET",
        );
        blocked(
            &SniperSettings {
                dry_run: true,
                ..ready.clone()
            },
            "DRY_RUN",
        );
        blocked(
            &SniperSettings {
                confirm_live: "no".into(),
                ..ready.clone()
            },
            "CONFIRM_LIVE_LISTING_SNIPER",
        );
        blocked(
            &SniperSettings {
                proven_edge: false,
                ..ready.clone()
            },
            "PROVEN_EDGE",
        );
        std::fs::write(&ready.emergency_stop_path, "halt").unwrap();
        let err = preflight(&ready, false).unwrap_err().to_string();
        assert!(err.contains("emergency stop"), "{err}");
    }

    async fn universe(server: &MockServer, symbols: &[(&str, &str, &str)]) {
        let rows: Vec<Value> = symbols
            .iter()
            .map(|(s, status, quote)| json!({"symbol": s, "status": status, "quoteAsset": quote, "baseAsset": "X"}))
            .collect();
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/exchangeInfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "symbols": rows })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn trading_symbols_filter_by_status_and_quote() {
        let server = MockServer::start().await;
        universe(
            &server,
            &[
                ("BTCUSDT", "TRADING", "USDT"),
                ("ETHUSDT", "BREAK", "USDT"),
                ("BTCUSDC", "TRADING", "USDC"),
                ("NEWUSDT", "TRADING", "USDT"),
            ],
        )
        .await;
        let got = market::trading_symbols_for_quote(&client(&server), "USDT")
            .await
            .unwrap();
        assert_eq!(got.into_iter().collect::<Vec<_>>(), ["BTCUSDT", "NEWUSDT"]);
    }

    #[tokio::test]
    async fn trading_symbols_http_error_is_reported() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/exchangeInfo"))
            .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
            .mount(&server)
            .await;
        let err = market::trading_symbols_for_quote(&client(&server), "USDT")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("503"), "{err}");
    }

    #[tokio::test]
    async fn paper_cycles_bootstrap_then_fire_on_diff_and_respect_cap() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let s = SniperSettings {
            daily_spend_cap_usdt: dec!(5),
            ..settings(dir.path())
        };
        let venue_client = client(&server);
        let venue = Venue {
            public: &venue_client,
            trading: None,
        };
        let alerter = Alerter::disabled();
        let mut state = SniperState::default();

        universe(&server, &[("BTCUSDT", "TRADING", "USDT")]).await;
        let report = run_cycle(&s, &mut state, SniperMode::Paper, venue, &alerter)
            .await
            .unwrap();
        assert!(report.new_symbols.is_empty());
        assert!(state.bootstrapped);
        assert_eq!(report.current_universe_size, 1);

        universe(
            &server,
            &[
                ("BTCUSDT", "TRADING", "USDT"),
                ("NEWUSDT", "TRADING", "USDT"),
                ("TWOUSDT", "TRADING", "USDT"),
            ],
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/ticker/price"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"symbol": "NEWUSDT", "price": "2"})),
            )
            .mount(&server)
            .await;
        let report = run_cycle(&s, &mut state, SniperMode::Paper, venue, &alerter)
            .await
            .unwrap();
        assert_eq!(report.new_symbols, ["NEWUSDT", "TWOUSDT"]);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["actions"][0]["mode"], "paper");
        assert_eq!(json["actions"][0]["action"], "round_trip_complete");
        assert_eq!(json["actions"][1]["action"], "skipped_daily_cap");
        assert_eq!(json["daily_spent_usdt"], json!(5));
        assert!(
            state.sniped_symbols.contains("NEWUSDT") && !state.sniped_symbols.contains("TWOUSDT")
        );

        let ledger: Vec<Value> = std::fs::read_to_string(&s.ledger_path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0]["symbol"], "NEWUSDT");
        assert!(ledger[0]["timestamp"].is_string() && ledger[0]["pnl_usdt"].is_number());

        let saved = SniperState::load(&s.state_path).unwrap();
        assert_eq!(saved, state);
        assert_eq!(
            std::fs::read_to_string(&s.audit_path)
                .unwrap()
                .lines()
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn plan_only_detects_without_firing_and_live_needs_gates() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let c = client(&server);
        let venue = Venue {
            public: &c,
            trading: Some(&c),
        };
        let mut state = SniperState {
            bootstrapped: true,
            known_trading_symbols: ["BTCUSDT".to_owned()].into(),
            ..SniperState::default()
        };
        universe(
            &server,
            &[
                ("BTCUSDT", "TRADING", "USDT"),
                ("NEWUSDT", "TRADING", "USDT"),
            ],
        )
        .await;
        let report = run_cycle(
            &s,
            &mut state,
            SniperMode::PlanOnly,
            venue,
            &Alerter::disabled(),
        )
        .await
        .unwrap();
        assert_eq!(report.new_symbols, ["NEWUSDT"]);
        assert!(report.actions.is_empty() && state.sniped_symbols.is_empty());
        assert!(!s.ledger_path.exists());

        let gated = SniperSettings {
            proven_edge: false,
            ..s
        };
        let err = run_cycle(
            &gated,
            &mut state,
            SniperMode::Live,
            venue,
            &Alerter::disabled(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ListingError::Gate(_)));
    }

    #[tokio::test]
    async fn live_low_balance_skips_without_ordering() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let s = SniperSettings {
            min_balance_usdt: dec!(10),
            ..settings(dir.path())
        };
        let c = client(&server);
        let venue = Venue {
            public: &c,
            trading: Some(&c),
        };
        let mut state = SniperState {
            bootstrapped: true,
            ..SniperState::default()
        };
        universe(&server, &[("NEWUSDT", "TRADING", "USDT")]).await;
        Mock::given(method("GET"))
            .and(path("/api/v3/time"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"serverTime": bb_core::time::now_ms()})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/account"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "balances": [{"asset": "USDT", "free": "3.5", "locked": "0"}]})))
            .mount(&server)
            .await;
        let report = run_cycle(
            &s,
            &mut state,
            SniperMode::Live,
            venue,
            &Alerter::disabled(),
        )
        .await
        .unwrap();
        assert_eq!(
            report.actions,
            [CycleAction::Skip(SkipAction::SkippedLowBalance {
                symbol: "NEWUSDT".into(),
                free_usdt: dec!(3.5),
                min_balance_usdt: dec!(10),
            })]
        );
        assert!(state.sniped_symbols.is_empty());
        let orders = server.received_requests().await.unwrap();
        assert!(orders.iter().all(|r| r.url.path() != "/api/v3/order"));
    }
}
