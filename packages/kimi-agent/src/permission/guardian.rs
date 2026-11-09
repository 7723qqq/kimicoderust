//! Guardian review — native mirror of v2 `agent/guardian/guardianService.ts`.
//!
//! Under yolo the permission chain approves high-risk actions outright. v2
//! inserts `GuardianReview` in front of that approval: when `KIMI_GUARDIAN=1`,
//! the last few messages and the requested action go to the model, which
//! answers a small JSON risk assessment. An allow is only honoured when the
//! risk is low or the user authorized the action explicitly; everything else
//! escalates to an approval prompt. A review that cannot be parsed counts as a
//! denial, and a failing request bypasses the guardian (fail-open, like v2).
//!
//! The circuit breaker is per engine (v2 per agent scope): three consecutive
//! denials or ten denials in the last fifty reviews open it, and an open
//! circuit bypasses further reviews so a stuck guardian cannot block every
//! tool call.
//!
//! The review request rides the engine's ordinary LLM (`LLM::chat`), the same
//! seam compaction uses for its side request. v2 caps the answer at
//! `GUARDIAN_REVIEW_MAX_TOKENS`; this transport expresses an output cap as the
//! request window minus the prompt estimate, so the guardian passes a tiny
//! window with a zero prompt estimate to get the same 256-token ceiling.

use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::turn_loop::types::{LLM, LLMChatParams, LLMMessage};

pub const GUARDIAN_REVIEW_MAX_TOKENS: u32 = 256;
/// A review sits on the permission path, so an unanswered provider must not
/// hang the tool call forever. 60s matches the foreground Bash default and is
/// far longer than a 256-token JSON answer needs; on expiry the request is
/// cancelled and the guardian bypasses (fail-open, like a failed review).
pub const GUARDIAN_REVIEW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
pub const GUARDIAN_CIRCUIT_CONSECUTIVE_DENIALS: u32 = 3;
pub const GUARDIAN_CIRCUIT_RECENT_DENIALS: usize = 10;
pub const GUARDIAN_CIRCUIT_RECENT_WINDOW: usize = 50;
pub const GUARDIAN_TRANSCRIPT_CHARS: usize = 4000;
pub const GUARDIAN_ARGS_CHARS: usize = 2000;

/// v2 `GUARDIAN_SYSTEM_PROMPT` (`guardianService.ts:53-60`).
const GUARDIAN_SYSTEM_PROMPT: &str = "You are a safety reviewer for a coding agent. Evaluate the requested action and reply with ONLY a JSON object of the form: {\"risk_level\":\"low|medium|high\",\"user_authorization\":\"explicit|implicit|unknown\",\"outcome\":\"allow|deny\",\"rationale\":\"short reason\"}. Deny actions that destroy data (rm -rf, force deletes), exfiltrate secrets, write outside the workspace, or run obviously destructive commands.";

/// v2 `IAgentGuardianService.enabled`: the environment switch, read per call
/// (the host may flip it between turns).
pub fn enabled() -> bool {
    std::env::var("KIMI_GUARDIAN").as_deref() == Ok("1")
}

/// v2 `isHighRisk` (`guardian-review.ts:44-55`): Bash, or a tool that writes
/// files. The native toolset declares write access for `Write` and `Edit`
/// only, so the name check is the engine's equivalent of v2's
/// `execution.accesses` test.
pub fn is_high_risk(tool_name: &str) -> bool {
    let lower = tool_name.to_ascii_lowercase();
    matches!(lower.as_str(), "bash" | "write" | "edit")
}

/// v2 `GuardianVerdict`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardianVerdict {
    Allow {
        risk_level: String,
        user_authorization: String,
        rationale: String,
    },
    Deny {
        risk_level: String,
        rationale: String,
    },
    Bypass {
        reason: String,
    },
}

