//! End-to-end candidate pipeline:
//! detector → pump.fun `CreateEvent` decode → safety scorer → [`TradeCandidate`].
//!
//! Raydium events are skipped: their init logs do not embed the mint, and
//! extracting it needs `getTransaction`, which is unreliable on public RPC.

use std::path::Path;
use std::time::Duration;

use futures::{Stream, StreamExt as _};
use serde::Serialize;

use crate::decoder::{PumpFunCreateEvent, decode_create_event_from_logs};
use crate::detector::{
    DEFAULT_COMMITMENT, PUMP_FUN_LABEL, PoolCreationEvent, PoolCreationStream, default_programs,
};
use crate::error::SniperError;
use crate::scorer::{RpcScorer, SafetyThresholds, TokenSafetyScore, TokenScorer};

/// A pool creation that survived mint extraction + safety scoring.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TradeCandidate {
    pub pool_event: PoolCreationEvent,
    pub create_event: PumpFunCreateEvent,
    pub safety: TokenSafetyScore,
    pub evaluated_at_ms: i64,
}

pub struct CandidatePipeline<E, S> {
    events: E,
    scorer: S,
    emit_rejected: bool,
}

impl<E, S> CandidatePipeline<E, S>
where
    E: Stream<Item = PoolCreationEvent> + Unpin + Send,
    S: TokenScorer,
{
    /// `emit_rejected` also yields candidates the scorer rejected (audit/debug runs).
    pub fn new(events: E, scorer: S, emit_rejected: bool) -> Self {
        Self {
            events,
            scorer,
            emit_rejected,
        }
    }

    /// Next evaluated candidate; `None` when the event source ends.
    pub async fn next_candidate(&mut self) -> Option<TradeCandidate> {
        while let Some(pool_event) = self.events.next().await {
            if pool_event.program_label != PUMP_FUN_LABEL {
                continue;
            }
            let Some(create) = decode_create_event_from_logs(&pool_event.log_excerpt) else {
                continue;
            };
            let safety = match self.scorer.score(&create.mint).await {
                Ok(safety) => safety,
                Err(err) => {
                    // Scorer errors never crash the stream.
                    tracing::debug!(mint = %create.mint, error = %err, "scorer failed; skipping");
                    continue;
                }
            };
            if !self.emit_rejected && !safety.is_tradeable() {
                continue;
            }
            return Some(TradeCandidate {
                pool_event,
                create_event: create,
                safety,
                evaluated_at_ms: bb_core::time::now_ms(),
            });
        }
        None
    }

    pub fn into_stream(self) -> impl Stream<Item = TradeCandidate> + Send + Unpin
    where
        E: 'static,
        S: 'static,
    {
        Box::pin(futures::stream::unfold(self, |mut pipeline| async move {
            pipeline
                .next_candidate()
                .await
                .map(|candidate| (candidate, pipeline))
        }))
    }
}

/// Connect the live detector to an RPC scorer with default thresholds.
pub async fn live_candidate_stream(
    ws_url: &str,
    rpc_http_url: &str,
    client: reqwest::Client,
    emit_rejected: bool,
) -> Result<impl Stream<Item = TradeCandidate> + Send + Unpin, SniperError> {
    let events = PoolCreationStream::connect(ws_url, default_programs(), DEFAULT_COMMITMENT)
        .await?
        .into_stream();
    let scorer = RpcScorer::new(client, rpc_http_url, SafetyThresholds::default());
    Ok(CandidatePipeline::new(events, scorer, emit_rejected).into_stream())
}

/// `sniper-pipeline` stdout.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum PipelineOutcome {
    Completed {
        seen: usize,
        tradeable: usize,
        rejected: usize,
        audit_path: String,
    },
    TimedOut {
        timed_out_after_seconds: f64,
        audit_path: String,
    },
}

