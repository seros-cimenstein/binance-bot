//! Per-token features, coordination flags, traction, tiers, exhaustion and rug
//! precursors (design doc "Features", "Traction score", "Tiers and focus",
//! "Exhaustion score"). Everything here is pure and takes an explicit `now_ms`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::market::MarketSnapshot;
use super::schema::{
    Coordination, ExhaustionFlag, Features, HaikuExhaustionFlag, Metrics, ReplyQuality,
    RugPrecursor, Tier, TractionStage, Venue, XPost, XReply,
};

pub const MINUTE_MS: i64 = 60_000;
const DAY_MS: i64 = 86_400_000;
/// Accounts younger than this are "fresh" (coordination, velocity weight).
pub const FRESH_ACCOUNT_MS: i64 = 30 * DAY_MS;

pub const WATCH_THRESHOLD: f64 = 20.0;
pub const FOCUS_THRESHOLD: f64 = 40.0;
pub const HOT_THRESHOLD: f64 = 60.0;
/// Focus/hot drop one tier after this long below their lower threshold minus 10.
pub const HYSTERESIS_POINTS: f64 = 10.0;
pub const HYSTERESIS_MS: i64 = 10 * MINUTE_MS;
/// At most this many tokens are focus or hot at once.
pub const MAX_FOCUS: usize = 5;
/// Tracked-set cap (by traction).
pub const MAX_TRACKED: usize = 50;
/// Unique authors in 60 min needed to track a token.
pub const MIN_TRACK_AUTHORS: i64 = 2;
/// `velocity_rollover` looks back this far.
pub const MENTION_HISTORY_MS: i64 = 2 * 3_600_000;

/// Window timestamp of a post: `created_at_ms`, clamped to `first_seen_ms` when
/// it is missing or in the future relative to the first sighting.
pub fn post_time_ms(post: &XPost) -> i64 {
    if post.created_at_ms <= 0
        || (post.first_seen_ms > 0 && post.created_at_ms > post.first_seen_ms)
    {
        post.first_seen_ms
    } else {
        post.created_at_ms
    }
}

fn in_window(ts: i64, now_ms: i64, window_ms: i64) -> bool {
    ts > now_ms - window_ms && ts <= now_ms
}

/// Engagement weights: replies 3, quotes 3, bookmarks 2.5, reposts 2, likes 1, views 0.01.
pub fn weighted_engagement(m: &Metrics) -> f64 {
    3.0 * m.replies as f64
        + 3.0 * m.quotes as f64
        + 2.5 * m.bookmarks as f64
        + 2.0 * m.reposts as f64
        + m.likes as f64
        + 0.01 * m.views.unwrap_or(0) as f64
}

/// Weighted growth from `a` to `b`; views count only when both reads have them.
fn weighted_delta(a: &Metrics, b: &Metrics) -> f64 {
    let views = match (a.views, b.views) {
        (Some(x), Some(y)) => (y - x) as f64,
        _ => 0.0,
    };
    3.0 * (b.replies - a.replies) as f64
        + 3.0 * (b.quotes - a.quotes) as f64
        + 2.5 * (b.bookmarks - a.bookmarks) as f64
        + 2.0 * (b.reposts - a.reposts) as f64
        + (b.likes - a.likes) as f64
        + 0.01 * views
}

/// Low-reach authors (< 100 followers or account younger than 30 days) count at 0.25.
fn author_weight(followers: i64, created_at_ms: Option<i64>, now_ms: i64) -> f64 {
    let fresh = created_at_ms.is_some_and(|c| c > 0 && now_ms - c < FRESH_ACCOUNT_MS);
    if followers < 100 || fresh { 0.25 } else { 1.0 }
}

/// `ok` engagement re-reads per post id, sorted by `sampled_at_ms`.
pub type EngagementIndex = BTreeMap<String, Vec<(i64, Metrics)>>;

/// Observation series of a post: first-seen counters, then re-reads (all <= now).
fn observations(post: &XPost, engagement: &EngagementIndex, now_ms: i64) -> Vec<(i64, Metrics)> {
    let mut obs = vec![(post.first_seen_ms, post.metrics)];
    if let Some(reads) = engagement.get(&post.post_id) {
        obs.extend(
            reads
                .iter()
                .filter(|(ts, _)| *ts > post.first_seen_ms && *ts <= now_ms)
                .copied(),
        );
    }
    obs
}

/// Latest known counters of a post (first-seen or newest re-read at or before now).
pub fn latest_metrics(post: &XPost, engagement: &EngagementIndex, now_ms: i64) -> Metrics {
    observations(post, engagement, now_ms)
        .last()
        .map_or(post.metrics, |(_, m)| *m)
}

/// `engagement_velocity_15m`: weighted counter growth per minute over the last
/// 15 min, from the two latest consecutive observations of each post (or
/// first-seen counters / post age for posts without a re-read).
pub fn engagement_velocity_15m(posts: &[&XPost], engagement: &EngagementIndex, now_ms: i64) -> f64 {
    let mut total = 0.0;
    for post in posts {
        let obs = observations(post, engagement, now_ms);
        let rate = match obs.as_slice() {
            [(first_seen, metrics)] => {
                if !in_window(*first_seen, now_ms, 15 * MINUTE_MS) {
                    continue;
                }
                let age_min = ((first_seen - post_time_ms(post)) as f64 / 60_000.0).max(1.0);
                weighted_engagement(metrics) / age_min
            }
            [.., (t_a, a), (t_b, b)] => {
                if !in_window(*t_b, now_ms, 15 * MINUTE_MS) {
                    continue;
                }
                let minutes = ((t_b - t_a) as f64 / 60_000.0).max(1.0);
                weighted_delta(a, b).max(0.0) / minutes
            }
            [] => continue,
        };
        total += rate * author_weight(post.author_followers, post.author_created_at_ms, now_ms);
    }
    total
}

