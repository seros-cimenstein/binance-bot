//! Row types for every Phase 43 file, mirroring `docs/signals/x-insighter-design.md`.
//!
//! The Python collector writes [`XPost`], [`XEngagement`], [`XReply`] and
//! [`XHeartbeat`]; the engine writes everything else. Readers are lenient
//! (`#[serde(default)]`) so a malformed or older row never stops a cycle.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const POST_SCHEMA: &str = "x-post-v1";
pub const ENGAGEMENT_SCHEMA: &str = "x-engagement-v1";
pub const REPLY_SCHEMA: &str = "x-reply-v1";
pub const HEARTBEAT_SCHEMA: &str = "x-heartbeat-v1";
pub const FOCUS_SCHEMA: &str = "x-focus-v1";
pub const TRACTION_SCHEMA: &str = "x-traction-v1";
pub const HAIKU_SCHEMA: &str = "x-haiku-v1";
pub const SIGNAL_SCHEMA: &str = "x-signal-v1";
pub const TICK_SCHEMA: &str = "x-tick-v1";

pub const PROMPT_VERSION: &str = "x-insighter-v1";
pub const TRACTION_VERSION: &str = "x-traction-v1";
pub const EXHAUSTION_VERSION: &str = "x-exhaustion-v1";

/// Data-dir file layout. Every path can be overridden with `X_INSIGHTER_<NAME>_PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XPaths {
    pub posts: PathBuf,
    pub engagement: PathBuf,
    pub replies: PathBuf,
    pub heartbeat: PathBuf,
    pub focus: PathBuf,
    pub traction: PathBuf,
    pub haiku: PathBuf,
    pub signals: PathBuf,
    pub ticks: PathBuf,
    pub snapshots: PathBuf,
    pub state: PathBuf,
    pub report_dir: PathBuf,
}

impl XPaths {
    /// Default layout under `data_dir`.
    pub fn in_dir(data_dir: &Path) -> Self {
        Self {
            posts: data_dir.join("x-posts.jsonl"),
            engagement: data_dir.join("x-engagement.jsonl"),
            replies: data_dir.join("x-replies.jsonl"),
            heartbeat: data_dir.join("x-collector-heartbeat.json"),
            focus: data_dir.join("x-focus.json"),
            traction: data_dir.join("x-traction.jsonl"),
            haiku: data_dir.join("x-haiku.jsonl"),
            signals: data_dir.join("x-signals.jsonl"),
            ticks: data_dir.join("x-ticks.jsonl"),
            snapshots: data_dir.join("x-signal-snapshots.jsonl"),
            state: data_dir.join("x-insighter-state.json"),
            report_dir: data_dir.to_path_buf(),
        }
    }

    /// Defaults under `data/` (relative, like the other signal files), with
    /// `X_INSIGHTER_<NAME>_PATH` overrides.
    pub fn from_env() -> Self {
        let mut paths = Self::in_dir(Path::new("data"));
        let fields: [(&str, &mut PathBuf); 12] = [
            ("POSTS", &mut paths.posts),
            ("ENGAGEMENT", &mut paths.engagement),
            ("REPLIES", &mut paths.replies),
            ("HEARTBEAT", &mut paths.heartbeat),
            ("FOCUS", &mut paths.focus),
            ("TRACTION", &mut paths.traction),
            ("HAIKU", &mut paths.haiku),
            ("SIGNALS", &mut paths.signals),
            ("TICKS", &mut paths.ticks),
            ("SNAPSHOTS", &mut paths.snapshots),
            ("STATE", &mut paths.state),
            ("REPORT_DIR", &mut paths.report_dir),
        ];
        for (name, slot) in fields {
            if let Ok(value) = std::env::var(format!("X_INSIGHTER_{name}_PATH"))
                && !value.trim().is_empty()
            {
                *slot = PathBuf::from(value.trim());
            }
        }
        paths
    }
}

