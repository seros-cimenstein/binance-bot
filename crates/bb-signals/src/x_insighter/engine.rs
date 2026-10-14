//! `bb x-insighter`: the 60-second paper-only loop (design doc "Engine").
//!
//! Each cycle reads new collector rows, resolves tokens, fetches public market
//! state, scores traction / tiers / exhaustion, calls Haiku where allowed, emits
//! paper signals and ticks, and rewrites `x-focus.json` and the engine state.
//! Nothing here can place an order: market access is public data only.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::features::{
    EngagementIndex, ExhaustionInputs, HaikuMemory, MAX_TRACKED, MIN_TRACK_AUTHORS, MentionSample,
    TierState, apply_focus_cap, coordination, engagement_growth, exhaustion,
    large_account_share_30m, latest_metrics, next_tier, post_time_ms, push_mention_sample,
    rug_precursors, rug_reply_warnings, social_features, traction_score, weighted_engagement,
};
use super::haiku::{
    HaikuInput, HaikuSettings, Insighter, MAX_PROMPT_POSTS, MAX_PROMPT_REPLIES, OmpInsighter,
    PromptPost, PromptReply, spent_today_usd,
};
use super::market::{
    Candle, DEX_BATCH, KLINE_LIMIT, LiveMarket, MarketSample, MarketSnapshot, MarketSource,
    binance_snapshot, push_sample, solana_snapshot,
};
use super::resolve::{ResolverState, TokenRef, resolution_rank};
use super::schema::{
    Coordination, EXHAUSTION_VERSION, ExhaustionFlag, FOCUS_SCHEMA, Features, FocusCoin,
    HAIKU_SCHEMA, HaikuExhaustionFlag, HaikuStatus, HaikuTrigger, PROMPT_VERSION, ReplyKind,
    ReplyQuality, RugPrecursor, SIGNAL_SCHEMA, SampleStatus, TICK_SCHEMA, TRACTION_SCHEMA,
    TRACTION_VERSION, Tier, TractionStage, Venue, Verdict, XEngagement, XFocus, XHaikuCall, XPaths,
    XPost, XReply, XSignal, XTick, XTraction, load_rows,
};
use crate::common::{
    Clock, SystemClock, append_ledger, atomic_write_json, http_client, read_json_lenient,
    shutdown_signal,
};
use crate::dexscreener_api::DexScreenerApi;

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;

pub const CYCLE_INTERVAL: Duration = Duration::from_secs(60);
pub const STATE_SCHEMA: &str = "x-insighter-state-v1";
/// Posts and replies kept in memory for the windows (by `first_seen_ms`).
pub const BUFFER_MS: i64 = 3 * HOUR_MS;
/// At most one signal per token per 24h; ticks continue 24h after a signal.
pub const SIGNAL_COOLDOWN_MS: i64 = DAY_MS;
/// Haiku: at most once per 10 min and 6 times per UTC day per token.
pub const HAIKU_MIN_GAP_MS: i64 = 10 * MINUTE_MS;
pub const HAIKU_MAX_PER_DAY: usize = 6;
/// Traction move since the last call that counts as material.
pub const MATERIAL_CHANGE_POINTS: f64 = 15.0;
/// Engagement re-reads after a post entered focus.
pub const RESAMPLE_OFFSETS_MS: [i64; 5] = [
    5 * MINUTE_MS,
    15 * MINUTE_MS,
    30 * MINUTE_MS,
    HOUR_MS,
    3 * HOUR_MS,
];
pub const RESAMPLE_TOP_POSTS: usize = 5;
pub const MAX_RESAMPLE_IDS: usize = 40;
pub const REPLY_SAMPLE_TOP_POSTS: usize = 3;
pub const REPLY_SAMPLE_INTERVAL_MS: i64 = 15 * MINUTE_MS;
pub const MAX_REPLY_SAMPLE_IDS: usize = 6;
/// DexScreener searches per cycle (rate-limit courtesy; the rest wait a cycle).
pub const DEX_SEARCHES_PER_CYCLE: usize = 20;
/// Venue floor.
pub const SOLANA_MIN_LIQUIDITY_USD: f64 = 5_000.0;
pub const SOLANA_MIN_PAIR_AGE_MINUTES: f64 = 5.0;
/// Haiku paper arm.
pub const HAIKU_ARM_MIN_CONVICTION: f64 = 0.6;
pub const HAIKU_ARM_MAX_COORDINATION_RISK: f64 = 0.7;
/// Max market term of the traction score (klines are fetched for tokens that
/// could reach focus with it).
const MAX_MARKET_POINTS: f64 = 10.0;
/// Idle token state is forgotten after this long.
const TOKEN_STATE_RETENTION_MS: i64 = 2 * DAY_MS;
/// Resample schedule entries are dropped this long after their last offset.
const RESAMPLE_GRACE_MS: i64 = HOUR_MS;

// ---------------------------------------------------------------- state

/// Byte offsets of the next unread line of each collector file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileOffsets {
    pub posts: u64,
    pub engagement: u64,
    pub replies: u64,
}

/// Per-token Haiku call bookkeeping.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HaikuLedger {
    /// Call times on the current UTC day.
    pub calls_ms: Vec<i64>,
    pub last_call_ms: Option<i64>,
    pub last_traction: Option<f64>,
    pub last_flags: Vec<ExhaustionFlag>,
    /// Trigger held back by the rate limits, retried while focused.
    pub pending: Option<HaikuTrigger>,
}

impl HaikuLedger {
    pub fn calls_on_day(&self, now_ms: i64) -> usize {
        let day = now_ms.div_euclid(DAY_MS);
        self.calls_ms
            .iter()
            .filter(|t| t.div_euclid(DAY_MS) == day)
            .count()
    }

    fn record(&mut self, now_ms: i64, traction: f64, flags: &[ExhaustionFlag]) {
        let day = now_ms.div_euclid(DAY_MS);
        self.calls_ms.retain(|t| t.div_euclid(DAY_MS) == day);
        self.calls_ms.push(now_ms);
        self.last_call_ms = Some(now_ms);
        self.last_traction = Some(traction);
        self.last_flags = flags.to_vec();
        self.pending = None;
    }
}

/// Per-token engine memory.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenState {
    pub token: Option<TokenRef>,
    pub tier: TierState,
    pub last_traction: f64,
    pub last_seen_ms: i64,
    pub haiku: HaikuLedger,
    /// Latest ok Haiku verdict: stage, reply read and exhaustion flags.
    pub stage: Option<TractionStage>,
    pub reply_quality: Option<ReplyQuality>,
    pub haiku_flags: Vec<HaikuExhaustionFlag>,
    pub mention_history: Vec<MentionSample>,
    pub market_history: Vec<MarketSample>,
}

/// A token's latest signal (drives the 24h dedupe, ticks and liquidity drop).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalState {
    pub detected_at_ms: i64,
    pub token: TokenRef,
    pub max_liquidity_usd: Option<f64>,
}

/// `x-insighter-state.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineState {
    pub schema: String,
    pub updated_at_ms: i64,
    pub offsets: FileOffsets,
    pub resolver: ResolverState,
    pub tokens: BTreeMap<String, TokenState>,
    pub signals: BTreeMap<String, SignalState>,
    /// Post id -> time it entered focus (engagement re-read schedule).
    pub resample: BTreeMap<String, i64>,
    /// Post id -> first time it was listed for reply sampling.
    pub reply_sample: BTreeMap<String, i64>,
}

// ---------------------------------------------------------------- pure policy

/// Haiku trigger for a focus/hot token this cycle, before rate limits.
pub fn haiku_trigger(
    prev: Option<Tier>,
    new: Option<Tier>,
    traction: f64,
    flags: &[ExhaustionFlag],
    ledger: &HaikuLedger,
) -> Option<HaikuTrigger> {
    if new < Some(Tier::Focus) {
        return None;
    }
    if prev < Some(Tier::Focus) {
        return Some(HaikuTrigger::FocusEntry);
    }
    if prev != new {
        return Some(HaikuTrigger::TierChange);
    }
    if ledger.last_call_ms.is_some() {
        let moved = ledger
            .last_traction
            .is_some_and(|last| (traction - last).abs() >= MATERIAL_CHANGE_POINTS);
        let new_flag = flags.iter().any(|f| !ledger.last_flags.contains(f));
        if moved || new_flag {
            return Some(HaikuTrigger::MaterialChange);
        }
    }
    ledger.pending
}

/// Per-token limits: >= 10 min since the last call and < 6 calls this UTC day.
pub fn haiku_allowed(ledger: &HaikuLedger, now_ms: i64) -> bool {
    ledger
        .last_call_ms
        .is_none_or(|last| now_ms - last >= HAIKU_MIN_GAP_MS)
        && ledger.calls_on_day(now_ms) < HAIKU_MAX_PER_DAY
}

