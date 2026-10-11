//! V2 early-launch detector: young Solana/WSOL pairs from freshly profiled
//! tokens, filtered for liquidity band, social presence, scam text and wash
//! volume. Qualifying launches go to `data/signals-v2-ledger.jsonl`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use bb_core::env;
use serde::Serialize;
use serde_json::Value;

use crate::common::{Clock, SeenAddresses, append_ledger, emergency_stop_active, secs};
use crate::dexscreener_api::DexScreenerApi;
use crate::error::SignalsError;
use crate::jsonv::{Object, first_max_by, float_or_zero, int_or_zero, nested, object, py_str};

pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
pub const SEEN_ADDRESS_LIMIT: usize = 5_000;
pub const DEFAULT_FILTER_VERSION: &str = "v2-launch-2026-06-27";
const TOP_ASSET_SYMBOLS: [&str; 8] = ["SOL", "BTC", "ETH", "USDC", "USDT", "BONK", "WIF", "TRUMP"];
const UNKNOWN: &str = "unknown";

#[derive(Debug, Clone, PartialEq)]
pub struct LaunchDetectorSettings {
    pub poll_interval_seconds: f64,
    pub max_pair_age_seconds: f64,
    pub min_liquidity_usd: f64,
    pub max_liquidity_usd: f64,
    pub ledger_path: PathBuf,
    pub filter_version: String,
    pub stop_path: PathBuf,
}

impl Default for LaunchDetectorSettings {
    fn default() -> Self {
        Self {
            poll_interval_seconds: 60.0,
            max_pair_age_seconds: 1_800.0,
            min_liquidity_usd: 3_000.0,
            max_liquidity_usd: 200_000.0,
            ledger_path: "data/signals-v2-ledger.jsonl".into(),
            filter_version: DEFAULT_FILTER_VERSION.into(),
            stop_path: "data/signals-v2-stop".into(),
        }
    }
}

impl LaunchDetectorSettings {
    pub fn from_env() -> Result<Self, SignalsError> {
        let d = Self::default();
        Ok(Self {
            poll_interval_seconds: env::float(
                &["SIGNALS_V2_POLL_INTERVAL_SECONDS"],
                d.poll_interval_seconds,
            )?,
            max_pair_age_seconds: env::float(
                &["SIGNALS_V2_MAX_PAIR_AGE_SECONDS"],
                d.max_pair_age_seconds,
            )?,
            min_liquidity_usd: env::float(&["SIGNALS_V2_MIN_LIQUIDITY_USD"], d.min_liquidity_usd)?,
            max_liquidity_usd: env::float(&["SIGNALS_V2_MAX_LIQUIDITY_USD"], d.max_liquidity_usd)?,
            ledger_path: env::string(&["SIGNALS_V2_LEDGER_PATH"], "data/signals-v2-ledger.jsonl")
                .into(),
            filter_version: env::string(&["SIGNALS_V2_FILTER_VERSION"], DEFAULT_FILTER_VERSION),
            stop_path: env::string(&["SIGNALS_V2_STOP_FILE"], "data/signals-v2-stop").into(),
        })
    }
}

/// Launch-quality features persisted under `features` in each ledger row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LaunchFeatures {
    pub chain: String,
    pub token_address: String,
    pub token_name: String,
    pub token_symbol: String,
    pub pair_address: String,
    pub quote_address: String,
    pub quote_symbol: String,
    pub pair_created_at_ms: i64,
    pub pair_age_seconds: f64,
    pub liquidity_usd: f64,
    pub price_usd: f64,
    pub volume_m5: f64,
    pub volume_h1: f64,
    pub volume_h6: f64,
    pub volume_h24: f64,
    pub has_social_link: bool,
    pub creator_recent_rug: String,
    pub liquidity_lock: String,
    pub wash_volume_ratio_m5_liquidity: Option<f64>,
}

