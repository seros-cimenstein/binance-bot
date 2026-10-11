//! Binance new-listing announcement detector (port of `src/listing_detector.py`).
//!
//! Polls the public CMS article list for the "New Cryptocurrency Listing"
//! catalog, classifies titles, extracts parenthesised tickers, arms the shared
//! watchlist for fresh `spot_list` announcements, and keeps a persistent
//! `last_seen_article_id` cursor so cron re-runs never re-fire.
//!
//! Modes: `detect_only` (audit + alert), `paper` (record a hypothetical round
//! trip for pairs already live), `live` (market buy, flat/trailing hold,
//! market sell; behind the full gate ladder).

use std::path::{Path, PathBuf};
use std::time::Duration;

use bb_binance::BinanceClient;
use bb_binance::spot::Order;
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::alerts::Alerter;
use crate::error::{ListingError, Result};
use crate::market;
use crate::util::{append_jsonl, timestamp, truncate};
use crate::watchlist::{DEFAULT_EXPIRY_DAYS, WatchEntry, Watchlist};

pub const ANNOUNCEMENTS_URL: &str =
    "https://www.binance.com/bapi/composite/v1/public/cms/article/list/query";
/// "New Cryptocurrency Listing".
pub const NEW_LISTING_CATALOG_ID: u32 = 48;
const PAGE_SIZE: u32 = 20;
const ANNOUNCEMENTS_TIMEOUT: Duration = Duration::from_secs(20);

/// Upper-case words in parentheses that are never tickers.
const TICKER_BLOCKLIST: [&str; 9] = ["UTC", "USD", "USDT", "EU", "USA", "UK", "AI", "BNB", "USDⓈ"];

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Detector hold shape for live round trips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectorHoldMode {
    /// Buy, sleep `hold_seconds`, market sell.
    Flat,
    /// Sleep `hold_seconds` (floor), then poll and sell once price retraces
    /// `trailing_stop_pct` from the running peak or `max_hold_seconds` elapses.
    Trailing,
}

