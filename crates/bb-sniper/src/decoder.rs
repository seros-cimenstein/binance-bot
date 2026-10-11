//! Decode pump.fun `CreateEvent` emissions directly from on-chain log strings.
//!
//! When a new pump.fun token is launched, the program emits a Borsh-encoded
//! `CreateEvent` as a `Program data: ...` log line:
//!
//! ```text
//! 8 bytes  Anchor event discriminator (constant for CreateEvent)
//! u32 LE   length of name + UTF-8 bytes
//! u32 LE   length of symbol + UTF-8 bytes
//! u32 LE   length of uri + UTF-8 bytes
//! 32 bytes mint pubkey
//! 32 bytes bonding-curve pubkey
//! 32 bytes user (deployer) pubkey
//! ```

use base64::Engine as _;
use base64::engine::{GeneralPurpose, GeneralPurposeConfig};
use serde::{Deserialize, Serialize};

use crate::error::SniperError;

/// Anchor discriminator for pump.fun's `CreateEvent` (first 8 payload bytes).
pub const CREATE_EVENT_DISCRIMINATOR: [u8; 8] = [0x1B, 0x72, 0xA9, 0x4D, 0xDE, 0xEB, 0x63, 0x76];

const PROGRAM_DATA_PREFIX: &str = "Program data: ";

/// Lenient standard base64, matching Python's `b64decode(validate=False)`:
/// characters outside the alphabet are discarded and non-canonical trailing bits accepted.
const LENIENT_B64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
);

/// Bitcoin-style base58 (Solana address convention).
pub fn base58_encode(data: &[u8]) -> String {
    bs58::encode(data).into_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PumpFunCreateEvent {
    pub name: String,
    pub symbol: String,
    pub uri: String,
    /// base58 mint pubkey.
    pub mint: String,
    pub bonding_curve: String,
    pub user: String,
}

/// Base64 payload of every `Program data: <payload>` log line.
fn program_data_payloads<S: AsRef<str>>(logs: &[S]) -> impl Iterator<Item = &str> {
    logs.iter()
        .filter_map(|line| line.as_ref().strip_prefix(PROGRAM_DATA_PREFIX))
        .map(str::trim)
}

fn lenient_b64_decode(payload: &str) -> Result<Vec<u8>, SniperError> {
    let filtered: String = payload
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
        .collect();
    LENIENT_B64
        .decode(filtered)
        .map_err(|err| SniperError::Decode(format!("base64 decode failed: {err}")))
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn take(&mut self, len: usize, what: impl FnOnce() -> String) -> Result<&[u8], SniperError> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| SniperError::Decode(what()))?;
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn read_string(&mut self) -> Result<String, SniperError> {
        let len_bytes = self.take(4, || "truncated before string length".to_owned())?;
        let mut le = [0u8; 4];
        le.copy_from_slice(len_bytes);
        let len = u32::from_le_bytes(le) as usize;
        let body = self.take(len, || format!("truncated string body (need {len})"))?;
        Ok(String::from_utf8_lossy(body).into_owned())
    }

    fn read_pubkey(&mut self) -> Result<String, SniperError> {
        let pk = self.take(32, || "truncated pubkey".to_owned())?;
        Ok(base58_encode(pk))
    }
}

/// Decode one base64 `CreateEvent` payload.
pub fn decode_create_event_payload(payload_b64: &str) -> Result<PumpFunCreateEvent, SniperError> {
    let data = lenient_b64_decode(payload_b64)?;
    if data.len() < CREATE_EVENT_DISCRIMINATOR.len() {
        return Err(SniperError::Decode(
            "payload too short to contain discriminator".to_owned(),
        ));
    }
    if data[..8] != CREATE_EVENT_DISCRIMINATOR {
        return Err(SniperError::Decode(
            "discriminator does not match CreateEvent".to_owned(),
        ));
    }
    let mut cursor = Cursor {
        data: &data,
        pos: 8,
    };
    Ok(PumpFunCreateEvent {
        name: cursor.read_string()?,
        symbol: cursor.read_string()?,
        uri: cursor.read_string()?,
        mint: cursor.read_pubkey()?,
        bonding_curve: cursor.read_pubkey()?,
        user: cursor.read_pubkey()?,
    })
}