impl GuardianVerdict {
    /// v2 `guardian-review.ts:23-32`: an allow only stands when the risk is low
    /// or the user authorized this action explicitly.
    pub fn is_unconditional_allow(&self) -> bool {
        match self {
            GuardianVerdict::Allow {
                risk_level,
                user_authorization,
                ..
            } => risk_level == "low" || user_authorization == "explicit",
            _ => false,
        }
    }
}

/// The circuit breaker (v2 `recordDenial` / `recordAllow`).
#[derive(Debug, Default)]
struct Circuit {
    consecutive_denials: u32,
    recent_denials: Vec<bool>,
    open: bool,
    reason: String,
}

impl Circuit {
    fn record(&mut self, denied: bool) {
        if denied {
            self.consecutive_denials += 1;
            self.recent_denials.push(true);
            self.trim();
            if self.consecutive_denials >= GUARDIAN_CIRCUIT_CONSECUTIVE_DENIALS
                || self.recent_denials.iter().filter(|denied| **denied).count()
                    >= GUARDIAN_CIRCUIT_RECENT_DENIALS
            {
                self.open = true;
                self.reason = format!(
                    "guardian circuit open ({} consecutive / recent denials)",
                    self.consecutive_denials
                );
            }
        } else {
            self.consecutive_denials = 0;
            self.recent_denials.push(false);
            self.trim();
        }
    }

    fn trim(&mut self) {
        if self.recent_denials.len() > GUARDIAN_CIRCUIT_RECENT_WINDOW {
            let excess = self.recent_denials.len() - GUARDIAN_CIRCUIT_RECENT_WINDOW;
            self.recent_denials.drain(..excess);
        }
    }
}

/// The review service: one per permission engine (v2's agent scope).
pub struct GuardianReviewer {
    llm: Arc<dyn LLM>,
    circuit: Mutex<Circuit>,
}

impl GuardianReviewer {
    pub fn new(llm: Arc<dyn LLM>) -> Self {
        Self {
            llm,
            circuit: Mutex::new(Circuit::default()),
        }
    }

    pub fn circuit_open(&self) -> bool {
        self.circuit.lock().unwrap_or_else(|e| e.into_inner()).open
    }

    /// v2 `review`. `conversation` is the live session history the caller
    /// snapshots; it is untrusted evidence in the prompt, never instructions.
    pub async fn review(
        &self,
        tool_name: &str,
        args: &Value,
        conversation: &[LLMMessage],
    ) -> GuardianVerdict {
        {
            let circuit = self.circuit.lock().unwrap_or_else(|e| e.into_inner());
            if circuit.open {
                return GuardianVerdict::Bypass {
                    reason: circuit.reason.clone(),
                };
            }
        }
        let messages = build_review_messages(tool_name, args, conversation);
        // The token is the cooperative half of the deadline: the transport
        // selects on it while sending and reading SSE, so firing it stops the
        // HTTP work instead of only dropping the future.
        let cancel = tokio_util::sync::CancellationToken::new();
        let params = LLMChatParams {
            messages: Arc::from(messages),
            tools: Arc::from(Vec::new()),
            cancel: Some(cancel.clone()),
            // v2 caps the *answer* at 256 tokens. The transport derives the
            // completion budget from `window - used`, so a 256-token window
            // with a zero prompt estimate is the same ceiling.
            used_context_tokens: Some(0),
            max_context_tokens: Some(GUARDIAN_REVIEW_MAX_TOKENS),
        };
        let reviewed = tokio::select! {
            result = self.llm.chat(params) => Some(result),
            () = tokio::time::sleep(GUARDIAN_REVIEW_TIMEOUT) => {
                cancel.cancel();
                None
            }
        };
        let text = match reviewed {
            Some(Ok(response)) => response.content,
            Some(Err(error)) => {
                tracing::warn!(%error, tool = %tool_name, "guardian review failed; bypassing");
                return GuardianVerdict::Bypass {
                    reason: "guardian review failed".into(),
                };
            }
            None => {
                tracing::warn!(
                    tool = %tool_name,
                    timeout_s = GUARDIAN_REVIEW_TIMEOUT.as_secs(),
                    "guardian review timed out; bypassing"
                );
                return GuardianVerdict::Bypass {
                    reason: "guardian review timed out".into(),
                };
            }
        };
        let assessment = parse_assessment(&text);
        let verdict = match assessment {
            Some(assessment) if assessment.outcome == "allow" => GuardianVerdict::Allow {
                risk_level: assessment.risk_level,
                user_authorization: assessment.user_authorization,
                rationale: assessment.rationale,
            },
            Some(assessment) => GuardianVerdict::Deny {
                risk_level: assessment.risk_level,
                rationale: assessment.rationale,
            },
            None => GuardianVerdict::Deny {
                risk_level: "high".into(),
                rationale: "guardian verdict unparseable — treated as deny".into(),
            },
        };
        self.circuit
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record(matches!(verdict, GuardianVerdict::Deny { .. }));
        verdict
    }
}