/// Audit every emitted candidate until the stream ends, `max_candidates`
/// is reached, or `run_duration` elapses.
pub async fn run_pipeline<C>(
    candidates: C,
    audit_path: &Path,
    max_candidates: Option<usize>,
    run_duration: Option<Duration>,
) -> Result<PipelineOutcome, SniperError>
where
    C: Stream<Item = TradeCandidate> + Unpin,
{
    let audit = bb_core::audit::JsonlLog::new(audit_path);
    let audit_path_str = audit_path.display().to_string();
    let drive = async {
        let mut candidates = candidates;
        let (mut seen, mut tradeable, mut rejected) = (0, 0, 0);
        while let Some(candidate) = candidates.next().await {
            audit.append(&candidate)?;
            seen += 1;
            if candidate.safety.is_tradeable() {
                tradeable += 1;
            } else {
                rejected += 1;
            }
            if max_candidates.is_some_and(|max| seen >= max) {
                break;
            }
        }
        Ok::<_, SniperError>(PipelineOutcome::Completed {
            seen,
            tradeable,
            rejected,
            audit_path: audit_path_str.clone(),
        })
    };
    match run_duration {
        None => drive.await,
        Some(duration) => match tokio::time::timeout(duration, drive).await {
            Ok(result) => result,
            Err(_) => Ok(PipelineOutcome::TimedOut {
                timed_out_after_seconds: duration.as_secs_f64(),
                audit_path: audit_path_str.clone(),
            }),
        },
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use parking_lot::Mutex;

    use super::*;
    use crate::decoder::tests::build_create_event_payload;
    use crate::detector::{PUMP_FUN_PROGRAM_ID, RAYDIUM_AMM_V4_PROGRAM_ID};

    pub(crate) fn pump_event(mint: &[u8; 32]) -> PoolCreationEvent {
        let payload =
            build_create_event_payload("Test", "TST", "https://x/m.json", mint, &[0; 32], &[0; 32]);
        PoolCreationEvent {
            program_label: "pump_fun".into(),
            program_id: PUMP_FUN_PROGRAM_ID.into(),
            signature: "testsig".into(),
            slot: 1,
            detected_at_ms: 0,
            matched_markers: vec!["Program log: Instruction: Create".into()],
            log_excerpt: vec![format!("Program data: {payload}")],
        }
    }

    fn raydium_event() -> PoolCreationEvent {
        PoolCreationEvent {
            program_label: "raydium_amm_v4".into(),
            program_id: RAYDIUM_AMM_V4_PROGRAM_ID.into(),
            signature: "raysig".into(),
            slot: 2,
            detected_at_ms: 0,
            matched_markers: vec!["Program log: initialize2".into()],
            log_excerpt: vec!["Program log: initialize2".into()],
        }
    }

    pub(crate) fn safe_score(mint: &str) -> TokenSafetyScore {
        TokenSafetyScore {
            mint: mint.into(),
            has_mint_authority: Some(false),
            has_freeze_authority: Some(false),
            supply: Some(1_000_000),
            decimals: Some(6),
            top_holder_pct: 2.0,
            top_10_pct: 20.0,
            holder_count_top_20: 50,
            top_holders: Vec::new(),
            score: 80.0,
            rejection_reason: None,
        }
    }

    enum Verdict {
        Safe,
        Rejected,
        Fails,
    }

    struct FakeScorer {
        verdict: Verdict,
        calls: Mutex<Vec<String>>,
    }

    impl FakeScorer {
        fn new(verdict: Verdict) -> Self {
            Self {
                verdict,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl TokenScorer for &FakeScorer {
        async fn score(&self, mint: &str) -> Result<TokenSafetyScore, SniperError> {
            self.calls.lock().push(mint.to_owned());
            match self.verdict {
                Verdict::Safe => Ok(safe_score(mint)),
                Verdict::Rejected => Ok(TokenSafetyScore {
                    has_mint_authority: Some(true),
                    score: 0.0,
                    rejection_reason: Some("mint authority not renounced".into()),
                    ..safe_score(mint)
                }),
                Verdict::Fails => Err(SniperError::Http(
                    reqwest::Client::new().get("not a url").build().unwrap_err(),
                )),
            }
        }
    }

    async fn collect(
        events: Vec<PoolCreationEvent>,
        scorer: &FakeScorer,
        emit_rejected: bool,
    ) -> Vec<TradeCandidate> {
        let mut pipeline =
            CandidatePipeline::new(futures::stream::iter(events), scorer, emit_rejected);
        let mut out = Vec::new();
        while let Some(c) = pipeline.next_candidate().await {
            out.push(c);
        }
        out
    }

    #[tokio::test]
    async fn skips_raydium_events() {
        let scorer = FakeScorer::new(Verdict::Safe);
        assert!(
            collect(vec![raydium_event()], &scorer, false)
                .await
                .is_empty()
        );
        assert!(scorer.calls.lock().is_empty());
    }

    #[tokio::test]
    async fn decodes_pump_event_and_calls_scorer() {
        let scorer = FakeScorer::new(Verdict::Safe);
        let mint: [u8; 32] = std::array::from_fn(|i| i as u8);
        let candidates = collect(vec![pump_event(&mint)], &scorer, false).await;
        assert_eq!(candidates.len(), 1);
        assert_eq!(*scorer.calls.lock(), vec![bs58::encode(mint).into_string()]);
        assert_eq!(candidates[0].create_event.name, "Test");
        assert!(candidates[0].safety.is_tradeable());
    }

    #[tokio::test]
    async fn filters_rejected_unless_emit_rejected() {
        let scorer = FakeScorer::new(Verdict::Rejected);
        assert!(
            collect(vec![pump_event(&[1; 32])], &scorer, false)
                .await
                .is_empty()
        );
        let candidates = collect(vec![pump_event(&[1; 32])], &scorer, true).await;
        assert_eq!(candidates.len(), 1);
        assert!(!candidates[0].safety.is_tradeable());
    }

    #[tokio::test]
    async fn scorer_errors_do_not_crash_stream() {
        let scorer = FakeScorer::new(Verdict::Fails);
        assert!(
            collect(vec![pump_event(&[1; 32])], &scorer, false)
                .await
                .is_empty()
        );
        assert_eq!(scorer.calls.lock().len(), 1);
    }

    #[tokio::test]
    async fn skips_pump_events_without_decodable_data() {
        let scorer = FakeScorer::new(Verdict::Safe);
        let mut event = pump_event(&[1; 32]);
        event.log_excerpt = vec!["Program ComputeBudget invoke [1]".into()];
        assert!(collect(vec![event], &scorer, false).await.is_empty());
        assert!(scorer.calls.lock().is_empty());
    }

    #[tokio::test]
    async fn run_pipeline_audits_and_counts_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("candidates.jsonl");
        let scorer = FakeScorer::new(Verdict::Safe);
        let events = vec![
            pump_event(&[1; 32]),
            raydium_event(),
            pump_event(&[2; 32]),
            pump_event(&[3; 32]),
        ];
        let pipeline = CandidatePipeline::new(futures::stream::iter(events), &scorer, false);
        let candidates = Box::pin(futures::stream::unfold(pipeline, |mut p| async move {
            p.next_candidate().await.map(|c| (c, p))
        }));
        let outcome = run_pipeline(candidates, &audit, Some(2), None)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            PipelineOutcome::Completed {
                seen: 2,
                tradeable: 2,
                rejected: 0,
                audit_path: audit.display().to_string()
            }
        );
        let rows: Vec<serde_json::Value> =
            bb_core::audit::JsonlLog::new(&audit).read_all().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["create_event"]["symbol"], "TST");
        assert_eq!(rows[0]["safety"]["score"], 80.0);
    }
}