/// A signal fires when a token first reaches `hot`, at most once per 24h.
pub fn signal_due(
    prev: Option<Tier>,
    new: Option<Tier>,
    last_signal_ms: Option<i64>,
    now_ms: i64,
) -> bool {
    new == Some(Tier::Hot)
        && prev != Some(Tier::Hot)
        && last_signal_ms.is_none_or(|last| now_ms - last >= SIGNAL_COOLDOWN_MS)
}

/// Venue floor: Solana liquidity >= $5,000 and pair age >= 5 min; Binance `TRADING`.
pub fn venue_floor(venue: Venue, market: Option<&MarketSnapshot>) -> bool {
    let Some(m) = market else {
        return false;
    };
    match venue {
        Venue::Solana => {
            m.pair_exists
                && m.liquidity_usd
                    .is_some_and(|l| l >= SOLANA_MIN_LIQUIDITY_USD)
                && m.pair_age_minutes
                    .is_some_and(|a| a >= SOLANA_MIN_PAIR_AGE_MINUTES)
        }
        Venue::Binance => m.symbol_status.as_deref() == Some("TRADING"),
    }
}

/// Haiku arm: floor passed, ok verdict with conviction >= 0.6, stage ignition or
/// building, no rug red flags and coordination risk < 0.7.
pub fn haiku_arm(floor_passed: bool, status: HaikuStatus, verdict: Option<&Verdict>) -> bool {
    let Some(v) = verdict else {
        return false;
    };
    floor_passed
        && status == HaikuStatus::Ok
        && v.conviction >= HAIKU_ARM_MIN_CONVICTION
        && matches!(
            v.traction_stage,
            TractionStage::Ignition | TractionStage::Building
        )
        && v.rug_red_flags.is_empty()
        && v.coordination_risk < HAIKU_ARM_MAX_COORDINATION_RISK
}

/// Tick for a token, or `None` when this cycle's fetch gives no definite
/// observation (fetch failed, or a Binance price is missing while not halted).
pub fn tick_for(token: &TokenRef, market: Option<&MarketSnapshot>, now_ms: i64) -> Option<XTick> {
    let m = market?;
    let pair_exists = match token.venue {
        Venue::Solana => m.pair_exists,
        Venue::Binance => {
            if m.symbol_status.as_deref().is_some_and(|s| s != "TRADING") {
                false
            } else if m.price_usd.is_some() {
                true
            } else {
                return None;
            }
        }
    };
    Some(XTick {
        schema: TICK_SCHEMA.to_owned(),
        ts_ms: now_ms,
        token_key: token.token_key.clone(),
        venue: token.venue,
        price_usd: m.price_usd,
        liquidity_usd: m.liquidity_usd,
        volume_5m_usd: m.volume_5m_usd,
        pair_exists,
    })
}

// ---------------------------------------------------------------- buffers

/// In-memory rows of the last [`BUFFER_MS`].
#[derive(Debug, Default)]
struct Buffers {
    posts: Vec<XPost>,
    post_ids: HashSet<String>,
    engagement: EngagementIndex,
    /// Newest `x-engagement` row per post id (any status).
    latest_engagement_ms: HashMap<String, i64>,
    gone: HashSet<String>,
    replies: Vec<XReply>,
    reply_ids: HashSet<(String, String)>,
    /// Newest `x-replies` row per parent post id.
    latest_reply_ms: HashMap<String, i64>,
}

impl Buffers {
    fn add_post(&mut self, post: XPost, now_ms: i64) {
        if post.post_id.is_empty()
            || post.first_seen_ms < now_ms - BUFFER_MS
            || !self.post_ids.insert(post.post_id.clone())
        {
            return;
        }
        self.posts.push(post);
    }

    fn add_engagement(&mut self, row: XEngagement) {
        if row.post_id.is_empty() {
            return;
        }
        let latest = self
            .latest_engagement_ms
            .entry(row.post_id.clone())
            .or_insert(row.sampled_at_ms);
        *latest = (*latest).max(row.sampled_at_ms);
        match (row.status, row.metrics) {
            (SampleStatus::Ok, Some(metrics)) => {
                let reads = self.engagement.entry(row.post_id).or_default();
                let at = reads.partition_point(|(t, _)| *t <= row.sampled_at_ms);
                reads.insert(at, (row.sampled_at_ms, metrics));
            }
            (SampleStatus::Gone, _) => {
                self.gone.insert(row.post_id);
            }
            _ => {}
        }
    }

    fn add_reply(&mut self, reply: XReply, now_ms: i64) {
        if reply.parent_post_id.is_empty() {
            return;
        }
        let latest = self
            .latest_reply_ms
            .entry(reply.parent_post_id.clone())
            .or_insert(reply.first_seen_ms);
        *latest = (*latest).max(reply.first_seen_ms);
        if reply.first_seen_ms < now_ms - BUFFER_MS
            || !self
                .reply_ids
                .insert((reply.parent_post_id.clone(), reply.post_id.clone()))
        {
            return;
        }
        self.replies.push(reply);
    }

    fn prune(&mut self, now_ms: i64, keep_ids: &BTreeSet<String>) {
        let cutoff = now_ms - BUFFER_MS;
        self.posts.retain(|p| p.first_seen_ms >= cutoff);
        self.post_ids = self.posts.iter().map(|p| p.post_id.clone()).collect();
        self.replies.retain(|r| r.first_seen_ms >= cutoff);
        self.reply_ids = self
            .replies
            .iter()
            .map(|r| (r.parent_post_id.clone(), r.post_id.clone()))
            .collect();
        let live = |id: &String| self.post_ids.contains(id) || keep_ids.contains(id);
        self.engagement.retain(|id, _| live(id));
        self.latest_engagement_ms.retain(|id, _| live(id));
        self.gone.retain(|id| live(id));
        self.latest_reply_ms.retain(|id, _| live(id));
    }
}

/// Feed every complete line in `[start, end)` to `f`; returns the offset after
/// the last complete line (a trailing partial line waits for the next read).
fn scan_lines(
    path: &Path,
    start: u64,
    end: Option<u64>,
    mut f: impl FnMut(&str),
) -> std::io::Result<u64> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(start),
        Err(err) => return Err(err),
    };
    file.seek(SeekFrom::Start(start))?;
    let limit = end.map_or(u64::MAX, |end| end.saturating_sub(start));
    let mut reader = BufReader::new(file.take(limit));
    let mut offset = start;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 || line.last() != Some(&b'\n') {
            break;
        }
        offset += read as u64;
        if let Ok(text) = std::str::from_utf8(&line) {
            let text = text.trim();
            if !text.is_empty() {
                f(text);
            }
        }
    }
    Ok(offset)
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

// ---------------------------------------------------------------- engine

/// One token's work item within a cycle.
#[derive(Debug, Clone)]
struct Work {
    token: TokenRef,
    posts: Vec<usize>,
    tracked: bool,
    features: Features,
    coord: Coordination,
    market: Option<MarketSnapshot>,
    traction: f64,
    prev_tier: Option<Tier>,
    tier: TierState,
    exhaustion: f64,
    flags: Vec<ExhaustionFlag>,
    rug: Vec<RugPrecursor>,
}

/// Counts for the cycle log line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CycleSummary {
    pub new_posts: usize,
    pub tracked: usize,
    pub focused: usize,
    pub haiku_calls: usize,
    pub signals: usize,
}

struct Mention {
    token: TokenRef,
    posts: Vec<usize>,
}

pub struct Engine {
    paths: XPaths,
    market: Arc<dyn MarketSource>,
    insighter: Arc<dyn Insighter>,
    clock: Arc<dyn Clock>,
    state: EngineState,
    buffers: Buffers,
    spend_day: i64,
    spend_usd: f64,
}