/// One V2 ledger row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DetectionRecord {
    pub detection_id: String,
    pub filter_version: String,
    pub detected_at_ms: i64,
    pub chain: String,
    pub token_address: String,
    pub pair_address: String,
    pub quote_address: String,
    pub features: LaunchFeatures,
    pub rejection_reasons: Vec<String>,
    pub creator_recent_rug: String,
    pub raw_pair: Object,
}

/// Extract features from a pair payload; `profile` is the token profile the
/// pair was discovered through (a second source of social links).
pub fn extract_launch_features(
    pair: &Object,
    profile: Option<&Object>,
    now_ms: i64,
) -> LaunchFeatures {
    let base = object(pair, "baseToken");
    let quote = object(pair, "quoteToken");
    let field = |o: Option<&Object>, key: &str| py_str(o.and_then(|o| o.get(key)));

    let pair_created_at_ms = int_or_zero(pair.get("pairCreatedAt"));
    let pair_age_seconds = if pair_created_at_ms == 0 {
        -1.0
    } else {
        (now_ms - pair_created_at_ms) as f64 / 1000.0
    };
    let liquidity_usd = float_or_zero(nested(pair, "liquidity", "usd"));
    let volume_m5 = float_or_zero(nested(pair, "volume", "m5"));

    LaunchFeatures {
        chain: py_str(pair.get("chainId")),
        token_address: field(base, "address"),
        token_name: field(base, "name"),
        token_symbol: field(base, "symbol"),
        pair_address: py_str(pair.get("pairAddress")),
        quote_address: field(quote, "address"),
        quote_symbol: field(quote, "symbol"),
        pair_created_at_ms,
        pair_age_seconds,
        liquidity_usd,
        price_usd: float_or_zero(pair.get("priceUsd")),
        volume_m5,
        volume_h1: float_or_zero(nested(pair, "volume", "h1")),
        volume_h6: float_or_zero(nested(pair, "volume", "h6")),
        volume_h24: float_or_zero(nested(pair, "volume", "h24")),
        has_social_link: has_social_link(pair, profile),
        creator_recent_rug: UNKNOWN.into(),
        liquidity_lock: UNKNOWN.into(),
        wash_volume_ratio_m5_liquidity: (liquidity_usd > 0.0).then(|| volume_m5 / liquidity_usd),
    }
}

/// Whether the pair passes V2 filters, with every rejection reason.
pub fn qualifies_launch_pair(
    features: &LaunchFeatures,
    settings: &LaunchDetectorSettings,
) -> (bool, Vec<&'static str>) {
    let mut reasons = Vec::new();
    if features.chain.to_lowercase() != "solana" {
        reasons.push("non_solana_chain");
    }
    if !is_wsol_quote(&features.quote_address, &features.quote_symbol) {
        reasons.push("non_wsol_quote");
    }
    let age = features.pair_age_seconds;
    if age < 0.0 || age > settings.max_pair_age_seconds {
        reasons.push("pair_age_out_of_range");
    }
    let liquidity = features.liquidity_usd;
    if liquidity < settings.min_liquidity_usd {
        reasons.push("liquidity_too_low");
    }
    if liquidity > settings.max_liquidity_usd {
        reasons.push("liquidity_too_high");
    }
    if !features.has_social_link {
        reasons.push("missing_social_link");
    }
    if scam_text_reject(&features.token_symbol, &features.token_name) {
        reasons.push("scam_text_pattern");
    }
    if (0.0..=600.0).contains(&age) && liquidity > 0.0 && features.volume_m5 > 20.0 * liquidity {
        reasons.push("wash_volume_spike");
    }
    (reasons.is_empty(), reasons)
}

