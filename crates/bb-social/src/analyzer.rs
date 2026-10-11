//! Optional headless-Claude reclassification of already-collected posts, run through one
//! stateless omp session per batch (`--no-session`, no built-in tools). Claude delivers the
//! classification by calling the `submit_classification` host tool, which validates it against
//! the schema; posts Claude does not classify (or a failed/timed-out turn) keep their heuristic
//! label, confidence and manipulation risk.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use bb_omp::{
    AgentSession, CancellationToken, HostTool, HostToolHandler, HostToolOutcome, OmpError,
    PromptStatus, SessionConfig, Transport,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::SocialSignalError;
use crate::record::{SocialLabel, SocialSignalRecord};
use crate::settings::SocialSettings;

/// The only host tool registered with omp.
pub const SUBMIT_TOOL: &str = "submit_classification";

/// Standing instructions for the classifier.
pub const SYSTEM_PROMPT: &str = "Classify read-only crypto social posts for trading evidence. \
Detect hype, coordinated pump/dump or rug/scam risk conservatively. \
Do not give trading instructions. Deliver the classification by calling submit_classification \
exactly once with a JSON object matching its schema; if it returns an error, fix the \
classification and call it again. Do not answer in plain text.";

/// Classifies posts with Claude through omp (`SOCIAL_SIGNAL_AI_COMMAND`, `SOCIAL_SIGNAL_AI_MODEL`,
/// `SOCIAL_SIGNAL_AI_TIMEOUT_SECONDS`).
#[derive(Debug, Clone)]
pub struct HeadlessSocialAnalyzer<'a> {
    settings: &'a SocialSettings,
}

impl<'a> HeadlessSocialAnalyzer<'a> {
    pub fn new(settings: &'a SocialSettings) -> Self {
        Self { settings }
    }

    /// `omp --mode rpc --no-tools ... --model <model> --system-prompt ... --no-session`; the
    /// command is shell-split so a wrapper (`bunx omp`) works.
    pub fn session_config(&self) -> Result<SessionConfig, SocialSignalError> {
        let mut argv = shlex::split(&self.settings.ai_command)
            .unwrap_or_default()
            .into_iter();
        let program = argv
            .next()
            .ok_or_else(|| SocialSignalError::Ai("social signal AI command is empty".to_owned()))?;
        Ok(SessionConfig {
            program: program.into(),
            program_args: argv.collect(),
            model: Some(self.settings.ai_model.clone()),
            system_prompt: Some(SYSTEM_PROMPT.to_owned()),
            session_dir: None,
            isolated: true,
            ..SessionConfig::default()
        })
    }

    /// Records with the AI-provided label/confidence/risk where Claude submitted one. Starts omp
    /// only for a non-empty batch and shuts it down afterwards; any AI failure keeps the
    /// heuristic records (logged as a warning).
    pub async fn classify(&self, records: Vec<SocialSignalRecord>) -> Vec<SocialSignalRecord> {
        if records.is_empty() {
            return records;
        }
        let session = match self.session_config() {
            Ok(config) => AgentSession::start(config).await.map_err(omp_error),
            Err(error) => Err(error),
        };
        self.run(session, records).await
    }

    /// [`Self::classify`] over an existing transport (tests, custom launchers).
    pub async fn classify_over(
        &self,
        transport: Transport,
        records: Vec<SocialSignalRecord>,
    ) -> Vec<SocialSignalRecord> {
        if records.is_empty() {
            return records;
        }
        let session = match self.session_config() {
            Ok(config) => AgentSession::connect(transport, config)
                .await
                .map_err(omp_error),
            Err(error) => Err(error),
        };
        self.run(session, records).await
    }

    async fn run(
        &self,
        session: Result<AgentSession, SocialSignalError>,
        records: Vec<SocialSignalRecord>,
    ) -> Vec<SocialSignalRecord> {
        let mut session = match session {
            Ok(session) => session,
            Err(error) => {
                tracing::warn!(%error, "social signal AI unavailable; keeping heuristic labels");
                return records;
            }
        };
        let tool = Arc::new(ClassificationTool::new(&records));
        let turn = self.turn(&mut session, tool.clone(), &records).await;
        if let Err(error) = session.shutdown().await {
            tracing::warn!(%error, "social signal omp shutdown failed");
        }
        if let Err(error) = &turn {
            tracing::warn!(%error, "social signal AI classification failed");
        }
        let Some(classified) = tool.take() else {
            if turn.is_ok() {
                tracing::warn!("social signal AI returned no classified signals");
            }
            return records;
        };
        records
            .into_iter()
            .map(|record| match classified.get(&record.message_id) {
                Some(item) => item.apply(record),
                None => record,
            })
            .collect()
    }

