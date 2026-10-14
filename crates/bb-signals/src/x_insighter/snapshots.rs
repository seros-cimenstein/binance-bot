//! `bb x-insighter-snapshotter`: forward snapshots of every x-signal at the V2
//! offsets (+5m … +7d) into `x-signal-snapshots.jsonl`.
//!
//! Due/captured/retry semantics are the V2 ones
//! ([`crate::snapshotter::compute_due_snapshots`]): a `(detection_id, offset)`
//! is captured once and `api_error` rows are retried on the next run. Solana
//! rows reuse the V2 DexScreener record shape; Binance rows read the public
//! `exchangeInfo` status and `ticker/24hr` (`pair_exists: false` when the symbol
//! is not `TRADING`). Every row also carries `venue`.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bb_binance::spot::Ticker24h;
use bb_binance::{BinanceClient, BinanceConfig};
use bb_core::env;
use serde_json::{Value, json};

use super::schema::{Venue, XPaths, XSignal, load_rows};
use crate::common::{Clock, SystemClock, append_ledger, http_client, load_jsonl_objects, secs};
use crate::dexscreener_api::DexScreenerApi;
use crate::error::SignalsError;
use crate::jsonv::Object;
use crate::snapshotter::{
    BATCH_SIZE, DueSnapshot, SNAPSHOT_OFFSETS_SECONDS, build_api_error_snapshot,
    build_snapshot_record, compute_due_snapshots, fetch_token_pairs_batch,
};

const TRADING: &str = "TRADING";

#[derive(Debug, Clone, PartialEq)]
pub struct XSnapshotSettings {
    pub signals_path: PathBuf,
    pub snapshots_path: PathBuf,
    pub max_snapshots_per_run: usize,
    pub request_timeout_seconds: f64,
}

impl XSnapshotSettings {
    pub fn from_env() -> Result<Self, SignalsError> {
        let paths = XPaths::from_env();
        Ok(Self {
            signals_path: paths.signals,
            snapshots_path: paths.snapshots,
            max_snapshots_per_run: env::integer(&["X_INSIGHTER_SNAPSHOTTER_MAX_PER_RUN"], 500)?,
            request_timeout_seconds: 10.0,
        })
    }
}

/// A due snapshot plus the venue fields of its signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XDueSnapshot {
    pub due: DueSnapshot,
    pub venue: Venue,
    /// Binance `BASEUSDT`; `None` for Solana.
    pub symbol: Option<String>,
}

/// Due snapshots of x-signal rows (V2 semantics), tagged with each signal's venue.
/// Binance rows without a symbol are skipped (nothing to look up).
pub fn compute_x_due_snapshots(
    signals: &[XSignal],
    snapshots: &[Object],
    now_ms: i64,
) -> Vec<XDueSnapshot> {
    let detections: Vec<Object> = signals
        .iter()
        .filter(|s| s.venue == Venue::Solana || s.symbol.as_deref().is_some_and(|v| !v.is_empty()))
        .map(|s| {
            let mut obj = Object::new();
            obj.insert("detection_id".into(), json!(s.detection_id));
            obj.insert("token_address".into(), json!(s.token_address));
            obj.insert("pair_address".into(), json!(s.pair_address));
            obj.insert("detected_at_ms".into(), json!(s.detected_at_ms));
            obj
        })
        .collect();
    let by_id: HashMap<&str, &XSignal> = signals
        .iter()
        .map(|s| (s.detection_id.as_str(), s))
        .collect();
    compute_due_snapshots(&detections, snapshots, now_ms, &SNAPSHOT_OFFSETS_SECONDS)
        .into_iter()
        .filter_map(|due| {
            let signal = by_id.get(due.detection_id.as_str())?;
            Some(XDueSnapshot {
                venue: signal.venue,
                symbol: signal.symbol.clone(),
                due,
            })
        })
        .collect()
}

fn with_venue(mut record: Value, venue: Venue, symbol: Option<&str>) -> Value {
    if let Some(obj) = record.as_object_mut() {
        obj.insert("venue".into(), json!(venue.as_str()));
        if let Some(symbol) = symbol {
            obj.insert("symbol".into(), json!(symbol));
        }
    }
    record
}

