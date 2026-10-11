//! V1 DexScreener signal detector: newly profiled tokens whose best young pair
//! clears liquidity and 24h volume floors.

use std::path::PathBuf;
use std::sync::Arc;

use bb_core::env;
use serde_json::Value;

use crate::alerts::Alerter;
use crate::candidate::SignalCandidate;
use crate::common::{
    AlertRateLimiter, Clock, SeenAddresses, append_ledger, emergency_stop_active, secs,
};
use crate::dexscreener_api::DexScreenerApi;
use crate::error::SignalsError;
use crate::jsonv::{Object, first_max_by, float_or_zero, int_or_zero, nested, object, py_str};
use crate::watchlist::Watchlist;

/// Market-statistic thresholds are `f64`: they filter API-reported liquidity and
/// volume and feed a log-scaled score; no funds are sized from them.
#[derive(Debug, Clone, PartialEq)]
pub struct DexScreenerSettings {
    pub poll_interval_seconds: f64,
    pub max_age_hours: f64,
    pub min_liquidity_usd: f64,
    pub min_volume_usd_24h: f64,
    pub ledger_path: PathBuf,
    pub watchlist_path: PathBuf,
    pub watchlist_ttl_hours: f64,
    pub max_alerts_per_hour: usize,
    pub stop_path: PathBuf,
}

impl Default for DexScreenerSettings {
    fn default() -> Self {
        Self {
            poll_interval_seconds: 30.0,
            max_age_hours: 24.0,
            min_liquidity_usd: 5_000.0,
            min_volume_usd_24h: 10_000.0,
            ledger_path: "data/signals-ledger.jsonl".into(),
            watchlist_path: "data/signals-watchlist.json".into(),
            watchlist_ttl_hours: 12.0,
            max_alerts_per_hour: 20,
            stop_path: "data/signals-dexscreener.stop".into(),
        }
    }
}

impl DexScreenerSettings {
    pub fn from_env() -> Result<Self, SignalsError> {
        let d = Self::default();
        Ok(Self {
            poll_interval_seconds: env::float(
                &["SIGNALS_DEXSCREENER_POLL_INTERVAL_SECONDS"],
                d.poll_interval_seconds,
            )?,
            max_age_hours: env::float(&["SIGNALS_DEXSCREENER_MAX_AGE_HOURS"], d.max_age_hours)?,
            min_liquidity_usd: env::float(
                &["SIGNALS_DEXSCREENER_MIN_LIQUIDITY_USD"],
                d.min_liquidity_usd,
            )?,
            min_volume_usd_24h: env::float(
                &["SIGNALS_DEXSCREENER_MIN_VOLUME_USD_24H"],
                d.min_volume_usd_24h,
            )?,
            ledger_path: env::string(&["SIGNALS_LEDGER_PATH"], "data/signals-ledger.jsonl").into(),
            watchlist_path: env::string(&["SIGNALS_WATCHLIST_PATH"], "data/signals-watchlist.json")
                .into(),
            watchlist_ttl_hours: env::float(
                &["SIGNALS_WATCHLIST_TTL_HOURS"],
                d.watchlist_ttl_hours,
            )?,
            max_alerts_per_hour: env::integer(
                &["SIGNALS_DEXSCREENER_MAX_ALERTS_PER_HOUR"],
                d.max_alerts_per_hour,
            )?,
            stop_path: env::string(
                &["SIGNALS_DEXSCREENER_STOP_PATH"],
                "data/signals-dexscreener.stop",
            )
            .into(),
        })
    }
}

pub struct DexScreenerDetector {
    settings: DexScreenerSettings,
    api: DexScreenerApi,
    clock: Arc<dyn Clock>,
    alerter: Arc<dyn Alerter>,
    seen: SeenAddresses,
    watchlist: Watchlist,
    limiter: AlertRateLimiter,
}