fn follower_bucket(followers: i64) -> u8 {
    if followers < 1_000 {
        0
    } else if followers < 10_000 {
        1
    } else if followers < 100_000 {
        2
    } else {
        3
    }
}

/// Social features of one token from its posts (market fields left `None`).
pub fn social_features(posts: &[&XPost], engagement: &EngagementIndex, now_ms: i64) -> Features {
    let count = |window: i64| -> i64 {
        posts
            .iter()
            .filter(|p| in_window(post_time_ms(p), now_ms, window))
            .count() as i64
    };
    let authors = |window: i64| -> BTreeSet<&str> {
        posts
            .iter()
            .filter(|p| in_window(post_time_ms(p), now_ms, window))
            .map(|p| p.author_hash.as_str())
            .collect()
    };
    let mentions_5m = count(5 * MINUTE_MS);
    let mentions_60m = count(60 * MINUTE_MS);
    let posts_60m: Vec<&&XPost> = posts
        .iter()
        .filter(|p| in_window(post_time_ms(p), now_ms, 60 * MINUTE_MS))
        .collect();
    let buckets: BTreeSet<u8> = posts_60m
        .iter()
        .map(|p| follower_bucket(p.author_followers))
        .collect();
    Features {
        mentions_5m,
        mentions_60m,
        acceleration: acceleration(mentions_5m, mentions_60m),
        unique_authors_15m: authors(15 * MINUTE_MS).len() as i64,
        unique_authors_60m: authors(60 * MINUTE_MS).len() as i64,
        curated_hits_60m: posts_60m.iter().filter(|p| p.curated).count() as i64,
        follower_spread_60m: buckets.len() as i64,
        engagement_velocity_15m: engagement_velocity_15m(posts, engagement, now_ms),
        ..Features::default()
    }
}

/// `(mentions_5m + 1) / (baseline_5m + 1)` with `baseline_5m = (m60 - m5) / 11`.
pub fn acceleration(mentions_5m: i64, mentions_60m: i64) -> f64 {
    let baseline = (mentions_60m - mentions_5m).max(0) as f64 / 11.0;
    (mentions_5m as f64 + 1.0) / (baseline + 1.0)
}

#[allow(clippy::expect_used)] // literal patterns; covered by tests
static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(https?://|www\.)\S+").expect("valid URL regex"));
#[allow(clippy::expect_used)]
static ADDRESS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(0x[a-fA-F0-9]{40}|[1-9A-HJ-NP-Za-km-z]{32,44})\b").expect("valid address regex")
});
#[allow(clippy::expect_used)]
static CASHTAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$[A-Za-z0-9_]+").expect("valid cashtag regex"));
#[allow(clippy::expect_used)]
static RUG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)can'?t sell|honeypot|rug(ged)?|dev sold|lp (pulled|removed)")
        .expect("valid rug regex")
});

/// Word trigrams of an excerpt with URLs, addresses and cashtags stripped.
fn trigrams(excerpt: &str) -> HashSet<String> {
    let text = URL_RE.replace_all(excerpt, " ");
    let text = ADDRESS_RE.replace_all(&text, " ");
    let text = CASHTAG_RE.replace_all(&text, " ").to_lowercase();
    let words: Vec<&str> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    match words.len() {
        0 => HashSet::new(),
        1..=2 => HashSet::from([words.join(" ")]),
        _ => words.windows(3).map(|w| w.join(" ")).collect(),
    }
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / union as f64
}

fn median(values: &mut [i64]) -> f64 {
    values.sort_unstable();
    let n = values.len();
    match n {
        0 => 0.0,
        _ if n % 2 == 1 => values[n / 2] as f64,
        _ => (values[n / 2 - 1] + values[n / 2]) as f64 / 2.0,
    }
}