fn decimal_f64(value: rust_decimal::Decimal) -> Option<f64> {
    let v = bb_core::money::to_f64(value);
    (v.is_finite() && v > 0.0).then_some(v)
}

/// Binance snapshot row in the V2 record shape. `status` is the symbol's
/// `exchangeInfo` status (`None` = symbol no longer listed). A non-`TRADING`
/// symbol is `pair_exists: false` with no price (a halted price is not tradable).
pub fn build_binance_record(
    item: &XDueSnapshot,
    status: Option<&str>,
    ticker: Option<&Ticker24h>,
    captured_at_ms: i64,
) -> Value {
    let trading = status == Some(TRADING);
    let row_status = match status {
        None => "symbol_gone",
        Some(TRADING) => "ok",
        Some(_) => "symbol_not_trading",
    };
    let price = ticker
        .filter(|_| trading)
        .and_then(|t| decimal_f64(t.last_price));
    let quote_volume = ticker
        .filter(|_| trading)
        .and_then(|t| decimal_f64(t.quote_volume));
    let raw = ticker.map_or(Value::Null, |t| {
        serde_json::to_value(t).unwrap_or(Value::Null)
    });
    with_venue(
        json!({
            "detection_id": item.due.detection_id,
            "scheduled_offset_seconds": item.due.scheduled_offset_seconds,
            "scheduled_for_ms": item.due.scheduled_for_ms,
            "captured_at_ms": captured_at_ms,
            "status": row_status,
            "pair_exists": trading,
            "symbol_status": status,
            "price_usd": price,
            "liquidity_usd": null,
            "volume": {"m5": null, "h1": null, "h6": null, "h24": quote_volume},
            "fdv": null,
            "market_cap": null,
            "pair_url": null,
            "raw_pair": null,
            "raw_ticker": raw,
        }),
        Venue::Binance,
        item.symbol.as_deref(),
    )
}

