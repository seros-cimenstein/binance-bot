//! Telegram ELI5 messages (HTML) for audit rows. Alerts are never fatal.

use std::sync::Arc;

use async_trait::async_trait;
use bb_core::telegram::Telegram;
use serde_json::Value;

use crate::audit::{AuditRow, RowStatus};

/// Marker of a cycle turn that failed on every configured model (no orders were placed).
pub const BRAIN_UNAVAILABLE: &str = "brain unavailable on all models";

/// One plain HTML notice about the trader's brain (model switches, outages).
pub fn brain_notice(text: &str) -> String {
    format!("<b>BOT SPOT BRAIN</b>\nModel: {}", code(text))
}

/// Python `TELEGRAM_MESSAGE_MAX_CHARS`.
pub const MAX_CHARS: usize = 3900;

/// Collapse whitespace runs (Python `_compact_text`).
pub fn compact(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `<code>escaped</code>` (Python `html.escape(quote=True)`).
pub fn code(text: &str) -> String {
    format!(
        "<code>{}</code>",
        html_escape::encode_quoted_attribute(&compact(text))
    )
}

/// Truncate to `limit` chars with `...` (0 = unlimited).
pub fn short_text(text: &str, limit: usize) -> String {
    let text = compact(text);
    if limit == 0 || text.chars().count() <= limit {
        return text;
    }
    let kept: String = text.chars().take(limit.saturating_sub(3)).collect();
    format!("{}...", kept.trim_end())
}

fn selected_str<'a>(row: &'a AuditRow, key: &str) -> &'a str {
    row.selected_route
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// `SRC->DST (route_id)`, the bare route id, or `n/a`.
pub fn route_label(row: &AuditRow) -> String {
    let pick = |decision: &str, key: &str| -> String {
        let value = if decision.trim().is_empty() {
            selected_str(row, key)
        } else {
            decision
        };
        value.trim().to_owned()
    };
    let route_id = pick(&row.decision.route_id, "route_id");
    let source = pick(&row.decision.source_asset, "source_asset");
    let destination = pick(&row.decision.destination_asset, "destination_asset");
    if !source.is_empty() && !destination.is_empty() {
        let label = format!("{source}->{destination}");
        return if route_id.is_empty() {
            label
        } else {
            format!("{label} ({route_id})")
        };
    }
    if route_id.is_empty() {
        "n/a".into()
    } else {
        route_id
    }
}

/// Decision reason, else the error, else the status.
pub fn decision_reason(row: &AuditRow) -> String {
    let reason = compact(&row.decision.reason);
    if !reason.is_empty() {
        return reason;
    }
    if row.error.is_empty() {
        row.status.as_str().into()
    } else {
        compact(&row.error)
    }
}

/// One plain-language sentence for the row.
pub fn eli5(row: &AuditRow) -> String {
    let route = route_label(row);
    let reason = decision_reason(row).to_lowercase();
    match row.status {
        RowStatus::Submitted => format!(
            "The bot chose and executed {route}: every leg passed balance, scope, exchange-filter, \
             order-test and fill reconciliation checks."
        ),
        RowStatus::Planned => {
            format!(
                "The bot chose {route}, but this cycle was plan-only, so no real order was allowed."
            )
        }
        RowStatus::Failed if row.submission_attempted => format!(
            "The bot tried {route}, but execution failed, so it stopped instead of continuing with an uncertain \
             wallet state."
        ),
        RowStatus::Failed => {
            "The cycle failed before any order was submitted, so the bot did not trade.".into()
        }
        RowStatus::Blocked => {
            "A safety or configuration check blocked the cycle before trading.".into()
        }
        RowStatus::Held => {
            if reason.contains(BRAIN_UNAVAILABLE) {
                "The bot's brain was unavailable on every configured model, so the bot placed no orders and held."
                    .into()
            } else if reason.contains("timed out") {
                "The bot did not answer in time, so the safe move was to hold.".into()
            } else if reason.contains("previous live route failed") {
                "A previous live route failed, so the bot is waiting instead of compounding an uncertain state.".into()
            } else if reason.contains("llm cost") {
                "The daily LLM cost cap was reached, so the bot paused instead of asking the model."
                    .into()
            } else if row.candidate_route_count == 0 {
                "No valid trade route passed the filters, so trading would mean guessing and paying fees.".into()
            } else {
                "The bot judged holding to be better than paying fees for the available trade setup."
                    .into()
            }
        }
        RowStatus::Reviewed => {
            "The cycle recorded a decision and kept the full details in the audit log.".into()
        }
    }
}