impl Engine {
    /// Load state, rebuild the 3h windows from the collector files (rows before
    /// the stored offsets) and the signal / spend memory from engine files.
    pub fn new(
        paths: XPaths,
        market: Arc<dyn MarketSource>,
        insighter: Arc<dyn Insighter>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let now = clock.now_ms();
        let mut state: EngineState = read_json_lenient(&paths.state)
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        for (offset, path) in [
            (&mut state.offsets.posts, &paths.posts),
            (&mut state.offsets.engagement, &paths.engagement),
            (&mut state.offsets.replies, &paths.replies),
        ] {
            if file_len(path) < *offset {
                tracing::warn!(path = %path.display(), "collector file shrank; rereading from start");
                *offset = 0;
            }
        }
        let mut buffers = Buffers::default();
        let offsets = state.offsets;
        let scans = [
            scan_lines(&paths.posts, 0, Some(offsets.posts), |line| {
                if let Ok(post) = serde_json::from_str::<XPost>(line) {
                    buffers.add_post(post, now);
                }
            }),
            scan_lines(&paths.engagement, 0, Some(offsets.engagement), |line| {
                if let Ok(row) = serde_json::from_str::<XEngagement>(line) {
                    buffers.add_engagement(row);
                }
            }),
            scan_lines(&paths.replies, 0, Some(offsets.replies), |line| {
                if let Ok(row) = serde_json::from_str::<XReply>(line) {
                    buffers.add_reply(row, now);
                }
            }),
        ];
        for result in scans {
            if let Err(err) = result {
                tracing::warn!(%err, "cannot rebuild x-insighter windows from collector file");
            }
        }
        // Signals written before a crash may be missing from the saved state.
        for row in load_rows::<XSignal>(&paths.signals) {
            if now - row.detected_at_ms >= SIGNAL_COOLDOWN_MS || row.token_key.is_empty() {
                continue;
            }
            let known = state
                .signals
                .get(&row.token_key)
                .is_some_and(|s| s.detected_at_ms >= row.detected_at_ms);
            if !known {
                let token = TokenRef {
                    token_key: row.token_key.clone(),
                    venue: row.venue,
                    token_address: row.token_address.clone(),
                    symbol: row.symbol.clone(),
                    pair_address: row.pair_address.clone(),
                    cashtag: row.cashtag.clone(),
                    resolution: row.resolution,
                };
                state.signals.insert(
                    row.token_key.clone(),
                    SignalState {
                        detected_at_ms: row.detected_at_ms,
                        token,
                        max_liquidity_usd: row.features.liquidity_usd,
                    },
                );
            }
        }
        let spend_usd = spent_today_usd(&load_rows::<XHaikuCall>(&paths.haiku), now);
        Self {
            paths,
            market,
            insighter,
            clock,
            state,
            buffers,
            spend_day: now.div_euclid(DAY_MS),
            spend_usd,
        }
    }

    pub fn state(&self) -> &EngineState {
        &self.state
    }

    /// One engine cycle (design doc steps 1-7).
    pub async fn run_cycle(&mut self) -> anyhow::Result<CycleSummary> {
        let now = self.clock.now_ms();
        let mut summary = CycleSummary {
            new_posts: self.ingest(now)?,
            ..CycleSummary::default()
        };
        self.refresh_universe(now).await;
        self.search_cashtags(now).await;
        self.state.resolver.prune(now);

        let mut work = self.select_work(now);
        summary.tracked = work.iter().filter(|w| w.tracked).count();
        self.fetch_market(&mut work, now).await;
        work.retain(|w| !w.market.as_ref().is_some_and(|m| m.stablecoin));
        self.score(&mut work, now);
        summary.focused = work
            .iter()
            .filter(|w| w.tier.tier >= Some(Tier::Focus))
            .count();

        self.write_traction(&work, now)?;
        let signal_keys: BTreeSet<String> = work
            .iter()
            .filter(|w| {
                w.tracked
                    && signal_due(
                        w.prev_tier,
                        w.tier.tier,
                        self.state
                            .signals
                            .get(&w.token.token_key)
                            .map(|s| s.detected_at_ms),
                        now,
                    )
            })
            .map(|w| w.token.token_key.clone())
            .collect();
        summary.haiku_calls = self.run_haiku(&work, &signal_keys, now).await?;
        summary.signals = self.emit_signals(&work, &signal_keys, now).await?;
        summary.haiku_calls += summary.signals;
        self.write_ticks(&work, now)?;
        self.write_focus(&work, now)?;
        self.save_state(&work, now)?;
        Ok(summary)
    }

    /// Step 1: new collector rows since the stored offsets.
    fn ingest(&mut self, now: i64) -> anyhow::Result<usize> {
        let paths = &self.paths;
        let offsets = &mut self.state.offsets;
        for (offset, path) in [
            (&mut offsets.posts, &paths.posts),
            (&mut offsets.engagement, &paths.engagement),
            (&mut offsets.replies, &paths.replies),
        ] {
            if file_len(path) < *offset {
                tracing::warn!(path = %path.display(), "collector file shrank; rereading from start");
                *offset = 0;
            }
        }
        let buffers = &mut self.buffers;
        let resolver = &mut self.state.resolver;
        let mut new_posts = 0;
        offsets.posts = scan_lines(&paths.posts, offsets.posts, None, |line| {
            if let Ok(post) = serde_json::from_str::<XPost>(line) {
                resolver.learn_links(&post, post_time_ms(&post));
                new_posts += 1;
                buffers.add_post(post, now);
            }
        })?;
        offsets.engagement = scan_lines(&paths.engagement, offsets.engagement, None, |line| {
            if let Ok(row) = serde_json::from_str::<XEngagement>(line) {
                buffers.add_engagement(row);
            }
        })?;
        offsets.replies = scan_lines(&paths.replies, offsets.replies, None, |line| {
            if let Ok(row) = serde_json::from_str::<XReply>(line) {
                buffers.add_reply(row, now);
            }
        })?;
        let keep: BTreeSet<String> = self
            .state
            .resample
            .keys()
            .chain(self.state.reply_sample.keys())
            .cloned()
            .collect();
        self.buffers.prune(now, &keep);
        Ok(new_posts)
    }

    /// Binance listings / exclusions, refreshed every 6h when cashtags need them.
    async fn refresh_universe(&mut self, now: i64) {
        let fresh = self
            .state
            .resolver
            .binance
            .as_ref()
            .is_some_and(|u| u.is_fresh(now));
        if fresh || !self.buffers.posts.iter().any(|p| !p.cashtags.is_empty()) {
            return;
        }
        match self.market.binance_universe(now).await {
            Ok(universe) => self.state.resolver.binance = Some(universe),
            Err(err) => tracing::warn!("Binance universe refresh failed: {err:#}"),
        }
    }

    /// Rule 4 lookups for pending cashtags with >= 2 authors in 60 min.
    async fn search_cashtags(&mut self, now: i64) {
        if self.state.resolver.binance.is_none() {
            return;
        }
        let mut authors: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
        for post in &self.buffers.posts {
            let ts = post_time_ms(post);
            if ts <= now - HOUR_MS || ts > now {
                continue;
            }
            for tag in self.state.resolver.resolve_post(post, now).pending {
                authors
                    .entry(tag)
                    .or_default()
                    .insert(post.author_hash.as_str());
            }
        }
        let mut due: Vec<(String, usize)> = authors
            .into_iter()
            .map(|(tag, a)| (tag, a.len()))
            .filter(|(_, n)| *n >= 2)
            .collect();
        due.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (tag, _) in due.into_iter().take(DEX_SEARCHES_PER_CYCLE) {
            match self.market.dex_search(&tag).await {
                Ok(pairs) => self.state.resolver.record_search(&tag, &pairs, now),
                Err(err) => tracing::warn!(cashtag = %tag, "DexScreener search failed: {err:#}"),
            }
        }
    }

    /// Step 2: token -> posts over the buffer, strongest resolution kept.
    fn resolve_mentions(&self, now: i64) -> BTreeMap<String, Mention> {
        let mut out: BTreeMap<String, Mention> = BTreeMap::new();
        for (idx, post) in self.buffers.posts.iter().enumerate() {
            for token in self.state.resolver.resolve_post(post, now).tokens {
                match out.get_mut(&token.token_key) {
                    Some(m) => {
                        m.posts.push(idx);
                        if resolution_rank(token.resolution) < resolution_rank(m.token.resolution) {
                            let cashtag = token.cashtag.clone().or_else(|| m.token.cashtag.take());
                            m.token = TokenRef { cashtag, ..token };
                        } else if m.token.cashtag.is_none() {
                            m.token.cashtag = token.cashtag;
                        }
                    }
                    None => {
                        out.insert(
                            token.token_key.clone(),
                            Mention {
                                token,
                                posts: vec![idx],
                            },
                        );
                    }
                }
            }
        }
        out
    }

    fn post_refs(&self, idx: &[usize]) -> Vec<&XPost> {
        idx.iter()
            .filter_map(|i| self.buffers.posts.get(*i))
            .collect()
    }

    fn new_work(&self, token: TokenRef, posts: Vec<usize>, tracked: bool, now: i64) -> Work {
        let refs = self.post_refs(&posts);
        let features = social_features(&refs, &self.buffers.engagement, now);
        let coord = coordination(&refs, now);
        let token_state = self.state.tokens.get(&token.token_key);
        let traction = traction_score(
            &features,
            coord,
            token_state.and_then(|t| t.reply_quality.as_ref()),
        );
        Work {
            token,
            posts,
            tracked,
            features,
            coord,
            market: None,
            traction,
            prev_tier: token_state.and_then(|t| t.tier.tier),
            tier: TierState::default(),
            exhaustion: 0.0,
            flags: Vec::new(),
            rug: Vec::new(),
        }
    }

