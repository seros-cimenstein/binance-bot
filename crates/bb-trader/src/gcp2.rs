//! GCP2 (gcp2.net, Global Consciousness Project 2.0) public data, read-only (Phase 49).
//!
//! History: the site's data-download form (no login, no captcha) returns one zipped CSV of
//! per-second network coherence per month. Live: the same public API the live page uses
//! (per-minute aggregates of the last 24 h, colour zones), with the bearer token the site
//! publishes in `/js/data/api_token.js`. Nothing here posts anything other than the
//! download form or logs in.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{Result, TraderError};

pub const DEFAULT_BASE_URL: &str = "https://gcp2.net";
/// The group whose network coherence the trader follows.
pub const GLOBAL_NETWORK: &str = "Global Network";
/// The colour zones file written next to the monthly CSVs.
pub const COLOR_ZONES_FILE: &str = "colorzones.json";
const DOWNLOAD_PAGE: &str = "/data-results/data-download";
const NETWORK_DOWNLOAD: &str = "/data-results/network-coherence-download";
const TOKEN_SCRIPT: &str = "/js/data/api_token.js";
const COLOR_ZONES_API: &str = "/api/getcolorzones";
const AGGREGATE_24H_API: &str = "/api/getNetVarAggregate24H";
const HTTP_TIMEOUT: Duration = Duration::from_secs(180);
/// Per-request timeout of the trader's live reads.
pub const LIVE_TIMEOUT: Duration = Duration::from_secs(10);
/// How far before the latest minute the live reading's earlier zone is taken.
const EARLIER_SECONDS: i64 = 20 * 60;
const MULTIPART_BOUNDARY: &str = "bbgcp2boundary7b3f";

fn http_error(what: &str, e: impl std::fmt::Display) -> TraderError {
    TraderError::config(format!("GCP2 {what}: {e}"))
}

/// The CSV name the site gives a monthly network-coherence download.
pub fn month_file_name(group: &str, year: i32, month: u32) -> String {
    format!(
        "GCP2_Network_Coherence_{}_{year}_{month:02}.csv",
        group.replace(' ', "_")
    )
}

/// The `_token` value of the download page's form.
fn csrf_token(html: &str) -> Option<&str> {
    let at = html.find("name=\"_token\"")?;
    let rest = &html[at..];
    let value = rest.find("value=\"")? + "value=\"".len();
    let end = rest[value..].find('"')?;
    Some(&rest[value..value + end])
}

/// `name=value` pairs of every `Set-Cookie` header, joined for a `Cookie` header.
fn cookie_header(headers: &reqwest::header::HeaderMap) -> String {
    headers
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

/// A `multipart/form-data` body of plain text fields.
fn multipart(fields: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (name, value) in fields {
        body.push_str(&format!(
            "--{MULTIPART_BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        ));
    }
    body.push_str(&format!("--{MULTIPART_BOUNDARY}--\r\n"));
    body
}

/// The single CSV inside a monthly download zip.
fn unzip_csv(bytes: &[u8]) -> Result<(String, Vec<u8>)> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| http_error("download is not a zip", e))?;
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| http_error("reading the zip", e))?;
        let full = file
            .name()
            .map_err(|e| http_error("reading a zip entry name", e))?
            .to_string();
        if full.ends_with(".csv") {
            let name = full.rsplit('/').next().unwrap_or_default().to_string();
            let mut out = Vec::with_capacity(usize::try_from(file.size()).unwrap_or(0));
            file.read_to_end(&mut out)?;
            return Ok((name, out));
        }
    }
    Err(TraderError::config("GCP2 download zip holds no CSV"))
}

/// The bearer token the site publishes for its live page (`const bearer = 'Bearer …'`).
pub fn parse_bearer(script: &str) -> Option<String> {
    let at = script.find("Bearer ")?;
    let rest = &script[at..];
    let end = rest.find(['\'', '"', '`'])?;
    Some(rest[..end].to_string())
}

/// One GCP2 colour zone: values in `[start, end)` are `label`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColorZone {
    pub label: String,
    pub start: f64,
    pub end: f64,
}

/// GCP2's colour zones as published, with when they were read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColorZones {
    pub fetched_at: String,
    pub zones: Vec<ColorZone>,
}

fn number(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str()?.trim().parse().ok())
}

/// Parse a `getcolorzones` reply; zones are sorted by start.
pub fn parse_color_zones(body: &Value, fetched_at: String) -> Result<ColorZones> {
    let list = body
        .get("colorzones")
        .and_then(Value::as_array)
        .ok_or_else(|| TraderError::config("GCP2 colour zones: no `colorzones` list"))?;
    let mut zones: Vec<ColorZone> = list
        .iter()
        .map(|z| {
            Some(ColorZone {
                label: z.get("label")?.as_str()?.to_string(),
                start: number(z.get("range_start")?)?,
                end: number(z.get("range_end")?)?,
            })
        })
        .collect::<Option<_>>()
        .ok_or_else(|| TraderError::config("GCP2 colour zones: a zone lacks label/range"))?;
    if zones.is_empty() {
        return Err(TraderError::config("GCP2 colour zones: empty list"));
    }
    zones.sort_by(|a, b| a.start.total_cmp(&b.start));
    Ok(ColorZones { fetched_at, zones })
}

impl ColorZones {
    /// Read `colorzones.json` from a cache directory (written by `bb gcp2-history`).
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(COLOR_ZONES_FILE);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            TraderError::config(format!("missing GCP2 colour zones {}: {e}", path.display()))
        })?;
        let zones: Self = serde_json::from_str(&text)?;
        if zones.zones.is_empty() {
            return Err(TraderError::config("GCP2 colour zones: empty list"));
        }
        Ok(zones)
    }

    /// Index of the zone holding `value`: the last zone whose start is at or below it
    /// (values under the first start fall in the first zone, values past the last end in
    /// the last one). Zones are sorted by start.
    pub fn index(&self, value: f64) -> usize {
        self.zones
            .iter()
            .rposition(|z| z.start <= value)
            .unwrap_or(0)
    }

    pub fn label(&self, index: usize) -> &str {
        self.zones
            .get(index)
            .map_or("unknown", |z| z.label.as_str())
    }
}

