//! Phase 43 X pre-pump signal insighter, read-only (`GET /api/signals`). Paper only.
//!
//! Built from the files the Camoufox collector, `bb x-insighter` and its report write in
//! the data dir (`docs/signals/x-insighter-design.md`): collector health, the engine's focus
//! list, 6h traction/exhaustion curves of focus/hot tokens, the latest Haiku take per token,
//! the newest signals and the weekly report. A missing file is an empty section with a
//! message, never an error.
//!
//! The JSONL files grow without bound, so each is read through a [`JsonlTail`]: a cold read
//! parses at most the last `max_bytes`, later reads parse only the bytes appended since (the
//! History tab's size+mtime reuse, made incremental), and the folded state keeps only what
//! the view needs.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

use bb_signals::x_insighter::schema::{
    CollectorStatus, ExhaustionFlag, RugPrecursor, Tier, TractionStage, Venue, XFocus, XHaikuCall,
    XHeartbeat, XPaths, XSignal, XTraction,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// A heartbeat older than this means the collector is not running.
pub const STALE_HEARTBEAT_MS: i64 = 5 * 60_000;
/// Traction curves cover this much history.
pub const CURVE_WINDOW_MS: i64 = 6 * 3_600_000;
/// Newest signals sent to the page.
pub const SIGNAL_LIMIT: usize = 50;
/// Most token curves sent to the page (at most 5 tokens are focus/hot at once).
pub const TOKEN_LIMIT: usize = 12;
/// Largest tail parsed on a cold read of `x-traction.jsonl` (up to 50 tracked tokens per
/// minute, so 6h stays well inside it).
pub const TRACTION_TAIL_BYTES: u64 = 32 << 20;
/// Largest tail parsed on a cold read of `x-haiku.jsonl` and `x-signals.jsonl`.
pub const SPARSE_TAIL_BYTES: u64 = 8 << 20;

const REPORT_FILE: &str = "x-insighter-report-latest.json";
const NO_DATA: &str = "collector not running / no data yet";

/// Folds parsed JSONL rows into the state a view needs.
pub trait TailFold: Default {
    type Row: DeserializeOwned;
    fn push(&mut self, row: Self::Row);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReadMark {
    len: u64,
    modified: Option<SystemTime>,
    /// Bytes consumed: the end of the last complete line read.
    offset: u64,
}

/// Incremental reader of an append-only JSONL file.
///
/// Unchanged size and mtime reuse the fold. Growth parses only the appended complete lines
/// (a trailing line without its newline waits for the next read). A shrunk file, or more new
/// bytes than `max_bytes`, starts over from the file's last `max_bytes`, dropping the line
/// the cut lands in.
#[derive(Debug)]
pub struct JsonlTail<F> {
    max_bytes: u64,
    read: Option<ReadMark>,
    fold: F,
}

impl<F: TailFold> JsonlTail<F> {
    pub fn new(max_bytes: u64) -> Self {
        Self {
            max_bytes: max_bytes.max(1),
            read: None,
            fold: F::default(),
        }
    }

    /// Bring the fold up to date with `path` (a missing or unreadable file is empty).
    pub fn refresh(&mut self, path: &Path) -> &mut F {
        let Ok(meta) = std::fs::metadata(path) else {
            self.reset();
            return &mut self.fold;
        };
        let len = meta.len();
        let modified = meta.modified().ok();
        if self
            .read
            .is_some_and(|mark| mark.len == len && mark.modified == modified)
        {
            return &mut self.fold;
        }
        let resume = self
            .read
            .filter(|mark| len >= mark.len && len - mark.offset <= self.max_bytes)
            .map(|mark| mark.offset);
        let result = match resume {
            Some(offset) => self.read_lines(path, offset, len, false),
            None => {
                self.fold = F::default();
                let start = len.saturating_sub(self.max_bytes);
                self.read_lines(path, start, len, start > 0)
            }
        };
        match result {
            Ok(offset) => {
                self.read = Some(ReadMark {
                    len,
                    modified,
                    offset,
                })
            }
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "jsonl tail read failed");
                self.reset();
            }
        }
        &mut self.fold
    }

    fn reset(&mut self) {
        self.read = None;
        self.fold = F::default();
    }

    /// Parse the complete lines in `start..len`; returns the end of the last one. With `cut`,
    /// the line `start` lands in is skipped (reading from one byte earlier keeps a line that
    /// starts exactly at `start`).
    fn read_lines(&mut self, path: &Path, start: u64, len: u64, cut: bool) -> std::io::Result<u64> {
        let from = if cut { start.saturating_sub(1) } else { start };
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(from))?;
        let mut reader = BufReader::new(file.take(len.saturating_sub(from)));
        let mut offset = from;
        let mut line = Vec::new();
        if cut {
            let n = reader.read_until(b'\n', &mut line)?;
            if line.last() != Some(&b'\n') {
                return Ok(from);
            }
            offset += n as u64;
        }
        loop {
            line.clear();
            let n = reader.read_until(b'\n', &mut line)?;
            if n == 0 || line.last() != Some(&b'\n') {
                return Ok(offset);
            }
            offset += n as u64;
            if let Ok(row) = serde_json::from_slice::<F::Row>(&line) {
                self.fold.push(row);
            }
        }
    }
}