pub fn build_detection_record(
    pair: &Object,
    features: LaunchFeatures,
    detected_at_ms: i64,
    filter_version: &str,
) -> DetectionRecord {
    DetectionRecord {
        detection_id: format!("v2-solana-{}-{detected_at_ms}", features.pair_address),
        filter_version: filter_version.to_owned(),
        detected_at_ms,
        chain: "solana".into(),
        token_address: features.token_address.clone(),
        pair_address: features.pair_address.clone(),
        quote_address: features.quote_address.clone(),
        features,
        rejection_reasons: Vec::new(),
        creator_recent_rug: UNKNOWN.into(),
        raw_pair: pair
            .iter()
            .filter(|(k, _)| !k.starts_with("__"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}

pub struct LaunchDetector {
    settings: LaunchDetectorSettings,
    api: DexScreenerApi,
    clock: Arc<dyn Clock>,
    seen: SeenAddresses,
}

impl LaunchDetector {
    pub fn new(
        settings: LaunchDetectorSettings,
        api: DexScreenerApi,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            settings,
            api,
            clock,
            seen: SeenAddresses::new(SEEN_ADDRESS_LIMIT),
        }
    }

    /// One polling cycle; returns the newly recorded launches.
    pub async fn run_cycle(&mut self) -> Result<Vec<DetectionRecord>, SignalsError> {
        let now_ms = self.clock.now_ms();
        let profiles = match self.api.latest_token_profiles().await {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(%err, "V2 DexScreener token profiles fetch failed");
                Vec::new()
            }
        };
        let by_address = solana_profiles_by_address(profiles);
        let fresh: HashSet<String> = self
            .seen
            .dedupe(by_address.iter().map(|(a, _)| a.as_str()))
            .into_iter()
            .collect();
        let mut detections = Vec::new();

        for (address, profile) in by_address.iter().filter(|(a, _)| fresh.contains(a)) {
            let pairs = match self.api.token_pairs(address).await {
                Ok(p) => p,
                Err(err) => {
                    tracing::warn!(%err, token = %address, "V2 DexScreener token pairs fetch failed");
                    Vec::new()
                }
            };
            let Some(pair) = best_wsol_solana_pair(&pairs) else {
                continue;
            };
            let features = extract_launch_features(pair, Some(profile), now_ms);
            let (qualifies, reasons) = qualifies_launch_pair(&features, &self.settings);
            if !qualifies {
                tracing::debug!(token = %address, reasons = %reasons.join(","), "V2 launch rejected");
                continue;
            }
            let record =
                build_detection_record(pair, features, now_ms, &self.settings.filter_version);
            append_ledger(&self.settings.ledger_path, &record)?;
            detections.push(record);
        }

        if !detections.is_empty() {
            tracing::info!(
                count = detections.len(),
                "V2 DexScreener emitted launch detections"
            );
        }
        Ok(detections)
    }

    pub async fn run(&mut self, once: bool) {
        loop {
            if emergency_stop_active(&self.settings.stop_path) {
                tracing::info!(path = %self.settings.stop_path.display(), "V2 DexScreener stop file present");
                return;
            }
            if let Err(err) = self.run_cycle().await {
                tracing::warn!(%err, "V2 DexScreener cycle failed");
            }
            if once {
                return;
            }
            tokio::time::sleep(secs(self.settings.poll_interval_seconds)).await;
        }
    }
}

/// Solana (or chain-less) profiles keyed by token address, first occurrence wins.
fn solana_profiles_by_address(profiles: Vec<Object>) -> Vec<(String, Object)> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for profile in profiles {
        let chain = py_str(profile.get("chainId")).to_lowercase();
        if !chain.is_empty() && chain != "solana" {
            continue;
        }
        let Some(address) = profile
            .get("tokenAddress")
            .and_then(Value::as_str)
            .filter(|a| !a.is_empty())
            .map(str::to_owned)
        else {
            continue;
        };
        if seen.insert(address.clone()) {
            out.push((address, profile));
        }
    }
    out
}