/// GCP2 publishes values to 4 decimals; readings are kept in these integer units so the
/// backtest and the live feed compute the same value (and the same dice) exactly.
pub const UNITS_PER_ONE: i64 = 10_000;
/// Seconds in GCP2's network-coherence aggregate window.
pub const AGGREGATE_SECONDS: i64 = 3_600;
/// An aggregate with fewer present seconds than this share is unavailable.
const MIN_COVERAGE: f64 = 0.9;

/// A 4-decimal GCP2 number (`"119.8923"`, `"-0.0827"`) in [`UNITS_PER_ONE`] units.
pub fn parse_units(text: &str) -> Option<i64> {
    let value: f64 = text.trim().parse().ok()?;
    value
        .is_finite()
        .then(|| (value * UNITS_PER_ONE as f64).round() as i64)
}

/// One GCP2 per-minute reading: the network-coherence aggregate of the hour ending at
/// second `end_s`, as the live page shows it (`getNetVarAggregate24H`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gcp2Reading {
    pub end_s: i64,
    /// `|Σ network_coherence|` over the hour, in [`UNITS_PER_ONE`] units.
    pub units: u64,
}

impl Gcp2Reading {
    pub fn value(&self) -> f64 {
        self.units as f64 / UNITS_PER_ONE as f64
    }

    /// The GCP2 dice for this minute (see [`dice`]).
    pub fn dice(&self) -> u64 {
        dice(self.units)
    }
}

/// The roll GCP2 gives for a reading: the SplitMix64 finalizer of its 4-decimal value in
/// integer units. No seed: the same GCP2 value always rolls the same, live or in history.
pub fn dice(units: u64) -> u64 {
    let mut z = units.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Per-second GCP2 network coherence, for point-in-time hourly aggregates.
///
/// The live minute value is `|Σ network_coherence|` over the 3,600 seconds ending at the
/// minute's last second (checked against `getNetVarAggregate24H`: 932 of 932 overlap
/// minutes exact on 2026-10-19). Missing seconds count as absent; an hour with less than
/// 90% of its seconds present is unavailable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Gcp2Series {
    first_s: i64,
    /// `sum[i]` = Σ units of the seconds `first_s .. first_s + i`.
    sum: Vec<i64>,
    /// `count[i]` = present seconds among `first_s .. first_s + i`.
    count: Vec<u32>,
}

impl Gcp2Series {
    /// From `(epoch second, units)` points in any order; a repeated second keeps its first value.
    pub fn from_points(mut points: Vec<(i64, i64)>) -> Self {
        points.sort_by_key(|p| p.0);
        points.dedup_by_key(|p| p.0);
        let (Some(first), Some(last)) = (points.first(), points.last()) else {
            return Self::default();
        };
        let first_s = first.0;
        let len = usize::try_from(last.0 - first_s + 1).unwrap_or(0);
        let mut sum = Vec::with_capacity(len + 1);
        let mut count = Vec::with_capacity(len + 1);
        sum.push(0);
        count.push(0);
        let mut next = points.iter().peekable();
        let (mut s, mut c) = (0_i64, 0_u32);
        for second in first_s..=last.0 {
            if let Some((_, units)) = next.next_if(|p| p.0 == second) {
                s += units;
                c += 1;
            }
            sum.push(s);
            count.push(c);
        }
        Self {
            first_s,
            sum,
            count,
        }
    }

    /// Every `GCP2_Network_Coherence_*.csv` in `dir` (`epoch_time_utc,network_coherence,…`).
    pub fn load_dir(dir: &Path) -> Result<Self> {
        let entries = std::fs::read_dir(dir).map_err(|e| {
            TraderError::config(format!("missing GCP2 cache {}: {e}", dir.display()))
        })?;
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.starts_with("GCP2_Network_Coherence_") && n.ends_with(".csv")
                })
            })
            .collect();
        if files.is_empty() {
            return Err(TraderError::config(format!(
                "missing GCP2 cache: no GCP2_Network_Coherence_*.csv in {}",
                dir.display()
            )));
        }
        files.sort();
        let mut points = Vec::new();
        for path in files {
            let text = std::fs::read_to_string(&path)?;
            for (n, line) in text.lines().enumerate().skip(1) {
                let mut cols = line.split(',');
                let parsed = cols
                    .next()
                    .and_then(|t| t.trim().parse::<i64>().ok())
                    .zip(cols.next().and_then(parse_units));
                match parsed {
                    Some(point) => points.push(point),
                    None if line.trim().is_empty() => {}
                    None => {
                        return Err(TraderError::config(format!(
                            "bad GCP2 row {} in {}: {line:?}",
                            n + 1,
                            path.display()
                        )));
                    }
                }
            }
        }
        Ok(Self::from_points(points))
    }

    pub fn is_empty(&self) -> bool {
        self.count.len() <= 1
    }

    /// Last second with data (`None` when empty).
    pub fn last_second(&self) -> Option<i64> {
        (!self.is_empty()).then(|| self.first_s + self.count.len() as i64 - 2)
    }

    /// Index into the prefix arrays for "every second before `second`", clamped.
    fn prefix(&self, second: i64) -> usize {
        let i = (second - self.first_s).clamp(0, self.count.len() as i64 - 1);
        usize::try_from(i).unwrap_or(0)
    }

    /// The reading of the hour ending at second `end_s` (inclusive); `None` below 90%
    /// coverage, and `None` when `end_s` is past the last second the series holds (that
    /// hour has not finished in the data, so it is not a gap).
    pub fn reading_ending(&self, end_s: i64) -> Option<Gcp2Reading> {
        if self.last_second().is_none_or(|last| end_s > last) {
            return None;
        }
        let (from, to) = (
            self.prefix(end_s - AGGREGATE_SECONDS + 1),
            self.prefix(end_s + 1),
        );
        let present = f64::from(self.count[to] - self.count[from]);
        (present >= MIN_COVERAGE * AGGREGATE_SECONDS as f64).then(|| Gcp2Reading {
            end_s,
            units: (self.sum[to] - self.sum[from]).unsigned_abs(),
        })
    }

    /// The newest minute reading known at `t_ms`: the hour ending at the last second of
    /// the last whole minute before `t_ms` (the live page's `end_epoch`).
    pub fn reading_at(&self, t_ms: i64) -> Option<Gcp2Reading> {
        let minute_start_s = t_ms.div_euclid(60_000) * 60;
        self.reading_ending(minute_start_s - 1)
    }

    /// The series cut after second `last_s` (the look-ahead tests' "data up to T").
    #[cfg(test)]
    pub(crate) fn truncated(&self, last_s: i64) -> Self {
        let keep = usize::try_from(last_s - self.first_s + 2)
            .unwrap_or(0)
            .clamp(1, self.count.len().max(1));
        Self {
            first_s: self.first_s,
            sum: self.sum[..keep].to_vec(),
            count: self.count[..keep].to_vec(),
        }
    }
}