/// One `x-traction` row reduced to what the curves show.
#[derive(Debug, Clone, PartialEq)]
pub struct TractionPoint {
    pub ts_ms: i64,
    pub tier: Tier,
    pub traction: f64,
    pub exhaustion: f64,
    pub exhaustion_flags: Vec<ExhaustionFlag>,
    pub rug_precursors: Vec<RugPrecursor>,
    pub stage: Option<TractionStage>,
}

#[derive(Debug, Clone, Default)]
pub struct TokenSeries {
    pub venue: Venue,
    pub symbol: Option<String>,
    pub cashtag: Option<String>,
    /// Oldest first, at most [`CURVE_WINDOW_MS`] behind the token's newest row.
    pub points: VecDeque<TractionPoint>,
}

/// Traction rows per token.
#[derive(Debug, Default)]
pub struct TractionFold {
    pub tokens: HashMap<String, TokenSeries>,
}

impl TailFold for TractionFold {
    type Row = XTraction;

    fn push(&mut self, row: XTraction) {
        let series = self.tokens.entry(row.token_key).or_default();
        series.venue = row.venue;
        if row.symbol.is_some() {
            series.symbol = row.symbol;
        }
        if row.cashtag.is_some() {
            series.cashtag = row.cashtag;
        }
        let cutoff = row.ts_ms - CURVE_WINDOW_MS;
        while series.points.front().is_some_and(|p| p.ts_ms < cutoff) {
            series.points.pop_front();
        }
        series.points.push_back(TractionPoint {
            ts_ms: row.ts_ms,
            tier: row.tier,
            traction: row.traction,
            exhaustion: row.exhaustion,
            exhaustion_flags: row.exhaustion_flags,
            rug_precursors: row.rug_precursors,
            stage: row.stage,
        });
    }
}

impl TractionFold {
    /// Drop points older than `cutoff_ms` and tokens left without points.
    pub fn prune(&mut self, cutoff_ms: i64) {
        self.tokens.retain(|_, series| {
            while series.points.front().is_some_and(|p| p.ts_ms < cutoff_ms) {
                series.points.pop_front();
            }
            !series.points.is_empty()
        });
    }
}

/// Latest Haiku call and latest verdict per token.
#[derive(Debug, Default)]
pub struct HaikuFold {
    pub last_call: HashMap<String, XHaikuCall>,
    pub last_take: HashMap<String, XHaikuCall>,
}

impl TailFold for HaikuFold {
    type Row = XHaikuCall;

    fn push(&mut self, row: XHaikuCall) {
        if row.verdict.is_some() {
            self.last_take.insert(row.token_key.clone(), row.clone());
        }
        self.last_call.insert(row.token_key.clone(), row);
    }
}

/// The newest [`SIGNAL_LIMIT`] signals, oldest first.
#[derive(Debug, Default)]
pub struct SignalFold {
    pub rows: VecDeque<XSignal>,
}