impl DetectorHoldMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "flat" => Ok(Self::Flat),
            "trailing" => Ok(Self::Trailing),
            other => Err(ListingError::InvalidSettings(format!(
                "unknown hold_mode: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DetectorSettings {
    /// Only pairs against this quote are resolved/traded.
    pub quote_asset: String,
    pub notional_usdt: Decimal,
    pub hold_mode: DetectorHoldMode,
    /// Flat hold, and the trailing-mode initial floor.
    pub hold_seconds: u64,
    pub trailing_stop_pct: Decimal,
    /// Trailing-mode hard cap.
    pub max_hold_seconds: u64,
    pub trailing_poll_interval_seconds: u64,
    pub enabled: bool,
    pub confirm_live: String,
    pub proven_edge: bool,
    pub use_testnet: bool,
    pub dry_run: bool,
    pub state_path: PathBuf,
    pub audit_path: PathBuf,
    pub ledger_path: PathBuf,
    pub emergency_stop_path: PathBuf,
    /// Shared watchlist consumed by the sniper.
    pub watchlist_path: PathBuf,
    pub announcements_url: String,
}

impl Default for DetectorSettings {
    fn default() -> Self {
        Self {
            quote_asset: "USDT".into(),
            notional_usdt: Decimal::from(5),
            hold_mode: DetectorHoldMode::Flat,
            hold_seconds: 600,
            trailing_stop_pct: Decimal::from(15),
            max_hold_seconds: 86_400,
            trailing_poll_interval_seconds: 60,
            enabled: false,
            confirm_live: "no".into(),
            proven_edge: false,
            use_testnet: false,
            dry_run: true,
            state_path: "data/listing-detector-state.json".into(),
            audit_path: "data/listing-detector-events.jsonl".into(),
            ledger_path: "data/listing-detector-ledger.jsonl".into(),
            emergency_stop_path: "data/listing-detector.stop".into(),
            watchlist_path: "data/listing-watchlist.json".into(),
            announcements_url: ANNOUNCEMENTS_URL.into(),
        }
    }
}

impl DetectorSettings {
    pub fn from_env() -> Result<Self> {
        use bb_core::env::{boolean, confirmed, decimal, integer, string};
        let d = Self::default();
        let path =
            |name: &str, default: &Path| PathBuf::from(string(&[name], &default.to_string_lossy()));
        Ok(Self {
            quote_asset: string(&["LISTING_DETECTOR_QUOTE_ASSET"], &d.quote_asset),
            notional_usdt: decimal(&["LISTING_DETECTOR_NOTIONAL_USDT"], d.notional_usdt)?,
            hold_mode: DetectorHoldMode::parse(&string(&["LISTING_DETECTOR_HOLD_MODE"], "flat"))?,
            hold_seconds: integer(&["LISTING_DETECTOR_HOLD_SECONDS"], d.hold_seconds)?,
            trailing_stop_pct: decimal(
                &["LISTING_DETECTOR_TRAILING_STOP_PCT"],
                d.trailing_stop_pct,
            )?,
            max_hold_seconds: integer(&["LISTING_DETECTOR_MAX_HOLD_SECONDS"], d.max_hold_seconds)?,
            trailing_poll_interval_seconds: integer(
                &["LISTING_DETECTOR_TRAILING_POLL_INTERVAL_SECONDS"],
                d.trailing_poll_interval_seconds,
            )?,
            enabled: boolean(&["LISTING_DETECTOR_ENABLED"], false)?,
            confirm_live: string(&["CONFIRM_LIVE_LISTING_DETECTOR"], "no"),
            proven_edge: confirmed(&["LISTING_DETECTOR_PROVEN_EDGE"]),
            use_testnet: boolean(&["USE_TESTNET"], false)?,
            dry_run: boolean(&["DRY_RUN"], true)?,
            state_path: path("LISTING_DETECTOR_STATE_PATH", &d.state_path),
            audit_path: path("LISTING_DETECTOR_AUDIT_PATH", &d.audit_path),
            ledger_path: path("LISTING_DETECTOR_LEDGER_PATH", &d.ledger_path),
            emergency_stop_path: path(
                "LISTING_DETECTOR_EMERGENCY_STOP_PATH",
                &d.emergency_stop_path,
            ),
            watchlist_path: path("LISTING_WATCHLIST_PATH", &d.watchlist_path),
            announcements_url: d.announcements_url,
        })
    }
}

// ---------------------------------------------------------------------------
// Events, classification, cursor state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListingType {
    SpotList,
    SpotAdd,
    HodlerAirdrop,
    FuturesOnly,
    Other,
}

/// Classify an announcement title. "Binance Futures" wins over "Will List".
pub fn classify_title(title: &str) -> ListingType {
    let lower = title.to_lowercase();
    if lower.contains("binance futures") {
        ListingType::FuturesOnly
    } else if lower.contains("binance will list") {
        ListingType::SpotList
    } else if lower.contains("binance will add") {
        ListingType::SpotAdd
    } else if is_hodler_intro(&lower) {
        ListingType::HodlerAirdrop
    } else {
        ListingType::Other
    }
}

/// `Introducing .* on Binance HODLer` (case-insensitive; `.` excludes newlines).
fn is_hodler_intro(lower: &str) -> bool {
    lower.split('\n').any(|line| {
        line.find("introducing ").is_some_and(|start| {
            line[start + "introducing ".len()..].contains(" on binance hodler")
        })
    })
}

/// Every `(TICKER)` — `[A-Z][A-Z0-9]{1,15}` inside parentheses — in title
/// order, de-duplicated, with common abbreviations filtered.
pub fn extract_tickers(title: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = title;
    while let Some(open) = rest.find('(') {
        let after = &rest[open + 1..];
        let Some(close) = after.find(')') else { break };
        let candidate = &after[..close];
        if is_ticker(candidate)
            && !TICKER_BLOCKLIST.contains(&candidate)
            && !out.iter().any(|t| t == candidate)
        {
            out.push(candidate.to_owned());
        }
        // A '(' inside the candidate may start the next match (regex semantics).
        rest = if candidate.contains('(') {
            after
        } else {
            &after[close + 1..]
        };
    }
    out
}

fn is_ticker(s: &str) -> bool {
    let bytes = s.as_bytes();
    (2..=16).contains(&bytes.len())
        && bytes[0].is_ascii_uppercase()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// First ticker, if any.
pub fn extract_ticker(title: &str) -> Option<String> {
    extract_tickers(title).into_iter().next()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListingEvent {
    pub article_id: i64,
    pub title: String,
    pub release_time_ms: i64,
    pub listing_type: ListingType,
    /// Tickers found in the title, in announcement order.
    pub detected_tokens: Vec<String>,
    /// `TOKEN/QUOTE` pairs that already trade on this venue (resolved by [`run_once`]).
    pub detected_pairs: Vec<String>,
}

impl ListingEvent {
    pub fn detected_token(&self) -> Option<&str> {
        self.detected_tokens.first().map(String::as_str)
    }

    pub fn detected_pair(&self) -> Option<&str> {
        self.detected_pairs.first().map(String::as_str)
    }
}

/// One CMS article row (only the fields the detector reads).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Article {
    #[serde(deserialize_with = "lenient_i64")]
    pub id: i64,
    #[serde(deserialize_with = "lenient_string")]
    pub title: String,
    #[serde(deserialize_with = "lenient_i64")]
    pub release_date: i64,
}

fn lenient_i64<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<i64, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    })
}

fn lenient_string<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => s,
        Value::Null => String::new(),
        other => other.to_string(),
    })
}

pub fn parse_announcement(article: &Article) -> ListingEvent {
    ListingEvent {
        article_id: article.id,
        title: article.title.clone(),
        release_time_ms: article.release_date,
        listing_type: classify_title(&article.title),
        detected_tokens: extract_tickers(&article.title),
        detected_pairs: Vec::new(),
    }
}

