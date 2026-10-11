//! Shared signal candidate model emitted by every V1 detector.

use serde::{Deserialize, Serialize};

use crate::jsonv::Object;

/// A source signal that is ready for downstream monitoring. Serialised as one
/// row of the shared signal ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalCandidate {
    pub source: String,
    pub chain: String,
    pub token_address: String,
    pub token_symbol: String,
    pub detected_at_ms: i64,
    pub raw_signal: Object,
    pub confidence: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn candidate() -> SignalCandidate {
        SignalCandidate {
            source: "dexscreener".into(),
            chain: "solana".into(),
            token_address: "Token111".into(),
            token_symbol: "TOK".into(),
            detected_at_ms: 123,
            raw_signal: json!({"pair": {"url": "https://example.test"}})
                .as_object()
                .cloned()
                .unwrap(),
            confidence: 0.75,
        }
    }

    #[test]
    fn json_round_trip_preserves_every_field() {
        let c = candidate();
        let encoded = serde_json::to_value(&c).unwrap();
        assert_eq!(
            encoded,
            json!({
                "source": "dexscreener",
                "chain": "solana",
                "token_address": "Token111",
                "token_symbol": "TOK",
                "detected_at_ms": 123,
                "raw_signal": {"pair": {"url": "https://example.test"}},
                "confidence": 0.75,
            })
        );
        let decoded: SignalCandidate = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, c);
    }
}