/// What a GCP2 reading means at one time: the value, its colour zone (by GCP2's own
/// bounds) and its dice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gcp2View {
    /// Last second of the hour the reading covers.
    pub end_s: i64,
    /// The value in [`UNITS_PER_ONE`] units (`value()` for the number).
    pub units: u64,
    /// Index into the colour zones, lowest first.
    pub zone: usize,
    pub zone_label: String,
    /// How many zones GCP2 publishes (so "the top two zones" needs no label in code).
    pub zone_count: usize,
    pub dice: u64,
}

impl Gcp2View {
    pub fn from_reading(reading: Gcp2Reading, zones: &ColorZones) -> Self {
        let zone = zones.index(reading.value());
        Self {
            end_s: reading.end_s,
            units: reading.units,
            zone,
            zone_label: zones.label(zone).to_string(),
            zone_count: zones.zones.len(),
            dice: reading.dice(),
        }
    }

    pub fn value(&self) -> f64 {
        self.units as f64 / UNITS_PER_ONE as f64
    }
}

/// Parse a `getNetVarAggregate24H` reply (`{"aggregates": [{end_epoch, netvar_aggregate}]}`)
/// into readings, oldest first. `end_epoch` is the last second of the minute's hour.
pub fn parse_aggregates(body: &Value) -> Result<Vec<Gcp2Reading>> {
    let list = body
        .get("aggregates")
        .and_then(Value::as_array)
        .ok_or_else(|| TraderError::config("GCP2 aggregates: no `aggregates` list"))?;
    let mut readings: Vec<Gcp2Reading> = list
        .iter()
        .map(|row| {
            let end = row.get("end_epoch")?;
            let end_s = end.as_i64().or_else(|| end.as_str()?.trim().parse().ok())?;
            let units = match row.get("netvar_aggregate")? {
                Value::String(text) => parse_units(text),
                Value::Number(n) => parse_units(&n.to_string()),
                _ => None,
            }?;
            Some(Gcp2Reading {
                end_s,
                units: units.unsigned_abs(),
            })
        })
        .collect::<Option<_>>()
        .ok_or_else(|| TraderError::config("GCP2 aggregates: a row lacks end_epoch/value"))?;
    if readings.is_empty() {
        return Err(TraderError::config("GCP2 aggregates: empty list"));
    }
    readings.sort_by_key(|r| r.end_s);
    readings.dedup_by_key(|r| r.end_s);
    Ok(readings)
}

/// What the trader reads from GCP2 live at a cycle.
#[derive(Debug, Clone, PartialEq)]
pub struct Gcp2Live {
    /// The newest minute, in the live colour zones.
    pub latest: Gcp2View,
    /// The newest minute at least 20 minutes before `latest` (`None` without one).
    pub earlier: Option<Gcp2View>,
    /// The last 24 h of minute readings, oldest first (ends with `latest`).
    pub minutes: Vec<Gcp2Reading>,
    /// The colour zones read with it.
    pub zones: ColorZones,
}

impl Gcp2Live {
    /// From parsed minutes (oldest first, see [`parse_aggregates`]) and the live zones.
    pub fn new(minutes: Vec<Gcp2Reading>, zones: ColorZones) -> Result<Self> {
        let last = *minutes
            .last()
            .ok_or_else(|| TraderError::config("GCP2 aggregates: empty list"))?;
        let earlier = minutes
            .iter()
            .rev()
            .find(|r| r.end_s <= last.end_s - EARLIER_SECONDS)
            .map(|r| Gcp2View::from_reading(*r, &zones));
        Ok(Self {
            latest: Gcp2View::from_reading(last, &zones),
            earlier,
            minutes,
            zones,
        })
    }

    /// Lowest and highest minute value of the last 24 h.
    pub fn range_24h(&self) -> (f64, f64) {
        let (low, high) = self
            .minutes
            .iter()
            .fold((u64::MAX, 0), |(lo, hi), r| (lo.min(r.units), hi.max(r.units)));
        (
            low as f64 / UNITS_PER_ONE as f64,
            high as f64 / UNITS_PER_ONE as f64,
        )
    }
}