impl TailFold for SignalFold {
    type Row = XSignal;

    fn push(&mut self, row: XSignal) {
        if self.rows.len() == SIGNAL_LIMIT {
            self.rows.pop_front();
        }
        self.rows.push_back(row);
    }
}

/// Incremental readers of the three JSONL files the view uses.
#[derive(Debug)]
pub struct SignalsCache {
    pub traction: JsonlTail<TractionFold>,
    pub haiku: JsonlTail<HaikuFold>,
    pub signals: JsonlTail<SignalFold>,
}

impl Default for SignalsCache {
    fn default() -> Self {
        Self {
            traction: JsonlTail::new(TRACTION_TAIL_BYTES),
            haiku: JsonlTail::new(SPARSE_TAIL_BYTES),
            signals: JsonlTail::new(SPARSE_TAIL_BYTES),
        }
    }
}

/// The Signals tab payload at `now_ms`.
pub fn signals_view(cache: &mut SignalsCache, paths: &XPaths, now_ms: i64) -> Value {
    let (focus, focus_view) = focus_view(&paths.focus, now_ms);
    let traction = cache.traction.refresh(&paths.traction);
    traction.prune(now_ms - CURVE_WINDOW_MS);
    let haiku = cache.haiku.refresh(&paths.haiku);
    let tokens = token_views(traction, haiku, focus.as_ref());
    let signals: Vec<Value> = cache
        .signals
        .refresh(&paths.signals)
        .rows
        .iter()
        .rev()
        .map(signal_view)
        .collect();
    let (report, report_error) = match read_json::<Value>(&paths.report_dir.join(REPORT_FILE)) {
        Ok(Some(report)) if report.is_object() => (report, None),
        Ok(Some(_)) => (Value::Null, Some("report is not a JSON object".to_owned())),
        Ok(None) => (Value::Null, None),
        Err(error) => (Value::Null, Some(error)),
    };
    let tokens_message = tokens.is_empty().then(|| {
        if paths.traction.exists() {
            "no focus or hot token in the last 6h".to_owned()
        } else {
            format!("{NO_DATA}: {} not found", paths.traction.display())
        }
    });
    json!({
        "generated_at_ms": now_ms,
        "paper_only": true,
        "collector": collector_view(&paths.heartbeat, now_ms),
        "focus": focus_view,
        "tokens": tokens,
        "tokens_message": tokens_message,
        "signals": signals,
        "signals_message": signals.is_empty().then(|| no_data_message(&paths.signals, "no signal yet")),
        "report": report,
        "report_error": report_error,
        "report_message": report.is_null().then(|| no_data_message(&paths.report_dir.join(REPORT_FILE), "no weekly report yet")),
    })
}

fn no_data_message(path: &Path, present: &str) -> String {
    if path.exists() {
        present.to_owned()
    } else {
        format!("{NO_DATA}: {} not found", path.display())
    }
}

/// `Ok(None)` for a missing file; a torn or invalid file is an error message.
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| format!("{} unreadable: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{} unreadable: {error}", path.display())),
    }
}

/// Collector health: healthy only when the heartbeat is at most [`STALE_HEARTBEAT_MS`] old
/// and its status is `ok`.
pub fn collector_view(path: &Path, now_ms: i64) -> Value {
    let beat = match read_json::<XHeartbeat>(path) {
        Ok(Some(beat)) => beat,
        Ok(None) => {
            return json!({
                "present": false, "healthy": false, "stale": true,
                "message": format!("{NO_DATA}: {} not found", path.display()),
            });
        }
        Err(error) => {
            return json!({"present": true, "healthy": false, "stale": true, "error": error});
        }
    };
    let age_ms = now_ms - beat.updated_at_ms;
    let stale = age_ms > STALE_HEARTBEAT_MS;
    let healthy = !stale && beat.status == CollectorStatus::Ok;
    json!({
        "present": true,
        "healthy": healthy,
        "stale": stale,
        "status": beat.status,
        "updated_at_ms": beat.updated_at_ms,
        "age_seconds": age_ms.max(0) / 1000,
        "pages_last_hour": beat.pages_last_hour,
        "posts_last_hour": beat.posts_last_hour,
        "replies_last_hour": beat.replies_last_hour,
        "last_error": beat.last_error,
        "backoff_until_ms": beat.backoff_until_ms,
        "backing_off": beat.backoff_until_ms.is_some_and(|until| until > now_ms),
        "camoufox_version": beat.camoufox_version,
        "browser_version": beat.browser_version,
        "message": stale.then(|| format!("heartbeat {} min old: collector not running", age_ms.max(0) / 60_000)),
    })
}