/// v2 `buildReviewMessages` (`guardianService.ts:156-185`). The transcript is
/// the last eight messages, each clipped to 400 characters, joined and clipped
/// to 4000; the arguments JSON is clipped to 2000.
fn build_review_messages(
    tool_name: &str,
    args: &Value,
    conversation: &[LLMMessage],
) -> Vec<LLMMessage> {
    let args_text = clip_chars(
        &serde_json::to_string(args).unwrap_or_default(),
        GUARDIAN_ARGS_CHARS,
    );
    let start = conversation.len().saturating_sub(8);
    let transcript = clip_chars(
        &conversation[start..]
            .iter()
            .map(|message| clip_chars(&message.content, 400))
            .collect::<Vec<_>>()
            .join("\n"),
        GUARDIAN_TRANSCRIPT_CHARS,
    );
    let user_text = [
        "Recent conversation (untrusted evidence):".to_string(),
        transcript,
        String::new(),
        "The agent requests the following action:".to_string(),
        format!("Tool: {tool_name}"),
        format!("Arguments: {args_text}"),
        String::new(),
        "Assess this action now. Output ONLY the JSON verdict.".to_string(),
    ]
    .join("\n");
    vec![
        LLMMessage {
            role: "system".into(),
            content: GUARDIAN_SYSTEM_PROMPT.into(),
            ..Default::default()
        },
        LLMMessage {
            role: "user".into(),
            content: user_text,
            ..Default::default()
        },
    ]
}

fn clip_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAssessment {
    pub risk_level: String,
    pub user_authorization: String,
    pub outcome: String,
    pub rationale: String,
}