/// Who decided the row, in words (rows before Phase 51 carry no `decider` and were the LLM's).
pub fn decider_label(decider: &str) -> &'static str {
    match decider {
        "algo" => "rule-based algorithm (no LLM)",
        _ => "LLM",
    }
}

fn lines(row: &AuditRow, explain: bool, reason_chars: usize, include_why: bool) -> Vec<String> {
    let mut lines = vec![
        "<b>BOT SPOT BRAIN</b>".to_owned(),
        format!("Status: {}", code(row.status.as_str())),
        format!("Decider: {}", code(decider_label(&row.decider))),
        format!("Action: {}", code(&row.decision.action)),
        format!("Route: {}", code(&route_label(row))),
    ];
    if explain {
        lines.push(format!("ELI5: {}", code(&eli5(row))));
        lines.push(format!(
            "Candidates: {}",
            code(&row.candidate_route_count.to_string())
        ));
        lines.push(format!(
            "Submitted: {}",
            code(if row.submission_attempted {
                "yes"
            } else {
                "no"
            })
        ));
        if include_why {
            let why = short_text(&decision_reason(row), reason_chars);
            if !why.is_empty() {
                lines.push(format!("Why: {}", code(&why)));
            }
        }
    }
    lines.push(format!("Audit: {}", code(&row.audit_path)));
    lines
}

/// Python `_format_alert_messages`: one message, or a header plus `WHY` chunks.
pub fn format_messages(row: &AuditRow, explain: bool, reason_chars: usize) -> Vec<String> {
    let message = lines(row, explain, reason_chars, true).join("\n");
    if message.chars().count() <= MAX_CHARS {
        return vec![message];
    }
    if !explain {
        return split_text(&message);
    }
    let header = lines(row, explain, reason_chars, false).join("\n");
    let why = short_text(&decision_reason(row), reason_chars);
    let mut messages = split_text(&header);
    if why.is_empty() {
        return messages;
    }
    let prefix = "<b>BOT SPOT WHY</b>\n";
    let limit = MAX_CHARS - prefix.chars().count();
    messages.extend(
        split_code_chunks(&why, limit)
            .into_iter()
            .map(|chunk| format!("{prefix}{}", code(&chunk))),
    );
    messages
}