async fn solana_records(
    api: &DexScreenerApi,
    batch: &[&XDueSnapshot],
    captured_at_ms: i64,
) -> Vec<Value> {
    let tokens: Vec<String> = batch
        .iter()
        .map(|d| d.due.token_address.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    match fetch_token_pairs_batch(api, &tokens).await {
        Ok(grouped) => batch
            .iter()
            .map(|item| {
                let pairs = grouped
                    .iter()
                    .find(|(a, _)| *a == item.due.token_address)
                    .map_or(&[][..], |(_, p)| p.as_slice());
                with_venue(
                    build_snapshot_record(&item.due, pairs, captured_at_ms),
                    Venue::Solana,
                    None,
                )
            })
            .collect(),
        Err(err) => {
            tracing::warn!(%err, "x-insighter snapshotter DexScreener batch failed");
            batch
                .iter()
                .map(|item| {
                    with_venue(
                        build_api_error_snapshot(&item.due, captured_at_ms, &err.to_string()),
                        Venue::Solana,
                        None,
                    )
                })
                .collect()
        }
    }
}

fn binance_error_record(item: &XDueSnapshot, captured_at_ms: i64, error: &str) -> Value {
    with_venue(
        build_api_error_snapshot(&item.due, captured_at_ms, error),
        Venue::Binance,
        item.symbol.as_deref(),
    )
}

async fn binance_records(
    client: &BinanceClient,
    items: &[&XDueSnapshot],
    clock: &dyn Clock,
) -> Vec<Value> {
    let info = match client.exchange_info(None).await {
        Ok(info) => info,
        Err(err) => {
            tracing::warn!(%err, "x-insighter snapshotter exchangeInfo failed");
            let captured_at_ms = clock.now_ms();
            return items
                .iter()
                .map(|item| binance_error_record(item, captured_at_ms, &err.to_string()))
                .collect();
        }
    };
    let statuses: HashMap<&str, &str> = info
        .symbols
        .iter()
        .map(|s| (s.symbol.as_str(), s.status.as_str()))
        .collect();
    let symbols: BTreeSet<&str> = items.iter().filter_map(|i| i.symbol.as_deref()).collect();
    let mut tickers: HashMap<&str, Result<Ticker24h, String>> = HashMap::new();
    for symbol in symbols {
        if statuses.get(symbol) == Some(&TRADING) {
            let result = client.ticker_24h(symbol).await.map_err(|e| e.to_string());
            tickers.insert(symbol, result);
        }
    }
    let captured_at_ms = clock.now_ms();
    items
        .iter()
        .map(|item| {
            let symbol = item.symbol.as_deref().unwrap_or_default();
            let status = statuses.get(symbol).copied();
            match tickers.get(symbol) {
                Some(Err(err)) => binance_error_record(item, captured_at_ms, err),
                Some(Ok(ticker)) => {
                    build_binance_record(item, status, Some(ticker), captured_at_ms)
                }
                None => build_binance_record(item, status, None, captured_at_ms),
            }
        })
        .collect()
}

/// Capture currently due x-signal snapshots (capped per run); returns rows written.
pub async fn run_snapshotter_once(
    settings: &XSnapshotSettings,
    dex: &DexScreenerApi,
    binance: &BinanceClient,
    clock: Arc<dyn Clock>,
    now_ms: i64,
) -> Result<usize, SignalsError> {
    let signals: Vec<XSignal> = load_rows(&settings.signals_path);
    let snapshots = load_jsonl_objects(&settings.snapshots_path);
    let mut due = compute_x_due_snapshots(&signals, &snapshots, now_ms);
    if due.len() > settings.max_snapshots_per_run {
        tracing::warn!(
            due = due.len(),
            processing = settings.max_snapshots_per_run,
            "x-insighter snapshotter due backlog exceeds per-run cap"
        );
        due.truncate(settings.max_snapshots_per_run);
    }
    if due.is_empty() {
        tracing::info!("x-insighter snapshotter found no due snapshots");
        return Ok(0);
    }

    let (solana, binance_due): (Vec<&XDueSnapshot>, Vec<&XDueSnapshot>) =
        due.iter().partition(|d| d.venue == Venue::Solana);
    let mut written = 0;
    let batches: Vec<&[&XDueSnapshot]> = solana.chunks(BATCH_SIZE).collect();
    for (index, batch) in batches.iter().enumerate() {
        for record in solana_records(dex, batch, clock.now_ms()).await {
            append_ledger(&settings.snapshots_path, &record)?;
            written += 1;
        }
        if index + 1 < batches.len() {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    if !binance_due.is_empty() {
        for record in binance_records(binance, &binance_due, clock.as_ref()).await {
            append_ledger(&settings.snapshots_path, &record)?;
            written += 1;
        }
    }
    tracing::info!(written, "x-insighter snapshotter wrote snapshot rows");
    Ok(written)
}

/// `bb x-insighter-snapshotter`: one run with public endpoints only.
pub async fn run_cli() -> anyhow::Result<()> {
    let settings = XSnapshotSettings::from_env()?;
    let dex = DexScreenerApi::new(http_client(secs(settings.request_timeout_seconds))?);
    let binance = BinanceClient::new(BinanceConfig::new(false))?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let now_ms = clock.now_ms();
    run_snapshotter_once(&settings, &dex, &binance, clock, now_ms).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::FixedClock;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const NOW_MS: i64 = 1_800_000_000_000;

    fn binance_signal(id: &str, detected_at_ms: i64, symbol: &str) -> XSignal {
        XSignal {
            detection_id: id.into(),
            detected_at_ms,
            venue: Venue::Binance,
            token_key: format!("binance:{}", symbol.trim_end_matches("USDT")),
            token_address: symbol.trim_end_matches("USDT").into(),
            pair_address: None,
            symbol: Some(symbol.into()),
            ..XSignal::default()
        }
    }

    fn snap(id: &str, offset: i64, status: &str) -> Object {
        json!({"detection_id": id, "scheduled_offset_seconds": offset, "status": status})
            .as_object()
            .cloned()
            .unwrap()
    }

    #[test]
    fn binance_rows_without_pair_are_due_and_api_errors_retry() {
        let signals = [binance_signal("x1", NOW_MS - 3_600_000, "PEPEUSDT")];
        let due = compute_x_due_snapshots(&signals, &[snap("x1", 300, "ok")], NOW_MS);
        let offsets: Vec<i64> = due.iter().map(|d| d.due.scheduled_offset_seconds).collect();
        assert_eq!(offsets, vec![900, 3600]);
        assert!(due.iter().all(|d| d.venue == Venue::Binance));
        assert_eq!(due[0].symbol.as_deref(), Some("PEPEUSDT"));
        assert_eq!(due[0].due.pair_address, "");

        let due = compute_x_due_snapshots(
            &signals,
            &[
                snap("x1", 300, "api_error"),
                snap("x1", 900, "symbol_not_trading"),
            ],
            NOW_MS,
        );
        let offsets: Vec<i64> = due.iter().map(|d| d.due.scheduled_offset_seconds).collect();
        assert_eq!(offsets, vec![300, 3600]);
    }

    #[test]
    fn non_trading_symbol_is_pair_gone_without_price() {
        let item = XDueSnapshot {
            due: DueSnapshot {
                detection_id: "x1".into(),
                token_address: "PEPE".into(),
                pair_address: String::new(),
                scheduled_offset_seconds: 300,
                scheduled_for_ms: NOW_MS,
            },
            venue: Venue::Binance,
            symbol: Some("PEPEUSDT".into()),
        };
        let ticker: Ticker24h = serde_json::from_value(json!({
            "symbol": "PEPEUSDT", "lastPrice": "0.0000125", "quoteVolume": "1234567.5"
        }))
        .unwrap();
        let ok = build_binance_record(&item, Some("TRADING"), Some(&ticker), NOW_MS);
        assert_eq!(ok["pair_exists"], json!(true));
        assert_eq!(ok["price_usd"].as_f64(), Some(0.000_012_5));
        assert_eq!(ok["volume"]["h24"].as_f64(), Some(1_234_567.5));
        assert_eq!(ok["venue"], "binance");

        let halted = build_binance_record(&item, Some("BREAK"), Some(&ticker), NOW_MS);
        assert_eq!(halted["pair_exists"], json!(false));
        assert_eq!(halted["status"], "symbol_not_trading");
        assert!(halted["price_usd"].is_null());

        let gone = build_binance_record(&item, None, None, NOW_MS);
        assert_eq!(gone["status"], "symbol_gone");
        assert_eq!(gone["pair_exists"], json!(false));
    }

    #[tokio::test]
    async fn run_writes_binance_status_rows_from_public_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let settings = XSnapshotSettings {
            signals_path: dir.path().join("x-signals.jsonl"),
            snapshots_path: dir.path().join("x-signal-snapshots.jsonl"),
            max_snapshots_per_run: 500,
            request_timeout_seconds: 5.0,
        };
        for s in [
            binance_signal("x1", NOW_MS - 300_000, "PEPEUSDT"),
            binance_signal("x2", NOW_MS - 300_000, "OLDUSDT"),
        ] {
            append_ledger(&settings.signals_path, &s).unwrap();
        }
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/exchangeInfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"symbols": [
                {"symbol": "PEPEUSDT", "status": "TRADING", "baseAsset": "PEPE", "quoteAsset": "USDT"},
                {"symbol": "OLDUSDT", "status": "BREAK", "baseAsset": "OLD", "quoteAsset": "USDT"}
            ]})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/ticker/24hr"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "symbol": "PEPEUSDT", "lastPrice": "2.5", "quoteVolume": "1000"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let binance = BinanceClient::new(
            BinanceConfig::new(false)
                .with_spot_rest_url(server.uri())
                .with_retries(0, Duration::ZERO, Duration::ZERO),
        )
        .unwrap();
        let dex = DexScreenerApi::with_base_url(reqwest::Client::new(), server.uri());
        let written = run_snapshotter_once(
            &settings,
            &dex,
            &binance,
            Arc::new(FixedClock(NOW_MS)),
            NOW_MS,
        )
        .await
        .unwrap();
        assert_eq!(written, 2);
        let rows = load_jsonl_objects(&settings.snapshots_path);
        let by_id = |id: &str| rows.iter().find(|r| r["detection_id"] == id).unwrap();
        assert_eq!(by_id("x1")["price_usd"].as_f64(), Some(2.5));
        assert_eq!(by_id("x2")["pair_exists"], json!(false));
    }
}