/// Coordination flags (design doc "Features").
pub fn coordination(posts: &[&XPost], now_ms: i64) -> Coordination {
    // near_duplicate: >= 3 posts from distinct authors in 10 min, trigram Jaccard >= 0.8.
    let recent: Vec<(&str, HashSet<String>)> = posts
        .iter()
        .filter(|p| in_window(post_time_ms(p), now_ms, 10 * MINUTE_MS))
        .map(|p| (p.author_hash.as_str(), trigrams(&p.excerpt)))
        .filter(|(_, grams)| !grams.is_empty())
        .collect();
    let near_duplicate = recent.iter().any(|(_, grams)| {
        recent
            .iter()
            .filter(|(_, other)| jaccard(grams, other) >= 0.8)
            .map(|(author, _)| *author)
            .collect::<BTreeSet<_>>()
            .len()
            >= 3
    });

    let posts_60m: Vec<&&XPost> = posts
        .iter()
        .filter(|p| in_window(post_time_ms(p), now_ms, 60 * MINUTE_MS))
        .collect();
    // fresh_accounts: >= 50% of 60-min authors younger than 30 days.
    let mut author_created: BTreeMap<&str, Option<i64>> = BTreeMap::new();
    for p in &posts_60m {
        author_created
            .entry(p.author_hash.as_str())
            .or_insert(p.author_created_at_ms);
    }
    let fresh = author_created
        .values()
        .filter(|c| c.is_some_and(|c| c > 0 && now_ms - c < FRESH_ACCOUNT_MS))
        .count();
    let fresh_accounts = !author_created.is_empty() && fresh * 2 >= author_created.len();

    // same_minute_burst: >= 5 distinct authors in one calendar minute, median followers < 500.
    let mut minutes: BTreeMap<i64, BTreeMap<&str, i64>> = BTreeMap::new();
    for p in &posts_60m {
        minutes
            .entry(post_time_ms(p).div_euclid(MINUTE_MS))
            .or_default()
            .insert(p.author_hash.as_str(), p.author_followers);
    }
    let same_minute_burst = minutes.values().any(|authors| {
        let mut followers: Vec<i64> = authors.values().copied().collect();
        authors.len() >= 5 && median(&mut followers) < 500.0
    });

    Coordination {
        near_duplicate,
        fresh_accounts,
        same_minute_burst,
    }
}

fn clamp01(x: f64) -> f64 {
    if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) }
}

/// Reply-quality multiplier: `0.5 + organic - bot` in [0.5, 1.2]; 1.0 without a reply read.
pub fn quality_factor(reply_quality: Option<&ReplyQuality>) -> f64 {
    reply_quality.map_or(1.0, |q| {
        let value = 0.5 + q.organic_share - q.bot_share;
        if value.is_nan() {
            1.0
        } else {
            value.clamp(0.5, 1.2)
        }
    })
}

/// Traction score `x-traction-v1` (0-100).
pub fn traction_score(
    features: &Features,
    coord: Coordination,
    reply_quality: Option<&ReplyQuality>,
) -> f64 {
    let social = 20.0 * clamp01(features.acceleration.log2() / 3.0)
        + 20.0 * clamp01((1.0 + features.unique_authors_15m as f64).log2() / 5.0)
        + 15.0 * clamp01(features.curated_hits_60m as f64 / 3.0)
        + 25.0 * clamp01((1.0 + features.engagement_velocity_15m.max(0.0)).log10() / 3.0)
        + 10.0 * features.follower_spread_60m as f64 / 4.0;
    let coord_factor = match coord.count() {
        0 => 1.0,
        1 => 0.8,
        _ => 0.6,
    };
    let market = match (features.volume_5m_usd, features.volume_1h_usd) {
        (Some(v5), Some(v1h)) if v1h > 0.0 => 10.0 * clamp01((v5 / (v1h / 12.0) - 1.0) / 2.0),
        _ => 0.0,
    };
    (social * quality_factor(reply_quality) * coord_factor + market).min(100.0)
}

/// Tier from traction alone (no hysteresis).
pub fn tier_for(traction: f64) -> Option<Tier> {
    if traction >= HOT_THRESHOLD {
        Some(Tier::Hot)
    } else if traction >= FOCUS_THRESHOLD {
        Some(Tier::Focus)
    } else if traction >= WATCH_THRESHOLD {
        Some(Tier::Watch)
    } else {
        None
    }
}

pub const fn lower_threshold(tier: Tier) -> f64 {
    match tier {
        Tier::Watch => WATCH_THRESHOLD,
        Tier::Focus => FOCUS_THRESHOLD,
        Tier::Hot => HOT_THRESHOLD,
    }
}

const fn one_below(tier: Tier) -> Tier {
    match tier {
        Tier::Hot => Tier::Focus,
        Tier::Focus | Tier::Watch => Tier::Watch,
    }
}

/// Tier plus the hysteresis clock.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TierState {
    pub tier: Option<Tier>,
    /// First cycle of the current run below `lower_threshold - 10` (focus/hot only).
    pub below_since_ms: Option<i64>,
}

/// Next tier: upgrades immediately; focus/hot drop one tier only after 10
/// consecutive minutes below their lower threshold minus 10 points.
pub fn next_tier(prev: TierState, traction: f64, now_ms: i64) -> TierState {
    let raw = tier_for(traction);
    match prev.tier {
        Some(current @ (Tier::Focus | Tier::Hot)) => {
            if raw >= Some(current) {
                return TierState {
                    tier: raw,
                    below_since_ms: None,
                };
            }
            if traction >= lower_threshold(current) - HYSTERESIS_POINTS {
                return TierState {
                    tier: Some(current),
                    below_since_ms: None,
                };
            }
            let since = prev.below_since_ms.unwrap_or(now_ms);
            if now_ms - since >= HYSTERESIS_MS {
                TierState {
                    tier: Some(one_below(current)),
                    below_since_ms: None,
                }
            } else {
                TierState {
                    tier: Some(current),
                    below_since_ms: Some(since),
                }
            }
        }
        _ => TierState {
            tier: raw,
            below_since_ms: None,
        },
    }
}