fn utc_second(end_s: i64) -> String {
    chrono::DateTime::from_timestamp(end_s, 0).map_or_else(
        || end_s.to_string(),
        |t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

/// The cycle's GCP2 input: a live reading, or why there is none.
#[derive(Debug, Clone, PartialEq)]
pub enum Gcp2Now {
    Live(Box<Gcp2Live>),
    /// Any failed read: the cycle runs with "GCP2 unavailable" (no mood switch).
    Unavailable(String),
}

impl Gcp2Now {
    pub fn live(&self) -> Option<&Gcp2Live> {
        match self {
            Self::Live(live) => Some(live),
            Self::Unavailable(_) => None,
        }
    }

    /// The cycle-context form (the 24 h minutes as their range, not every value).
    pub fn payload(&self) -> Value {
        match self {
            Self::Live(live) => {
                let (low, high) = live.range_24h();
                let latest = &live.latest;
                json!({
                    "status": "live",
                    "minute_end": utc_second(latest.end_s),
                    "value": latest.value(),
                    "zone": latest.zone_label,
                    "zone_index": latest.zone,
                    "zone_count": latest.zone_count,
                    "dice": latest.dice,
                    "earlier": live.earlier.as_ref().map(|e| json!({
                        "minute_end": utc_second(e.end_s),
                        "value": e.value(),
                        "zone": e.zone_label,
                    })),
                    "range_24h": {"low": low, "high": high, "minutes": live.minutes.len()},
                    "zones_fetched_at": live.zones.fetched_at,
                })
            }
            Self::Unavailable(error) => json!({"status": "unavailable", "error": error}),
        }
    }
}

/// Cached GCP2 history plus its colour zones, for point-in-time views.
#[derive(Debug, Clone, PartialEq)]
pub struct Gcp2Feed {
    series: Gcp2Series,
    zones: ColorZones,
}

impl Gcp2Feed {
    pub fn new(series: Gcp2Series, zones: ColorZones) -> Self {
        Self { series, zones }
    }

    /// The monthly CSVs and `colorzones.json` of a `bb gcp2-history` cache directory.
    pub fn load_dir(dir: &Path) -> Result<Self> {
        Ok(Self::new(
            Gcp2Series::load_dir(dir)?,
            ColorZones::load(dir)?,
        ))
    }

    pub fn series(&self) -> &Gcp2Series {
        &self.series
    }

    pub fn zones(&self) -> &ColorZones {
        &self.zones
    }

    /// The view known at `t_ms` (see [`Gcp2Series::reading_at`]); `None` without a reading.
    pub fn view_at(&self, t_ms: i64) -> Option<Gcp2View> {
        self.series
            .reading_at(t_ms)
            .map(|r| Gcp2View::from_reading(r, &self.zones))
    }

    #[cfg(test)]
    pub(crate) fn truncated(&self, last_s: i64) -> Self {
        Self::new(self.series.truncated(last_s), self.zones.clone())
    }
}

/// Reads GCP2's public download form and live API.
#[derive(Debug)]
pub struct Gcp2Client {
    http: reqwest::Client,
    base: String,
}

impl Gcp2Client {
    pub fn new(base: impl Into<String>) -> Result<Self> {
        Self::with_timeout(base, HTTP_TIMEOUT)
    }

    /// A client whose every request gives up after `timeout`.
    pub fn with_timeout(base: impl Into<String>, timeout: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent("binance-bot gcp2 (read-only)")
            .build()
            .map_err(|e| http_error("client", e))?;
        Ok(Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
        })
    }

    async fn get_text(&self, path: &str) -> Result<(String, String)> {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .map_err(|e| http_error(path, e))?
            .error_for_status()
            .map_err(|e| http_error(path, e))?;
        let cookies = cookie_header(response.headers());
        let text = response.text().await.map_err(|e| http_error(path, e))?;
        Ok((cookies, text))
    }

    /// One month of per-second network coherence for `group`: `(file name, CSV bytes)`.
    pub async fn network_month(
        &self,
        group: &str,
        year: i32,
        month: u32,
    ) -> Result<(String, Vec<u8>)> {
        let (cookies, page) = self.get_text(DOWNLOAD_PAGE).await?;
        let token = csrf_token(&page)
            .ok_or_else(|| TraderError::config("GCP2 download page has no form token"))?;
        let (year_s, month_s) = (year.to_string(), month.to_string());
        let body = multipart(&[
            ("_token", token),
            ("group_2", group),
            ("year_2", &year_s),
            ("month_2", &month_s),
        ]);
        let response = self
            .http
            .post(format!("{}{NETWORK_DOWNLOAD}", self.base))
            .header(reqwest::header::COOKIE, cookies)
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}"),
            )
            .body(body)
            .send()
            .await
            .map_err(|e| http_error("network-coherence download", e))?
            .error_for_status()
            .map_err(|e| http_error("network-coherence download", e))?;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| http_error("network-coherence download", e))?;
        let (name, csv) = unzip_csv(&bytes)?;
        let expected = month_file_name(group, year, month);
        if name != expected {
            return Err(TraderError::config(format!(
                "GCP2 download for {year}-{month:02} returned {name:?}, expected {expected:?}"
            )));
        }
        Ok((name, csv))
    }

    /// The bearer token published for the live page.
    pub async fn bearer(&self) -> Result<String> {
        let (_, script) = self.get_text(TOKEN_SCRIPT).await?;
        parse_bearer(&script).ok_or_else(|| TraderError::config("GCP2 token script has no bearer"))
    }

    /// GET a public API path with `bearer`; `Ok(None)` when the token is refused (401/403).
    async fn api_json(&self, api: &str, bearer: &str) -> Result<Option<Value>> {
        let response = self
            .http
            .get(format!("{}{api}", self.base))
            .header(reqwest::header::AUTHORIZATION, bearer)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| http_error(api, e))?;
        if matches!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            return Ok(None);
        }
        let body = response
            .error_for_status()
            .map_err(|e| http_error(api, e))?
            .json()
            .await
            .map_err(|e| http_error(api, e))?;
        Ok(Some(body))
    }

    /// The colour zones with `bearer` (from [`Self::bearer`]).
    pub async fn color_zones(&self, bearer: &str, fetched_at: String) -> Result<ColorZones> {
        let body = self
            .api_json(COLOR_ZONES_API, bearer)
            .await?
            .ok_or_else(|| http_error(COLOR_ZONES_API, "token refused"))?;
        parse_color_zones(&body, fetched_at)
    }
}

