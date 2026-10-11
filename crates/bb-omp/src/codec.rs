//! JSONL framing for the omp RPC protocol, including protocol-v2 `rpc_chunk` reassembly.
//!
//! Every physical frame is one JSON object followed by `\n`. After protocol v2 is negotiated the
//! server may split an oversized logical frame into an uninterrupted sequence of `rpc_chunk`
//! frames carrying base64 segments of the UTF-8 JSON text. Decoding errors are recoverable: the
//! offending frame (or chunk sequence) is dropped and reading continues with the next frame.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// Physical frame cap advertised by omp (including the trailing newline).
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1_048_576;
/// Reassembled logical frame cap advertised by omp.
pub const DEFAULT_MAX_REASSEMBLED_BYTES: usize = 67_108_864;

/// Transport limits, normally taken from the server's `ready` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLimits {
    pub max_frame_bytes: usize,
    pub max_reassembled_bytes: usize,
}

impl Default for FrameLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_reassembled_bytes: DEFAULT_MAX_REASSEMBLED_BYTES,
        }
    }
}

/// Chunk-sequence validation failures. The in-progress sequence is discarded on any of them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChunkError {
    #[error("malformed rpc_chunk frame: {0}")]
    Malformed(String),
    #[error("rpc_chunk {chunk_id} interleaved with in-progress sequence {active}")]
    Interleaved { active: String, chunk_id: String },
    #[error("rpc_chunk {chunk_id}: expected index {expected}, got {got}")]
    OutOfOrder {
        chunk_id: String,
        expected: u64,
        got: u64,
    },
    #[error("rpc_chunk {chunk_id}: count/byteLength changed mid-sequence")]
    Inconsistent { chunk_id: String },
    #[error("rpc_chunk {chunk_id}: sequence interrupted by a non-chunk frame")]
    Interrupted { chunk_id: String },
    #[error("rpc_chunk {chunk_id}: stream ended mid-sequence")]
    Truncated { chunk_id: String },
    #[error("rpc_chunk {chunk_id}: {byte_length} bytes exceeds the {limit}-byte reassembly limit")]
    TooLarge {
        chunk_id: String,
        byte_length: u64,
        limit: usize,
    },
    #[error("rpc_chunk {chunk_id}: declared {declared} bytes, received {received}")]
    LengthMismatch {
        chunk_id: String,
        declared: u64,
        received: u64,
    },
    #[error("rpc_chunk {chunk_id}: invalid base64: {message}")]
    Base64 { chunk_id: String, message: String },
}

/// Frame decoding/encoding failures. Only [`FrameError::Io`] is fatal for a reader.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("stdout read failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame exceeds the {limit}-byte physical frame limit")]
    LineTooLong { limit: usize },
    #[error("frame is not valid JSON: {0}")]
    InvalidJson(String),
    #[error("frame is not a JSON object")]
    NotObject,
    #[error("reassembled frame is not valid UTF-8")]
    InvalidUtf8,
    #[error(transparent)]
    Chunk(#[from] ChunkError),
    #[error("outbound frame of {size} bytes exceeds the {limit}-byte frame limit")]
    OutboundTooLarge { size: usize, limit: usize },
    #[error("outbound frame could not be serialised: {0}")]
    Serialize(String),
}

impl FrameError {
    /// Fatal errors end the stream; everything else drops one frame.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Io(_))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpcChunk {
    chunk_id: String,
    index: u64,
    count: u64,
    byte_length: u64,
    data: String,
}

#[derive(Debug)]
struct Sequence {
    chunk_id: String,
    count: u64,
    byte_length: u64,
    next_index: u64,
    buf: Vec<u8>,
}

/// Validating state machine for `rpc_chunk` sequences.
#[derive(Debug, Default)]
pub struct ChunkAssembler {
    max_reassembled_bytes: usize,
    active: Option<Sequence>,
}