fn focus_view(path: &Path, now_ms: i64) -> (Option<XFocus>, Value) {
    match read_json::<XFocus>(path) {
        Ok(Some(focus)) => {
            let view = json!({
                "present": true,
                "generated_at_ms": focus.generated_at_ms,
                "age_seconds": (now_ms - focus.generated_at_ms).max(0) / 1000,
                "coins": focus.coins,
                "resample_posts": focus.resample_post_ids.len(),
                "reply_sample_posts": focus.reply_sample_post_ids.len(),
            });
            (Some(focus), view)
        }
        Ok(None) => (
            None,
            json!({"present": false, "coins": [], "message": format!("{NO_DATA}: {} not found", path.display())}),
        ),
        Err(error) => (None, json!({"present": true, "coins": [], "error": error})),
    }
}

/// Curves of tokens that are in the focus list or were focus/hot in the window: current
/// ones first, then by latest traction.
fn token_views(traction: &TractionFold, haiku: &HaikuFold, focus: Option<&XFocus>) -> Vec<Value> {
    let focused: HashMap<&str, Tier> = focus
        .map(|f| {
            f.coins
                .iter()
                .map(|c| (c.token_key.as_str(), c.tier))
                .collect()
        })
        .unwrap_or_default();
    let mut keys: HashSet<&str> = focused.keys().copied().collect();
    for (key, series) in &traction.tokens {
        if series.points.iter().any(|p| p.tier >= Tier::Focus) {
            keys.insert(key);
        }
    }
    let mut rows: Vec<(bool, f64, &str)> = keys
        .into_iter()
        .map(|key| {
            let last = traction.tokens.get(key).and_then(|s| s.points.back());
            let current = focused.contains_key(key) || last.is_some_and(|p| p.tier >= Tier::Focus);
            (current, last.map_or(f64::NEG_INFINITY, |p| p.traction), key)
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.total_cmp(&a.1)).then(a.2.cmp(b.2)));
    rows.into_iter()
        .take(TOKEN_LIMIT)
        .map(|(current, _, key)| {
            let coin = focus.and_then(|f| f.coins.iter().find(|c| c.token_key == key));
            token_view(
                key,
                current,
                traction.tokens.get(key),
                coin.map(|c| (c.tier, c.cashtag.as_deref())),
                haiku,
            )
        })
        .collect()
}