/// First valid `CreateEvent` among the `Program data:` lines, if any.
///
/// Every line is tried because pump.fun transactions emit other event types
/// (Buy etc.) in the same transaction.
pub fn decode_create_event_from_logs<S: AsRef<str>>(logs: &[S]) -> Option<PumpFunCreateEvent> {
    program_data_payloads(logs).find_map(|payload| decode_create_event_payload(payload).ok())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn build_create_event_payload(
        name: &str,
        symbol: &str,
        uri: &str,
        mint: &[u8; 32],
        bonding_curve: &[u8; 32],
        user: &[u8; 32],
    ) -> String {
        let mut body = CREATE_EVENT_DISCRIMINATOR.to_vec();
        for s in [name, symbol, uri] {
            body.extend_from_slice(&(s.len() as u32).to_le_bytes());
            body.extend_from_slice(s.as_bytes());
        }
        body.extend_from_slice(mint);
        body.extend_from_slice(bonding_curve);
        body.extend_from_slice(user);
        base64::engine::general_purpose::STANDARD.encode(body)
    }

    fn seq32(start: u8) -> [u8; 32] {
        std::array::from_fn(|i| start + i as u8)
    }

    #[test]
    fn base58_known_vectors() {
        assert_eq!(base58_encode(b"Hello World!"), "2NEpo7TZRRrLZSi2U");
        assert_eq!(base58_encode(b""), "");
        assert_eq!(base58_encode(&[0u8; 32]), "1".repeat(32));
    }

    #[test]
    fn base58_preserves_leading_zeros() {
        assert_eq!(base58_encode(&[0, 0, 1]), "112");
    }

    #[test]
    fn decode_synthetic_event_roundtrip() {
        let (mint, bonding, user) = (seq32(0), seq32(32), seq32(64));
        let payload = build_create_event_payload(
            "My Coin",
            "MYC",
            "https://example.com/meta.json",
            &mint,
            &bonding,
            &user,
        );
        let logs = vec![
            "Program ComputeBudget111... invoke [1]".to_owned(),
            format!("Program data: {payload}"),
            "Program 6EF8rrec... success".to_owned(),
        ];
        let event = decode_create_event_from_logs(&logs).unwrap();
        assert_eq!(event.name, "My Coin");
        assert_eq!(event.symbol, "MYC");
        assert_eq!(event.uri, "https://example.com/meta.json");
        assert_eq!(event.mint, base58_encode(&mint));
        assert_eq!(event.bonding_curve, base58_encode(&bonding));
        assert_eq!(event.user, base58_encode(&user));
    }

    #[test]
    fn decode_real_world_event() {
        let payload = concat!(
            "G3KpTd7rY3YbAAAAQXJ0aWZpY2lhbCBHb2QgSW50ZWxsaWdlbmNlAwAAAEFHSTYAAABo",
            "dHRwczovL21ldGFkYXRhLmo3dHJhY2tlci5pby9tZXRhZGF0YS85N3FGVUk0ajJBLmpz",
            "b27JFO7g7qHu7QxcLPLNwt2j/dGqxM8+f8hp08sKdWLZf/Z7AKbeNzMQXg0J1zRyhwBl",
            "GgZJsZmpHrf16WoiPN/EDgNABSfFdZjhcdGbzonmWk944q0wzrWh8PItSgBoGndmWxs="
        );
        let logs = [format!("Program data: {payload}")];
        let event = decode_create_event_from_logs(&logs).unwrap();
        assert_eq!(event.name, "Artificial God Intelligence");
        assert_eq!(event.symbol, "AGI");
        assert!(event.uri.contains("metadata.j7tracker"));
        assert_eq!(event.mint, "EXwTiUzN6uEpDimj3kD6A97w8rHjEMkNA2fDu29gpump");
    }

    #[test]
    fn decode_returns_none_without_program_data_lines() {
        let logs = [
            "Program ComputeBudget invoke [1]",
            "Program ComputeBudget success",
        ];
        assert!(decode_create_event_from_logs(&logs).is_none());
    }

    #[test]
    fn decode_returns_none_on_discriminator_mismatch() {
        let mut body = vec![0u8; 8];
        body.extend_from_slice(b"\x05\x00\x00\x00hello");
        body.extend_from_slice(&[0u8; 16]);
        let bogus = base64::engine::general_purpose::STANDARD.encode(body);
        let logs = [format!("Program data: {bogus}")];
        assert!(decode_create_event_from_logs(&logs).is_none());
    }

    #[test]
    fn decode_skips_unrelated_program_data() {
        let real = build_create_event_payload("X", "X", "x", &seq32(0), &[0; 32], &[0; 32]);
        let logs = [
            "Program data: AAECAwQFBgcEAAAA".to_owned(),
            format!("Program data: {real}"),
        ];
        let event = decode_create_event_from_logs(&logs).unwrap();
        assert_eq!(event.name, "X");
    }

    #[test]
    fn decode_handles_invalid_base64() {
        assert!(decode_create_event_from_logs(&["Program data: not-valid-base64-!@#$"]).is_none());
    }

    #[test]
    fn decode_handles_truncated_payload() {
        let mut body = CREATE_EVENT_DISCRIMINATOR.to_vec();
        body.extend_from_slice(&[5, 0, 0]);
        let truncated = base64::engine::general_purpose::STANDARD.encode(body);
        assert!(decode_create_event_from_logs(&[format!("Program data: {truncated}")]).is_none());
        assert!(matches!(
            decode_create_event_payload(&truncated),
            Err(SniperError::Decode(msg)) if msg.contains("truncated")
        ));
    }
}