/// The trader's live GCP2 reader: one per runtime, so the published token is cached
/// across cycles and fetched again only when the API refuses it.
#[derive(Debug)]
pub struct Gcp2LiveReader {
    /// The HTTP client, or why it could not be built (every read then fails).
    client: std::result::Result<Gcp2Client, String>,
    bearer: tokio::sync::Mutex<Option<String>>,
}

impl Gcp2LiveReader {
    pub fn new(base: &str, timeout: Duration) -> Self {
        Self {
            client: Gcp2Client::with_timeout(base, timeout).map_err(|e| e.to_string()),
            bearer: tokio::sync::Mutex::new(None),
        }
    }

    /// GET `api` with the cached token; on 401/403 fetch the token again and retry once.
    async fn api(&self, client: &Gcp2Client, api: &str) -> Result<Value> {
        let mut slot = self.bearer.lock().await;
        let cached = slot.clone();
        let fresh = cached.is_none();
        let bearer = match cached {
            Some(bearer) => bearer,
            None => client.bearer().await?,
        };
        *slot = Some(bearer.clone());
        if let Some(body) = client.api_json(api, &bearer).await? {
            return Ok(body);
        }
        *slot = None;
        if fresh {
            return Err(http_error(api, "the freshly published token was refused"));
        }
        let bearer = client.bearer().await?;
        *slot = Some(bearer.clone());
        if let Some(body) = client.api_json(api, &bearer).await? {
            return Ok(body);
        }
        *slot = None;
        Err(http_error(api, "token refused after a refresh"))
    }

    /// The last 24 h of minutes and the live colour zones.
    pub async fn read(&self, fetched_at: String) -> Result<Gcp2Live> {
        let client = self
            .client
            .as_ref()
            .map_err(|e| http_error("client", e))?;
        let minutes = parse_aggregates(&self.api(client, AGGREGATE_24H_API).await?)?;
        let zones = parse_color_zones(&self.api(client, COLOR_ZONES_API).await?, fetched_at)?;
        Gcp2Live::new(minutes, zones)
    }

    /// [`Self::read`] for a cycle: any failure becomes [`Gcp2Now::Unavailable`].
    pub async fn now(&self, fetched_at: String) -> Gcp2Now {
        match self.read(fetched_at).await {
            Ok(live) => Gcp2Now::Live(Box::new(live)),
            Err(error) => {
                tracing::warn!(%error, "GCP2 unavailable this cycle");
                Gcp2Now::Unavailable(error.to_string())
            }
        }
    }
}

/// Download GCP2 history into the cache directory (`bb gcp2-history`).
#[derive(clap::Args, Debug, Clone)]
pub struct Gcp2HistoryArgs {
    /// Year of the months to download.
    #[arg(long)]
    pub year: i32,
    /// Month (1-12), repeatable.
    #[arg(long = "month", required = true)]
    pub months: Vec<u32>,
    /// Group to download.
    #[arg(long, default_value = GLOBAL_NETWORK)]
    pub group: String,
    /// Where the CSVs and `colorzones.json` go.
    #[arg(long, default_value = "data/gcp2-cache")]
    pub cache_dir: PathBuf,
    /// Site base URL.
    #[arg(long, default_value = DEFAULT_BASE_URL)]
    pub base_url: String,
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Download every requested month and the current colour zones; returns the paths written.
pub async fn download_history(args: &Gcp2HistoryArgs, now: &str) -> Result<Vec<PathBuf>> {
    if let Some(bad) = args.months.iter().find(|m| !(1..=12).contains(*m)) {
        return Err(TraderError::config(format!("--month {bad} is not 1-12")));
    }
    let client = Gcp2Client::new(&args.base_url)?;
    std::fs::create_dir_all(&args.cache_dir)?;
    let mut written = Vec::new();
    for month in &args.months {
        let (name, csv) = client.network_month(&args.group, args.year, *month).await?;
        let path = args.cache_dir.join(name);
        write_atomic(&path, &csv)?;
        written.push(path);
    }
    let bearer = client.bearer().await?;
    let zones = client.color_zones(&bearer, now.to_string()).await?;
    let path = args.cache_dir.join(COLOR_ZONES_FILE);
    write_atomic(
        &path,
        format!("{}\n", serde_json::to_string_pretty(&zones)?).as_bytes(),
    )?;
    written.push(path);
    Ok(written)
}

/// `bb gcp2-history`.
pub async fn run_history_cli(args: Gcp2HistoryArgs) -> Result<ExitCode> {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    for path in download_history(&args, &now).await? {
        let lines = std::fs::read(&path)?
            .iter()
            .filter(|b| **b == b'\n')
            .count();
        println!("wrote {} ({lines} lines)", path.display());
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn zipped(name: &str, body: &str) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut out);
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(body.as_bytes()).unwrap();
        zip.finish().unwrap();
        out.into_inner()
    }

    const CSV: &str = "epoch_time_utc,network_coherence,active_devices\n1788220800,0.1030,397\n1788220801,-0.0827,397\n";