fn token_view(
    key: &str,
    current: bool,
    series: Option<&TokenSeries>,
    focus: Option<(Tier, Option<&str>)>,
    haiku: &HaikuFold,
) -> Value {
    let empty = VecDeque::new();
    let points = series.map_or(&empty, |s| &s.points);
    let last = points.back();
    let changes = |pick: fn(&TractionPoint) -> Option<Value>| {
        let mut out = Vec::new();
        let mut prev: Option<Value> = None;
        for point in points {
            let Some(value) = pick(point) else { continue };
            if let Some(before) = prev.as_ref().filter(|before| **before != value) {
                out.push(json!({"ts_ms": point.ts_ms, "from": before, "to": value}));
            }
            prev = Some(value);
        }
        out
    };
    let venue = series.map_or_else(
        || {
            if key.starts_with("solana:") {
                Venue::Solana
            } else {
                Venue::Binance
            }
        },
        |s| s.venue,
    );
    let cashtag = series
        .and_then(|s| s.cashtag.as_deref())
        .or(focus.and_then(|(_, tag)| tag));
    json!({
        "token_key": key,
        "venue": venue,
        "symbol": series.and_then(|s| s.symbol.as_deref()),
        "cashtag": cashtag,
        "current": current,
        "focus_tier": focus.map(|(tier, _)| tier),
        "tier": last.map(|p| p.tier),
        "traction": last.map(|p| p.traction),
        "exhaustion": last.map(|p| p.exhaustion),
        "exhaustion_flags": last.map_or(&[][..], |p| &p.exhaustion_flags),
        "rug_precursors": last.map_or(&[][..], |p| &p.rug_precursors),
        "stage": last.and_then(|p| p.stage),
        "updated_at_ms": last.map(|p| p.ts_ms),
        "points": points.iter().map(|p| json!({
            "ts_ms": p.ts_ms, "traction": p.traction, "exhaustion": p.exhaustion, "tier": p.tier, "stage": p.stage,
        })).collect::<Vec<_>>(),
        "stage_changes": changes(|p| p.stage.map(|stage| json!(stage))),
        "tier_changes": changes(|p| Some(json!(p.tier))),
        "haiku": haiku.last_take.get(key),
        "haiku_last_call": haiku.last_call.get(key).map(|call| json!({
            "ts_ms": call.ts_ms, "trigger": call.trigger, "status": call.status, "error": call.error,
        })),
    })
}

