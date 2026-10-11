//! Minimal async client for the Jupiter swap-quote API (public lite tier, no auth).

use std::str::FromStr as _;
use std::time::Duration;

use rust_decimal::Decimal;
use serde_json::Value;

use crate::error::{SniperError, truncate_chars};

pub const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
pub const DEFAULT_QUOTE_URL: &str = "https://lite-api.jup.ag/swap/v1/quote";
pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

const QUOTE_TIMEOUT: Duration = Duration::from_secs(10);

/// One exact-in swap quote.
#[derive(Debug, Clone, PartialEq)]
pub struct JupiterQuote {
    pub input_mint: String,
    pub output_mint: String,
    /// Raw atoms of the input mint.
    pub in_amount: u64,
    /// Raw atoms of the output mint.
    pub out_amount: u64,
    pub price_impact_pct: f64,
    pub swap_usd_value: Decimal,
    pub route_labels: Vec<String>,
    pub context_slot: u64,
    /// Full response, forwarded verbatim to `/swap`.
    pub raw: Value,
}

fn atoms(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::String(s) => s.trim().parse().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

/// Python `float(x or 0.0)` semantics, as an exact decimal; malformed → 0.
pub(crate) fn decimal_or_zero(value: Option<&Value>) -> Decimal {
    let text = match value {
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(Value::Number(n)) => n.to_string(),
        _ => return Decimal::ZERO,
    };
    Decimal::from_str(&text)
        .or_else(|_| Decimal::from_scientific(&text))
        .unwrap_or(Decimal::ZERO)
}

fn float_or_zero(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn str_field(payload: &Value, key: &str) -> String {
    match payload.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Parse a `/quote` response body.
pub fn parse_quote(payload: Value) -> Result<JupiterQuote, SniperError> {
    let (Some(in_amount), Some(out_amount)) = (
        atoms(payload.get("inAmount")),
        atoms(payload.get("outAmount")),
    ) else {
        return Err(SniperError::Quote(format!(
            "missing/invalid amount fields: inAmount={:?} outAmount={:?}",
            payload.get("inAmount"),
            payload.get("outAmount")
        )));
    };
    let context_slot = match payload.get("contextSlot") {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    };
    let route_labels = payload
        .get("routePlan")
        .and_then(Value::as_array)
        .map(|hops| {
            hops.iter()
                .filter_map(|hop| hop.get("swapInfo")?.get("label"))
                .filter_map(|label| match label {
                    Value::String(s) if !s.is_empty() => Some(s.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(JupiterQuote {
        input_mint: str_field(&payload, "inputMint"),
        output_mint: str_field(&payload, "outputMint"),
        in_amount,
        out_amount,
        price_impact_pct: float_or_zero(payload.get("priceImpactPct")),
        swap_usd_value: decimal_or_zero(payload.get("swapUsdValue")),
        route_labels,
        context_slot,
        raw: payload,
    })
}

/// Anything that can price an exact-in swap (Jupiter in production, fakes in tests).
pub trait QuoteProvider: Send + Sync {
    fn get_quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount_atoms: u64,
        slippage_bps: u32,
    ) -> impl Future<Output = Result<JupiterQuote, SniperError>> + Send;
}

#[derive(Debug, Clone)]
pub struct JupiterClient {
    client: reqwest::Client,
    quote_url: String,
}

impl JupiterClient {
    pub fn new(client: reqwest::Client, quote_url: impl Into<String>) -> Self {
        Self {
            client,
            quote_url: quote_url.into(),
        }
    }
}

impl QuoteProvider for JupiterClient {
    /// One exact-in quote. HTTP failure, malformed response and zero output
    /// (no route) are [`SniperError::Quote`]; transport errors are [`SniperError::Http`].
    async fn get_quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount_atoms: u64,
        slippage_bps: u32,
    ) -> Result<JupiterQuote, SniperError> {
        if amount_atoms == 0 {
            return Err(SniperError::Quote("amount_atoms must be positive".into()));
        }
        let amount = amount_atoms.to_string();
        let slippage = slippage_bps.to_string();
        let resp = self
            .client
            .get(&self.quote_url)
            .query(&[
                ("inputMint", input_mint),
                ("outputMint", output_mint),
                ("amount", amount.as_str()),
                ("slippageBps", slippage.as_str()),
            ])
            .timeout(QUOTE_TIMEOUT)
            .send()
            .await?;
        let status = resp.status();
        if status != reqwest::StatusCode::OK {
            let body = resp.text().await.unwrap_or_default();
            return Err(SniperError::Quote(format!(
                "HTTP {}: {}",
                status.as_u16(),
                truncate_chars(&body, 200)
            )));
        }
        let quote = parse_quote(resp.json::<Value>().await?)?;
        if quote.out_amount == 0 {
            return Err(SniperError::Quote(
                "zero out_amount — no route available".into(),
            ));
        }
        Ok(quote)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use serde_json::json;
    use wiremock::matchers::{method, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn parse_quote_extracts_amounts_and_route() {
        let q = parse_quote(json!({
            "inputMint": SOL_MINT,
            "outputMint": "MINT",
            "inAmount": "10000000",
            "outAmount": "176520444997",
            "priceImpactPct": "0.5",
            "swapUsdValue": "0.4148",
            "contextSlot": 12345,
            "routePlan": [{"swapInfo": {"label": "Pump.fun", "ammKey": "X"}}],
        }))
        .unwrap();
        assert_eq!(q.in_amount, 10_000_000);
        assert_eq!(q.out_amount, 176_520_444_997);
        assert!((q.price_impact_pct - 0.5).abs() < 1e-12);
        assert_eq!(q.swap_usd_value, dec!(0.4148));
        assert_eq!(q.route_labels, vec!["Pump.fun".to_owned()]);
        assert_eq!(q.context_slot, 12345);
    }

    #[test]
    fn parse_quote_defaults_on_missing_optional_fields() {
        let q = parse_quote(json!({"inAmount": "1", "outAmount": "2"})).unwrap();
        assert!(q.price_impact_pct.abs() < f64::EPSILON);
        assert_eq!(q.swap_usd_value, Decimal::ZERO);
        assert!(q.route_labels.is_empty());
        assert_eq!(q.context_slot, 0);
    }

    #[test]
    fn parse_quote_raises_on_missing_amounts() {
        for bad in [
            json!({}),
            json!({"inAmount": "not a number", "outAmount": "5"}),
        ] {
            let err = parse_quote(bad).unwrap_err();
            assert!(matches!(&err, SniperError::Quote(m) if m.contains("amount fields")));
        }
    }

    #[tokio::test]
    async fn get_quote_rejects_non_positive_amount() {
        let client = JupiterClient::new(reqwest::Client::new(), "http://127.0.0.1:9");
        let err = client.get_quote(SOL_MINT, "X", 0, 500).await.unwrap_err();
        assert!(err.to_string().contains("amount_atoms must be positive"));
    }

    #[tokio::test]
    async fn get_quote_returns_parsed_quote_on_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("amount", "10000000"))
            .and(query_param("slippageBps", "500"))
            .and(query_param("inputMint", SOL_MINT))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "inputMint": SOL_MINT, "outputMint": "MINT",
                "inAmount": "10000000", "outAmount": "999",
                "priceImpactPct": "0", "swapUsdValue": "0.5",
                "contextSlot": 1, "routePlan": [],
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = JupiterClient::new(reqwest::Client::new(), server.uri());
        let quote = client
            .get_quote(SOL_MINT, "MINT", 10_000_000, 500)
            .await
            .unwrap();
        assert_eq!(quote.out_amount, 999);
    }

    #[tokio::test]
    async fn get_quote_raises_on_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500).set_body_string("server boom"))
            .mount(&server)
            .await;
        let client = JupiterClient::new(reqwest::Client::new(), server.uri());
        let err = client
            .get_quote(SOL_MINT, "MINT", 1, 500)
            .await
            .unwrap_err();
        assert!(matches!(&err, SniperError::Quote(m) if m == "HTTP 500: server boom"));
    }

    #[tokio::test]
    async fn get_quote_raises_on_zero_output() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "inputMint": SOL_MINT, "outputMint": "MINT",
                "inAmount": "10000000", "outAmount": "0",
            })))
            .mount(&server)
            .await;
        let client = JupiterClient::new(reqwest::Client::new(), server.uri());
        let err = client
            .get_quote(SOL_MINT, "MINT", 10_000_000, 500)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no route available"));
    }
}