/// Articles newer than the cursor, oldest release first.
pub fn detect_new_listings(articles: &[Article], last_seen_article_id: i64) -> Vec<ListingEvent> {
    let mut fresh: Vec<ListingEvent> = articles
        .iter()
        .map(parse_announcement)
        .filter(|e| e.article_id > last_seen_article_id)
        .collect();
    fresh.sort_by_key(|e| e.release_time_ms);
    fresh
}

/// `data/listing-detector-state.json`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DetectorState {
    #[serde(deserialize_with = "lenient_i64")]
    pub last_poll_time_ms: i64,
    #[serde(deserialize_with = "lenient_i64")]
    pub last_seen_article_id: i64,
}

impl DetectorState {
    /// Missing file = zero cursor; unreadable/corrupt file is an error.
    pub fn load(path: &Path) -> Result<Self> {
        bb_core::fsutil::read_json(path)
            .map(Option::unwrap_or_default)
            .map_err(|source| ListingError::StateRead {
                path: path.to_owned(),
                source,
            })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        bb_core::fsutil::write_json_atomic(path, self).map_err(|source| ListingError::Write {
            path: path.to_owned(),
            source,
        })
    }
}

// ---------------------------------------------------------------------------
// Announcements API
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CmsResponse {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: Option<CmsData>,
}

#[derive(Debug, Deserialize)]
struct CmsData {
    #[serde(default)]
    catalogs: Option<Vec<CmsCatalog>>,
}

#[derive(Debug, Deserialize)]
struct CmsCatalog {
    #[serde(default)]
    articles: Option<Vec<Article>>,
}