impl ChunkAssembler {
    pub fn new(max_reassembled_bytes: usize) -> Self {
        Self {
            max_reassembled_bytes,
            active: None,
        }
    }

    /// Feeds one `rpc_chunk` frame object. Returns the complete logical frame bytes on the final chunk.
    pub fn push(&mut self, frame: Map<String, Value>) -> Result<Option<Vec<u8>>, ChunkError> {
        let chunk: RpcChunk = serde_json::from_value(Value::Object(frame)).map_err(|error| {
            self.active = None;
            ChunkError::Malformed(error.to_string())
        })?;
        let result = self.accept(chunk);
        if result.is_err() {
            self.active = None;
        }
        result
    }

    fn accept(&mut self, chunk: RpcChunk) -> Result<Option<Vec<u8>>, ChunkError> {
        if chunk.count == 0 || chunk.index >= chunk.count {
            return Err(ChunkError::Malformed(format!(
                "index {} out of range for count {}",
                chunk.index, chunk.count
            )));
        }
        let limit = self.max_reassembled_bytes;
        if usize::try_from(chunk.byte_length).map_or(true, |len| len > limit) {
            return Err(ChunkError::TooLarge {
                chunk_id: chunk.chunk_id,
                byte_length: chunk.byte_length,
                limit,
            });
        }
        let sequence = match self.active.as_mut() {
            None => {
                if chunk.index != 0 {
                    return Err(ChunkError::OutOfOrder {
                        chunk_id: chunk.chunk_id,
                        expected: 0,
                        got: chunk.index,
                    });
                }
                self.active.insert(Sequence {
                    chunk_id: chunk.chunk_id.clone(),
                    count: chunk.count,
                    byte_length: chunk.byte_length,
                    next_index: 0,
                    buf: Vec::new(),
                })
            }
            Some(active) => {
                if active.chunk_id != chunk.chunk_id {
                    return Err(ChunkError::Interleaved {
                        active: active.chunk_id.clone(),
                        chunk_id: chunk.chunk_id,
                    });
                }
                if active.count != chunk.count || active.byte_length != chunk.byte_length {
                    return Err(ChunkError::Inconsistent {
                        chunk_id: chunk.chunk_id,
                    });
                }
                if active.next_index != chunk.index {
                    return Err(ChunkError::OutOfOrder {
                        chunk_id: chunk.chunk_id,
                        expected: active.next_index,
                        got: chunk.index,
                    });
                }
                active
            }
        };
        let decoded = BASE64
            .decode(chunk.data.as_bytes())
            .map_err(|error| ChunkError::Base64 {
                chunk_id: chunk.chunk_id.clone(),
                message: error.to_string(),
            })?;
        sequence.buf.extend_from_slice(&decoded);
        let received = sequence.buf.len() as u64;
        if received > sequence.byte_length {
            return Err(ChunkError::LengthMismatch {
                chunk_id: chunk.chunk_id,
                declared: sequence.byte_length,
                received,
            });
        }
        sequence.next_index += 1;
        if sequence.next_index < sequence.count {
            return Ok(None);
        }
        if received != sequence.byte_length {
            return Err(ChunkError::LengthMismatch {
                chunk_id: chunk.chunk_id,
                declared: sequence.byte_length,
                received,
            });
        }
        Ok(self.active.take().map(|done| done.buf))
    }

    /// A non-chunk frame arrived: any in-progress sequence was interrupted.
    pub fn interrupt(&mut self) -> Option<ChunkError> {
        self.active.take().map(|sequence| ChunkError::Interrupted {
            chunk_id: sequence.chunk_id,
        })
    }

    /// The stream ended: any in-progress sequence is truncated.
    pub fn finish(&mut self) -> Option<ChunkError> {
        self.active.take().map(|sequence| ChunkError::Truncated {
            chunk_id: sequence.chunk_id,
        })
    }
}