    #[test]
    fn parses_the_form_token_the_bearer_and_zones() {
        let html = r#"<form><input type="hidden" name="_token" value="abc123" autocomplete="off">"#;
        assert_eq!(csrf_token(html), Some("abc123"));
        assert_eq!(csrf_token("<form></form>"), None);
        let script = "// API authentication token\nconst bearer = 'Bearer 5|xyz';\n";
        assert_eq!(parse_bearer(script).as_deref(), Some("Bearer 5|xyz"));
        let zones = parse_color_zones(
            &serde_json::json!({"colorzones": [
                {"label": "Elevated", "range_start": "140", "range_end": 166},
                {"label": "Normal", "range_start": 0, "range_end": 140}
            ]}),
            "t".into(),
        )
        .unwrap();
        assert_eq!(
            zones
                .zones
                .iter()
                .map(|z| z.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Normal", "Elevated"]
        );
        assert!((zones.zones[1].start - 140.0).abs() < 1e-12);
        assert!(parse_color_zones(&serde_json::json!({"colorzones": []}), "t".into()).is_err());
    }

    #[tokio::test]
    async fn downloads_a_month_and_the_zones_into_the_cache() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(DOWNLOAD_PAGE))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("set-cookie", "laravel_session=s1; path=/; httponly")
                    .set_body_string(r#"<input type="hidden" name="_token" value="tok42">"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(NETWORK_DOWNLOAD))
            .and(header("cookie", "laravel_session=s1"))
            .and(body_string_contains("tok42"))
            .and(body_string_contains("Global Network"))
            .and(body_string_contains("name=\"month_2\"\r\n\r\n9\r\n"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(zipped(
                "GCP2_Network_Coherence_Global_Network_2026_09.csv",
                CSV,
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(TOKEN_SCRIPT))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("const bearer = 'Bearer 5|t';"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(COLOR_ZONES_API))
            .and(header("authorization", "Bearer 5|t"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"colorzones": [
                    {"label": "Normal", "range_start": 0, "range_end": 140},
                    {"label": "Elevated", "range_start": 140, "range_end": 166}
                ]}),
            ))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let args = Gcp2HistoryArgs {
            year: 2026,
            months: vec![9],
            group: GLOBAL_NETWORK.into(),
            cache_dir: dir.path().to_path_buf(),
            base_url: server.uri(),
        };
        let written = download_history(&args, "2026-10-20T00:00:00Z")
            .await
            .unwrap();
        assert_eq!(written.len(), 2);
        let csv = std::fs::read_to_string(
            dir.path()
                .join("GCP2_Network_Coherence_Global_Network_2026_09.csv"),
        )
        .unwrap();
        assert_eq!(csv, CSV);
        let zones: ColorZones = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(COLOR_ZONES_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(zones.fetched_at, "2026-10-20T00:00:00Z");
        assert_eq!(zones.zones.len(), 2);

        // A month outside 1-12 fails before any request.
        let mut bad = args.clone();
        bad.months = vec![13];
        assert!(download_history(&bad, "t").await.is_err());
    }

    #[tokio::test]
    async fn a_mismatched_or_non_zip_download_fails_without_writing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(DOWNLOAD_PAGE))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"name="_token" value="t""#))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(NETWORK_DOWNLOAD))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>error</html>"))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let args = Gcp2HistoryArgs {
            year: 2026,
            months: vec![8],
            group: GLOBAL_NETWORK.into(),
            cache_dir: dir.path().to_path_buf(),
            base_url: server.uri(),
        };
        let err = download_history(&args, "t").await.unwrap_err().to_string();
        assert!(err.contains("not a zip"), "{err}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    const T0_S: i64 = 1_790_812_800; // 2026-10-01T00:00Z

    /// `n` seconds from `T0_S` with `units(i)` each.
    fn seconds(n: i64, units: impl Fn(i64) -> i64) -> Vec<(i64, i64)> {
        (0..n).map(|i| (T0_S + i, units(i))).collect()
    }

    #[test]
    fn parses_four_decimal_units() {
        assert_eq!(parse_units("119.8923"), Some(1_198_923));
        assert_eq!(parse_units("-0.0827"), Some(-827));
        assert_eq!(parse_units(" 0.1030 "), Some(1_030));
        assert_eq!(parse_units("0.004"), Some(40));
        assert_eq!(parse_units("nan"), None);
        assert_eq!(parse_units("x"), None);
    }

    #[test]
    fn the_reading_is_the_absolute_hour_sum_ending_before_the_minute() {
        // Seconds alternate +0.5 and -0.2 for 2 hours, then a run of -1.0.
        let mut points = seconds(7_200, |i| if i % 2 == 0 { 5_000 } else { -2_000 });
        points.extend((7_200..9_000).map(|i| (T0_S + i, -10_000)));
        let series = Gcp2Series::from_points(points);
        // At 02:00:00 the newest minute ends at 01:59:59: seconds 3600..7199.
        let at_2h = series.reading_at((T0_S + 7_200) * 1000 + 30_000).unwrap();
        assert_eq!(at_2h.end_s, T0_S + 7_199);
        assert_eq!(at_2h.units, 1_800 * (5_000 - 2_000));
        assert!((at_2h.value() - 540.0).abs() < 1e-12);
        // At 02:30:00 the hour 01:30:00..02:29:59 sums 900 × 0.5 + 900 × (−0.2) − 1800 × 1.0
        // = −1530; the reading is its absolute value.
        let at = series.reading_at((T0_S + 9_000) * 1000).unwrap();
        assert_eq!(at.units, 15_300_000);
        assert_eq!(series.last_second(), Some(T0_S + 8_999));
    }

    #[test]
    fn an_hour_under_ninety_percent_coverage_is_unavailable() {
        let full = Gcp2Series::from_points(seconds(3_600, |_| 1));
        assert!(full.reading_ending(T0_S + 3_599).is_some());
        // 360 of 3600 seconds missing (exactly 90% present): still available.
        let edge = Gcp2Series::from_points(
            seconds(3_600, |_| 1)
                .into_iter()
                .filter(|p| p.0 - T0_S >= 360)
                .collect(),
        );
        assert_eq!(
            edge.reading_ending(T0_S + 3_599).map(|r| r.units),
            Some(3_240)
        );
        // 361 missing: unavailable. Before the first second or past the last: unavailable.
        let short = Gcp2Series::from_points(
            seconds(3_600, |_| 1)
                .into_iter()
                .filter(|p| p.0 - T0_S >= 361)
                .collect(),
        );
        assert!(short.reading_ending(T0_S + 3_599).is_none());
        assert!(full.reading_ending(T0_S + 100).is_none());
        assert!(full.reading_ending(T0_S + 3_599 + 400).is_none());
        // One second past the end is unavailable even though 3599 of 3600 seconds exist.
        assert!(full.reading_ending(T0_S + 3_600).is_none());
        assert!(Gcp2Series::default().reading_at(T0_S * 1000).is_none());
    }

    fn zones() -> ColorZones {
        parse_color_zones(
            &serde_json::json!({"colorzones": [
                {"label": "Normal", "range_start": 0, "range_end": 140},
                {"label": "Elevated", "range_start": 140, "range_end": 166},
                {"label": "High", "range_start": 166, "range_end": 219},
                {"label": "Very High", "range_start": 219, "range_end": 279},
                {"label": "Extreme", "range_start": 279, "range_end": 10000}
            ]}),
            "t".into(),
        )
        .unwrap()
    }

    #[test]
    fn zones_come_from_the_published_bounds() {
        let z = zones();
        assert_eq!(z.label(z.index(139.9999)), "Normal");
        assert_eq!(z.label(z.index(140.0)), "Elevated");
        assert_eq!(z.label(z.index(218.9)), "High");
        assert_eq!(z.label(z.index(219.0)), "Very High");
        assert_eq!(z.label(z.index(20_000.0)), "Extreme");
        // Shifted bounds move the answer: nothing about the bounds lives in code.
        let mut shifted = zones();
        shifted.zones[1].start = 150.0;
        assert_eq!(shifted.label(shifted.index(145.0)), "Normal");
        // Round trip through the cache file.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(COLOR_ZONES_FILE),
            serde_json::to_string(&z).unwrap(),
        )
        .unwrap();
        assert_eq!(ColorZones::load(dir.path()).unwrap(), z);
        assert!(ColorZones::load(&dir.path().join("none")).is_err());
    }