/// Keep at most [`MAX_FOCUS`] focus/hot entries (highest traction wins); the
/// rest drop to `watch` (or nothing below 20). Returns the demoted indices.
pub fn apply_focus_cap(entries: &mut [(f64, TierState)]) -> Vec<usize> {
    let mut focused: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, (_, s))| s.tier >= Some(Tier::Focus))
        .map(|(i, _)| i)
        .collect();
    focused.sort_by(|a, b| entries[*b].0.total_cmp(&entries[*a].0).then(a.cmp(b)));
    let demoted: Vec<usize> = focused.into_iter().skip(MAX_FOCUS).collect();
    for &i in &demoted {
        let traction = entries[i].0;
        entries[i].1 = TierState {
            tier: tier_for(traction).min(Some(Tier::Watch)),
            below_since_ms: None,
        };
    }
    demoted
}

/// One past cycle of a token's mention velocity (for `velocity_rollover`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MentionSample {
    pub ts_ms: i64,
    pub mentions_5m: i64,
    pub acceleration: f64,
}

/// Keep the last 2h of samples and append `sample`.
pub fn push_mention_sample(history: &mut Vec<MentionSample>, sample: MentionSample) {
    history.retain(|s| sample.ts_ms - s.ts_ms <= MENTION_HISTORY_MS && s.ts_ms < sample.ts_ms);
    history.push(sample);
}

/// Likes+reposts and views growth of a token's posts over the last 15 min and
/// the 15 min before, with counters interpolated between observations.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EngagementGrowth {
    pub likes_reposts_recent: f64,
    pub likes_reposts_prev: f64,
    pub views_recent: Option<f64>,
    pub views_prev: Option<f64>,
}

/// Counter value at `t` on a piecewise-linear series starting at 0 at creation;
/// flat after the last observation (no extrapolation).
fn value_at(series: &[(i64, f64)], t: i64) -> f64 {
    let Some(first) = series.first() else {
        return 0.0;
    };
    if t <= first.0 {
        return first.1;
    }
    for pair in series.windows(2) {
        let ((t0, v0), (t1, v1)) = (pair[0], pair[1]);
        if t <= t1 {
            if t1 == t0 {
                return v1;
            }
            return v0 + (v1 - v0) * (t - t0) as f64 / (t1 - t0) as f64;
        }
    }
    series.last().map_or(0.0, |(_, v)| *v)
}

pub fn engagement_growth(
    posts: &[&XPost],
    engagement: &EngagementIndex,
    now_ms: i64,
) -> EngagementGrowth {
    let (t0, t1, t2) = (now_ms - 30 * MINUTE_MS, now_ms - 15 * MINUTE_MS, now_ms);
    let mut out = EngagementGrowth::default();
    let (mut views_recent, mut views_prev, mut has_views) = (0.0, 0.0, false);
    for post in posts {
        let created = post_time_ms(post);
        let obs = observations(post, engagement, now_ms);
        let mut lr = vec![(created, 0.0)];
        lr.extend(obs.iter().map(|(t, m)| (*t, (m.likes + m.reposts) as f64)));
        out.likes_reposts_recent += value_at(&lr, t2) - value_at(&lr, t1);
        out.likes_reposts_prev += value_at(&lr, t1) - value_at(&lr, t0);
        let mut views = vec![(created, 0.0)];
        views.extend(
            obs.iter()
                .filter_map(|(t, m)| m.views.map(|v| (*t, v as f64))),
        );
        if views.len() > 1 {
            has_views = true;
            views_recent += value_at(&views, t2) - value_at(&views, t1);
            views_prev += value_at(&views, t1) - value_at(&views, t0);
        }
    }
    if has_views {
        out.views_recent = Some(views_recent);
        out.views_prev = Some(views_prev);
    }
    out
}

/// Share of 30-min posts from non-curated authors with >= 100k followers.
pub fn large_account_share_30m(posts: &[&XPost], now_ms: i64) -> Option<f64> {
    let recent: Vec<&&XPost> = posts
        .iter()
        .filter(|p| in_window(post_time_ms(p), now_ms, 30 * MINUTE_MS))
        .collect();
    if recent.is_empty() {
        return None;
    }
    let large = recent
        .iter()
        .filter(|p| !p.curated && p.author_followers >= 100_000)
        .count();
    Some(large as f64 / recent.len() as f64)
}

/// Latest Haiku read of a token, as used by the exhaustion flags.
#[derive(Debug, Clone, Copy, Default)]
pub struct HaikuMemory<'a> {
    pub stage: Option<TractionStage>,
    pub reply_quality: Option<&'a ReplyQuality>,
    pub flags: &'a [HaikuExhaustionFlag],
}

/// Inputs of the exhaustion score at one cycle.
#[derive(Debug, Clone, Copy)]
pub struct ExhaustionInputs<'a> {
    pub features: &'a Features,
    /// Earlier mention samples (last 2h, before now).
    pub mention_history: &'a [MentionSample],
    pub growth: EngagementGrowth,
    pub large_account_share_30m: Option<f64>,
    pub volume_5m_mean_15m_usd: Option<f64>,
    pub haiku: HaikuMemory<'a>,
}