    /// Tracked set (>= 2 authors in 60 min, or currently focus/hot), capped at
    /// 50 by traction, plus untracked tokens with a signal in the last 24h.
    fn select_work(&self, now: i64) -> Vec<Work> {
        let mut mentions = self.resolve_mentions(now);
        let mut work: Vec<Work> = mentions
            .values()
            .map(|m| self.new_work(m.token.clone(), m.posts.clone(), true, now))
            .filter(|w| {
                w.features.unique_authors_60m >= MIN_TRACK_AUTHORS
                    || w.prev_tier >= Some(Tier::Focus)
            })
            .collect();
        work.sort_by(|a, b| {
            b.traction
                .total_cmp(&a.traction)
                .then_with(|| a.token.token_key.cmp(&b.token.token_key))
        });
        work.truncate(MAX_TRACKED);
        for (key, signal) in &self.state.signals {
            if now - signal.detected_at_ms >= SIGNAL_COOLDOWN_MS
                || work.iter().any(|w| &w.token.token_key == key)
            {
                continue;
            }
            let posts = mentions.remove(key).map(|m| m.posts).unwrap_or_default();
            work.push(self.new_work(signal.token.clone(), posts, false, now));
        }
        work
    }

    /// Step 3: public market state (Binance `ticker/price` once, klines + book +
    /// status for focus candidates; DexScreener tokens in batches of 30).
    async fn fetch_market(&self, work: &mut [Work], now: i64) {
        let detail = |w: &Work| -> bool {
            w.prev_tier >= Some(Tier::Focus)
                || w.traction + MAX_MARKET_POINTS >= super::features::FOCUS_THRESHOLD
                || self.signal_open(&w.token.token_key, now)
        };
        // Binance.
        if work.iter().any(|w| w.token.venue == Venue::Binance) {
            match self.market.binance_prices().await {
                Err(err) => tracing::warn!("Binance ticker/price failed: {err:#}"),
                Ok(prices) => {
                    let detailed: Vec<String> = work
                        .iter()
                        .filter(|w| w.token.venue == Venue::Binance && detail(w))
                        .filter_map(|w| w.token.symbol.clone())
                        .collect();
                    let statuses = if detailed.is_empty() {
                        BTreeMap::new()
                    } else {
                        self.market
                            .binance_symbol_status(&detailed)
                            .await
                            .unwrap_or_else(|err| {
                                tracing::warn!("Binance exchangeInfo failed: {err:#}");
                                BTreeMap::new()
                            })
                    };
                    for w in work.iter_mut().filter(|w| w.token.venue == Venue::Binance) {
                        let Some(symbol) = w.token.symbol.clone() else {
                            continue;
                        };
                        let price = prices.get(&symbol).copied();
                        let (klines, book): (Option<Vec<Candle>>, Option<(f64, f64)>) =
                            if detailed.contains(&symbol) {
                                (
                                self.market
                                    .binance_klines_1m(&symbol, KLINE_LIMIT)
                                    .await
                                    .map_err(
                                        |err| tracing::warn!(%symbol, "klines failed: {err:#}"),
                                    )
                                    .ok(),
                                self.market
                                    .binance_book(&symbol)
                                    .await
                                    .map_err(
                                        |err| tracing::warn!(%symbol, "bookTicker failed: {err:#}"),
                                    )
                                    .ok(),
                            )
                            } else {
                                (None, None)
                            };
                        w.market = Some(binance_snapshot(
                            price,
                            klines.as_deref(),
                            book,
                            statuses.get(&symbol).cloned(),
                            now,
                        ));
                    }
                }
            }
        }
        // Solana.
        let mints: Vec<String> = work
            .iter()
            .filter(|w| w.token.venue == Venue::Solana)
            .map(|w| w.token.token_address.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut pairs_by_mint: BTreeMap<String, Vec<crate::jsonv::Object>> = BTreeMap::new();
        let mut fetched: HashSet<String> = HashSet::new();
        for chunk in mints.chunks(DEX_BATCH) {
            match self.market.dex_token_pairs(chunk).await {
                Ok(pairs) => {
                    fetched.extend(chunk.iter().cloned());
                    for pair in pairs {
                        let base = crate::jsonv::py_str(crate::jsonv::nested(
                            &pair,
                            "baseToken",
                            "address",
                        ));
                        pairs_by_mint.entry(base).or_default().push(pair);
                    }
                }
                Err(err) => tracing::warn!("DexScreener tokens batch failed: {err:#}"),
            }
        }
        for w in work.iter_mut().filter(|w| w.token.venue == Venue::Solana) {
            if !fetched.contains(&w.token.token_address) {
                continue;
            }
            let pairs: Vec<&crate::jsonv::Object> = pairs_by_mint
                .get(&w.token.token_address)
                .map(|p| p.iter().collect())
                .unwrap_or_default();
            let preferred = self
                .state
                .signals
                .get(&w.token.token_key)
                .and_then(|s| s.token.pair_address.clone())
                .or_else(|| w.token.pair_address.clone());
            let history = self
                .state
                .tokens
                .get(&w.token.token_key)
                .map(|t| t.market_history.as_slice())
                .unwrap_or_default();
            w.market = Some(solana_snapshot(&pairs, preferred.as_deref(), history, now));
        }
    }

    fn signal_open(&self, key: &str, now: i64) -> bool {
        self.state
            .signals
            .get(key)
            .is_some_and(|s| now - s.detected_at_ms < SIGNAL_COOLDOWN_MS)
    }

    /// Step 4: traction, tiers (hysteresis, focus cap), histories, exhaustion, rug.
    fn score(&mut self, work: &mut [Work], now: i64) {
        for w in work.iter_mut() {
            if let Some(m) = &w.market {
                m.fill_features(&mut w.features);
            }
            let ts = self
                .state
                .tokens
                .entry(w.token.token_key.clone())
                .or_default();
            w.traction = traction_score(&w.features, w.coord, ts.reply_quality.as_ref());
            w.prev_tier = ts.tier.tier;
            w.tier = if w.tracked {
                next_tier(ts.tier, w.traction, now)
            } else {
                TierState::default()
            };
        }
        let mut entries: Vec<(f64, TierState)> =
            work.iter().map(|w| (w.traction, w.tier)).collect();
        apply_focus_cap(&mut entries);
        for (w, (_, tier)) in work.iter_mut().zip(entries) {
            w.tier = tier;
        }

        let active: BTreeSet<&str> = work.iter().map(|w| w.token.token_key.as_str()).collect();
        for (key, ts) in &mut self.state.tokens {
            if !active.contains(key.as_str()) {
                ts.tier = TierState::default();
            }
        }

        for w in work.iter_mut() {
            let key = w.token.token_key.clone();
            let open_signal = self.signal_open(&key, now);
            if w.tier.tier >= Some(Tier::Focus) || open_signal {
                let refs: Vec<&XPost> = w
                    .posts
                    .iter()
                    .filter_map(|i| self.buffers.posts.get(*i))
                    .collect();
                let ids: HashSet<&str> = refs.iter().map(|p| p.post_id.as_str()).collect();
                let warnings = rug_reply_warnings(
                    self.buffers
                        .replies
                        .iter()
                        .filter(|r| ids.contains(r.parent_post_id.as_str())),
                );
                let max_liquidity = match (self.state.signals.get_mut(&key), &w.market) {
                    (Some(signal), Some(m)) if open_signal => {
                        if let Some(l) = m.liquidity_usd {
                            signal.max_liquidity_usd =
                                Some(signal.max_liquidity_usd.map_or(l, |max| max.max(l)));
                        }
                        signal.max_liquidity_usd
                    }
                    (Some(signal), None) if open_signal => signal.max_liquidity_usd,
                    _ => None,
                };
                w.rug = rug_precursors(w.token.venue, w.market.as_ref(), max_liquidity, warnings);
                let ts = self.state.tokens.get(&key);
                let empty: &[HaikuExhaustionFlag] = &[];
                let inputs = ExhaustionInputs {
                    features: &w.features,
                    mention_history: ts.map(|t| t.mention_history.as_slice()).unwrap_or_default(),
                    growth: engagement_growth(&refs, &self.buffers.engagement, now),
                    large_account_share_30m: large_account_share_30m(&refs, now),
                    volume_5m_mean_15m_usd: w
                        .market
                        .as_ref()
                        .and_then(|m| m.volume_5m_mean_15m_usd),
                    haiku: HaikuMemory {
                        stage: ts.and_then(|t| t.stage),
                        reply_quality: ts.and_then(|t| t.reply_quality.as_ref()),
                        flags: ts.map_or(empty, |t| t.haiku_flags.as_slice()),
                    },
                };
                let (score, flags) = exhaustion(&inputs);
                w.exhaustion = score;
                w.flags = flags;
            }
            let ts = self.state.tokens.entry(key).or_default();
            ts.token = Some(w.token.clone());
            ts.tier = w.tier;
            ts.last_traction = w.traction;
            ts.last_seen_ms = now;
            push_mention_sample(
                &mut ts.mention_history,
                MentionSample {
                    ts_ms: now,
                    mentions_5m: w.features.mentions_5m,
                    acceleration: w.features.acceleration,
                },
            );
            if let Some(m) = &w.market {
                push_sample(&mut ts.market_history, m.sample(now));
            }
        }
    }

    fn write_traction(&self, work: &[Work], now: i64) -> anyhow::Result<()> {
        for w in work.iter().filter(|w| w.tier.tier.is_some()) {
            let Some(tier) = w.tier.tier else {
                continue;
            };
            let row = XTraction {
                schema: TRACTION_SCHEMA.to_owned(),
                ts_ms: now,
                token_key: w.token.token_key.clone(),
                venue: w.token.venue,
                symbol: w.token.symbol.clone(),
                cashtag: w.token.cashtag.clone(),
                tier,
                traction: round4(w.traction),
                features: w.features.clone(),
                coordination: w.coord,
                exhaustion: w.exhaustion,
                exhaustion_flags: w.flags.clone(),
                rug_precursors: w.rug.clone(),
                stage: self
                    .state
                    .tokens
                    .get(&w.token.token_key)
                    .and_then(|t| t.stage),
                traction_version: TRACTION_VERSION.to_owned(),
                exhaustion_version: EXHAUSTION_VERSION.to_owned(),
            };
            append_ledger(&self.paths.traction, &row)?;
        }
        Ok(())
    }

    /// Step 5: rate-limited Haiku calls for focus/hot tokens. Tokens about to
    /// signal get the `signal` call instead (step 6).
    async fn run_haiku(
        &mut self,
        work: &[Work],
        signal_keys: &BTreeSet<String>,
        now: i64,
    ) -> anyhow::Result<usize> {
        let mut calls = 0;
        for w in work {
            let key = &w.token.token_key;
            let Some(ts) = self.state.tokens.get_mut(key) else {
                continue;
            };
            if w.tier.tier < Some(Tier::Focus) || signal_keys.contains(key) {
                ts.haiku.pending = None;
                continue;
            }
            let Some(trigger) =
                haiku_trigger(w.prev_tier, w.tier.tier, w.traction, &w.flags, &ts.haiku)
            else {
                continue;
            };
            if !haiku_allowed(&ts.haiku, now) {
                ts.haiku.pending = Some(trigger);
                continue;
            }
            self.call_haiku(w, trigger, now).await?;
            calls += 1;
        }
        Ok(calls)
    }

    fn spent_today(&mut self, now: i64) -> f64 {
        let day = now.div_euclid(DAY_MS);
        if day != self.spend_day {
            self.spend_day = day;
            self.spend_usd = 0.0;
        }
        self.spend_usd
    }

    fn haiku_input(&self, w: &Work, trigger: HaikuTrigger, now: i64) -> HaikuInput {
        let age_days = |created: Option<i64>| {
            created
                .filter(|c| *c > 0)
                .map(|c| (now - c).div_euclid(DAY_MS))
        };
        let mut posts: Vec<&XPost> = self.post_refs(&w.posts);
        posts.sort_by_key(|p| std::cmp::Reverse(post_time_ms(p)));
        let ids: HashSet<&str> = posts.iter().map(|p| p.post_id.as_str()).collect();
        let prompt_posts = posts
            .iter()
            .take(MAX_PROMPT_POSTS)
            .map(|p| {
                let m = latest_metrics(p, &self.buffers.engagement, now);
                PromptPost {
                    excerpt: p.excerpt.clone(),
                    author_followers: p.author_followers,
                    author_age_days: age_days(p.author_created_at_ms),
                    curated: p.curated,
                    minutes_ago: (now - post_time_ms(p)).div_euclid(MINUTE_MS),
                    replies: m.replies,
                    reposts: m.reposts,
                    quotes: m.quotes,
                    likes: m.likes,
                    bookmarks: m.bookmarks,
                    views: m.views,
                }
            })
            .collect();
        let mut replies: Vec<&XReply> = self
            .buffers
            .replies
            .iter()
            .filter(|r| ids.contains(r.parent_post_id.as_str()))
            .collect();
        replies.sort_by_key(|r| std::cmp::Reverse(r.created_at_ms.max(r.first_seen_ms)));
        let prompt_replies = replies
            .iter()
            .take(MAX_PROMPT_REPLIES)
            .map(|r| PromptReply {
                kind: match r.kind {
                    ReplyKind::Reply => "reply",
                    ReplyKind::Quote => "quote",
                }
                .to_owned(),
                excerpt: r.excerpt.clone(),
                author_followers: r.author_followers,
                author_age_days: age_days(r.author_created_at_ms),
                minutes_ago: (now
                    - if r.created_at_ms > 0 {
                        r.created_at_ms
                    } else {
                        r.first_seen_ms
                    })
                .div_euclid(MINUTE_MS),
            })
            .collect();
        HaikuInput {
            token_key: w.token.token_key.clone(),
            cashtag: w.token.cashtag.clone(),
            venue: w.token.venue,
            trigger,
            traction: round4(w.traction),
            features: w.features.clone(),
            market: w.market.clone().unwrap_or_default().to_map(),
            posts: prompt_posts,
            replies: prompt_replies,
        }
    }

    /// One Haiku assessment: append the `x-haiku` row and update token memory.
    async fn call_haiku(
        &mut self,
        w: &Work,
        trigger: HaikuTrigger,
        now: i64,
    ) -> anyhow::Result<XHaikuCall> {
        let input = self.haiku_input(w, trigger, now);
        let spent = self.spent_today(now);
        let outcome = self.insighter.assess(&input, spent).await;
        self.spend_usd += outcome.usage.cost_usd.max(0.0);
        let ts = self
            .state
            .tokens
            .entry(w.token.token_key.clone())
            .or_default();
        let previous_stage = ts.stage;
        let verdict = outcome
            .verdict
            .filter(|_| outcome.status == HaikuStatus::Ok);
        let stage_changed = match (&verdict, previous_stage) {
            (Some(v), Some(prev)) => v.traction_stage != prev,
            _ => false,
        };
        if let Some(v) = &verdict {
            ts.stage = Some(v.traction_stage);
            if let Some(rq) = &v.reply_quality {
                ts.reply_quality = Some(rq.clone());
            }
            ts.haiku_flags.clone_from(&v.exhaustion_flags);
        }
        ts.haiku.record(now, w.traction, &w.flags);
        let row = XHaikuCall {
            schema: HAIKU_SCHEMA.to_owned(),
            ts_ms: now,
            token_key: w.token.token_key.clone(),
            trigger,
            status: outcome.status,
            verdict,
            previous_stage,
            stage_changed,
            usage: outcome.usage,
            prompt_version: PROMPT_VERSION.to_owned(),
            error: outcome.error,
        };
        append_ledger(&self.paths.haiku, &row)?;
        Ok(row)
    }

    /// Step 6a: signals for tokens that first reached `hot`.
    async fn emit_signals(
        &mut self,
        work: &[Work],
        signal_keys: &BTreeSet<String>,
        now: i64,
    ) -> anyhow::Result<usize> {
        let mut emitted = 0;
        for w in work
            .iter()
            .filter(|w| signal_keys.contains(&w.token.token_key))
        {
            let call = self.call_haiku(w, HaikuTrigger::Signal, now).await?;
            let floor_passed = venue_floor(w.token.venue, w.market.as_ref());
            let arm = haiku_arm(floor_passed, call.status, call.verdict.as_ref());
            let market = w.market.clone().unwrap_or_default();
            let pair_address = match w.token.venue {
                Venue::Solana => market
                    .pair_address
                    .clone()
                    .or_else(|| w.token.pair_address.clone()),
                Venue::Binance => None,
            };
            let token = TokenRef {
                pair_address: pair_address.clone(),
                ..w.token.clone()
            };
            let mut features = w.features.clone();
            features.price_usd = market.price_usd;
            let row = XSignal {
                schema: SIGNAL_SCHEMA.to_owned(),
                detection_id: format!(
                    "x-{}-{}-{now}",
                    w.token.venue.as_str(),
                    w.token.token_address
                ),
                detected_at_ms: now,
                venue: w.token.venue,
                token_key: w.token.token_key.clone(),
                token_address: w.token.token_address.clone(),
                pair_address,
                symbol: w.token.symbol.clone(),
                cashtag: w.token.cashtag.clone(),
                resolution: w.token.resolution,
                features,
                market: market.to_map(),
                traction: round4(w.traction),
                floor_passed,
                haiku_status: call.status,
                haiku: call.verdict,
                haiku_arm: arm,
                prompt_version: PROMPT_VERSION.to_owned(),
                traction_version: TRACTION_VERSION.to_owned(),
                exhaustion_version: EXHAUSTION_VERSION.to_owned(),
            };
            append_ledger(&self.paths.signals, &row)?;
            tracing::info!(
                detection_id = %row.detection_id,
                traction = row.traction,
                floor_passed,
                haiku_arm = arm,
                "x-insighter paper signal"
            );
            self.state.signals.insert(
                w.token.token_key.clone(),
                SignalState {
                    detected_at_ms: now,
                    token,
                    max_liquidity_usd: market.liquidity_usd,
                },
            );
            emitted += 1;
        }
        Ok(emitted)
    }

    /// Step 6b: ticks for focus/hot tokens and tokens with a signal in 24h.
    fn write_ticks(&self, work: &[Work], now: i64) -> anyhow::Result<()> {
        for w in work {
            if w.tier.tier < Some(Tier::Focus) && !self.signal_open(&w.token.token_key, now) {
                continue;
            }
            if let Some(tick) = tick_for(&w.token, w.market.as_ref(), now) {
                append_ledger(&self.paths.ticks, &tick)?;
            }
        }
        Ok(())
    }

    /// Posts of a token by latest weighted engagement (highest first).
    fn top_posts(&self, w: &Work, now: i64, n: usize) -> Vec<String> {
        let mut posts: Vec<(f64, &XPost)> = self
            .post_refs(&w.posts)
            .into_iter()
            .filter(|p| !self.buffers.gone.contains(&p.post_id))
            .map(|p| {
                (
                    weighted_engagement(&latest_metrics(p, &self.buffers.engagement, now)),
                    p,
                )
            })
            .collect();
        posts.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| a.1.post_id.cmp(&b.1.post_id))
        });
        posts
            .into_iter()
            .take(n)
            .map(|(_, p)| p.post_id.clone())
            .collect()
    }

    /// Step 7a: `x-focus.json`.
    fn write_focus(&mut self, work: &[Work], now: i64) -> anyhow::Result<()> {
        let mut focused: Vec<&Work> = work
            .iter()
            .filter(|w| w.tier.tier >= Some(Tier::Focus))
            .collect();
        focused.sort_by(|a, b| b.traction.total_cmp(&a.traction));

        let mut coins = Vec::new();
        let mut reply_candidates: Vec<String> = Vec::new();
        for w in &focused {
            let mut search_queries: Vec<String> =
                w.token.cashtag.iter().map(|t| format!("${t}")).collect();
            if w.token.venue == Venue::Solana {
                search_queries.push(w.token.token_address.clone());
            }
            coins.push(FocusCoin {
                token_key: w.token.token_key.clone(),
                tier: w.tier.tier.unwrap_or(Tier::Focus),
                cashtag: w.token.cashtag.clone(),
                search_queries,
            });
            for id in self.top_posts(w, now, RESAMPLE_TOP_POSTS) {
                self.state.resample.entry(id).or_insert(now);
            }
            reply_candidates.extend(self.top_posts(w, now, REPLY_SAMPLE_TOP_POSTS));
        }

        // Engagement re-reads at +5m/+15m/+30m/+1h/+3h after entering focus.
        let last_offset = RESAMPLE_OFFSETS_MS[RESAMPLE_OFFSETS_MS.len() - 1];
        let latest_eng = &self.buffers.latest_engagement_ms;
        let gone = &self.buffers.gone;
        self.state.resample.retain(|id, entered| {
            let done = latest_eng
                .get(id)
                .is_some_and(|t| *t >= *entered + last_offset);
            !gone.contains(id) && !done && now <= *entered + last_offset + RESAMPLE_GRACE_MS
        });
        let mut due: Vec<(i64, String)> = self
            .state
            .resample
            .iter()
            .filter_map(|(id, entered)| {
                let slot = RESAMPLE_OFFSETS_MS
                    .iter()
                    .map(|o| entered + o)
                    .filter(|t| *t <= now)
                    .max()?;
                let sampled = latest_eng.get(id).is_some_and(|t| *t >= slot);
                (!sampled).then(|| (slot, id.clone()))
            })
            .collect();
        due.sort();
        let resample_post_ids: Vec<String> = due
            .into_iter()
            .take(MAX_RESAMPLE_IDS)
            .map(|(_, id)| id)
            .collect();

        // Reply/quote sampling of the top 3 posts once per 15 min.
        let candidates: BTreeSet<String> = reply_candidates.iter().cloned().collect();
        self.state
            .reply_sample
            .retain(|id, _| candidates.contains(id));
        let mut reply_sample_post_ids = Vec::new();
        for id in reply_candidates {
            let first = *self.state.reply_sample.entry(id.clone()).or_insert(now);
            let slot = first
                + (now - first).div_euclid(REPLY_SAMPLE_INTERVAL_MS) * REPLY_SAMPLE_INTERVAL_MS;
            let sampled = self
                .buffers
                .latest_reply_ms
                .get(&id)
                .is_some_and(|t| *t >= slot);
            if !sampled
                && reply_sample_post_ids.len() < MAX_REPLY_SAMPLE_IDS
                && !reply_sample_post_ids.contains(&id)
            {
                reply_sample_post_ids.push(id);
            }
        }

        let focus = XFocus {
            schema: FOCUS_SCHEMA.to_owned(),
            generated_at_ms: now,
            coins,
            resample_post_ids,
            reply_sample_post_ids,
        };
        atomic_write_json(&self.paths.focus, &focus)?;
        Ok(())
    }

    /// Step 7b: prune and atomically persist the engine state.
    fn save_state(&mut self, work: &[Work], now: i64) -> anyhow::Result<()> {
        let active: BTreeSet<&str> = work.iter().map(|w| w.token.token_key.as_str()).collect();
        let day = now.div_euclid(DAY_MS);
        self.state.tokens.retain(|key, ts| {
            active.contains(key.as_str()) || now - ts.last_seen_ms < TOKEN_STATE_RETENTION_MS
        });
        for ts in self.state.tokens.values_mut() {
            ts.haiku.calls_ms.retain(|t| t.div_euclid(DAY_MS) == day);
        }
        self.state
            .signals
            .retain(|_, s| now - s.detected_at_ms < SIGNAL_COOLDOWN_MS);
        self.state.schema = STATE_SCHEMA.to_owned();
        self.state.updated_at_ms = now;
        atomic_write_json(&self.paths.state, &self.state)?;
        Ok(())
    }
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