/// Latest `page_size` articles of the new-listing catalog.
pub async fn fetch_announcements(
    http: &reqwest::Client,
    url: &str,
    page_size: u32,
) -> Result<Vec<Article>> {
    let fail = |msg: String| ListingError::Announcements(msg);
    let resp = http
        .get(url)
        .query(&[
            ("type", 1),
            ("catalogId", NEW_LISTING_CATALOG_ID),
            ("pageSize", page_size),
            ("pageNo", 1),
        ])
        .timeout(ANNOUNCEMENTS_TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            fail(format!(
                "announcements API request failed: {}",
                e.without_url()
            ))
        })?;
    let status = resp.status();
    if !status.is_success() {
        return Err(fail(format!(
            "announcements API returned HTTP {}",
            status.as_u16()
        )));
    }
    let payload: CmsResponse = resp.json().await.map_err(|e| {
        fail(format!(
            "announcements API returned malformed JSON: {}",
            e.without_url()
        ))
    })?;
    if !payload.success {
        return Err(fail(
            "announcements API returned non-success payload".into(),
        ));
    }
    Ok(payload
        .data
        .and_then(|d| d.catalogs)
        .and_then(|c| c.into_iter().next())
        .and_then(|c| c.articles)
        .unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Gates
// ---------------------------------------------------------------------------

pub fn ensure_no_emergency_stop(path: &Path) -> Result<()> {
    if bb_core::fsutil::stop_requested(path) {
        return Err(ListingError::EmergencyStop(path.to_owned()));
    }
    Ok(())
}

pub fn preflight_live(settings: &DetectorSettings) -> Result<()> {
    ensure_no_emergency_stop(&settings.emergency_stop_path)?;
    let gate = |msg: &str| Err(ListingError::Gate(msg.to_owned()));
    if !settings.enabled {
        return gate("LISTING_DETECTOR_ENABLED=true is required for live submission");
    }
    if settings.use_testnet {
        return gate("listing detector live submission requires USE_TESTNET=false");
    }
    if settings.dry_run {
        return gate("listing detector live submission requires DRY_RUN=false");
    }
    if settings.confirm_live != "yes" {
        return gate("listing detector live submission requires CONFIRM_LIVE_LISTING_DETECTOR=yes");
    }
    if !settings.proven_edge {
        return gate(
            "listing detector live submission requires LISTING_DETECTOR_PROVEN_EDGE=yes. \
             Run --paper for at least 2-4 detected events first, review the ledger, \
             and set this only when paper P&L looks coherent.",
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Live execution
// ---------------------------------------------------------------------------

/// Sleep per `hold_mode`; returns `(hold_seconds, exit_reason)`.
pub async fn hold_then_sell(
    settings: &DetectorSettings,
    client: &BinanceClient,
    pair: &str,
    avg_buy: Decimal,
) -> (u64, String) {
    tokio::time::sleep(Duration::from_secs(settings.hold_seconds)).await;
    if settings.hold_mode == DetectorHoldMode::Flat {
        return (settings.hold_seconds, "flat_hold_elapsed".into());
    }
    let mut peak = avg_buy.max(market::last_price(client, pair).await.unwrap_or(avg_buy));
    let mut elapsed = settings.hold_seconds;
    let stop_factor = Decimal::ONE - settings.trailing_stop_pct / Decimal::ONE_HUNDRED;
    while elapsed < settings.max_hold_seconds {
        tokio::time::sleep(Duration::from_secs(settings.trailing_poll_interval_seconds)).await;
        elapsed += settings.trailing_poll_interval_seconds;
        let Some(last) = market::last_price(client, pair).await else {
            continue;
        };
        if last > peak {
            peak = last;
            continue;
        }
        if last <= peak * stop_factor {
            return (
                elapsed,
                format!(
                    "trailing_stop_hit: last={:.6} <= peak*{:.4}={:.6}",
                    last.round_dp(6),
                    stop_factor.round_dp(4),
                    (peak * stop_factor).round_dp(6)
                ),
            );
        }
    }
    (elapsed, "max_hold_seconds_reached".into())
}

/// Outcome of one live (or paper) action on a pair.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PairOutcome {
    BuyFailed {
        symbol: String,
        reason: String,
    },
    BuyZeroFill {
        symbol: String,
        buy_order: Box<Order>,
    },
    SellFailed {
        symbol: String,
        buy_order: Box<Order>,
        reason: String,
        hold_seconds: u64,
        exit_reason: String,
    },
    RoundTripComplete {
        symbol: String,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        filled_qty: Decimal,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        avg_buy: Decimal,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        avg_sell: Decimal,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        sold_qty: Decimal,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        pnl_quote: Decimal,
        hold_mode: DetectorHoldMode,
        hold_seconds: u64,
        exit_reason: String,
        buy_order_id: String,
        sell_order_id: String,
    },
    PaperBuySell {
        symbol: String,
        #[serde(with = "rust_decimal::serde::arbitrary_precision")]
        notional_usdt: Decimal,
        hold_seconds: u64,
        note: String,
    },
}

/// Market buy `pair` for `notional_usdt` quote, hold per `hold_mode`, market sell.
pub async fn execute_pair_live(
    settings: &DetectorSettings,
    client: &BinanceClient,
    pair: &str,
) -> PairOutcome {
    let symbol = pair.to_owned();
    let buy = match market::market_buy_quote(client, pair, settings.notional_usdt).await {
        Ok(order) => order,
        Err(reason) => return PairOutcome::BuyFailed { symbol, reason },
    };
    let filled_qty = buy.executed_qty;
    let avg_buy = market::average_price(&buy);
    if filled_qty <= Decimal::ZERO {
        return PairOutcome::BuyZeroFill {
            symbol,
            buy_order: Box::new(buy),
        };
    }
    let (hold_seconds, exit_reason) = hold_then_sell(settings, client, pair, avg_buy).await;
    let sell_qty = market::sellable_quantity(client, pair, &buy).await;
    let sell = match market::market_sell(client, pair, sell_qty).await {
        Ok(order) => order,
        Err(reason) => {
            return PairOutcome::SellFailed {
                symbol,
                buy_order: Box::new(buy),
                reason,
                hold_seconds,
                exit_reason,
            };
        }
    };
    let avg_sell = market::average_price(&sell);
    PairOutcome::RoundTripComplete {
        symbol,
        filled_qty,
        avg_buy,
        avg_sell,
        sold_qty: sell.executed_qty,
        pnl_quote: (avg_sell - avg_buy) * filled_qty,
        hold_mode: settings.hold_mode,
        hold_seconds,
        exit_reason,
        buy_order_id: buy.order_id.to_string(),
        sell_order_id: sell.order_id.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Cycle
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectorMode {
    DetectOnly,
    Paper,
    Live,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum DetectorAction {
    Watchlist(WatchlistAction),
    Pair {
        article_id: i64,
        pair: String,
        #[serde(flatten)]
        outcome: PairOutcome,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WatchlistAction {
    WatchlistArmed { tokens: Vec<String> },
    WatchlistError { reason: String },
}

/// One audit row in `data/listing-detector-events.jsonl` (also printed by the CLI).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DetectorReport {
    pub actions: Vec<DetectorAction>,
    pub events: Vec<ListingEvent>,
    pub mode: DetectorMode,
    pub new_events_count: usize,
    pub previous_cursor_article_id: i64,
    pub timestamp: String,
}

/// Add every token of each fresh `spot_list` event to the shared watchlist.
fn arm_watchlist(
    settings: &DetectorSettings,
    events: &[ListingEvent],
) -> Result<Option<WatchlistAction>> {
    let now = bb_core::time::now();
    let mut watchlist = match Watchlist::load(&settings.watchlist_path, true, now) {
        Ok(wl) => wl,
        Err(ListingError::Watchlist(reason)) => {
            return Ok(Some(WatchlistAction::WatchlistError {
                reason: format!("WatchlistError: {}", truncate(&reason, 200)),
            }));
        }
        Err(other) => return Err(other),
    };
    let mut added = Vec::new();
    for event in events
        .iter()
        .filter(|e| e.listing_type == ListingType::SpotList)
    {
        let announced = if event.release_time_ms != 0 {
            event.release_time_ms
        } else {
            now.timestamp_millis()
        };
        for token in &event.detected_tokens {
            if watchlist.add(WatchEntry::new(
                token,
                event.article_id,
                announced,
                DEFAULT_EXPIRY_DAYS,
            )) {
                added.push(token.clone());
            }
        }
    }
    if added.is_empty() {
        return Ok(None);
    }
    watchlist.save(&settings.watchlist_path)?;
    Ok(Some(WatchlistAction::WatchlistArmed { tokens: added }))
}

/// One poll: fetch announcements, detect fresh events, arm the watchlist,
/// alert, optionally act, advance the cursor and append the audit row.
/// `client` resolves pairs (paper/live) and trades (live).
pub async fn run_once(
    settings: &DetectorSettings,
    mode: DetectorMode,
    http: &reqwest::Client,
    client: Option<&BinanceClient>,
    alerter: &Alerter,
) -> Result<DetectorReport> {
    ensure_no_emergency_stop(&settings.emergency_stop_path)?;
    if mode == DetectorMode::Live {
        preflight_live(settings)?;
    }
    let state = DetectorState::load(&settings.state_path)?;
    let articles = fetch_announcements(http, &settings.announcements_url, PAGE_SIZE).await?;
    let new_events = detect_new_listings(&articles, state.last_seen_article_id);

    let mut actions = Vec::new();
    if !new_events.is_empty()
        && let Some(action) = arm_watchlist(settings, &new_events)?
    {
        actions.push(DetectorAction::Watchlist(action));
    }

    let mut enriched = Vec::with_capacity(new_events.len());
    for mut event in new_events.iter().cloned() {
        let is_spot =
            event.listing_type == ListingType::SpotList && !event.detected_tokens.is_empty();
        if is_spot && let Some(client) = client {
            for token in &event.detected_tokens {
                let pair = format!("{token}/{}", settings.quote_asset);
                if market::exchange_has_pair(client, &pair).await {
                    event.detected_pairs.push(pair);
                }
            }
        }
        if is_spot {
            let pair_note = if event.detected_pairs.is_empty() {
                " (pair(s) not yet live on spot)".to_owned()
            } else {
                format!(" ({} live on spot)", event.detected_pairs.join(", "))
            };
            alerter
                .send(&format!(
                    "New Binance spot listing detected: {}{pair_note}\nTitle: {}",
                    event.detected_tokens.join(", "),
                    truncate(&event.title, 200)
                ))
                .await;
        }
        if event.listing_type == ListingType::SpotList {
            for pair in &event.detected_pairs {
                let outcome = match (mode, client) {
                    (DetectorMode::Live, Some(client)) => {
                        execute_pair_live(settings, client, pair).await
                    }
                    (DetectorMode::Paper, _) => PairOutcome::PaperBuySell {
                        symbol: pair.clone(),
                        notional_usdt: settings.notional_usdt,
                        hold_seconds: settings.hold_seconds,
                        note: "real-execution P&L requires live mode".into(),
                    },
                    _ => continue,
                };
                actions.push(DetectorAction::Pair {
                    article_id: event.article_id,
                    pair: pair.clone(),
                    outcome,
                });
            }
        }
        enriched.push(event);
    }

    if let Some(newest) = new_events.iter().map(|e| e.article_id).max() {
        DetectorState {
            last_seen_article_id: state.last_seen_article_id.max(newest),
            last_poll_time_ms: bb_core::time::now_ms(),
        }
        .save(&settings.state_path)?;
    }

    let report = DetectorReport {
        actions,
        events: enriched,
        mode,
        new_events_count: new_events.len(),
        previous_cursor_article_id: state.last_seen_article_id,
        timestamp: timestamp(),
    };
    append_jsonl(&settings.audit_path, &report)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use bb_binance::{BinanceConfig, Credentials};
    use pretty_assertions::assert_eq;
    use rust_decimal_macros::dec;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn article(id: i64, title: &str, release: i64) -> Article {
        Article {
            id,
            title: title.into(),
            release_date: release,
        }
    }

    #[test]
    fn classifies_titles() {
        use ListingType::*;
        assert_eq!(
            classify_title("Binance Will List Gensyn (AIGENSYN) with Seed Tag Applied"),
            SpotList
        );
        assert_eq!(
            classify_title("Binance Futures Will Launch QNTXUSDT USDⓈ-Margined Perpetual Contract"),
            FuturesOnly
        );
        assert_eq!(
            classify_title(
                "Binance Will Add Gensyn (AIGENSYN) on Earn, Buy Crypto, Convert, VIP Loan & Margin"
            ),
            SpotAdd
        );
        assert_eq!(
            classify_title("Introducing Genius Terminal (GENIUS) on Binance HODLer Airdrops!"),
            HodlerAirdrop
        );
        assert_eq!(classify_title("binance WILL list x"), SpotList);
        assert_eq!(classify_title("Binance Daily Market Wrap"), Other);
    }

    #[test]
    fn extracts_tickers() {
        assert_eq!(
            extract_ticker("Binance Will List Gensyn (AIGENSYN) with Seed Tag").as_deref(),
            Some("AIGENSYN")
        );
        assert_eq!(
            extract_ticker("Foo (FOO) listing at 12:00 (UTC) on the (USDT) market").as_deref(),
            Some("FOO")
        );
        assert_eq!(extract_ticker("Binance Daily Market Wrap"), None);
        assert_eq!(
            extract_tickers(
                "Binance Will List Genius Terminal (GENIUS) and OpenGradient (OPG) with Seed Tag Applied"
            ),
            ["GENIUS", "OPG"]
        );
        assert_eq!(extract_tickers("Foo (FOO) on FOO/(FOO) pair"), ["FOO"]);
        assert!(extract_tickers("Binance Daily Wrap").is_empty());
        assert_eq!(
            extract_tickers("Foo (FOO) launches at 12:00 (UTC), priced in (USDT)"),
            ["FOO"]
        );
        assert_eq!(
            extract_ticker("Binance Will List Genius (GENIUS) and OpenGradient (OPG)").as_deref(),
            Some("GENIUS")
        );
        // Regex edge cases: nested '(' restarts the match; length and charset bounds.
        assert_eq!(
            extract_tickers("((AB) (A) (Ab1) (A1234567890123456) (A123456789012345)"),
            ["AB", "A123456789012345"]
        );
    }

    #[test]
    fn parses_announcements() {
        let e = parse_announcement(&article(123, "Binance Will List Gensyn (AIGENSYN)", 999));
        assert_eq!(e.article_id, 123);
        assert_eq!(e.detected_token(), Some("AIGENSYN"));
        assert_eq!(e.listing_type, ListingType::SpotList);
        assert_eq!(e.release_time_ms, 999);

        let empty: Article = serde_json::from_str("{}").unwrap();
        let e = parse_announcement(&empty);
        assert_eq!(
            (e.article_id, e.detected_token(), e.listing_type),
            (0, None, ListingType::Other)
        );

        let lenient: Article =
            serde_json::from_str(r#"{"id":"77","title":null,"releaseDate":5}"#).unwrap();
        assert_eq!(lenient, article(77, "", 5));

        let e = parse_announcement(&article(
            1,
            "Binance Will List Genius (GENIUS) and OpenGradient (OPG)",
            0,
        ));
        assert_eq!(e.detected_tokens, ["GENIUS", "OPG"]);
        assert_eq!(e.detected_token(), Some("GENIUS"));
        assert!(e.detected_pairs.is_empty());
        assert_eq!(e.detected_pair(), None);
    }

    #[test]
    fn detection_filters_cursor_and_sorts_oldest_first() {
        let arts = [
            article(100, "Binance Will List Old (OLD)", 10),
            article(200, "Binance Will List New (NEW)", 20),
        ];
        assert_eq!(
            detect_new_listings(&arts, 100)
                .iter()
                .map(|e| e.article_id)
                .collect::<Vec<_>>(),
            [200]
        );
        assert!(
            detect_new_listings(&[article(50, "Binance Will List Foo (FOO)", 0)], 50).is_empty()
        );

        let arts = [
            article(300, "Binance Will List Cee (CEE)", 30),
            article(100, "Binance Will List Aye (AYE)", 10),
            article(200, "Binance Will List Bee (BEE)", 20),
        ];
        let tokens: Vec<_> = detect_new_listings(&arts, 0)
            .iter()
            .map(|e| e.detected_tokens[0].clone())
            .collect();
        assert_eq!(tokens, ["AYE", "BEE", "CEE"]);

        let arts = [
            article(100, "Binance Will List Foo (FOO)", 0),
            article(200, "Binance Futures Will Launch BarPerp", 0),
        ];
        let mut types: Vec<_> = detect_new_listings(&arts, 0)
            .iter()
            .map(|e| e.listing_type)
            .collect();
        types.sort_by_key(|t| format!("{t:?}"));
        assert_eq!(types, [ListingType::FuturesOnly, ListingType::SpotList]);
    }

    #[test]
    fn state_round_trips_and_reports_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        let s = DetectorState {
            last_seen_article_id: 12345,
            last_poll_time_ms: 999,
        };
        s.save(&p).unwrap();
        assert_eq!(DetectorState::load(&p).unwrap(), s);
        assert_eq!(
            DetectorState::load(&dir.path().join("nope.json"))
                .unwrap()
                .last_seen_article_id,
            0
        );
        std::fs::write(&p, "not valid json").unwrap();
        let err = DetectorState::load(&p).unwrap_err().to_string();
        assert!(err.contains("could not read state"), "{err}");
    }

    #[test]
    fn rejects_unknown_hold_mode() {
        let err = DetectorHoldMode::parse("bogus").unwrap_err().to_string();
        assert!(err.contains("unknown hold_mode"), "{err}");
    }

    #[test]
    fn live_gates() {
        let dir = tempfile::tempdir().unwrap();
        let ready = DetectorSettings {
            enabled: true,
            confirm_live: "yes".into(),
            proven_edge: true,
            dry_run: false,
            emergency_stop_path: dir.path().join("stop"),
            ..DetectorSettings::default()
        };
        preflight_live(&ready).unwrap();
        let blocked = |s: DetectorSettings, needle: &str| {
            let err = preflight_live(&s).unwrap_err().to_string();
            assert!(err.contains(needle), "{err}");
        };
        blocked(
            DetectorSettings {
                enabled: false,
                ..ready.clone()
            },
            "ENABLED",
        );
        blocked(
            DetectorSettings {
                use_testnet: true,
                ..ready.clone()
            },
            "USE_TESTNET",
        );
        blocked(
            DetectorSettings {
                dry_run: true,
                ..ready.clone()
            },
            "DRY_RUN",
        );
        blocked(
            DetectorSettings {
                confirm_live: "YES ".into(),
                ..ready.clone()
            },
            "CONFIRM_LIVE",
        );
        blocked(
            DetectorSettings {
                proven_edge: false,
                ..ready.clone()
            },
            "PROVEN_EDGE",
        );
        std::fs::write(dir.path().join("stop"), "halt").unwrap();
        blocked(ready, "emergency stop");
    }

    async fn price_server(prices: &[&str]) -> MockServer {
        let server = MockServer::start().await;
        for p in prices {
            Mock::given(method("GET"))
                .and(path("/api/v3/ticker/price"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"symbol": "XUSDT", "price": p})),
                )
                .up_to_n_times(1)
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/api/v3/ticker/price"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"symbol": "XUSDT", "price": "0"})),
            )
            .mount(&server)
            .await;
        server
    }

    fn client(server: &MockServer) -> BinanceClient {
        let cfg = BinanceConfig::new(false)
            .with_spot_rest_url(server.uri())
            .with_retries(0, Duration::ZERO, Duration::ZERO)
            .with_credentials(Credentials::new("k", "s").unwrap());
        BinanceClient::new(cfg).unwrap()
    }

    fn hold_settings(mode: DetectorHoldMode, interval: u64, max_hold: u64) -> DetectorSettings {
        DetectorSettings {
            hold_mode: mode,
            hold_seconds: 0,
            trailing_stop_pct: dec!(15),
            max_hold_seconds: max_hold,
            trailing_poll_interval_seconds: interval,
            ..DetectorSettings::default()
        }
    }

    #[tokio::test]
    async fn flat_hold_returns_elapsed_and_reason() {
        let server = price_server(&[]).await;
        let s = hold_settings(DetectorHoldMode::Flat, 0, 600);
        let (elapsed, reason) = hold_then_sell(&s, &client(&server), "X/USDT", dec!(1)).await;
        assert_eq!((elapsed, reason.as_str()), (0, "flat_hold_elapsed"));
    }

    #[tokio::test]
    async fn trailing_hold_exits_on_retrace_from_peak() {
        let server = price_server(&["1.0", "1.5", "1.2"]).await;
        let s = hold_settings(DetectorHoldMode::Trailing, 0, 1_000_000);
        let (_, reason) = hold_then_sell(&s, &client(&server), "X/USDT", dec!(1)).await;
        assert!(reason.starts_with("trailing_stop_hit"), "{reason}");
        assert_eq!(
            reason,
            "trailing_stop_hit: last=1.200000 <= peak*0.8500=1.275000"
        );
    }

    #[tokio::test]
    async fn trailing_hold_respects_max_hold_cap() {
        let server = price_server(&["1.0", "1.01", "1.02", "1.03", "1.04", "1.05"]).await;
        let s = hold_settings(DetectorHoldMode::Trailing, 1, 3);
        let (elapsed, reason) = hold_then_sell(&s, &client(&server), "X/USDT", dec!(1)).await;
        assert_eq!((elapsed, reason.as_str()), (3, "max_hold_seconds_reached"));
    }

    async fn mount_order_endpoints(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/v3/time"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"serverTime": bb_core::time::now_ms()})),
            )
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v3/order"))
            .and(query_param("side", "BUY"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "symbol": "AIGENSYNUSDT", "orderId": 1, "status": "FILLED",
                "executedQty": "10", "cummulativeQuoteQty": "10"})))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v3/order"))
            .and(query_param("side", "SELL"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "symbol": "AIGENSYNUSDT", "orderId": 2, "status": "FILLED",
                "executedQty": "10", "cummulativeQuoteQty": "15"})))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/exchangeInfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"symbols": [{
                "symbol": "AIGENSYNUSDT", "status": "TRADING", "baseAsset": "AIGENSYN", "quoteAsset": "USDT",
                "filters": [{"filterType": "LOT_SIZE", "minQty": "0.1", "maxQty": "100000", "stepSize": "0.1"}]}]})))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn live_round_trip_buys_by_quote_and_sells_base() {
        let server = price_server(&[]).await;
        mount_order_endpoints(&server).await;
        let s = DetectorSettings {
            notional_usdt: dec!(15),
            ..hold_settings(DetectorHoldMode::Flat, 0, 600)
        };
        let outcome = execute_pair_live(&s, &client(&server), "AIGENSYN/USDT").await;
        let PairOutcome::RoundTripComplete {
            symbol,
            hold_mode,
            avg_sell,
            pnl_quote,
            ..
        } = outcome
        else {
            panic!("unexpected outcome {outcome:?}")
        };
        assert_eq!(
            (symbol.as_str(), hold_mode),
            ("AIGENSYN/USDT", DetectorHoldMode::Flat)
        );
        assert_eq!((avg_sell, pnl_quote), (dec!(1.5), dec!(5)));
        let orders: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path() == "/api/v3/order")
            .filter_map(|r| r.url.query().map(str::to_owned))
            .collect();
        assert!(
            orders[0].contains("quoteOrderQty=15") && orders[0].contains("side=BUY"),
            "{}",
            orders[0]
        );
        assert!(
            orders[1].contains("side=SELL") && orders[1].contains("quantity=10"),
            "{}",
            orders[1]
        );
    }

    #[tokio::test]
    async fn run_once_detects_arms_watchlist_and_advances_cursor() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cms"))
            .and(query_param("catalogId", "48"))
            .and(query_param("pageSize", "20"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "data": {"catalogs": [{"articles": [
                    {"id": 11, "title": "Binance Will List Genius (GENIUS) and OpenGradient (OPG)", "releaseDate": 1_780_000_000_000_i64},
                    {"id": 10, "title": "Binance Futures Will Launch FOOUSDT Perpetual", "releaseDate": 1_779_000_000_000_i64},
                    {"id": 9, "title": "Binance Will List Old (OLD)", "releaseDate": 1}
                ]}]}
            })))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let s = DetectorSettings {
            state_path: dir.path().join("state.json"),
            audit_path: dir.path().join("events.jsonl"),
            emergency_stop_path: dir.path().join("stop"),
            watchlist_path: dir.path().join("wl.json"),
            announcements_url: format!("{}/cms", server.uri()),
            ..DetectorSettings::default()
        };
        DetectorState {
            last_seen_article_id: 9,
            last_poll_time_ms: 0,
        }
        .save(&s.state_path)
        .unwrap();
        let http = reqwest::Client::new();
        let report = run_once(
            &s,
            DetectorMode::DetectOnly,
            &http,
            None,
            &Alerter::disabled(),
        )
        .await
        .unwrap();
        assert_eq!(report.new_events_count, 2);
        assert_eq!(report.previous_cursor_article_id, 9);
        assert_eq!(report.events[0].article_id, 10);
        assert_eq!(
            report.actions,
            [DetectorAction::Watchlist(WatchlistAction::WatchlistArmed {
                tokens: vec!["GENIUS".into(), "OPG".into()]
            })]
        );
        let wl = Watchlist::load(&s.watchlist_path, false, bb_core::time::now()).unwrap();
        assert_eq!(wl.entries.len(), 2);
        assert_eq!(
            DetectorState::load(&s.state_path)
                .unwrap()
                .last_seen_article_id,
            11
        );
        let audit = std::fs::read_to_string(&s.audit_path).unwrap();
        let row: Value = serde_json::from_str(audit.trim()).unwrap();
        assert_eq!(row["events"][1]["listing_type"], "spot_list");
        assert_eq!(row["actions"][0]["action"], "watchlist_armed");

        // Second run: cursor suppresses everything, still audits.
        let report = run_once(
            &s,
            DetectorMode::DetectOnly,
            &http,
            None,
            &Alerter::disabled(),
        )
        .await
        .unwrap();
        assert_eq!(report.new_events_count, 0);
        assert!(report.actions.is_empty());

        std::fs::write(&s.emergency_stop_path, "").unwrap();
        assert!(matches!(
            run_once(
                &s,
                DetectorMode::DetectOnly,
                &http,
                None,
                &Alerter::disabled()
            )
            .await,
            Err(ListingError::EmergencyStop(_))
        ));
    }

    #[test]
    fn pair_action_serialises_flat_with_action_tag() {
        let a = DetectorAction::Pair {
            article_id: 1,
            pair: "X/USDT".into(),
            outcome: PairOutcome::PaperBuySell {
                symbol: "X/USDT".into(),
                notional_usdt: dec!(5.0),
                hold_seconds: 600,
                note: "n".into(),
            },
        };
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            serde_json::json!({"article_id": 1, "pair": "X/USDT", "action": "paper_buy_sell",
                "symbol": "X/USDT", "notional_usdt": 5.0, "hold_seconds": 600, "note": "n"})
        );
    }
}