    #[test]
    fn the_dice_follow_the_gcp2_value_only() {
        let a = Gcp2Reading {
            end_s: 1,
            units: 1_198_923,
        };
        let same_value_other_minute = Gcp2Reading {
            end_s: 61,
            units: 1_198_923,
        };
        let other = Gcp2Reading {
            end_s: 1,
            units: 1_198_924,
        };
        assert_eq!(a.dice(), same_value_other_minute.dice());
        assert_ne!(a.dice(), other.dice());
        assert_eq!(
            a.dice(),
            dice(parse_units("119.8923").unwrap().unsigned_abs())
        );
    }

    #[test]
    fn loads_monthly_csvs_and_fails_without_them() {
        let dir = tempfile::tempdir().unwrap();
        let row = |s: i64, v: &str| format!("{s},{v},397\n");
        let header = "epoch_time_utc,network_coherence,active_devices\n";
        std::fs::write(
            dir.path()
                .join("GCP2_Network_Coherence_Global_Network_2026_10.csv"),
            format!(
                "{header}{}{}",
                row(T0_S + 1, "0.5000"),
                row(T0_S, "-0.2500")
            ),
        )
        .unwrap();
        std::fs::write(
            dir.path()
                .join("GCP2_Network_Coherence_Global_Network_2026_09.csv"),
            format!("{header}{}", row(T0_S - 1, "1.0000")),
        )
        .unwrap();
        std::fs::write(dir.path().join("colorzones.json"), "{}").unwrap();
        let series = Gcp2Series::load_dir(dir.path()).unwrap();
        assert_eq!(series.last_second(), Some(T0_S + 1));
        assert_eq!(series.sum, vec![0, 10_000, 7_500, 12_500]);
        let empty = tempfile::tempdir().unwrap();
        let err = Gcp2Series::load_dir(empty.path()).unwrap_err().to_string();
        assert!(err.contains("missing GCP2 cache"), "{err}");
        std::fs::write(
            dir.path()
                .join("GCP2_Network_Coherence_Global_Network_2026_08.csv"),
            format!("{header}oops,1,2\n"),
        )
        .unwrap();
        assert!(
            Gcp2Series::load_dir(dir.path())
                .unwrap_err()
                .to_string()
                .contains("bad GCP2 row 2")
        );
    }

    /// `getNetVarAggregate24H` minutes ending at `T0_S + 59 + 60 i` with the given values,
    /// listed newest first (the parser must sort them).
    fn aggregates(values: &[&str]) -> Value {
        let rows: Vec<Value> = values
            .iter()
            .enumerate()
            .rev()
            .map(|(i, v)| json!({"end_epoch": T0_S + 59 + 60 * i as i64, "netvar_aggregate": v}))
            .collect();
        json!({ "aggregates": rows })
    }

    fn zones_json() -> Value {
        json!({"colorzones": [
            {"label": "Normal", "range_start": 0, "range_end": 140},
            {"label": "Elevated", "range_start": 140, "range_end": 166},
            {"label": "High", "range_start": 166, "range_end": 219},
            {"label": "Very High", "range_start": 219, "range_end": 279},
            {"label": "Extreme", "range_start": 279, "range_end": 10000}
        ]})
    }

    async fn mount_token(server: &MockServer, bearer: &str, times: Option<u64>) {
        let mock = Mock::given(method("GET")).and(path(TOKEN_SCRIPT)).respond_with(
            ResponseTemplate::new(200).set_body_string(format!("const bearer = '{bearer}';")),
        );
        match times {
            Some(n) => mock.up_to_n_times(n).mount(server).await,
            None => mock.mount(server).await,
        }
    }

    async fn mount_api(server: &MockServer, bearer: &str, api: &str, body: Value) {
        Mock::given(method("GET"))
            .and(path(api))
            .and(header("authorization", bearer))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn token_fetches(server: &MockServer) -> usize {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == TOKEN_SCRIPT)
            .count()
    }

