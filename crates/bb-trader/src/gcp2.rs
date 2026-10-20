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
}