/// Visible engagement meters of a post. `views` is often hidden.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Metrics {
    pub replies: i64,
    pub reposts: i64,
    pub quotes: i64,
    pub likes: i64,
    pub bookmarks: i64,
    pub views: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostSourceKind {
    #[default]
    Search,
    Account,
    FocusSearch,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PostSource {
    pub kind: PostSourceKind,
    pub query: Option<String>,
}

/// `x-posts.jsonl`: one row per post, first sighting only.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct XPost {
    pub schema: String,
    pub post_id: String,
    pub author_hash: String,
    pub author_handle: Option<String>,
    pub curated: bool,
    pub author_followers: i64,
    pub author_created_at_ms: Option<i64>,
    pub author_verified: bool,
    pub created_at_ms: i64,
    pub first_seen_ms: i64,
    pub excerpt: String,
    pub lang: Option<String>,
    pub cashtags: Vec<String>,
    pub solana_addresses: Vec<String>,
    pub evm_addresses: Vec<String>,
    pub urls: Vec<String>,
    pub is_reply: bool,
    pub is_quote: bool,
    pub in_reply_to_post_id: Option<String>,
    pub quoted_post_id: Option<String>,
    pub metrics: Metrics,
    pub source: PostSource,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleStatus {
    #[default]
    Ok,
    Gone,
    Error,
}

/// `x-engagement.jsonl`: counter re-read of a focused post.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct XEngagement {
    pub schema: String,
    pub post_id: String,
    pub sampled_at_ms: i64,
    pub status: SampleStatus,
    pub metrics: Option<Metrics>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyKind {
    #[default]
    Reply,
    Quote,
}

/// `x-replies.jsonl`: sampled reply or quote under a focused post.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct XReply {
    pub schema: String,
    pub parent_post_id: String,
    pub post_id: String,
    pub kind: ReplyKind,
    pub author_hash: String,
    pub author_followers: i64,
    pub author_created_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub first_seen_ms: i64,
    pub excerpt: String,
    pub metrics: Metrics,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectorStatus {
    #[default]
    Ok,
    Backoff,
    LoginRequired,
    Challenge,
    Stopped,
}

/// `x-collector-heartbeat.json` (atomic rewrite by the collector).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct XHeartbeat {
    pub schema: String,
    pub updated_at_ms: i64,
    pub status: CollectorStatus,
    pub pages_last_hour: i64,
    pub posts_last_hour: i64,
    pub replies_last_hour: i64,
    pub last_error: Option<String>,
    pub backoff_until_ms: Option<i64>,
    pub camoufox_version: Option<String>,
    pub browser_version: Option<String>,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    #[default]
    Binance,
    Solana,
}

impl Venue {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Binance => "binance",
            Self::Solana => "solana",
        }
    }
}

/// Which resolution rule mapped a mention to a market.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    #[default]
    MintInPost,
    BinanceListing,
    CashtagLink,
    DexscreenerSearch,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    #[default]
    Watch,
    Focus,
    Hot,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FocusCoin {
    pub token_key: String,
    pub tier: Tier,
    pub cashtag: Option<String>,
    pub search_queries: Vec<String>,
}

/// `x-focus.json`: the engine's work list for the collector.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct XFocus {
    pub schema: String,
    pub generated_at_ms: i64,
    pub coins: Vec<FocusCoin>,
    pub resample_post_ids: Vec<String>,
    pub reply_sample_post_ids: Vec<String>,
}