/// Highest-liquidity Solana pair quoted in WSOL.
pub fn best_wsol_solana_pair(pairs: &[Object]) -> Option<&Object> {
    let qualifying = pairs.iter().filter(|pair| {
        let quote = object(pair, "quoteToken");
        py_str(pair.get("chainId")).to_lowercase() == "solana"
            && is_wsol_quote(
                &py_str(quote.and_then(|q| q.get("address"))),
                &py_str(quote.and_then(|q| q.get("symbol"))),
            )
    });
    first_max_by(qualifying, |pair| {
        float_or_zero(nested(pair, "liquidity", "usd"))
    })
}

fn is_wsol_quote(address: &str, symbol: &str) -> bool {
    if address == WSOL_MINT {
        return true;
    }
    address.is_empty() && matches!(symbol.to_uppercase().as_str(), "SOL" | "WSOL")
}

fn has_social_link(pair: &Object, profile: Option<&Object>) -> bool {
    if let Some(info) = object(pair, "info")
        && info.values().any(payload_has_url)
    {
        return true;
    }
    profile.is_some_and(|p| p.values().any(payload_has_url))
}

fn payload_has_url(value: &Value) -> bool {
    match value {
        Value::String(s) => {
            let lowered = s.to_lowercase();
            ["http://", "https://", "t.me/"]
                .iter()
                .any(|prefix| lowered.starts_with(prefix))
        }
        Value::Object(map) => map.values().any(payload_has_url),
        Value::Array(items) => items.iter().any(payload_has_url),
        _ => false,
    }
}