fn signal_view(signal: &XSignal) -> Value {
    let market = |key: &str| signal.market.get(key).filter(|v| !v.is_null()).cloned();
    json!({
        "detection_id": signal.detection_id,
        "detected_at_ms": signal.detected_at_ms,
        "venue": signal.venue,
        "token_key": signal.token_key,
        "symbol": signal.symbol,
        "cashtag": signal.cashtag,
        "resolution": signal.resolution,
        "traction": signal.traction,
        "floor_passed": signal.floor_passed,
        "haiku_status": signal.haiku_status,
        "haiku_arm": signal.haiku_arm,
        "conviction": signal.haiku.as_ref().map(|v| v.conviction),
        "stage": signal.haiku.as_ref().map(|v| v.traction_stage),
        "price_usd": market("price_usd").or_else(|| signal.features.price_usd.map(Value::from)),
        "liquidity_usd": market("liquidity_usd"),
        "spread_bps": market("spread_bps"),
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const NOW: i64 = 1_791_900_000_000;
    const MIN: i64 = 60_000;

    fn traction_row(
        ts_ms: i64,
        key: &str,
        tier: &str,
        traction: f64,
        stage: Option<&str>,
    ) -> Value {
        json!({
            "schema": "x-traction-v1", "ts_ms": ts_ms, "token_key": key, "venue": "binance",
            "symbol": "PEPEUSDT", "cashtag": "PEPE", "tier": tier, "traction": traction,
            "features": {"mentions_5m": 3}, "coordination": {"near_duplicate": false},
            "exhaustion": 0.15, "exhaustion_flags": ["late_spread"], "rug_precursors": [],
            "stage": stage, "traction_version": "x-traction-v1", "exhaustion_version": "x-exhaustion-v1",
        })
    }

    fn append(path: &Path, text: &str) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn line(row: &Value) -> String {
        format!("{row}\n")
    }

    fn times(fold: &TractionFold, key: &str) -> Vec<i64> {
        fold.tokens
            .get(key)
            .map_or_else(Vec::new, |s| s.points.iter().map(|p| p.ts_ms).collect())
    }

    #[test]
    fn missing_files_give_empty_sections_with_messages() {
        let dir = tempfile::tempdir().unwrap();
        let paths = XPaths::in_dir(dir.path());

        let view = signals_view(&mut SignalsCache::default(), &paths, NOW);

        assert_eq!(view["collector"]["present"], json!(false));
        assert_eq!(view["collector"]["healthy"], json!(false));
        assert!(
            view["collector"]["message"]
                .as_str()
                .unwrap()
                .contains(NO_DATA)
        );
        assert_eq!(view["focus"]["coins"], json!([]));
        assert!(view["focus"]["message"].as_str().unwrap().contains(NO_DATA));
        assert_eq!(view["tokens"], json!([]));
        assert!(view["tokens_message"].as_str().unwrap().contains(NO_DATA));
        assert_eq!(view["signals"], json!([]));
        assert!(view["signals_message"].as_str().unwrap().contains(NO_DATA));
        assert_eq!(view["report"], Value::Null);
        assert_eq!(view["report_error"], Value::Null);
        assert!(view["report_message"].as_str().unwrap().contains(NO_DATA));
    }

    #[test]
    fn tail_reads_only_appended_complete_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x-traction.jsonl");
        let mut tail = JsonlTail::<TractionFold>::new(TRACTION_TAIL_BYTES);
        let key = "binance:PEPE";

        append(&path, &line(&traction_row(NOW, key, "watch", 21.0, None)));
        assert_eq!(times(tail.refresh(&path), key), [NOW]);

        // A row still being written (no newline yet) is not consumed.
        let next = line(&traction_row(
            NOW + MIN,
            key,
            "focus",
            45.0,
            Some("ignition"),
        ));
        let (head, rest) = next.split_at(40);
        append(&path, head);
        assert_eq!(times(tail.refresh(&path), key), [NOW]);

        append(&path, rest);
        append(
            &path,
            &line(&traction_row(
                NOW + 2 * MIN,
                key,
                "hot",
                62.0,
                Some("building"),
            )),
        );
        assert_eq!(
            times(tail.refresh(&path), key),
            [NOW, NOW + MIN, NOW + 2 * MIN]
        );
        // Unchanged file: same fold, no duplicates.
        assert_eq!(
            times(tail.refresh(&path), key),
            [NOW, NOW + MIN, NOW + 2 * MIN]
        );

        // A rewritten (shrunk) file starts over.
        std::fs::write(
            &path,
            line(&traction_row(NOW + 3 * MIN, key, "watch", 20.0, None)),
        )
        .unwrap();
        assert_eq!(times(tail.refresh(&path), key), [NOW + 3 * MIN]);

        std::fs::remove_file(&path).unwrap();
        assert!(tail.refresh(&path).tokens.is_empty());
    }

    #[test]
    fn cold_read_parses_only_the_tail_and_drops_the_cut_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x-traction.jsonl");
        let key = "binance:PEPE";
        let rows: Vec<String> = (0..4)
            .map(|i| line(&traction_row(NOW + i * MIN, key, "watch", 20.0, None)))
            .collect();
        std::fs::write(&path, rows.concat()).unwrap();

        // Cut inside row 2: rows 2 and 3 survive only partly/whole; row 2 is dropped.
        let budget = (rows[2].len() + rows[3].len() - 5) as u64;
        let mut tail = JsonlTail::<TractionFold>::new(budget);
        assert_eq!(times(tail.refresh(&path), key), [NOW + 3 * MIN]);

        // Cut exactly at the start of row 2 keeps it.
        let budget = (rows[2].len() + rows[3].len()) as u64;
        let mut tail = JsonlTail::<TractionFold>::new(budget);
        assert_eq!(
            times(tail.refresh(&path), key),
            [NOW + 2 * MIN, NOW + 3 * MIN]
        );

        // More appended bytes than the budget: re-read the tail instead of the whole gap.
        let more: Vec<String> = (4..8)
            .map(|i| line(&traction_row(NOW + i * MIN, key, "watch", 20.0, None)))
            .collect();
        append(&path, &more.concat());
        assert_eq!(
            times(tail.refresh(&path), key),
            [NOW + 6 * MIN, NOW + 7 * MIN]
        );
    }

    #[test]
    fn heartbeat_is_stale_after_five_minutes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x-collector-heartbeat.json");
        let beat = |age_ms: i64, status: &str| {
            std::fs::write(
                &path,
                json!({"schema": "x-heartbeat-v1", "updated_at_ms": NOW - age_ms, "status": status,
                       "pages_last_hour": 40, "posts_last_hour": 310, "replies_last_hour": 12,
                       "last_error": null, "backoff_until_ms": null})
                .to_string(),
            )
            .unwrap();
            collector_view(&path, NOW)
        };

        let fresh = beat(STALE_HEARTBEAT_MS, "ok");
        assert_eq!(
            (fresh["healthy"].clone(), fresh["stale"].clone()),
            (json!(true), json!(false))
        );
        assert_eq!(fresh["posts_last_hour"], json!(310));
        let stale = beat(STALE_HEARTBEAT_MS + 1, "ok");
        assert_eq!(
            (stale["healthy"].clone(), stale["stale"].clone()),
            (json!(false), json!(true))
        );
        assert_eq!(beat(0, "backoff")["healthy"], json!(false));

        std::fs::write(&path, "{\"schema\": \"x-heartb").unwrap();
        let torn = collector_view(&path, NOW);
        assert_eq!(torn["healthy"], json!(false));
        assert!(torn["error"].as_str().is_some());
    }

    #[test]
    fn view_marks_stage_changes_and_keeps_six_hours_of_focus_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let paths = XPaths::in_dir(dir.path());
        let pepe = "binance:PEPE";
        let mint = "solana:So11111111111111111111111111111111111111112";
        let rows = [
            traction_row(
                NOW - CURVE_WINDOW_MS - MIN,
                pepe,
                "hot",
                70.0,
                Some("ignition"),
            ),
            traction_row(NOW - 3 * MIN, pepe, "focus", 41.0, Some("ignition")),
            traction_row(NOW - 2 * MIN, pepe, "hot", 61.0, Some("ignition")),
            traction_row(NOW - MIN, pepe, "hot", 66.0, Some("building")),
            traction_row(NOW - MIN, "binance:DOGE", "watch", 25.0, None),
        ];
        std::fs::write(&paths.traction, rows.iter().map(line).collect::<String>()).unwrap();
        std::fs::write(
            &paths.focus,
            json!({"schema": "x-focus-v1", "generated_at_ms": NOW,
                   "coins": [{"token_key": mint, "tier": "focus", "cashtag": "WIF", "search_queries": ["$WIF"]}]})
            .to_string(),
        )
        .unwrap();
        let signals: String = (0..(SIGNAL_LIMIT as i64 + 5))
            .map(|i| {
                line(&json!({"schema": "x-signal-v1", "detection_id": format!("x-{i}"), "detected_at_ms": NOW - 1000 + i,
                             "venue": "binance", "token_key": pepe, "market": {"price_usd": 0.5}, "floor_passed": true}))
            })
            .collect();
        std::fs::write(&paths.signals, signals).unwrap();

        let view = signals_view(&mut SignalsCache::default(), &paths, NOW);

        let tokens = view["tokens"].as_array().unwrap();
        let keys: Vec<&str> = tokens
            .iter()
            .map(|t| t["token_key"].as_str().unwrap())
            .collect();
        assert_eq!(
            keys,
            [pepe, mint],
            "watch-only DOGE excluded; current tokens by traction"
        );
        assert_eq!(
            tokens[0]["points"].as_array().unwrap().len(),
            3,
            "row older than 6h dropped"
        );
        assert_eq!(
            tokens[0]["stage_changes"],
            json!([{"ts_ms": NOW - MIN, "from": "ignition", "to": "building"}])
        );
        assert_eq!(
            tokens[0]["tier_changes"],
            json!([{"ts_ms": NOW - 2 * MIN, "from": "focus", "to": "hot"}])
        );
        assert_eq!(
            (tokens[1]["venue"].clone(), tokens[1]["cashtag"].clone()),
            (json!("solana"), json!("WIF"))
        );
        assert_eq!(tokens[1]["points"], json!([]));

        let signals = view["signals"].as_array().unwrap();
        assert_eq!(signals.len(), SIGNAL_LIMIT);
        assert_eq!(
            signals[0]["detection_id"],
            json!(format!("x-{}", SIGNAL_LIMIT + 4))
        );
        assert_eq!(signals[0]["price_usd"], json!(0.5));
    }
}