/// Reads logical JSON object frames from an omp stdout stream.
pub struct FrameReader<R> {
    inner: BufReader<R>,
    limits: FrameLimits,
    assembler: ChunkAssembler,
    deferred: Option<Map<String, Value>>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(reader: R, limits: FrameLimits) -> Self {
        Self {
            inner: BufReader::with_capacity(64 * 1024, reader),
            limits,
            assembler: ChunkAssembler::new(limits.max_reassembled_bytes),
            deferred: None,
        }
    }

    /// Applies limits advertised by the server's `ready` frame.
    pub fn set_limits(&mut self, limits: FrameLimits) {
        self.limits = limits;
        self.assembler.max_reassembled_bytes = limits.max_reassembled_bytes;
    }

    /// Next logical frame; `Ok(None)` at end of stream. Non-fatal errors drop one frame only.
    pub async fn next_frame(&mut self) -> Result<Option<Map<String, Value>>, FrameError> {
        if let Some(frame) = self.deferred.take() {
            return Ok(Some(frame));
        }
        loop {
            let Some(line) = self.read_line().await? else {
                return match self.assembler.finish() {
                    Some(error) => Err(error.into()),
                    None => Ok(None),
                };
            };
            if line.trim_ascii().is_empty() {
                continue;
            }
            let frame = parse_object(&line)?;
            if frame.get("type").and_then(Value::as_str) == Some("rpc_chunk") {
                if let Some(bytes) = self.assembler.push(frame)? {
                    let text = String::from_utf8(bytes).map_err(|_| FrameError::InvalidUtf8)?;
                    return parse_object(text.as_bytes()).map(Some);
                }
                continue;
            }
            if let Some(error) = self.assembler.interrupt() {
                self.deferred = Some(frame);
                return Err(error.into());
            }
            return Ok(Some(frame));
        }
    }

    /// One physical line without its terminator; oversized lines are consumed and rejected.
    async fn read_line(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        // The physical cap includes the newline.
        let max_content = self.limits.max_frame_bytes.saturating_sub(1);
        let mut line = Vec::new();
        let mut too_long = false;
        loop {
            let available = self.inner.fill_buf().await?;
            if available.is_empty() {
                if too_long {
                    return Err(FrameError::LineTooLong {
                        limit: self.limits.max_frame_bytes,
                    });
                }
                return Ok(if line.is_empty() { None } else { Some(line) });
            }
            let (segment, consumed, complete) = match available.iter().position(|b| *b == b'\n') {
                Some(pos) => (&available[..pos], pos + 1, true),
                None => (available, available.len(), false),
            };
            if !too_long {
                if line.len() + segment.len() > max_content {
                    too_long = true;
                    line = Vec::new();
                } else {
                    line.extend_from_slice(segment);
                }
            }
            self.inner.consume(consumed);
            if complete {
                if too_long {
                    return Err(FrameError::LineTooLong {
                        limit: self.limits.max_frame_bytes,
                    });
                }
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(line));
            }
        }
    }
}

fn parse_object(bytes: &[u8]) -> Result<Map<String, Value>, FrameError> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(FrameError::NotObject),
        Err(error) => Err(FrameError::InvalidJson(error.to_string())),
    }
}

/// Serialises one inbound frame as a JSONL line, enforcing the physical frame limit.
pub fn encode_frame(frame: &Value, limits: FrameLimits) -> Result<Vec<u8>, FrameError> {
    let mut bytes =
        serde_json::to_vec(frame).map_err(|error| FrameError::Serialize(error.to_string()))?;
    bytes.push(b'\n');
    if bytes.len() > limits.max_frame_bytes {
        return Err(FrameError::OutboundTooLarge {
            size: bytes.len(),
            limit: limits.max_frame_bytes,
        });
    }
    Ok(bytes)
}