/// Per-token social and market features at a cycle (design doc "Features").
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Features {
    pub mentions_5m: i64,
    pub mentions_60m: i64,
    pub acceleration: f64,
    pub unique_authors_15m: i64,
    pub unique_authors_60m: i64,
    pub curated_hits_60m: i64,
    pub follower_spread_60m: i64,
    pub engagement_velocity_15m: f64,
    pub price_usd: Option<f64>,
    pub price_change_5m: Option<f64>,
    pub price_change_15m: Option<f64>,
    pub price_change_1h: Option<f64>,
    pub volume_5m_usd: Option<f64>,
    pub volume_1h_usd: Option<f64>,
    pub liquidity_usd: Option<f64>,
    pub fdv_usd: Option<f64>,
    pub pair_age_minutes: Option<f64>,
    pub spread_bps: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Coordination {
    pub near_duplicate: bool,
    pub fresh_accounts: bool,
    pub same_minute_burst: bool,
}

impl Coordination {
    pub fn count(self) -> usize {
        usize::from(self.near_duplicate)
            + usize::from(self.fresh_accounts)
            + usize::from(self.same_minute_burst)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExhaustionFlag {
    VelocityRollover,
    EngagementSaturation,
    ReplyShift,
    LateSpread,
    PriceVerticalVolumeWeak,
    StagePeakOrFading,
}

impl ExhaustionFlag {
    /// Pre-registered weight (design doc "Exhaustion score").
    pub const fn weight(self) -> f64 {
        match self {
            Self::VelocityRollover | Self::StagePeakOrFading => 0.20,
            Self::EngagementSaturation
            | Self::ReplyShift
            | Self::LateSpread
            | Self::PriceVerticalVolumeWeak => 0.15,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RugPrecursor {
    PairGone,
    LiquidityDrop,
    ReplyRugWarnings,
    SymbolNotTrading,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TractionStage {
    #[default]
    Ignition,
    Building,
    Peak,
    Fading,
}

/// `x-traction.jsonl`: one row per tracked token (tier >= watch) per cycle.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct XTraction {
    pub schema: String,
    pub ts_ms: i64,
    pub token_key: String,
    pub venue: Venue,
    pub symbol: Option<String>,
    pub cashtag: Option<String>,
    pub tier: Tier,
    pub traction: f64,
    pub features: Features,
    pub coordination: Coordination,
    pub exhaustion: f64,
    pub exhaustion_flags: Vec<ExhaustionFlag>,
    pub rug_precursors: Vec<RugPrecursor>,
    pub stage: Option<TractionStage>,
    pub traction_version: String,
    pub exhaustion_version: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalystType {
    Meme,
    InfluencerCall,
    Listing,
    Launch,
    Partnership,
    News,
    Airdrop,
    Other,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HaikuExhaustionFlag {
    LateStageHype,
    PriceTargetSpam,
    GiveawaySpam,
    GenericLargeAccounts,
    ComplaintsRising,
    ArgumentRising,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RugRedFlag {
    CantSell,
    Honeypot,
    DevSold,
    LpPulled,
    Impersonation,
    Other,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplyQuality {
    pub organic_share: f64,
    pub bot_share: f64,
    pub warning_share: f64,
    pub warnings: Vec<String>,
}

/// `submit_verdict` arguments (design doc "Verdict schema").
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub token: String,
    pub catalyst_type: CatalystType,
    pub narrative_summary: String,
    pub traction_stage: TractionStage,
    pub exhaustion_flags: Vec<HaikuExhaustionFlag>,
    pub shill_risk: f64,
    pub bot_risk: f64,
    pub coordination_risk: f64,
    pub rug_red_flags: Vec<RugRedFlag>,
    pub reply_quality: Option<ReplyQuality>,
    pub conviction: f64,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HaikuTrigger {
    #[default]
    FocusEntry,
    TierChange,
    MaterialChange,
    Signal,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HaikuStatus {
    #[default]
    Ok,
    Error,
    CostCap,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

/// `x-haiku.jsonl`: one row per Haiku call (or skipped call).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct XHaikuCall {
    pub schema: String,
    pub ts_ms: i64,
    pub token_key: String,
    pub trigger: HaikuTrigger,
    pub status: HaikuStatus,
    pub verdict: Option<Verdict>,
    pub previous_stage: Option<TractionStage>,
    pub stage_changed: bool,
    pub usage: LlmUsage,
    pub prompt_version: String,
    pub error: Option<String>,
}

/// `x-signals.jsonl`: a token's first `hot` cycle (at most one per 24h).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct XSignal {
    pub schema: String,
    pub detection_id: String,
    pub detected_at_ms: i64,
    pub venue: Venue,
    pub token_key: String,
    pub token_address: String,
    pub pair_address: Option<String>,
    pub symbol: Option<String>,
    pub cashtag: Option<String>,
    pub resolution: Resolution,
    pub features: Features,
    /// Detection-time market snapshot (raw venue fields kept for the report).
    pub market: BTreeMap<String, serde_json::Value>,
    pub traction: f64,
    pub floor_passed: bool,
    pub haiku_status: HaikuStatus,
    pub haiku: Option<Verdict>,
    pub haiku_arm: bool,
    pub prompt_version: String,
    pub traction_version: String,
    pub exhaustion_version: String,
}

/// `x-ticks.jsonl`: 1-minute price path for focused tokens and open signals.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct XTick {
    pub schema: String,
    pub ts_ms: i64,
    pub token_key: String,
    pub venue: Venue,
    pub price_usd: Option<f64>,
    pub liquidity_usd: Option<f64>,
    pub volume_5m_usd: Option<f64>,
    pub pair_exists: bool,
}

/// Load every row of a JSONL file that parses as `T`; missing file = empty.
pub fn load_rows<T: serde::de::DeserializeOwned>(path: &Path) -> Vec<T> {
    crate::common::load_jsonl_objects(path)
        .into_iter()
        .filter_map(|obj| serde_json::from_value(serde_json::Value::Object(obj)).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_post_row_parses_with_missing_and_null_fields() {
        let row = serde_json::json!({
            "schema": "x-post-v1", "post_id": "1", "author_hash": "ab",
            "created_at_ms": 5, "first_seen_ms": 6, "excerpt": "$PEPE up",
            "cashtags": ["PEPE"], "metrics": {"likes": 3, "views": null},
            "source": {"kind": "focus_search", "query": "$PEPE"}
        });
        let post: XPost = serde_json::from_value(row).unwrap();
        assert_eq!(post.metrics.likes, 3);
        assert_eq!(post.metrics.views, None);
        assert_eq!(post.source.kind, PostSourceKind::FocusSearch);
        assert!(post.solana_addresses.is_empty());
    }

    #[test]
    fn exhaustion_weights_sum_to_one() {
        let all = [
            ExhaustionFlag::VelocityRollover,
            ExhaustionFlag::EngagementSaturation,
            ExhaustionFlag::ReplyShift,
            ExhaustionFlag::LateSpread,
            ExhaustionFlag::PriceVerticalVolumeWeak,
            ExhaustionFlag::StagePeakOrFading,
        ];
        let sum: f64 = all.iter().map(|f| f.weight()).sum();
        assert!((sum - 1.0).abs() < 1e-9);
    }
}