/// Line-preserving split at [`MAX_CHARS`].
pub fn split_text(message: &str) -> Vec<String> {
    if message.chars().count() <= MAX_CHARS {
        return vec![message.to_owned()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in message.lines() {
        let candidate = if current.is_empty() {
            line.to_owned()
        } else {
            format!("{current}\n{line}")
        };
        if candidate.chars().count() <= MAX_CHARS {
            current = candidate;
            continue;
        }
        if !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        if line.chars().count() <= MAX_CHARS {
            current = line.to_owned();
        } else {
            let chars: Vec<char> = line.chars().collect();
            chunks.extend(
                chars
                    .chunks(MAX_CHARS)
                    .map(|part| part.iter().collect::<String>()),
            );
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Chunks whose `<code>`-wrapped, escaped form fits `max_wrapped`, preferring word breaks.
pub fn split_code_chunks(text: &str, max_wrapped: usize) -> Vec<String> {
    let clean = compact(text);
    let max_wrapped = max_wrapped.max("<code>x</code>".len());
    let fits =
        |chars: &[char]| code(&chars.iter().collect::<String>()).chars().count() <= max_wrapped;
    let mut chunks = Vec::new();
    let mut remaining: Vec<char> = clean.chars().collect();
    while !remaining.is_empty() {
        if fits(&remaining) {
            chunks.push(remaining.iter().collect());
            break;
        }
        let (mut low, mut high, mut best) = (1usize, remaining.len(), 1usize);
        while low <= high {
            let middle = (low + high) / 2;
            let slice = trim_end_chars(&remaining[..middle]);
            if fits(slice) {
                best = middle;
                low = middle + 1;
            } else {
                high = middle - 1;
            }
        }
        let mut split_at = best;
        if let Some(space) = remaining[..best].iter().rposition(|c| *c == ' ')
            && space > best / 2
        {
            split_at = space;
        }
        let mut chunk: String = remaining[..split_at]
            .iter()
            .collect::<String>()
            .trim()
            .to_owned();
        if chunk.is_empty() {
            chunk = remaining[..best]
                .iter()
                .collect::<String>()
                .trim()
                .to_owned();
            split_at = best.max(1);
        }
        chunks.push(chunk);
        remaining = remaining[split_at..]
            .iter()
            .collect::<String>()
            .trim()
            .chars()
            .collect();
    }
    chunks
}

fn trim_end_chars(chars: &[char]) -> &[char] {
    let end = chars
        .iter()
        .rposition(|c| !c.is_whitespace())
        .map_or(0, |i| i + 1);
    &chars[..end]
}

/// Delivers one HTML alert; `true` when delivered. Implemented for [`Telegram`].
#[async_trait]
pub trait AlertSink: Send + Sync + std::fmt::Debug {
    async fn send(&self, html: &str) -> bool;
}

#[async_trait]
impl AlertSink for Telegram {
    async fn send(&self, html: &str) -> bool {
        Telegram::send(self, html).await
    }
}

/// Alert sender bound to the alert settings.
#[derive(Debug, Clone)]
pub struct Alerter {
    sink: Option<Arc<dyn AlertSink>>,
    explain: bool,
    reason_chars: usize,
}

impl Alerter {
    pub fn new(sink: Option<Arc<dyn AlertSink>>, explain: bool, reason_chars: usize) -> Self {
        Self {
            sink,
            explain,
            reason_chars,
        }
    }

    /// `true` only when every message was delivered.
    pub async fn send(&self, row: &AuditRow) -> bool {
        let Some(sink) = &self.sink else {
            return false;
        };
        let mut sent = true;
        for message in format_messages(row, self.explain, self.reason_chars) {
            sent = sink.send(&message).await && sent;
        }
        sent
    }

    /// Send one [`brain_notice`] (callers send it after the cycle's audit row is durable).
    pub async fn send_notice(&self, text: &str) -> bool {
        match &self.sink {
            Some(sink) => sink.send(&brain_notice(text)).await,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::audit::{Decision, RowType};
    use crate::universe::tests::config;

    fn row(status: RowStatus, decision: Decision) -> AuditRow {
        let mut row = AuditRow::new(&config(&[]), RowType::Route, status, decision);
        row.candidate_route_count = 3;
        row
    }

    fn rebalance(reason: &str) -> Decision {
        Decision {
            action: "rebalance".into(),
            reason: reason.into(),
            route_id: "USDT->ETH:abc".into(),
            source_asset: "USDT".into(),
            destination_asset: "ETH".into(),
            spend_mode: "all_available".into(),
            spend_amount: None,
        }
    }

    #[test]
    fn eli5_classes() {
        let mut held = row(RowStatus::Held, Decision::hold("nothing beats fees"));
        assert!(eli5(&held).contains("holding to be better"));
        held.candidate_route_count = 0;
        assert!(eli5(&held).contains("No valid trade route"));
        held.decision.reason = "The bot timed out after 300s".into();
        assert!(eli5(&held).contains("did not answer in time"));
        held.decision.reason = format!(
            "{BRAIN_UNAVAILABLE}: anthropic/x timeout (The bot timed out after 150s); no further orders this cycle"
        );
        assert!(eli5(&held).contains("unavailable on every configured model"));
        assert!(eli5(&held).contains("no orders"));
        assert!(brain_notice("back on <primary>").contains("back on &lt;primary&gt;"));
        held.decision.reason = "previous live route failed after submitting".into();
        assert!(eli5(&held).contains("previous live route failed"));
        held.decision.reason = "daily LLM cost cap reached".into();
        assert!(eli5(&held).contains("cost cap"));

        let planned = row(RowStatus::Planned, rebalance("momentum"));
        assert!(
            eli5(&planned)
                .contains("The bot chose USDT->ETH (USDT->ETH:abc), but this cycle was plan-only")
        );
        assert!(
            eli5(&row(RowStatus::Submitted, rebalance("go")))
                .contains("The bot chose and executed USDT->ETH")
        );
        let mut failed = row(RowStatus::Failed, rebalance("go"));
        assert!(eli5(&failed).contains("before any order"));
        failed.submission_attempted = true;
        assert!(eli5(&failed).contains("execution failed"));
        assert!(
            eli5(&row(RowStatus::Blocked, Decision::hold(""))).contains("safety or configuration")
        );
    }

    #[test]
    fn alert_escapes_html_and_truncates_reason() {
        let message = format_messages(
            &row(
                RowStatus::Planned,
                rebalance("<b>buy</b> & \"hold\" 'x' long tail"),
            ),
            true,
            20,
        )
        .join("\n");
        assert!(message.starts_with("<b>BOT SPOT BRAIN</b>"));
        assert!(
            message.contains("Why: <code>&lt;b&gt;buy&lt;/b&gt; &amp; &quot;hol...</code>"),
            "{message}"
        );
        assert!(message.contains("Route: <code>USDT-&gt;ETH (USDT-&gt;ETH:abc)</code>"));
        assert!(message.contains("Audit: <code>data/bot-spot-decisions.jsonl</code>"));
    }

    #[test]
    fn alert_names_the_decider() {
        let mut planned = row(RowStatus::Planned, rebalance("score"));
        let llm = format_messages(&planned, true, 0).join("\n");
        assert!(llm.contains("Decider: <code>LLM</code>"), "{llm}");
        planned.decider = "algo".into();
        let algo = format_messages(&planned, true, 0).join("\n");
        assert!(algo.contains("Decider: <code>rule-based algorithm (no LLM)</code>"), "{algo}");
    }

    #[test]
    fn reason_chars_zero_keeps_full_reason_and_long_reason_splits() {
        let reason = "full why text ".repeat(80);
        let message =
            format_messages(&row(RowStatus::Held, Decision::hold(&reason)), true, 0).join("\n");
        assert!(message.contains(reason.trim()));

        let long = "after fee edge remains strong ".repeat(450);
        let messages = format_messages(&row(RowStatus::Held, Decision::hold(&long)), true, 0);
        assert!(messages.len() > 2);
        assert!(messages.iter().all(|m| m.chars().count() <= MAX_CHARS));
        assert!(!messages[0].contains("Why:"));
        assert!(messages[1].starts_with("<b>BOT SPOT WHY</b>\n<code>"));
        let rebuilt: Vec<String> = messages[1..]
            .iter()
            .map(|m| {
                m.trim_start_matches("<b>BOT SPOT WHY</b>\n<code>")
                    .trim_end_matches("</code>")
                    .to_owned()
            })
            .collect();
        assert_eq!(rebuilt.join(" "), compact(&long));
    }

    #[test]
    fn route_label_falls_back_to_selected_route() {
        let mut row = row(RowStatus::Failed, Decision::hold(""));
        assert_eq!(route_label(&row), "n/a");
        row.selected_route =
            json!({"route_id": "BTC->ETH:1", "source_asset": "BTC", "destination_asset": "ETH"});
        assert_eq!(route_label(&row), "BTC->ETH (BTC->ETH:1)");
    }

    #[tokio::test]
    async fn disabled_telegram_reports_not_sent() {
        let alerter = Alerter::new(None, true, 0);
        assert!(
            !alerter
                .send(&row(RowStatus::Held, Decision::hold("x")))
                .await
        );
    }
}