    #[tokio::test]
    async fn live_reader_gives_the_latest_minute_its_zone_the_earlier_zone_and_the_range() {
        let server = MockServer::start().await;
        mount_token(&server, "Bearer 5|a", None).await;
        // 22 minutes: 150 (Elevated) for the first two, then 200 (High), the newest 230.5.
        let mut values = vec!["150.0000", "150.0000"];
        values.extend(std::iter::repeat_n("200.0000", 19));
        values.push("230.5000");
        mount_api(&server, "Bearer 5|a", AGGREGATE_24H_API, aggregates(&values)).await;
        mount_api(&server, "Bearer 5|a", COLOR_ZONES_API, zones_json()).await;

        let reader = Gcp2LiveReader::new(&server.uri(), LIVE_TIMEOUT);
        let live = reader.read("2026-10-20T10:00:00Z".into()).await.unwrap();
        assert_eq!(live.minutes.len(), 22);
        assert_eq!(live.latest.end_s, T0_S + 59 + 60 * 21);
        assert_eq!(live.latest.units, 2_305_000);
        assert_eq!(live.latest.zone_label, "Very High");
        assert_eq!(live.latest.zone_count, 5);
        assert_eq!(live.latest.dice, dice(2_305_000));
        // 20 minutes before the newest minute is the second one (Elevated).
        let earlier = live.earlier.as_ref().unwrap();
        assert_eq!(earlier.end_s, T0_S + 59 + 60);
        assert_eq!(earlier.zone_label, "Elevated");
        assert_eq!(live.range_24h(), (150.0, 230.5));
        let payload = Gcp2Now::Live(Box::new(live)).payload();
        assert_eq!(payload["status"], "live");
        assert_eq!(payload["zone"], "Very High");
        assert_eq!(payload["earlier"]["zone"], "Elevated");
        assert_eq!(payload["dice"], json!(dice(2_305_000)));
        assert_eq!(payload["range_24h"]["high"], json!(230.5));

        // The token is cached across reads.
        reader.read("t".into()).await.unwrap();
        assert_eq!(token_fetches(&server).await, 1);
    }

    #[tokio::test]
    async fn a_refused_cached_token_is_fetched_again_once() {
        let server = MockServer::start().await;
        // The site publishes "old" first, then rotates to "new".
        mount_token(&server, "Bearer old", Some(1)).await;
        mount_token(&server, "Bearer new", None).await;
        // "old" works for the first read only, then the API refuses it.
        for api in [AGGREGATE_24H_API, COLOR_ZONES_API] {
            let body = if api == COLOR_ZONES_API {
                zones_json()
            } else {
                aggregates(&["100.0000"])
            };
            Mock::given(method("GET"))
                .and(path(api))
                .and(header("authorization", "Bearer old"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
                .up_to_n_times(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path(api))
                .and(header("authorization", "Bearer old"))
                .respond_with(ResponseTemplate::new(401))
                .mount(&server)
                .await;
        }
        mount_api(&server, "Bearer new", AGGREGATE_24H_API, aggregates(&["120.0000"])).await;
        mount_api(&server, "Bearer new", COLOR_ZONES_API, zones_json()).await;

        let reader = Gcp2LiveReader::new(&server.uri(), LIVE_TIMEOUT);
        let first = reader.read("t".into()).await.unwrap();
        assert_eq!(first.latest.units, 1_000_000);
        let second = reader.read("t".into()).await.unwrap();
        assert_eq!(second.latest.units, 1_200_000);
        assert_eq!(second.latest.zone_label, "Normal");
        // One fetch at start, one refresh on the 401; the new token then stays cached.
        assert_eq!(token_fetches(&server).await, 2);
        reader.read("t".into()).await.unwrap();
        assert_eq!(token_fetches(&server).await, 2);
    }

    #[tokio::test]
    async fn a_refused_fresh_token_a_timeout_or_bad_data_leave_gcp2_unavailable() {
        // A token the API refuses right after it was published: no retry loop.
        let server = MockServer::start().await;
        mount_token(&server, "Bearer x", None).await;
        Mock::given(method("GET"))
            .and(path(AGGREGATE_24H_API))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let reader = Gcp2LiveReader::new(&server.uri(), LIVE_TIMEOUT);
        let now = reader.now("t".into()).await;
        let Gcp2Now::Unavailable(error) = &now else {
            panic!("{now:?}")
        };
        assert!(error.contains("refused"), "{error}");
        assert_eq!(token_fetches(&server).await, 1);
        assert_eq!(now.payload()["status"], "unavailable");
        assert!(now.live().is_none());

        // An API slower than the timeout.
        let slow = MockServer::start().await;
        mount_token(&slow, "Bearer s", None).await;
        Mock::given(method("GET"))
            .and(path(AGGREGATE_24H_API))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(aggregates(&["100.0000"]))
                    .set_delay(Duration::from_secs(5)),
            )
            .mount(&slow)
            .await;
        let reader = Gcp2LiveReader::new(&slow.uri(), Duration::from_millis(200));
        let started = std::time::Instant::now();
        assert!(reader.now("t".into()).await.live().is_none());
        assert!(started.elapsed() < Duration::from_secs(4));

        // Unparseable or empty aggregates.
        assert!(parse_aggregates(&json!({"aggregates": []})).is_err());
        assert!(parse_aggregates(&json!({"aggregates": [{"end_epoch": 1}]})).is_err());
        assert!(parse_aggregates(&json!({"other": 1})).is_err());
        assert_eq!(
            parse_aggregates(&json!({"aggregates": [
                {"end_epoch": "120", "netvar_aggregate": 90.362},
                {"end_epoch": 60, "netvar_aggregate": "81.8725"}
            ]}))
            .unwrap(),
            vec![
                Gcp2Reading { end_s: 60, units: 818_725 },
                Gcp2Reading { end_s: 120, units: 903_620 },
            ]
        );
    }
}