    async fn turn(
        &self,
        session: &mut AgentSession,
        tool: Arc<ClassificationTool>,
        records: &[SocialSignalRecord],
    ) -> Result<(), SocialSignalError> {
        session
            .set_host_tools(vec![tool_definition()], tool)
            .await
            .map_err(omp_error)?;
        let outcome = match session
            .prompt(&prompt(records), self.settings.ai_timeout)
            .await
        {
            Ok(outcome) => outcome,
            Err(OmpError::PromptTimeout { after, outcome }) => {
                tracing::info!(cost_usd = %outcome.usage.cost_usd, "social signal AI turn aborted");
                return Err(SocialSignalError::Ai(format!(
                    "social signal AI classification timed out after {}s",
                    after.as_secs_f64()
                )));
            }
            Err(error) => return Err(omp_error(error)),
        };
        tracing::info!(
            status = ?outcome.status,
            posts = records.len(),
            input_tokens = outcome.usage.input_tokens,
            output_tokens = outcome.usage.output_tokens,
            cost_usd = %outcome.usage.cost_usd,
            "social signal AI turn finished"
        );
        match outcome.status {
            PromptStatus::Completed => Ok(()),
            status => Err(SocialSignalError::Ai(format!(
                "social signal AI turn ended {status:?}: {:?}",
                outcome.error
            ))),
        }
    }
}

fn omp_error(error: OmpError) -> SocialSignalError {
    SocialSignalError::Ai(format!("social signal AI omp error: {error}"))
}

/// User turn: the posts to classify.
fn prompt(records: &[SocialSignalRecord]) -> String {
    let posts: Vec<Value> = records
        .iter()
        .map(|r| {
            json!({
                "assets": r.assets,
                "excerpt": r.excerpt,
                "message_id": r.message_id,
                "source": r.source,
            })
        })
        .collect();
    format!(
        "Classify these posts and submit the result with {SUBMIT_TOOL}:\n{}",
        Value::Array(posts)
    )
}

/// `submit_classification` with the classification schema as its parameters.
pub fn tool_definition() -> HostTool {
    HostTool::new(
        SUBMIT_TOOL,
        "Submit classification",
        "Submit the classification of the posts: {\"signals\": [{message_id, label, confidence 0..1, \
manipulation_risk 0..1}]}, one entry per post, using the message_id values from the prompt. \
Validated against the schema; call again only after an error.",
        schema(),
    )
}

/// JSON schema of the tool arguments.
pub fn schema() -> Value {
    let labels: Vec<&str> = SocialLabel::ALL.iter().map(|l| l.as_str()).collect();
    json!({
        "type": "object",
        "required": ["signals"],
        "properties": {
            "signals": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["message_id", "label", "confidence", "manipulation_risk"],
                    "properties": {
                        "message_id": {"type": "string"},
                        "label": {"type": "string", "enum": labels},
                        "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                        "manipulation_risk": {"type": "number", "minimum": 0, "maximum": 1},
                    },
                },
            }
        },
    })
}

#[derive(Deserialize)]
struct Submission {
    signals: Vec<SubmittedSignal>,
}

#[derive(Deserialize)]
struct SubmittedSignal {
    message_id: String,
    label: String,
    confidence: f64,
    manipulation_risk: f64,
}

/// One validated classification.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Classification {
    label: SocialLabel,
    confidence: f64,
    manipulation_risk: f64,
}

impl Classification {
    fn apply(&self, record: SocialSignalRecord) -> SocialSignalRecord {
        SocialSignalRecord {
            label: self.label,
            confidence: self.confidence,
            manipulation_risk: self.manipulation_risk,
            ..record
        }
    }
}

