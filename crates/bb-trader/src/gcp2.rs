//! GCP2 (gcp2.net, Global Consciousness Project 2.0) public data, read-only (Phase 49).
//!
//! History: the site's data-download form (no login, no captcha) returns one zipped CSV of
//! per-second network coherence per month. Colour zones: the same public API the live page
//! uses, with the bearer token the site publishes in `/js/data/api_token.js`. Nothing here
//! posts anything other than the download form, logs in, or touches the trader.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
const HTTP_TIMEOUT: Duration = Duration::from_secs(180);
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
            dice: reading.dice(),
        }
    }

    pub fn value(&self) -> f64 {
        self.units as f64 / UNITS_PER_ONE as f64
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

/// Reads GCP2's public download form and colour zones.
pub struct Gcp2Client {
    http: reqwest::Client,
    base: String,
}

impl Gcp2Client {
    pub fn new(base: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent("binance-bot gcp2-history (read-only research)")
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

    /// The colour zones with `bearer` (from [`Self::bearer`]).
    pub async fn color_zones(&self, bearer: &str, fetched_at: String) -> Result<ColorZones> {
        let body: Value = self
            .http
            .get(format!("{}{COLOR_ZONES_API}", self.base))
            .header(reqwest::header::AUTHORIZATION, bearer)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| http_error("colour zones", e))?
            .error_for_status()
            .map_err(|e| http_error("colour zones", e))?
            .json()
            .await
            .map_err(|e| http_error("colour zones", e))?;
        parse_color_zones(&body, fetched_at)
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
}