/// Exhaustion score `x-exhaustion-v1`: sum of the weights of the true flags.
pub fn exhaustion(inputs: &ExhaustionInputs<'_>) -> (f64, Vec<ExhaustionFlag>) {
    let f = inputs.features;
    let mut flags = Vec::new();

    let current = MentionSample {
        ts_ms: 0,
        mentions_5m: f.mentions_5m,
        acceleration: f.acceleration,
    };
    let peak = inputs
        .mention_history
        .iter()
        .chain(std::iter::once(&current))
        .max_by(|a, b| {
            a.mentions_5m
                .cmp(&b.mentions_5m)
                .then(a.acceleration.total_cmp(&b.acceleration))
        });
    if let Some(peak) = peak
        && (f.mentions_5m as f64) < 0.6 * peak.mentions_5m as f64
        && peak.acceleration >= 3.0
    {
        flags.push(ExhaustionFlag::VelocityRollover);
    }

    let g = inputs.growth;
    if let (Some(views_recent), Some(views_prev)) = (g.views_recent, g.views_prev)
        && g.likes_reposts_prev > 0.0
        && views_prev > 0.0
        && g.likes_reposts_recent < 0.25 * g.likes_reposts_prev
        && views_recent >= 0.5 * views_prev
    {
        flags.push(ExhaustionFlag::EngagementSaturation);
    }

    if inputs
        .haiku
        .reply_quality
        .is_some_and(|q| q.warning_share >= 0.2 || q.bot_share >= 0.5)
    {
        flags.push(ExhaustionFlag::ReplyShift);
    }

    let hype = inputs.haiku.flags.iter().any(|flag| {
        matches!(
            flag,
            HaikuExhaustionFlag::LateStageHype
                | HaikuExhaustionFlag::PriceTargetSpam
                | HaikuExhaustionFlag::GiveawaySpam
        )
    });
    if hype || inputs.large_account_share_30m.is_some_and(|s| s >= 0.3) {
        flags.push(ExhaustionFlag::LateSpread);
    }

    if let (Some(change), Some(v5), Some(mean)) = (
        f.price_change_15m,
        f.volume_5m_usd,
        inputs.volume_5m_mean_15m_usd,
    ) && change >= 30.0
        && v5 < mean
    {
        flags.push(ExhaustionFlag::PriceVerticalVolumeWeak);
    }

    if matches!(
        inputs.haiku.stage,
        Some(TractionStage::Peak | TractionStage::Fading)
    ) {
        flags.push(ExhaustionFlag::StagePeakOrFading);
    }

    let score: f64 = flags.iter().map(|flag| flag.weight()).sum();
    ((score * 10_000.0).round() / 10_000.0, flags)
}

/// At least 2 distinct repliers whose text matches the rug-warning regex.
pub fn rug_reply_warnings<'a>(replies: impl IntoIterator<Item = &'a XReply>) -> bool {
    replies
        .into_iter()
        .filter(|r| RUG_RE.is_match(&r.excerpt.replace('\u{2019}', "'")))
        .map(|r| r.author_hash.as_str())
        .collect::<BTreeSet<_>>()
        .len()
        >= 2
}