impl DexScreenerDetector {
    /// Loads the persisted watchlist; the seen-address set starts empty.
    pub fn new(
        settings: DexScreenerSettings,
        api: DexScreenerApi,
        clock: Arc<dyn Clock>,
        alerter: Arc<dyn Alerter>,
    ) -> Self {
        let watchlist = Watchlist::load(&settings.watchlist_path);
        let limiter = AlertRateLimiter::new(settings.max_alerts_per_hour);
        Self {
            settings,
            api,
            clock,
            alerter,
            seen: SeenAddresses::new(SeenAddresses::DEFAULT_CAPACITY),
            watchlist,
            limiter,
        }
    }

    pub fn watchlist(&self) -> &Watchlist {
        &self.watchlist
    }

    /// One polling cycle: new profiled tokens → qualifying candidates, appended
    /// to the ledger and watchlist and alerted (rate-limited).
    pub async fn run_cycle(&mut self) -> Result<Vec<SignalCandidate>, SignalsError> {
        let now_ms = self.clock.now_ms();
        self.watchlist.prune(now_ms);

        let profiles = match self.api.latest_token_profiles().await {
            Ok(profiles) => profiles,
            Err(err) => {
                tracing::warn!(%err, "DexScreener token profiles fetch failed");
                Vec::new()
            }
        };
        let fresh = self.seen.dedupe(profile_addresses(&profiles));
        let mut candidates = Vec::new();

        for address in fresh {
            let pairs = match self.api.token_pairs(&address).await {
                Ok(pairs) => pairs,
                Err(err) => {
                    tracing::warn!(%err, token = %address, "DexScreener token pairs fetch failed");
                    continue;
                }
            };
            let Some(candidate) =
                candidate_from_pairs(&pairs, &self.settings, now_ms, self.clock.now_ms())
            else {
                continue;
            };
            append_ledger(&self.settings.ledger_path, &candidate)?;
            self.watchlist
                .add(&candidate, self.settings.watchlist_ttl_hours);
            self.alert(&candidate).await;
            candidates.push(candidate);
        }

        self.watchlist.prune(self.clock.now_ms());
        self.watchlist.save()?;
        Ok(candidates)
    }

    /// Poll until the stop file appears (or once). Cycle failures are logged.
    pub async fn run(&mut self, once: bool) {
        loop {
            if emergency_stop_active(&self.settings.stop_path) {
                tracing::info!(path = %self.settings.stop_path.display(), "DexScreener stop file present");
                return;
            }
            if let Err(err) = self.run_cycle().await {
                tracing::warn!(%err, "DexScreener cycle failed");
            }
            if once {
                return;
            }
            tokio::time::sleep(secs(self.settings.poll_interval_seconds)).await;
        }
    }

    async fn alert(&mut self, candidate: &SignalCandidate) {
        if !self.limiter.allow(self.clock.now_ms()) {
            tracing::debug!(token = %candidate.token_address, "DexScreener alert suppressed by detector rate limit");
            return;
        }
        let token = if candidate.token_symbol.is_empty() {
            &candidate.token_address
        } else {
            &candidate.token_symbol
        };
        let msg = format!(
            "DexScreener signal\nToken: {token}\nChain: {}\nAddress: {}\nConfidence: {:.3}",
            candidate.chain, candidate.token_address, candidate.confidence
        );
        self.alerter.alert(&msg).await;
    }
}