/// Splits a logical frame into `rpc_chunk` frames (used by tests and fake servers).
pub fn chunk_frame(chunk_id: &str, frame: &Value, segment_bytes: usize) -> Vec<Value> {
    let text = frame.to_string();
    let bytes = text.as_bytes();
    let segment_bytes = segment_bytes.max(1);
    let segments: Vec<&[u8]> = bytes.chunks(segment_bytes).collect();
    let count = segments.len();
    segments
        .iter()
        .enumerate()
        .map(|(index, segment)| {
            serde_json::json!({
                "type": "rpc_chunk",
                "chunkId": chunk_id,
                "index": index,
                "count": count,
                "byteLength": bytes.len(),
                "data": BASE64.encode(segment),
            })
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    fn chunks(id: &str, frame: &Value, segment: usize) -> Vec<Map<String, Value>> {
        chunk_frame(id, frame, segment)
            .into_iter()
            .map(object)
            .collect()
    }

    #[test]
    fn reassembles_in_order_sequence() {
        let frame = json!({"type": "response", "id": "x", "data": "é".repeat(40)});
        let mut assembler = ChunkAssembler::new(DEFAULT_MAX_REASSEMBLED_BYTES);
        let parts = chunks("c1", &frame, 7);
        assert!(parts.len() > 3);
        let last = parts.len() - 1;
        for (i, part) in parts.into_iter().enumerate() {
            let out = assembler.push(part).unwrap();
            if i == last {
                let value: Value = serde_json::from_slice(&out.unwrap()).unwrap();
                assert_eq!(value, frame);
            } else {
                assert!(out.is_none());
            }
        }
    }

    #[test]
    fn rejects_interleaved_sequences() {
        let mut assembler = ChunkAssembler::new(DEFAULT_MAX_REASSEMBLED_BYTES);
        let a = chunks("a", &json!({"type": "x", "v": "aaaaaaaaaaaaaaaa"}), 5);
        let b = chunks("b", &json!({"type": "y", "v": "bbbbbbbbbbbbbbbb"}), 5);
        assembler.push(a[0].clone()).unwrap();
        let err = assembler.push(b[0].clone()).unwrap_err();
        assert_eq!(
            err,
            ChunkError::Interleaved {
                active: "a".into(),
                chunk_id: "b".into()
            }
        );
        // The interrupted sequence was discarded: its continuation is now out of order.
        assert!(matches!(
            assembler.push(a[1].clone()),
            Err(ChunkError::OutOfOrder {
                expected: 0,
                got: 1,
                ..
            })
        ));
    }

    #[test]
    fn rejects_short_and_inconsistent_sequences() {
        let frame = json!({"type": "x", "v": "0123456789abcdef"});
        let mut parts = chunks("s", &frame, 6);
        // Declared byteLength larger than the delivered payload.
        let mut assembler = ChunkAssembler::new(DEFAULT_MAX_REASSEMBLED_BYTES);
        for part in &mut parts {
            part.insert("byteLength".into(), json!(10_000));
        }
        let last = parts.len() - 1;
        for part in parts.iter().take(last).cloned() {
            assembler.push(part).unwrap();
        }
        assert!(matches!(
            assembler.push(parts[last].clone()),
            Err(ChunkError::LengthMismatch {
                declared: 10_000,
                ..
            })
        ));

        // Count changing mid-sequence.
        let mut parts = chunks("s", &frame, 6);
        let mut assembler = ChunkAssembler::new(DEFAULT_MAX_REASSEMBLED_BYTES);
        assembler.push(parts[0].clone()).unwrap();
        parts[1].insert("count".into(), json!(99));
        assert!(matches!(
            assembler.push(parts[1].clone()),
            Err(ChunkError::Inconsistent { .. })
        ));

        // Skipped index.
        let parts = chunks("s", &frame, 6);
        let mut assembler = ChunkAssembler::new(DEFAULT_MAX_REASSEMBLED_BYTES);
        assembler.push(parts[0].clone()).unwrap();
        assert!(matches!(
            assembler.push(parts[2].clone()),
            Err(ChunkError::OutOfOrder {
                expected: 1,
                got: 2,
                ..
            })
        ));

        // Interrupted, then truncated.
        let mut assembler = ChunkAssembler::new(DEFAULT_MAX_REASSEMBLED_BYTES);
        assembler.push(parts[0].clone()).unwrap();
        assert!(matches!(
            assembler.interrupt(),
            Some(ChunkError::Interrupted { .. })
        ));
        assembler.push(parts[0].clone()).unwrap();
        assert!(matches!(
            assembler.finish(),
            Some(ChunkError::Truncated { .. })
        ));
    }

    #[test]
    fn enforces_reassembly_limit_and_index_bounds() {
        let frame = json!({"type": "x", "v": "0123456789abcdef"});
        let parts = chunks("big", &frame, 4);
        let mut assembler = ChunkAssembler::new(8);
        assert!(matches!(
            assembler.push(parts[0].clone()),
            Err(ChunkError::TooLarge { limit: 8, .. })
        ));
        let mut bad = parts[0].clone();
        bad.insert("index".into(), json!(50));
        let mut assembler = ChunkAssembler::new(DEFAULT_MAX_REASSEMBLED_BYTES);
        assert!(matches!(assembler.push(bad), Err(ChunkError::Malformed(_))));
        let mut bad = parts[0].clone();
        bad.insert("data".into(), json!("***"));
        assert!(matches!(
            assembler.push(bad),
            Err(ChunkError::Base64 { .. })
        ));
    }

    #[tokio::test]
    async fn reader_recovers_after_bad_frames() {
        let good = json!({"type": "response", "id": "1"});
        let chunked = chunk_frame("c", &json!({"type": "big", "n": 1}), 4);
        let mut input = String::new();
        input.push_str("not json\n");
        input.push_str("[1,2]\n");
        input.push('\n');
        input.push_str(&format!("{}\n", "x".repeat(300)));
        input.push_str(&format!("{}\n", chunked[0]));
        input.push_str(&format!("{good}\r\n"));
        for part in &chunked {
            input.push_str(&format!("{part}\n"));
        }
        input.push_str(&format!("{}", chunked[0]));
        let limits = FrameLimits {
            max_frame_bytes: 200,
            max_reassembled_bytes: 1024,
        };
        let mut reader = FrameReader::new(input.as_bytes(), limits);
        assert!(matches!(
            reader.next_frame().await,
            Err(FrameError::InvalidJson(_))
        ));
        assert!(matches!(
            reader.next_frame().await,
            Err(FrameError::NotObject)
        ));
        assert!(matches!(
            reader.next_frame().await,
            Err(FrameError::LineTooLong { limit: 200 })
        ));
        assert!(matches!(
            reader.next_frame().await,
            Err(FrameError::Chunk(ChunkError::Interrupted { .. }))
        ));
        assert_eq!(
            Value::Object(reader.next_frame().await.unwrap().unwrap()),
            good
        );
        assert_eq!(
            Value::Object(reader.next_frame().await.unwrap().unwrap()),
            json!({"type": "big", "n": 1})
        );
        assert!(matches!(
            reader.next_frame().await,
            Err(FrameError::Chunk(ChunkError::Truncated { .. }))
        ));
        assert!(reader.next_frame().await.unwrap().is_none());
    }

    #[test]
    fn encode_enforces_limit() {
        let limits = FrameLimits {
            max_frame_bytes: 16,
            max_reassembled_bytes: 16,
        };
        assert_eq!(
            encode_frame(&json!({"a": 1}), limits).unwrap(),
            b"{\"a\":1}\n"
        );
        assert!(matches!(
            encode_frame(&json!({"a": "0123456789abcdef"}), limits),
            Err(FrameError::OutboundTooLarge { .. })
        ));
    }
}