/// Rug precursors of a token. `max_liquidity_since_signal` is set only for
/// tokens with a signal; `market` is `None` when the fetch failed (no verdict).
pub fn rug_precursors(
    venue: Venue,
    market: Option<&MarketSnapshot>,
    max_liquidity_since_signal: Option<f64>,
    reply_warnings: bool,
) -> Vec<RugPrecursor> {
    let mut out = Vec::new();
    if let Some(m) = market {
        match venue {
            Venue::Solana => {
                if !m.pair_exists {
                    out.push(RugPrecursor::PairGone);
                } else if let (Some(max), Some(now)) = (max_liquidity_since_signal, m.liquidity_usd)
                    && max > 0.0
                    && now <= 0.7 * max
                {
                    out.push(RugPrecursor::LiquidityDrop);
                }
            }
            Venue::Binance => {
                if m.symbol_status.as_deref().is_some_and(|s| s != "TRADING") {
                    out.push(RugPrecursor::SymbolNotTrading);
                }
            }
        }
    }
    if reply_warnings {
        out.push(RugPrecursor::ReplyRugWarnings);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_791_900_000_000;

    fn post(id: &str, author: &str, age_min: i64) -> XPost {
        XPost {
            post_id: id.into(),
            author_hash: author.into(),
            author_followers: 5_000,
            created_at_ms: NOW - age_min * MINUTE_MS,
            first_seen_ms: NOW - age_min * MINUTE_MS + 1_000,
            ..XPost::default()
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn traction_matches_hand_computed_example() {
        let features = Features {
            acceleration: 2.0,
            unique_authors_15m: 3,
            curated_hits_60m: 1,
            engagement_velocity_15m: 9.0,
            follower_spread_60m: 2,
            volume_5m_usd: Some(200.0),
            volume_1h_usd: Some(1_200.0),
            ..Features::default()
        };
        // social = 20*(1/3) + 20*(2/5) + 15*(1/3) + 25*(1/3) + 10*(2/4) = 33.0
        // quality = 0.5 + 0.5 - 0.2 = 0.8; coord = 1.0; market = 10 * ((200/100 - 1)/2) = 5
        let rq = ReplyQuality {
            organic_share: 0.5,
            bot_share: 0.2,
            ..ReplyQuality::default()
        };
        let t = traction_score(&features, Coordination::default(), Some(&rq));
        assert!(close(t, 33.0 * 0.8 + 5.0), "{t}");
        // No reply read -> quality 1.0; one coordination flag -> 0.8; two -> 0.6.
        let one = Coordination {
            near_duplicate: true,
            ..Coordination::default()
        };
        assert!(close(
            traction_score(&features, one, None),
            33.0 * 0.8 + 5.0
        ));
        let two = Coordination {
            near_duplicate: true,
            fresh_accounts: true,
            ..Coordination::default()
        };
        assert!(close(
            traction_score(&features, two, None),
            33.0 * 0.6 + 5.0
        ));
        // Missing market data contributes 0; quality is clamped to [0.5, 1.2].
        let no_market = Features {
            volume_5m_usd: None,
            ..features.clone()
        };
        let bad = ReplyQuality {
            organic_share: 0.0,
            bot_share: 0.9,
            ..ReplyQuality::default()
        };
        assert!(close(
            traction_score(&no_market, Coordination::default(), Some(&bad)),
            33.0 * 0.5
        ));
    }

    #[test]
    fn features_count_windows_and_acceleration() {
        let mut posts = [
            post("1", "a", 1),
            post("2", "b", 4),
            post("3", "a", 10),
            post("4", "c", 59),
            post("5", "d", 61),
        ];
        posts[0].curated = true;
        posts[1].author_followers = 150_000;
        let refs: Vec<&XPost> = posts.iter().collect();
        let f = social_features(&refs, &EngagementIndex::new(), NOW);
        assert_eq!(f.mentions_5m, 2);
        assert_eq!(f.mentions_60m, 4);
        assert!(close(f.acceleration, 3.0 / (2.0 / 11.0 + 1.0)));
        assert_eq!(f.unique_authors_15m, 2);
        assert_eq!(f.unique_authors_60m, 3);
        assert_eq!(f.curated_hits_60m, 1);
        assert_eq!(f.follower_spread_60m, 2);
    }

    #[test]
    fn velocity_uses_consecutive_reads_and_low_reach_weight() {
        let mut fresh = post("1", "a", 4);
        fresh.metrics = Metrics {
            likes: 30,
            ..Metrics::default()
        };
        // first seen 3 min after creation -> 30 / 3 per minute.
        fresh.first_seen_ms = fresh.created_at_ms + 3 * MINUTE_MS;
        let mut reread = post("2", "b", 100);
        reread.author_followers = 50; // low reach -> 0.25
        let mut idx = EngagementIndex::new();
        idx.insert(
            "2".into(),
            vec![
                (
                    NOW - 20 * MINUTE_MS,
                    Metrics {
                        replies: 1,
                        ..Metrics::default()
                    },
                ),
                (
                    NOW - 10 * MINUTE_MS,
                    Metrics {
                        replies: 11,
                        ..Metrics::default()
                    },
                ),
            ],
        );
        let v = engagement_velocity_15m(&[&fresh, &reread], &idx, NOW);
        assert!(close(v, 10.0 + 0.25 * (30.0 / 10.0)), "{v}");
    }

    #[test]
    fn tier_thresholds_and_hysteresis_boundaries() {
        assert_eq!(tier_for(19.999), None);
        assert_eq!(tier_for(20.0), Some(Tier::Watch));
        assert_eq!(tier_for(40.0), Some(Tier::Focus));
        assert_eq!(tier_for(60.0), Some(Tier::Hot));

        let hot = TierState {
            tier: Some(Tier::Hot),
            below_since_ms: None,
        };
        // 50 is exactly hot's floor (60 - 10): not below -> stays hot, clock reset.
        assert_eq!(next_tier(hot, 50.0, NOW), hot);
        // Below 50: clock starts; still hot until 10 full minutes elapsed.
        let s = next_tier(hot, 49.9, NOW);
        assert_eq!(s.tier, Some(Tier::Hot));
        assert_eq!(s.below_since_ms, Some(NOW));
        let s9 = next_tier(s, 10.0, NOW + 9 * MINUTE_MS + 59_999);
        assert_eq!(s9.tier, Some(Tier::Hot));
        let s10 = next_tier(s9, 10.0, NOW + 10 * MINUTE_MS);
        assert_eq!(s10.tier, Some(Tier::Focus));
        assert_eq!(s10.below_since_ms, None);
        // A recovery cycle resets the clock.
        let reset = next_tier(
            next_tier(s, 55.0, NOW + 5 * MINUTE_MS),
            49.0,
            NOW + 11 * MINUTE_MS,
        );
        assert_eq!(reset.tier, Some(Tier::Hot));
        assert_eq!(reset.below_since_ms, Some(NOW + 11 * MINUTE_MS));
        // Focus floor 30; drop goes to watch; watch has no hysteresis.
        let focus = TierState {
            tier: Some(Tier::Focus),
            below_since_ms: Some(NOW - 10 * MINUTE_MS),
        };
        assert_eq!(next_tier(focus, 29.0, NOW).tier, Some(Tier::Watch));
        let watch = TierState {
            tier: Some(Tier::Watch),
            below_since_ms: None,
        };
        assert_eq!(next_tier(watch, 19.0, NOW).tier, None);
        // Upgrades are immediate.
        assert_eq!(next_tier(watch, 61.0, NOW).tier, Some(Tier::Hot));
    }

    #[test]
    fn focus_cap_keeps_top_five_by_traction() {
        let state = |tier| TierState {
            tier: Some(tier),
            below_since_ms: None,
        };
        let mut entries = vec![
            (45.0, state(Tier::Focus)),
            (70.0, state(Tier::Hot)),
            (41.0, state(Tier::Focus)),
            (25.0, state(Tier::Watch)),
            (55.0, state(Tier::Focus)),
            (62.0, state(Tier::Hot)),
            (48.0, state(Tier::Focus)),
            (15.0, state(Tier::Focus)), // held by hysteresis, lowest
        ];
        let demoted = apply_focus_cap(&mut entries);
        assert_eq!(demoted, vec![2, 7]);
        assert_eq!(entries[2].1.tier, Some(Tier::Watch));
        assert_eq!(entries[7].1.tier, None);
        let focused = entries
            .iter()
            .filter(|(_, s)| s.tier >= Some(Tier::Focus))
            .count();
        assert_eq!(focused, MAX_FOCUS);
    }

    #[test]
    fn coordination_flags_at_boundaries() {
        let mut posts: Vec<XPost> = (0..3)
            .map(|i| {
                let mut p = post(&i.to_string(), &format!("a{i}"), 2);
                p.excerpt = format!("Buy $PEPE now going to the moon https://x.co/{i} fast");
                p
            })
            .collect();
        let refs: Vec<&XPost> = posts.iter().collect();
        assert!(coordination(&refs, NOW).near_duplicate);
        // Only two distinct authors -> no flag.
        posts[2].author_hash = "a0".into();
        let refs: Vec<&XPost> = posts.iter().collect();
        assert!(!coordination(&refs, NOW).near_duplicate);

        // fresh_accounts: exactly 50% fresh -> true; below -> false.
        let mut a = post("x", "fresh", 1);
        a.author_created_at_ms = Some(NOW - 29 * DAY_MS);
        let mut b = post("y", "old", 1);
        b.author_created_at_ms = Some(NOW - 31 * DAY_MS);
        let c = post("z", "unknown", 1);
        assert!(coordination(&[&a, &b], NOW).fresh_accounts);
        assert!(!coordination(&[&a, &b, &c], NOW).fresh_accounts);

        // same_minute_burst: 5 authors in one minute with median followers < 500.
        let minute = (NOW - 3 * MINUTE_MS).div_euclid(MINUTE_MS) * MINUTE_MS;
        let mut burst: Vec<XPost> = (0..5)
            .map(|i| {
                let mut p = post(&format!("b{i}"), &format!("u{i}"), 3);
                p.created_at_ms = minute + i * 1_000;
                p.first_seen_ms = minute + 50_000;
                p.author_followers = [10, 20, 499, 10_000, 20_000][i as usize];
                p
            })
            .collect();
        let refs: Vec<&XPost> = burst.iter().collect();
        assert!(coordination(&refs, NOW).same_minute_burst);
        burst[2].author_followers = 500;
        let refs: Vec<&XPost> = burst.iter().collect();
        assert!(!coordination(&refs, NOW).same_minute_burst);
        let refs: Vec<&XPost> = burst.iter().take(4).collect();
        assert!(!coordination(&refs, NOW).same_minute_burst);
    }

    fn base_inputs<'a>(
        features: &'a Features,
        history: &'a [MentionSample],
    ) -> ExhaustionInputs<'a> {
        ExhaustionInputs {
            features,
            mention_history: history,
            growth: EngagementGrowth::default(),
            large_account_share_30m: None,
            volume_5m_mean_15m_usd: None,
            haiku: HaikuMemory::default(),
        }
    }

    #[test]
    fn exhaustion_flags_at_boundaries() {
        let sample = |m5, acc| MentionSample {
            ts_ms: NOW - 30 * MINUTE_MS,
            mentions_5m: m5,
            acceleration: acc,
        };
        // velocity_rollover: 5 < 0.6 * 10 with acceleration 3 at the peak.
        let f = Features {
            mentions_5m: 5,
            ..Features::default()
        };
        let peak = [sample(10, 3.0)];
        assert_eq!(
            exhaustion(&base_inputs(&f, &peak)).1,
            vec![ExhaustionFlag::VelocityRollover]
        );
        // 6 is exactly 60% -> not below.
        let f6 = Features {
            mentions_5m: 6,
            ..Features::default()
        };
        assert!(exhaustion(&base_inputs(&f6, &peak)).1.is_empty());
        // Peak acceleration 2.99 -> no flag.
        assert!(
            exhaustion(&base_inputs(&f, &[sample(10, 2.99)]))
                .1
                .is_empty()
        );

        // engagement_saturation boundaries.
        let mut inputs = base_inputs(&f6, &[]);
        inputs.growth = EngagementGrowth {
            likes_reposts_recent: 24.9,
            likes_reposts_prev: 100.0,
            views_recent: Some(500.0),
            views_prev: Some(1_000.0),
        };
        assert_eq!(
            exhaustion(&inputs).1,
            vec![ExhaustionFlag::EngagementSaturation]
        );
        inputs.growth.likes_reposts_recent = 25.0;
        assert!(exhaustion(&inputs).1.is_empty());
        inputs.growth.likes_reposts_recent = 10.0;
        inputs.growth.views_recent = Some(499.0);
        assert!(exhaustion(&inputs).1.is_empty());

        // reply_shift, late_spread, stage flags.
        let rq = ReplyQuality {
            warning_share: 0.2,
            ..ReplyQuality::default()
        };
        let hype = [HaikuExhaustionFlag::GiveawaySpam];
        let mut inputs = base_inputs(&f6, &[]);
        inputs.haiku = HaikuMemory {
            stage: Some(TractionStage::Peak),
            reply_quality: Some(&rq),
            flags: &hype,
        };
        let (score, flags) = exhaustion(&inputs);
        assert_eq!(
            flags,
            vec![
                ExhaustionFlag::ReplyShift,
                ExhaustionFlag::LateSpread,
                ExhaustionFlag::StagePeakOrFading
            ]
        );
        assert!(close(score, 0.5));
        let rq_low = ReplyQuality {
            warning_share: 0.19,
            bot_share: 0.49,
            ..ReplyQuality::default()
        };
        let mut inputs = base_inputs(&f6, &[]);
        inputs.haiku.reply_quality = Some(&rq_low);
        inputs.haiku.stage = Some(TractionStage::Building);
        inputs.large_account_share_30m = Some(0.29);
        assert!(exhaustion(&inputs).1.is_empty());
        inputs.large_account_share_30m = Some(0.3);
        assert_eq!(exhaustion(&inputs).1, vec![ExhaustionFlag::LateSpread]);

        // price_vertical_volume_weak: +30% and latest 5m volume below the 15-min mean.
        let fp = Features {
            mentions_5m: 6,
            price_change_15m: Some(30.0),
            volume_5m_usd: Some(99.0),
            ..Features::default()
        };
        let mut inputs = base_inputs(&fp, &[]);
        inputs.volume_5m_mean_15m_usd = Some(100.0);
        assert_eq!(
            exhaustion(&inputs).1,
            vec![ExhaustionFlag::PriceVerticalVolumeWeak]
        );
        let fp2 = Features {
            price_change_15m: Some(29.99),
            ..fp.clone()
        };
        let mut inputs = base_inputs(&fp2, &[]);
        inputs.volume_5m_mean_15m_usd = Some(100.0);
        assert!(exhaustion(&inputs).1.is_empty());
    }

    #[test]
    fn engagement_growth_interpolates_between_reads() {
        let mut p = post("1", "a", 40);
        p.first_seen_ms = NOW - 30 * MINUTE_MS;
        p.created_at_ms = NOW - 30 * MINUTE_MS;
        p.metrics = Metrics {
            likes: 0,
            views: Some(0),
            ..Metrics::default()
        };
        let mut idx = EngagementIndex::new();
        idx.insert(
            "1".into(),
            vec![
                (
                    NOW - 15 * MINUTE_MS,
                    Metrics {
                        likes: 100,
                        views: Some(1_000),
                        ..Metrics::default()
                    },
                ),
                (
                    NOW,
                    Metrics {
                        likes: 110,
                        views: Some(1_800),
                        ..Metrics::default()
                    },
                ),
            ],
        );
        let g = engagement_growth(&[&p], &idx, NOW);
        assert!(close(g.likes_reposts_prev, 100.0) && close(g.likes_reposts_recent, 10.0));
        assert_eq!(g.views_prev, Some(1_000.0));
        assert_eq!(g.views_recent, Some(800.0));
    }

    fn reply(author: &str, text: &str) -> XReply {
        XReply {
            author_hash: author.into(),
            excerpt: text.into(),
            ..XReply::default()
        }
    }

    #[test]
    fn rug_reply_regex_needs_two_distinct_repliers() {
        let one = [reply("a", "I CANT SELL this"), reply("a", "honeypot!!")];
        assert!(!rug_reply_warnings(&one));
        let two = [reply("a", "can't sell"), reply("b", "LP removed, dev sold")];
        assert!(rug_reply_warnings(&two));
        let curly = [reply("a", "I can\u{2019}t sell"), reply("b", "rugged")];
        assert!(rug_reply_warnings(&curly));
        let clean = [reply("a", "great project"), reply("b", "lp locked")];
        assert!(!rug_reply_warnings(&clean));
    }

    #[test]
    fn rug_precursors_by_venue() {
        let gone = MarketSnapshot::default();
        assert_eq!(
            rug_precursors(Venue::Solana, Some(&gone), None, false),
            vec![RugPrecursor::PairGone]
        );
        let liq = |l| MarketSnapshot {
            pair_exists: true,
            liquidity_usd: Some(l),
            ..MarketSnapshot::default()
        };
        assert_eq!(
            rug_precursors(Venue::Solana, Some(&liq(7_000.0)), Some(10_000.0), false),
            vec![RugPrecursor::LiquidityDrop]
        );
        assert!(
            rug_precursors(Venue::Solana, Some(&liq(7_001.0)), Some(10_000.0), false).is_empty()
        );
        let halted = MarketSnapshot {
            symbol_status: Some("BREAK".into()),
            ..MarketSnapshot::default()
        };
        assert_eq!(
            rug_precursors(Venue::Binance, Some(&halted), None, true),
            vec![
                RugPrecursor::SymbolNotTrading,
                RugPrecursor::ReplyRugWarnings
            ]
        );
        assert!(rug_precursors(Venue::Solana, None, None, false).is_empty());
    }
}