/// `tokenAddress` of every profile, in response order.
pub fn profile_addresses(profiles: &[Object]) -> Vec<String> {
    profiles
        .iter()
        .filter_map(|p| p.get("tokenAddress").and_then(Value::as_str))
        .filter(|a| !a.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Highest-liquidity pair that is young enough and clears both floors.
pub fn candidate_from_pairs(
    pairs: &[Object],
    settings: &DexScreenerSettings,
    now_ms: i64,
    detected_at_ms: i64,
) -> Option<SignalCandidate> {
    let max_age_ms = settings.max_age_hours * 3_600_000.0;
    let qualifying = pairs.iter().filter_map(|pair| {
        let created_at = int_or_zero(pair.get("pairCreatedAt"));
        let liquidity = float_or_zero(nested(pair, "liquidity", "usd"));
        let volume = float_or_zero(nested(pair, "volume", "h24"));
        let too_old = (created_at as f64) < now_ms as f64 - max_age_ms;
        (!too_old
            && liquidity >= settings.min_liquidity_usd
            && volume >= settings.min_volume_usd_24h)
            .then_some((liquidity, pair))
    });
    let (liquidity, pair) = first_max_by(qualifying, |item| item.0)?;

    let volume = float_or_zero(nested(pair, "volume", "h24"));
    let base_token = object(pair, "baseToken");
    let mut raw_signal = pair.clone();
    raw_signal.insert(
        "dexscreener_pair_url".into(),
        Value::String(py_str(pair.get("url"))),
    );
    Some(SignalCandidate {
        source: "dexscreener".into(),
        chain: py_str(pair.get("chainId")),
        token_address: py_str(base_token.and_then(|b| b.get("address"))),
        token_symbol: py_str(base_token.and_then(|b| b.get("symbol"))),
        detected_at_ms,
        raw_signal,
        confidence: confidence(liquidity, volume),
    })
}

/// Liquidity strength above $5k (log-scaled to $500k), scaled by 24h volume up to $100k.
pub fn confidence(liquidity_usd: f64, volume_h24: f64) -> f64 {
    let liquidity_score = ((liquidity_usd.max(1.0) / 5_000.0).log10() / 2.0).min(1.0);
    (liquidity_score * (volume_h24 / 100_000.0).min(1.0)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::testing::RecordingAlerter;
    use crate::common::FixedClock;
    use serde_json::json;
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn settings(dir: &std::path::Path) -> DexScreenerSettings {
        DexScreenerSettings {
            poll_interval_seconds: 0.0,
            ledger_path: dir.join("ledger.jsonl"),
            watchlist_path: dir.join("watchlist.json"),
            stop_path: dir.join("stop"),
            ..DexScreenerSettings::default()
        }
    }

    fn pair(token: &str, created_at: i64, liquidity: f64, volume: f64) -> Value {
        json!({
            "chainId": "solana",
            "dexId": "raydium",
            "url": "https://dexscreener.com/solana/pair",
            "pairCreatedAt": created_at,
            "liquidity": {"usd": liquidity},
            "volume": {"h24": volume},
            "priceChange": {"h24": 5.0},
            "baseToken": {"address": token, "symbol": "TOK", "name": "Token"},
            "quoteToken": {"address": "So111", "symbol": "SOL"},
        })
    }

    fn good_pair(token: &str) -> Value {
        pair(token, 1_999_999, 25_000.0, 50_000.0)
    }

    async fn server(profiles: Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/token-profiles/latest/v1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(profiles))
            .mount(&server)
            .await;
        server
    }

    async fn mount_pairs(server: &MockServer, pairs: Value, expect: u64) {
        Mock::given(method("GET"))
            .and(path_regex("^/latest/dex/tokens/.+$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "pairs": pairs })))
            .expect(expect)
            .mount(server)
            .await;
    }

    fn detector(
        server: &MockServer,
        settings: DexScreenerSettings,
        alerter: Arc<RecordingAlerter>,
    ) -> DexScreenerDetector {
        let api = DexScreenerApi::with_base_url(reqwest::Client::new(), server.uri());
        DexScreenerDetector::new(settings, api, Arc::new(FixedClock(2_000_000)), alerter)
    }

    fn read_lines(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn read_json(path: &std::path::Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn full_cycle_emits_ledger_and_watchlist() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let server = server(json!([{"tokenAddress": "Token111", "chainId": "solana"}])).await;
        Mock::given(method("GET"))
            .and(path("/latest/dex/tokens/Token111"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"pairs": [good_pair("Token111")]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let alerter = Arc::new(RecordingAlerter::default());
        let mut d = detector(&server, s.clone(), alerter.clone());

        let candidates = d.run_cycle().await.unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].source, "dexscreener");
        assert_eq!(candidates[0].token_address, "Token111");
        let rows = read_lines(&s.ledger_path);
        assert!(
            rows[0]["raw_signal"]["dexscreener_pair_url"]
                .as_str()
                .unwrap()
                .starts_with("https://dexscreener.com")
        );
        let wl = read_json(&s.watchlist_path);
        assert_eq!(wl["watching"][0]["token"], "Token111");
        assert_eq!(wl["watching"][0]["source"], "dexscreener");
        assert_eq!(alerter.sent().len(), 1);
    }

    #[tokio::test]
    async fn skip_cycles_do_not_emit() {
        for p in [
            pair("Token111", -84_400_001, 25_000.0, 50_000.0),
            pair("Token111", 1_999_999, 4_999.0, 50_000.0),
            pair("Token111", 1_999_999, 25_000.0, 9_999.0),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let s = settings(dir.path());
            let server = server(json!([{"tokenAddress": "Token111"}])).await;
            mount_pairs(&server, json!([p]), 1).await;
            let mut d = detector(&server, s.clone(), Arc::default());
            assert!(d.run_cycle().await.unwrap().is_empty());
            assert!(!s.ledger_path.exists());
            assert_eq!(read_json(&s.watchlist_path), json!({"watching": []}));
        }
    }

    #[tokio::test]
    async fn dedupe_across_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let server = server(json!([{"tokenAddress": "Token111"}])).await;
        mount_pairs(&server, json!([good_pair("Token111")]), 1).await;
        let mut d = detector(&server, settings(dir.path()), Arc::default());
        assert_eq!(d.run_cycle().await.unwrap().len(), 1);
        assert!(d.run_cycle().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn once_with_no_profiles_writes_empty_watchlist() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        let server = server(json!([])).await;
        let mut d = detector(&server, s.clone(), Arc::default());
        d.run(true).await;
        assert_eq!(read_json(&s.watchlist_path), json!({"watching": []}));
    }

    #[tokio::test]
    async fn alert_rate_limit_suppresses_21st_alert() {
        let dir = tempfile::tempdir().unwrap();
        let profiles: Vec<Value> = (0..21)
            .map(|i| json!({"tokenAddress": format!("Token{i}")}))
            .collect();
        let server = server(Value::Array(profiles)).await;
        for i in 0..21 {
            let token = format!("Token{i}");
            Mock::given(method("GET"))
                .and(path(format!("/latest/dex/tokens/{token}")))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"pairs": [good_pair(&token)]})),
                )
                .mount(&server)
                .await;
        }
        let alerter = Arc::new(RecordingAlerter::default());
        let mut d = detector(&server, settings(dir.path()), alerter.clone());
        assert_eq!(d.run_cycle().await.unwrap().len(), 21);
        assert_eq!(alerter.sent().len(), 20);
    }

    #[tokio::test]
    async fn stop_file_presence_exits_without_fetching() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings(dir.path());
        std::fs::write(&s.stop_path, b"").unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        detector(&server, s, Arc::default()).run(true).await;
    }

    #[tokio::test]
    async fn malformed_api_responses_do_not_crash() {
        let dir = tempfile::tempdir().unwrap();
        let bad = server(json!({"not": "a list"})).await;
        let mut d = detector(&bad, settings(dir.path()), Arc::default());
        assert!(d.run_cycle().await.unwrap().is_empty());

        let server = server(json!([{"tokenAddress": "Token111"}])).await;
        Mock::given(method("GET"))
            .and(path_regex("^/latest/dex/tokens/.+$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"pairs": {}})))
            .mount(&server)
            .await;
        let mut d = detector(&server, settings(dir.path()), Arc::default());
        assert!(d.run_cycle().await.unwrap().is_empty());
    }

    #[test]
    fn confidence_scales_with_liquidity_and_volume() {
        assert_eq!(confidence(5_000.0, 100_000.0), 0.0);
        assert!((confidence(500_000.0, 100_000.0) - 1.0).abs() < 1e-12);
        assert!((confidence(50_000.0, 50_000.0) - 0.25).abs() < 1e-12);
    }
}