/// `message_id → classification` when `arguments` match the schema and name only posts of the batch.
fn validate(
    arguments: Value,
    message_ids: &HashSet<String>,
) -> Result<HashMap<String, Classification>, String> {
    let submission: Submission = serde_json::from_value(arguments)
        .map_err(|err| format!("arguments do not match the schema: {err}"))?;
    let mut classified = HashMap::with_capacity(submission.signals.len());
    for (index, signal) in submission.signals.into_iter().enumerate() {
        if !message_ids.contains(&signal.message_id) {
            return Err(format!(
                "signals[{index}].message_id {:?} is not one of the posts to classify",
                signal.message_id
            ));
        }
        let label = SocialLabel::parse(&signal.label).ok_or_else(|| {
            let labels: Vec<&str> = SocialLabel::ALL.iter().map(|l| l.as_str()).collect();
            format!(
                "signals[{index}].label {:?} must be one of {}",
                signal.label,
                labels.join(", ")
            )
        })?;
        let confidence = unit_interval(index, "confidence", signal.confidence)?;
        let manipulation_risk =
            unit_interval(index, "manipulation_risk", signal.manipulation_risk)?;
        classified.insert(
            signal.message_id,
            Classification {
                label,
                confidence,
                manipulation_risk,
            },
        );
    }
    Ok(classified)
}

fn unit_interval(index: usize, field: &str, value: f64) -> Result<f64, String> {
    if (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "signals[{index}].{field} {value} must be between 0 and 1"
        ))
    }
}

/// Serves `submit_classification` and keeps the last accepted submission.
struct ClassificationTool {
    message_ids: HashSet<String>,
    accepted: Mutex<Option<HashMap<String, Classification>>>,
}

impl ClassificationTool {
    fn new(records: &[SocialSignalRecord]) -> Self {
        Self {
            message_ids: records.iter().map(|r| r.message_id.clone()).collect(),
            accepted: Mutex::new(None),
        }
    }