// ---------------------------------------------------------------- CLI

/// `bb x-insighter [--once|--daemon]`. Refuses to start (exit 2) unless
/// `X_INSIGHTER_ENABLED=true`. Public market data only; no credentials.
pub async fn run_cli(once: bool) -> anyhow::Result<ExitCode> {
    match bb_core::env::boolean(&["X_INSIGHTER_ENABLED"], false) {
        Ok(true) => {}
        Ok(false) => {
            tracing::error!(
                "X_INSIGHTER_ENABLED is not true; the Phase 43 paper-only X insighter stays off"
            );
            return Ok(ExitCode::from(2));
        }
        Err(err) => {
            tracing::error!("{err}");
            return Ok(ExitCode::from(2));
        }
    }
    let paths = XPaths::from_env();
    let binance = bb_binance::BinanceClient::new(bb_binance::BinanceConfig::new(false))?;
    let dex = DexScreenerApi::new(http_client(Duration::from_secs(10))?);
    let market: Arc<dyn MarketSource> = Arc::new(LiveMarket::new(binance, dex));
    let insighter: Arc<dyn Insighter> = Arc::new(OmpInsighter::new(HaikuSettings::from_env()));
    let mut engine = Engine::new(paths, market, insighter, Arc::new(SystemClock));

    if once {
        let summary = Box::pin(engine.run_cycle()).await?;
        tracing::info!(?summary, "x-insighter cycle done");
        return Ok(ExitCode::SUCCESS);
    }
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            result = Box::pin(engine.run_cycle()) => match result {
                Ok(summary) => tracing::info!(?summary, "x-insighter cycle done"),
                Err(err) => tracing::error!("x-insighter cycle failed: {err:#}"),
            },
        }
        tokio::select! {
            () = &mut shutdown => break,
            () = tokio::time::sleep(CYCLE_INTERVAL) => {}
        }
    }
    tracing::info!("x-insighter stopped by signal");
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::FixedClock;
    use crate::jsonv::Object;
    use crate::x_insighter::haiku::HaikuOutcome;
    use crate::x_insighter::resolve::BinanceUniverse;
    use crate::x_insighter::schema::{LlmUsage, Metrics, Resolution};
    use tokio::sync::Mutex;

    const NOW: i64 = 1_791_900_000_000;
    const MINT: &str = "So1MintPEPE111111111111111111111111111111";

    struct FakeMarket {
        universe: BinanceUniverse,
        pairs: Vec<Object>,
        prices: BTreeMap<String, f64>,
    }

    #[async_trait::async_trait]
    impl MarketSource for FakeMarket {
        async fn binance_universe(&self, _now_ms: i64) -> anyhow::Result<BinanceUniverse> {
            Ok(self.universe.clone())
        }
        async fn binance_prices(&self) -> anyhow::Result<BTreeMap<String, f64>> {
            Ok(self.prices.clone())
        }
        async fn binance_klines_1m(
            &self,
            _symbol: &str,
            _limit: u32,
        ) -> anyhow::Result<Vec<Candle>> {
            Ok(Vec::new())
        }
        async fn binance_book(&self, _symbol: &str) -> anyhow::Result<(f64, f64)> {
            Ok((1.0, 1.001))
        }
        async fn binance_symbol_status(
            &self,
            symbols: &[String],
        ) -> anyhow::Result<BTreeMap<String, String>> {
            Ok(symbols
                .iter()
                .map(|s| (s.clone(), "TRADING".to_owned()))
                .collect())
        }
        async fn dex_token_pairs(&self, mints: &[String]) -> anyhow::Result<Vec<Object>> {
            assert!(mints.len() <= DEX_BATCH);
            Ok(self.pairs.clone())
        }
        async fn dex_search(&self, _query: &str) -> anyhow::Result<Vec<Object>> {
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct MockInsighter {
        calls: Mutex<Vec<(HaikuTrigger, f64, usize)>>,
    }

    #[async_trait::async_trait]
    impl Insighter for MockInsighter {
        async fn assess(&self, input: &HaikuInput, spent_today_usd: f64) -> HaikuOutcome {
            self.calls
                .lock()
                .await
                .push((input.trigger, spent_today_usd, input.posts.len()));
            HaikuOutcome {
                status: HaikuStatus::Ok,
                verdict: Some(Verdict {
                    token: "PEPE".into(),
                    traction_stage: TractionStage::Building,
                    conviction: 0.8,
                    coordination_risk: 0.1,
                    ..Verdict::default()
                }),
                usage: LlmUsage {
                    input_tokens: 1_000,
                    output_tokens: 200,
                    cost_usd: 0.002,
                },
                error: None,
            }
        }
    }

    fn hot_posts() -> Vec<XPost> {
        let followers = [
            50, 800, 2_000, 15_000, 250_000, 600, 3_000, 40_000, 120_000, 900, 7_000, 60_000,
        ];
        (0..12)
            .map(|i| {
                let created = NOW - 4 * MINUTE_MS + i * 20_000;
                XPost {
                    schema: "x-post-v1".into(),
                    post_id: format!("p{i}"),
                    author_hash: format!("author{i}"),
                    curated: i < 3,
                    author_followers: followers[i as usize],
                    created_at_ms: created,
                    first_seen_ms: created + MINUTE_MS,
                    excerpt: format!("take {i} on this coin: variant {} words {}", i * 7, i * 13),
                    cashtags: vec!["PEPE".into()],
                    solana_addresses: vec![MINT.into()],
                    metrics: Metrics {
                        likes: 1_000,
                        reposts: 50,
                        views: Some(10_000),
                        ..Metrics::default()
                    },
                    ..XPost::default()
                }
            })
            .collect()
    }

    fn write_jsonl<T: Serialize>(path: &Path, rows: &[T]) {
        for row in rows {
            append_ledger(path, row).unwrap();
        }
    }

    fn engine(dir: &Path, now: i64, insighter: Arc<MockInsighter>) -> Engine {
        let pair = serde_json::json!({
            "chainId": "solana", "pairAddress": "PAIRPEPE",
            "baseToken": {"address": MINT, "symbol": "PEPE"},
            "priceUsd": "0.0021", "priceChange": {"m5": 4.0, "h1": 5.0, "h6": 9.0, "h24": 12.0},
            "volume": {"m5": 1_000.0, "h1": 6_000.0}, "liquidity": {"usd": 40_000.0},
            "fdv": 2_000_000, "pairCreatedAt": NOW - 3 * HOUR_MS
        });
        let market = FakeMarket {
            universe: BinanceUniverse::default(),
            pairs: vec![pair.as_object().cloned().unwrap()],
            prices: BTreeMap::new(),
        };
        Engine::new(
            XPaths::in_dir(dir),
            Arc::new(market),
            insighter,
            Arc::new(FixedClock(now)),
        )
    }

    #[tokio::test]
    async fn once_cycle_writes_schema_rows_and_signals_once() {
        let dir = tempfile::tempdir().unwrap();
        let paths = XPaths::in_dir(dir.path());
        write_jsonl(&paths.posts, &hot_posts());
        // Earlier spend today is passed to the insighter (cost cap input).
        write_jsonl(
            &paths.haiku,
            &[XHaikuCall {
                schema: HAIKU_SCHEMA.into(),
                ts_ms: NOW - HOUR_MS,
                usage: LlmUsage {
                    cost_usd: 0.3,
                    ..LlmUsage::default()
                },
                ..XHaikuCall::default()
            }],
        );
        let mock = Arc::new(MockInsighter::default());
        let mut eng = engine(dir.path(), NOW, mock.clone());
        let summary = eng.run_cycle().await.unwrap();
        assert_eq!(summary.new_posts, 12);
        assert_eq!(summary.signals, 1);

        let key = format!("solana:{MINT}");
        let traction: Vec<XTraction> = load_rows(&paths.traction);
        assert_eq!(traction.len(), 1);
        assert_eq!(traction[0].token_key, key);
        assert_eq!(traction[0].tier, Tier::Hot);
        assert!(traction[0].traction >= 60.0);
        assert_eq!(traction[0].features.price_usd, Some(0.0021));

        let calls = mock.calls.lock().await.clone();
        assert_eq!(calls.len(), 1, "signal call replaces the focus-entry call");
        assert_eq!(calls[0].0, HaikuTrigger::Signal);
        assert!((calls[0].1 - 0.3).abs() < 1e-9);
        assert_eq!(calls[0].2, 12);
        let haiku: Vec<XHaikuCall> = load_rows(&paths.haiku);
        assert_eq!(haiku.len(), 2);
        assert_eq!(haiku[1].trigger, HaikuTrigger::Signal);
        assert_eq!(haiku[1].previous_stage, None);
        assert!(!haiku[1].stage_changed);

        let signals: Vec<XSignal> = load_rows(&paths.signals);
        assert_eq!(signals.len(), 1);
        let s = &signals[0];
        assert_eq!(s.detection_id, format!("x-solana-{MINT}-{NOW}"));
        assert_eq!(s.resolution, Resolution::MintInPost);
        assert_eq!(s.cashtag.as_deref(), Some("PEPE"));
        assert_eq!(s.pair_address.as_deref(), Some("PAIRPEPE"));
        assert!(s.floor_passed && s.haiku_arm);
        assert_eq!(s.features.price_usd, Some(0.0021));
        for k in [
            "price_usd",
            "price_ref_1h_usd",
            "liquidity_usd",
            "volume_5m_usd",
            "volume_1h_usd",
            "fdv_usd",
            "pair_age_minutes",
            "spread_bps",
            "bid",
            "ask",
            "symbol_status",
            "price_change_5m",
            "price_change_1h",
        ] {
            assert!(s.market.contains_key(k), "{k}");
        }
        assert!((s.market["price_ref_1h_usd"].as_f64().unwrap() - 0.002).abs() < 1e-12);

        let ticks: Vec<XTick> = load_rows(&paths.ticks);
        assert_eq!(ticks.len(), 1);
        assert!(ticks[0].pair_exists);

        let focus: XFocus = serde_json::from_slice(&std::fs::read(&paths.focus).unwrap()).unwrap();
        assert_eq!(focus.coins.len(), 1);
        assert_eq!(
            focus.coins[0].search_queries,
            vec!["$PEPE".to_owned(), MINT.to_owned()]
        );
        assert!(focus.resample_post_ids.is_empty(), "no offset reached yet");
        assert_eq!(focus.reply_sample_post_ids.len(), 3);

        // Restart one minute later: windows rebuilt from the posts file, no new
        // signal (24h dedupe), no Haiku call (no trigger), resample due at +5m only.
        drop(eng);
        let mut eng = engine(dir.path(), NOW + MINUTE_MS, mock.clone());
        let summary = eng.run_cycle().await.unwrap();
        assert_eq!(summary.new_posts, 0);
        assert_eq!(summary.signals, 0);
        assert_eq!(load_rows::<XSignal>(&paths.signals).len(), 1);
        let traction: Vec<XTraction> = load_rows(&paths.traction);
        assert_eq!(traction.len(), 2);
        assert_eq!(traction[1].tier, Tier::Hot);
        assert_eq!(load_rows::<XTick>(&paths.ticks).len(), 2);
        assert_eq!(mock.calls.lock().await.len(), 1);

        let mut eng = engine(dir.path(), NOW + 5 * MINUTE_MS, mock.clone());
        eng.run_cycle().await.unwrap();
        let focus: XFocus = serde_json::from_slice(&std::fs::read(&paths.focus).unwrap()).unwrap();
        assert_eq!(focus.resample_post_ids.len(), RESAMPLE_TOP_POSTS);
        // A newer engagement row drops the id from the list.
        let first = focus.resample_post_ids[0].clone();
        write_jsonl(
            &paths.engagement,
            &[XEngagement {
                schema: "x-engagement-v1".into(),
                post_id: first.clone(),
                sampled_at_ms: NOW + 5 * MINUTE_MS + 1,
                status: SampleStatus::Ok,
                metrics: Some(Metrics::default()),
            }],
        );
        let mut eng = engine(dir.path(), NOW + 6 * MINUTE_MS, mock);
        eng.run_cycle().await.unwrap();
        let focus: XFocus = serde_json::from_slice(&std::fs::read(&paths.focus).unwrap()).unwrap();
        assert_eq!(focus.resample_post_ids.len(), RESAMPLE_TOP_POSTS - 1);
        assert!(!focus.resample_post_ids.contains(&first));
    }

    #[test]
    fn partial_trailing_line_waits_for_next_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.jsonl");
        std::fs::write(&path, b"{\"a\":1}\n{\"b\":").unwrap();
        let mut lines = Vec::new();
        let offset = scan_lines(&path, 0, None, |l| lines.push(l.to_owned())).unwrap();
        assert_eq!(lines, vec!["{\"a\":1}".to_owned()]);
        assert_eq!(offset, 8);
    }

    #[test]
    fn haiku_triggers_and_rate_limits() {
        let ledger = HaikuLedger::default();
        let flags = [ExhaustionFlag::LateSpread];
        assert_eq!(
            haiku_trigger(Some(Tier::Watch), Some(Tier::Watch), 30.0, &[], &ledger),
            None
        );
        assert_eq!(
            haiku_trigger(Some(Tier::Watch), Some(Tier::Focus), 45.0, &[], &ledger),
            Some(HaikuTrigger::FocusEntry)
        );
        assert_eq!(
            haiku_trigger(Some(Tier::Focus), Some(Tier::Hot), 61.0, &[], &ledger),
            Some(HaikuTrigger::TierChange)
        );
        let mut called = HaikuLedger::default();
        called.record(NOW, 45.0, &flags);
        // 14.9 points and no new flag: nothing; 15 points or a new flag: material.
        assert_eq!(
            haiku_trigger(Some(Tier::Focus), Some(Tier::Focus), 59.9, &flags, &called),
            None
        );
        assert_eq!(
            haiku_trigger(Some(Tier::Focus), Some(Tier::Focus), 30.0, &flags, &called),
            Some(HaikuTrigger::MaterialChange)
        );
        assert_eq!(
            haiku_trigger(
                Some(Tier::Focus),
                Some(Tier::Focus),
                45.0,
                &[ExhaustionFlag::LateSpread, ExhaustionFlag::ReplyShift],
                &called
            ),
            Some(HaikuTrigger::MaterialChange)
        );
        // Pending trigger is retried.
        called.pending = Some(HaikuTrigger::FocusEntry);
        assert_eq!(
            haiku_trigger(Some(Tier::Focus), Some(Tier::Focus), 45.0, &flags, &called),
            Some(HaikuTrigger::FocusEntry)
        );

        // >= 10 min apart.
        assert!(!haiku_allowed(&called, NOW + HAIKU_MIN_GAP_MS - 1));
        assert!(haiku_allowed(&called, NOW + HAIKU_MIN_GAP_MS));
        // <= 6 per UTC day; the count resets on the next UTC day.
        let day_start = NOW.div_euclid(DAY_MS) * DAY_MS;
        let mut busy = HaikuLedger::default();
        for i in 0..6 {
            busy.record(day_start + i * HOUR_MS, 45.0, &[]);
        }
        assert_eq!(busy.calls_on_day(day_start + 7 * HOUR_MS), 6);
        assert!(!haiku_allowed(&busy, day_start + 7 * HOUR_MS));
        assert!(haiku_allowed(&busy, day_start + DAY_MS));
    }

    #[test]
    fn signal_due_once_per_24h_on_first_hot() {
        assert!(signal_due(Some(Tier::Focus), Some(Tier::Hot), None, NOW));
        assert!(signal_due(None, Some(Tier::Hot), None, NOW));
        assert!(!signal_due(Some(Tier::Hot), Some(Tier::Hot), None, NOW));
        assert!(!signal_due(Some(Tier::Watch), Some(Tier::Focus), None, NOW));
        assert!(!signal_due(
            Some(Tier::Focus),
            Some(Tier::Hot),
            Some(NOW - DAY_MS + 1),
            NOW
        ));
        assert!(signal_due(
            Some(Tier::Focus),
            Some(Tier::Hot),
            Some(NOW - DAY_MS),
            NOW
        ));
    }

    #[test]
    fn venue_floor_and_haiku_arm_boundaries() {
        let sol = |liq: f64, age: f64| MarketSnapshot {
            pair_exists: true,
            liquidity_usd: Some(liq),
            pair_age_minutes: Some(age),
            ..MarketSnapshot::default()
        };
        assert!(venue_floor(Venue::Solana, Some(&sol(5_000.0, 5.0))));
        assert!(!venue_floor(Venue::Solana, Some(&sol(4_999.9, 5.0))));
        assert!(!venue_floor(Venue::Solana, Some(&sol(5_000.0, 4.9))));
        assert!(!venue_floor(Venue::Solana, None));
        let bin = |status: &str| MarketSnapshot {
            symbol_status: Some(status.into()),
            ..MarketSnapshot::default()
        };
        assert!(venue_floor(Venue::Binance, Some(&bin("TRADING"))));
        assert!(!venue_floor(Venue::Binance, Some(&bin("BREAK"))));

        let good = Verdict {
            conviction: 0.6,
            traction_stage: TractionStage::Ignition,
            coordination_risk: 0.69,
            ..Verdict::default()
        };
        assert!(haiku_arm(true, HaikuStatus::Ok, Some(&good)));
        assert!(!haiku_arm(false, HaikuStatus::Ok, Some(&good)));
        assert!(!haiku_arm(true, HaikuStatus::CostCap, Some(&good)));
        assert!(!haiku_arm(true, HaikuStatus::Ok, None));
        for bad in [
            Verdict {
                conviction: 0.59,
                ..good.clone()
            },
            Verdict {
                traction_stage: TractionStage::Peak,
                ..good.clone()
            },
            Verdict {
                coordination_risk: 0.7,
                ..good.clone()
            },
            Verdict {
                rug_red_flags: vec![crate::x_insighter::schema::RugRedFlag::Honeypot],
                ..good.clone()
            },
        ] {
            assert!(!haiku_arm(true, HaikuStatus::Ok, Some(&bad)));
        }
    }

    #[test]
    fn ticks_only_on_definite_observations() {
        let bin = TokenRef::binance("PEPE");
        let snap = |price: Option<f64>, status: Option<&str>| MarketSnapshot {
            price_usd: price,
            symbol_status: status.map(str::to_owned),
            ..MarketSnapshot::default()
        };
        assert!(tick_for(&bin, None, NOW).is_none());
        assert!(tick_for(&bin, Some(&snap(None, Some("TRADING"))), NOW).is_none());
        assert!(
            tick_for(&bin, Some(&snap(Some(1.0), None)), NOW)
                .unwrap()
                .pair_exists
        );
        assert!(
            !tick_for(&bin, Some(&snap(None, Some("BREAK"))), NOW)
                .unwrap()
                .pair_exists
        );
        let sol = TokenRef::solana(MINT, None, None, Resolution::MintInPost);
        assert!(
            !tick_for(&sol, Some(&MarketSnapshot::default()), NOW)
                .unwrap()
                .pair_exists
        );
    }
}