fn scam_text_reject(symbol: &str, name: &str) -> bool {
    let symbol = symbol.trim();
    if symbol.is_empty() || name.trim().is_empty() {
        return true;
    }
    let upper = symbol.to_uppercase();
    let len = upper.chars().count();
    if len > 12 {
        return true;
    }
    let distinct: HashSet<char> = upper.chars().collect();
    if len >= 6 && distinct.len() == 1 {
        return true;
    }
    if ["HTTP", "WWW", ".COM", "T.ME"]
        .iter()
        .any(|fragment| upper.contains(fragment))
    {
        return true;
    }
    TOP_ASSET_SYMBOLS.iter().any(|asset| {
        upper == *asset
            || upper == format!("{asset}$")
            || upper == format!("{asset}2.0")
            || upper == format!("${asset}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::FixedClock;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const NOW_MS: i64 = 1_800_000_000_000;

    fn pair_payload(overrides: Value) -> Object {
        let mut pair = json!({
            "chainId": "solana",
            "pairAddress": "PAIR123",
            "pairCreatedAt": NOW_MS - 5 * 60 * 1000,
            "baseToken": {"address": "TOKEN123", "symbol": "MOONX", "name": "Moon X"},
            "quoteToken": {"address": WSOL_MINT, "symbol": "SOL"},
            "priceUsd": "0.0001",
            "liquidity": {"usd": 20_000},
            "volume": {"m5": 1_000, "h1": 2_000, "h6": 3_000, "h24": 4_000},
            "info": {"websites": [{"url": "https://example.com"}]},
            "url": "https://dexscreener.com/solana/PAIR123",
        });
        let target = pair.as_object_mut().unwrap();
        for (key, value) in overrides.as_object().unwrap() {
            match (target.get_mut(key), value) {
                (Some(Value::Object(existing)), Value::Object(patch))
                    if ["baseToken", "quoteToken", "liquidity", "volume"]
                        .contains(&key.as_str()) =>
                {
                    for (k, v) in patch {
                        existing.insert(k.clone(), v.clone());
                    }
                }
                _ => {
                    target.insert(key.clone(), value.clone());
                }
            }
        }
        pair.as_object().cloned().unwrap()
    }

    fn settings() -> LaunchDetectorSettings {
        LaunchDetectorSettings {
            filter_version: "test-filter".into(),
            ..LaunchDetectorSettings::default()
        }
    }

    fn features_for(pair: &Object) -> LaunchFeatures {
        extract_launch_features(pair, None, NOW_MS)
    }

    #[test]
    fn filter_accepts_clean_launch_payload() {
        let (ok, reasons) =
            qualifies_launch_pair(&features_for(&pair_payload(json!({}))), &settings());
        assert!(ok);
        assert!(reasons.is_empty());
    }

    #[test]
    fn filter_rejects_bad_launch_payloads() {
        let cases = [
            (
                json!({"pairCreatedAt": NOW_MS - 31 * 60 * 1000}),
                "pair_age_out_of_range",
            ),
            (json!({"liquidity": {"usd": 2_999}}), "liquidity_too_low"),
            (json!({"liquidity": {"usd": 200_001}}), "liquidity_too_high"),
            (json!({"info": {}}), "missing_social_link"),
            (
                json!({"quoteToken": {"address": "USDC", "symbol": "USDC"}}),
                "non_wsol_quote",
            ),
            (json!({"chainId": "ethereum"}), "non_solana_chain"),
            (
                json!({"baseToken": {"symbol": "", "name": "Moon X"}}),
                "scam_text_pattern",
            ),
            (
                json!({"baseToken": {"symbol": "TOOLONGSYMBOL", "name": "Moon X"}}),
                "scam_text_pattern",
            ),
            (
                json!({"baseToken": {"symbol": "BTC$", "name": "Moon X"}}),
                "scam_text_pattern",
            ),
            (json!({"volume": {"m5": 500_000}}), "wash_volume_spike"),
        ];
        for (overrides, reason) in cases {
            let (ok, reasons) =
                qualifies_launch_pair(&features_for(&pair_payload(overrides.clone())), &settings());
            assert!(!ok, "{overrides}");
            assert!(reasons.contains(&reason), "{overrides}: {reasons:?}");
        }
    }

    #[test]
    fn social_link_from_profile_counts() {
        let pair = pair_payload(json!({"info": {}}));
        let profile = json!({"links": [{"url": "t.me/moonx"}]})
            .as_object()
            .cloned()
            .unwrap();
        assert!(extract_launch_features(&pair, Some(&profile), NOW_MS).has_social_link);
    }

    #[tokio::test]
    async fn dedup_same_token_seen_twice_in_cycle_emits_one_detection() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/token-profiles/latest/v1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"chainId": "solana", "tokenAddress": "TOKEN123"},
                {"chainId": "solana", "tokenAddress": "TOKEN123"},
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/latest/dex/tokens/TOKEN123"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"pairs": [Value::Object(pair_payload(json!({})))]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let s = LaunchDetectorSettings {
            ledger_path: dir.path().join("ledger.jsonl"),
            ..settings()
        };
        let api = DexScreenerApi::with_base_url(reqwest::Client::new(), server.uri());
        let mut detector = LaunchDetector::new(s.clone(), api, Arc::new(FixedClock(NOW_MS)));

        let detections = detector.run_cycle().await.unwrap();

        assert_eq!(detections.len(), 1);
        let rows = std::fs::read_to_string(&s.ledger_path).unwrap();
        assert_eq!(rows.lines().count(), 1);
    }

    #[test]
    fn detection_schema_uses_expected_id_and_filter_version() {
        let pair = pair_payload(json!({}));
        let record = build_detection_record(&pair, features_for(&pair), NOW_MS, "custom-filter");
        assert_eq!(record.detection_id, format!("v2-solana-PAIR123-{NOW_MS}"));
        assert_eq!(record.filter_version, "custom-filter");
        assert_eq!(record.creator_recent_rug, "unknown");
        let encoded = serde_json::to_value(&record).unwrap();
        assert_eq!(encoded["features"]["price_usd"].as_f64(), Some(0.0001));
        assert_eq!(encoded["rejection_reasons"], json!([]));
        assert_eq!(encoded["raw_pair"]["pairAddress"], "PAIR123");
    }
}