    fn take(&self) -> Option<HashMap<String, Classification>> {
        self.accepted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

#[async_trait]
impl HostToolHandler for ClassificationTool {
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        _cancel: CancellationToken,
    ) -> HostToolOutcome {
        if name != SUBMIT_TOOL {
            return HostToolOutcome::error(format!("unknown tool {name}"));
        }
        match validate(arguments, &self.message_ids) {
            Ok(classified) => {
                let count = classified.len();
                *self.accepted.lock().unwrap_or_else(PoisonError::into_inner) = Some(classified);
                HostToolOutcome::text(json!({"accepted": true, "classified": count}).to_string())
            }
            Err(message) => HostToolOutcome::error(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use bb_omp::codec::{FrameLimits, FrameReader};
    use serde_json::Map;
    use tokio::io::{AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};

    use super::*;
    use crate::record::SocialSource;

    /// In-process fake omp RPC server over `tokio::io::duplex`.
    struct Server {
        reader: FrameReader<ReadHalf<DuplexStream>>,
        writer: WriteHalf<DuplexStream>,
    }

    impl Server {
        fn pair() -> (Self, Transport) {
            let (client, server) = tokio::io::duplex(1 << 20);
            let (client_read, client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            (
                Self {
                    reader: FrameReader::new(server_read, FrameLimits::default()),
                    writer: server_write,
                },
                Transport::new(client_read, client_write),
            )
        }

        async fn send(&mut self, frame: Value) {
            let mut line = frame.to_string();
            line.push('\n');
            self.writer.write_all(line.as_bytes()).await.unwrap();
        }

        async fn expect(&mut self, kind: &str) -> (String, Map<String, Value>) {
            let frame = tokio::time::timeout(Duration::from_secs(5), self.reader.next_frame())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(frame["type"], kind, "{frame:?}");
            (frame["id"].as_str().unwrap_or_default().to_owned(), frame)
        }

        async fn respond(&mut self, id: &str, command: &str, data: Value) {
            self.send(json!({"id": id, "type": "response", "command": command, "success": true, "data": data}))
                .await;
        }

        /// Handshake, host tool registration, then the prompt; returns (prompt id, prompt text).
        async fn boot_and_prompt(&mut self) -> (String, String) {
            self.send(
                json!({"type": "ready", "protocolVersion": 1, "supportedProtocolVersions": [1, 2],
                             "maxFrameBytes": 1_048_576, "maxReassembledFrameBytes": 67_108_864}),
            )
            .await;
            let (id, _) = self.expect("negotiate_protocol").await;
            self.respond(&id, "negotiate_protocol", json!({"protocolVersion": 2}))
                .await;
            let (id, frame) = self.expect("set_event_filter").await;
            self.respond(&id, "set_event_filter", json!({"events": frame["events"]}))
                .await;
            let (id, frame) = self.expect("set_host_tools").await;
            let tools = frame["tools"].as_array().unwrap();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0]["name"], SUBMIT_TOOL);
            assert_eq!(tools[0]["parameters"], schema());
            self.respond(&id, "set_host_tools", json!({"toolNames": [SUBMIT_TOOL]}))
                .await;
            let (pid, frame) = self.expect("prompt").await;
            let message = frame["message"].as_str().unwrap().to_owned();
            self.respond(&pid, "prompt", Value::Null).await;
            (pid, message)
        }

        /// Claude calls `submit_classification` with `args`; returns the host result frame.
        async fn submit(&mut self, call_id: &str, args: Value) -> Map<String, Value> {
            self.send(json!({"type": "tool_execution_start", "toolCallId": call_id, "toolName": SUBMIT_TOOL, "args": args}))
                .await;
            self.send(json!({"type": "host_tool_call", "id": format!("h-{call_id}"), "toolCallId": call_id,
                             "toolName": SUBMIT_TOOL, "arguments": args}))
                .await;
            let (_, result) = self.expect("host_tool_result").await;
            let is_error = result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            self.send(
                json!({"type": "tool_execution_end", "toolCallId": call_id, "toolName": SUBMIT_TOOL,
                             "result": result["result"], "isError": is_error}),
            )
            .await;
            result
        }

        async fn finish(&mut self, pid: &str) {
            self.send(json!({"type": "message_end", "message": {"role": "assistant",
                "content": [{"type": "text", "text": "Submitted."}], "stopReason": "stop",
                "usage": {"input": 700, "output": 60, "cacheRead": 0, "cacheWrite": 0, "cost": {"total": 0.001}}}}))
                .await;
            self.send(json!({"type": "prompt_result", "id": pid, "agentInvoked": true, "status": "completed", "sessionSettled": true}))
                .await;
        }
    }

    fn record(id: &str) -> SocialSignalRecord {
        SocialSignalRecord {
            assets: vec!["EDEN".into()],
            author_hash: "a".into(),
            channel_hash: "c".into(),
            confidence: 0.4,
            engagement: BTreeMap::new(),
            excerpt: "EDEN neutral mention".into(),
            label: SocialLabel::NeutralMention,
            manipulation_risk: 0.0,
            message_id: id.into(),
            source: SocialSource::Telegram,
            timestamp: "2026-05-27T12:00:00+00:00".into(),
            urls: vec![],
        }
    }

    fn settings() -> SocialSettings {
        SocialSettings {
            ai_enabled: true,
            ai_model: "haiku".into(),
            ..SocialSettings::default()
        }
    }

    #[tokio::test]
    async fn submitted_classification_overrides_heuristic_label() {
        let (mut server, transport) = Server::pair();
        let script = tokio::spawn(async move {
            let (pid, message) = server.boot_and_prompt().await;
            let result = server
                .submit("t1", json!({"signals": [
                    {"message_id": "m1", "label": "bearish_warning", "confidence": 0.82, "manipulation_risk": 0.25},
                    {"message_id": "m2", "label": "manipulation_risk", "confidence": 0.9, "manipulation_risk": 1}]}))
                .await;
            server.finish(&pid).await;
            (message, result)
        });
        let settings = settings();
        let classified = HeadlessSocialAnalyzer::new(&settings)
            .classify_over(transport, vec![record("m1"), record("m2"), record("m3")])
            .await;
        let (message, result) = script.await.unwrap();

        assert!(message.contains("\"message_id\":\"m1\"") && message.contains(SUBMIT_TOOL));
        assert!(message.contains("EDEN neutral mention"));
        assert_ne!(result.get("isError"), Some(&json!(true)));
        assert!(result["result"].to_string().contains("accepted"));
        assert_eq!(classified[0].label, SocialLabel::BearishWarning);
        assert!((classified[0].confidence - 0.82).abs() < f64::EPSILON);
        assert!((classified[0].manipulation_risk - 0.25).abs() < f64::EPSILON);
        assert_eq!(classified[1].label, SocialLabel::ManipulationRisk);
        assert!((classified[1].manipulation_risk - 1.0).abs() < f64::EPSILON);
        // Not classified by Claude: heuristic record kept.
        assert_eq!(classified[2], record("m3"));
    }

    #[tokio::test]
    async fn invalid_tool_arguments_are_rejected_and_fall_back() {
        let (mut server, transport) = Server::pair();
        let script = tokio::spawn(async move {
            let (pid, _) = server.boot_and_prompt().await;
            let bad_label = server
                .submit("t1", json!({"signals": [
                    {"message_id": "m1", "label": "bogus", "confidence": 0.5, "manipulation_risk": 0.1}]}))
                .await;
            let out_of_range = server
                .submit("t2", json!({"signals": [
                    {"message_id": "m1", "label": "bullish_call", "confidence": 7, "manipulation_risk": 0.1}]}))
                .await;
            let unknown_post = server
                .submit("t3", json!({"signals": [
                    {"message_id": "ghost", "label": "bullish_call", "confidence": 0.5, "manipulation_risk": 0.1}]}))
                .await;
            let wrong_type = server
                .submit("t4", json!({"signals": [
                    {"message_id": "m1", "label": "bullish_call", "confidence": 0.5, "manipulation_risk": "x"}]}))
                .await;
            server.finish(&pid).await;
            [bad_label, out_of_range, unknown_post, wrong_type]
        });
        let settings = settings();
        let classified = HeadlessSocialAnalyzer::new(&settings)
            .classify_over(transport, vec![record("m1"), record("m2")])
            .await;
        let results = script.await.unwrap();

        for result in &results {
            assert_eq!(result["isError"], true, "{result:?}");
        }
        assert!(results[0]["result"].to_string().contains("must be one of"));
        assert!(results[1]["result"].to_string().contains("between 0 and 1"));
        assert!(
            results[2]["result"]
                .to_string()
                .contains("not one of the posts")
        );
        assert!(
            results[3]["result"]
                .to_string()
                .contains("do not match the schema")
        );
        assert_eq!(classified, vec![record("m1"), record("m2")]);
    }

    #[tokio::test]
    async fn timed_out_turn_is_aborted_and_falls_back() {
        let (mut server, transport) = Server::pair();
        let script = tokio::spawn(async move {
            let (pid, _) = server.boot_and_prompt().await;
            let (aid, _) = server.expect("abort").await;
            server.respond(&aid, "abort", Value::Null).await;
            server
                .send(json!({"type": "prompt_result", "id": pid, "agentInvoked": true, "status": "aborted", "sessionSettled": true}))
                .await;
        });
        let settings = SocialSettings {
            ai_timeout: Duration::from_millis(200),
            ..settings()
        };
        let classified = HeadlessSocialAnalyzer::new(&settings)
            .classify_over(transport, vec![record("m1")])
            .await;
        script.await.unwrap();
        assert_eq!(classified, vec![record("m1")]);
    }

    #[tokio::test]
    async fn empty_batch_or_command_skips_omp() {
        let settings = settings();
        assert!(
            HeadlessSocialAnalyzer::new(&settings)
                .classify(Vec::new())
                .await
                .is_empty()
        );
        let blank = SocialSettings {
            ai_command: "  ".into(),
            ..settings
        };
        assert_eq!(
            HeadlessSocialAnalyzer::new(&blank)
                .classify(vec![record("m1")])
                .await,
            vec![record("m1")]
        );
    }

    #[test]
    fn session_is_isolated_stateless_and_uses_the_configured_model() {
        let settings = SocialSettings {
            ai_command: "bunx omp".into(),
            ..settings()
        };
        let config = HeadlessSocialAnalyzer::new(&settings)
            .session_config()
            .unwrap();
        assert_eq!(config.program, std::path::PathBuf::from("bunx"));
        assert_eq!(config.program_args, ["omp"]);
        let args = config.args();
        assert!(args.contains(&"--no-session".to_owned()));
        assert!(args.contains(&"--no-tools".to_owned()));
        assert!(args.windows(2).any(|w| w == ["--model", "haiku"]));
        assert_eq!(SocialSettings::default().ai_command, "omp");
    }
}