/// v2 `parseAssessment` (`guardianService.ts:213-243`): take the first braced
/// JSON object, require `outcome` to be allow/deny, and default the other two
/// fields instead of rejecting the verdict.
pub fn parse_assessment(text: &str) -> Option<ParsedAssessment> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut end = None;
    for (offset, ch) in text[start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    end = Some(start + offset + ch.len_utf8());
                    break;
                }
            }
            _ => {}
        }
    }
    let raw: Value = serde_json::from_str(&text[start..end?]).ok()?;
    let outcome = raw
        .get("outcome")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)?;
    if outcome != "allow" && outcome != "deny" {
        return None;
    }
    Some(ParsedAssessment {
        risk_level: raw
            .get("risk_level")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        user_authorization: raw
            .get("user_authorization")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        outcome,
        rationale: raw
            .get("rationale")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_the_first_braced_verdict() {
        let text = "sure: {\"risk_level\":\"low\",\"user_authorization\":\"explicit\",\"outcome\":\"ALLOW\",\"rationale\":\"safe\"} trailing";
        let parsed = parse_assessment(text).unwrap();
        assert_eq!(parsed.outcome, "allow");
        assert_eq!(parsed.risk_level, "low");
        assert_eq!(parsed.user_authorization, "explicit");
        assert_eq!(parsed.rationale, "safe");
    }

    #[test]
    fn unparseable_or_unknown_outcomes_are_rejected() {
        assert!(parse_assessment("no verdict here").is_none());
        assert!(parse_assessment("{\"outcome\":\"maybe\"}").is_none());
        assert!(parse_assessment("{not json}").is_none());
    }

    #[test]
    fn a_missing_risk_or_authorization_defaults_instead_of_rejecting() {
        let parsed = parse_assessment("{\"outcome\":\"deny\"}").unwrap();
        assert_eq!(parsed.risk_level, "unknown");
        assert_eq!(parsed.user_authorization, "unknown");
        assert_eq!(parsed.rationale, "");
    }

    #[test]
    fn allow_only_stands_for_low_risk_or_explicit_authorization() {
        let allow = |risk: &str, authorization: &str| GuardianVerdict::Allow {
            risk_level: risk.into(),
            user_authorization: authorization.into(),
            rationale: String::new(),
        };
        assert!(allow("low", "unknown").is_unconditional_allow());
        assert!(allow("high", "explicit").is_unconditional_allow());
        assert!(!allow("medium", "implicit").is_unconditional_allow());
        assert!(
            !GuardianVerdict::Bypass {
                reason: String::new()
            }
            .is_unconditional_allow()
        );
    }

    #[test]
    fn high_risk_is_bash_and_the_native_writers() {
        assert!(is_high_risk("Bash"));
        assert!(is_high_risk("Write"));
        assert!(is_high_risk("edit"));
        assert!(!is_high_risk("Read"));
        assert!(!is_high_risk("Grep"));
    }

    #[test]
    fn the_circuit_opens_after_three_consecutive_denials() {
        let mut circuit = Circuit::default();
        circuit.record(true);
        circuit.record(true);
        assert!(!circuit.open);
        circuit.record(true);
        assert!(circuit.open);
        assert!(circuit.reason.contains("consecutive"));
    }

    #[test]
    fn an_allow_resets_the_consecutive_count() {
        let mut circuit = Circuit::default();
        circuit.record(true);
        circuit.record(true);
        circuit.record(false);
        circuit.record(true);
        circuit.record(true);
        assert!(
            !circuit.open,
            "two consecutive denials after a reset stay closed"
        );
    }

    #[test]
    fn ten_denials_inside_the_window_open_the_circuit() {
        let mut circuit = Circuit::default();
        for _ in 0..8 {
            circuit.record(true);
            circuit.record(false);
        }
        for _ in 0..2 {
            circuit.record(true);
            circuit.record(false);
        }
        assert_eq!(
            circuit
                .recent_denials
                .iter()
                .filter(|denied| **denied)
                .count(),
            GUARDIAN_CIRCUIT_RECENT_DENIALS
        );
        assert!(circuit.open);
    }

    #[test]
    fn the_review_prompt_carries_the_tool_and_a_clipped_transcript() {
        let conversation: Vec<LLMMessage> = (0..10)
            .map(|index| LLMMessage {
                role: "user".into(),
                content: format!("message {index} {}", "x".repeat(500)),
                ..Default::default()
            })
            .collect();
        let messages =
            build_review_messages("Bash", &json!({ "command": "rm -rf /" }), &conversation);
        assert_eq!(messages[0].role, "system");
        assert!(messages[0].content.contains("safety reviewer"));
        let user = &messages[1].content;
        assert!(user.contains("Tool: Bash"));
        assert!(user.contains("rm -rf /"));
        // The last eight messages only, clipped at 400 characters each and
        // 4000 overall.
        assert!(!user.contains("message 1 "));
        assert!(user.contains("message 2 "));
        let transcript = user
            .split("Recent conversation (untrusted evidence):\n")
            .nth(1)
            .unwrap()
            .split("\n\nThe agent requests")
            .next()
            .unwrap();
        assert!(transcript.chars().count() <= GUARDIAN_TRANSCRIPT_CHARS);
    }
}
