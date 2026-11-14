use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::goal::GoalSnapshot;
use crate::turn_loop::types::{LLM, LLMChatParams, LLMMessage};

pub const JUDGE_SYSTEM_PROMPT: &str = r#"You are an independent judge evaluating whether a goal has been completed.

Your role is to verify — with concrete evidence — that the stated goal and its
completion criterion have been fully satisfied. You are NOT the assistant; you
must NOT defer to the assistant's self-assessment.

## Evaluation Framework

1. **Decompose** the completion criterion into individual checkable conditions.
2. **For each condition**, search the transcript for DIRECT evidence:
   - Command outputs (exit codes, stdout/stderr)
   - File contents shown in tool results
   - Test results (pass/fail counts)
   - Explicit confirmations from tool execution
3. **Classify evidence**:
   - DIRECT: a tool result or command output that unambiguously proves the condition
   - INFERRED: the condition is likely met based on context but not explicitly shown
   - MISSING: no evidence found in the transcript
4. **Verdict rules**:
   - ALL conditions have DIRECT evidence → {"ok": true}
   - Any condition has only INFERRED or MISSING evidence → {"ok": false}
   - Goal is self-contradictory or impossible → {"ok": false, "impossible": true}

## Output Format

Return ONLY a JSON object (no markdown fences, no extra text):

{"ok": true, "reason": "Each condition verified: (1) ... [evidence: ...], (2) ... [evidence: ...]"}

or

{"ok": false, "reason": "Condition X not verified: expected ... but transcript shows ..."}

or

{"ok": false, "impossible": true, "reason": "Goal cannot be achieved because ..."}

## Important

- "The assistant said it was done" is NOT evidence. Look for tool outputs.
- When in doubt, return {"ok": false}.
- A partially completed goal is NOT complete.
- Do not invent evidence that is not in the transcript."#;

const RETRY_SYSTEM_PROMPT: &str = "You are a judge. Return ONLY a JSON object with fields \"ok\" (boolean) and \"reason\" (string). No other text.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeVerdict {
    pub ok: bool,
    pub impossible: bool,
    pub reason: String,
}

pub fn goal_snapshot_from_domain(value: &serde_json::Value) -> Option<GoalSnapshot> {
    let nested = value.get("goal")?;
    if nested.is_null() {
        return None;
    }
    serde_json::from_value(nested.clone()).ok()
}

impl JudgeVerdict {
    fn allow(reason: impl Into<String>) -> Self {
        Self {
            ok: true,
            impossible: false,
            reason: reason.into(),
        }
    }
}

pub fn build_judge_user_prompt(objective: &str, completion_criterion: Option<&str>) -> String {
    let mut lines: Vec<String> = vec!["## Goal Objective".into(), objective.to_string()];
    if let Some(criterion) = completion_criterion {
        lines.push(String::new());
        lines.push("## Completion Criterion".into());
        lines.push(criterion.to_string());
        lines.push(String::new());
        lines.push("## Evaluation Instructions".into());
        lines.push(
            "You MUST verify EACH condition in the completion criterion independently.".into(),
        );
        lines.push(
            "For each condition, cite the specific transcript evidence (tool output, command \
             result, file content) that proves it."
                .into(),
        );
        lines.push("If any condition lacks direct evidence, the goal is NOT complete.".into());
    } else {
        lines.push(String::new());
        lines.push("## Evaluation Instructions".into());
        lines.push(
            "No explicit completion criterion was provided. Judge whether the objective".into(),
        );
        lines.push("has been fully achieved based on the transcript evidence.".into());
    }
    lines.push(String::new());
    lines.push("## Task".into());
    lines.push(
        "Based on the conversation transcript above, produce your verdict as a single JSON object."
            .into(),
    );
    lines.join("\n")
}

pub fn build_retry_user_prompt(goal: &GoalSnapshot) -> String {
    let mut text = format!("The goal objective is: {}\n", goal.objective);
    if let Some(criterion) = &goal.completion_criterion {
        text.push_str(&format!("Completion criterion: {criterion}\n"));
    }
    text.push_str(
        "Based on the conversation transcript above, is this goal complete? Return ONLY \
         {\"ok\": true/false, \"reason\": \"...\"}",
    );
    text
}

pub fn parse_verdict(text: &str) -> Option<JudgeVerdict> {
    if let Some(verdict) = try_parse_verdict_json(text.trim()) {
        return Some(verdict);
    }
    if let Some(fenced) = fenced_json(text) {
        if let Some(verdict) = try_parse_verdict_json(fenced.trim()) {
            return Some(verdict);
        }
    }
    let candidates = extract_json_candidates(text);
    for candidate in candidates.iter().rev() {
        if let Some(verdict) = try_parse_verdict_json(candidate) {
            return Some(verdict);
        }
    }
    None
}

fn fenced_json(text: &str) -> Option<&str> {
    let start = text.find("```")?;
    let after = &text[start + 3..];
    let after = after
        .strip_prefix("json")
        .or_else(|| after.strip_prefix("JSON"))
        .unwrap_or(after);
    let body_start = after.find('\n').map(|index| index + 1).unwrap_or(0);
    let body = &after[body_start..];
    let end = body.find("```")?;
    Some(&body[..end])
}

fn extract_json_candidates(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut candidates = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '{' {
            index += 1;
            continue;
        }
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escape = false;
        let mut end = None;
        for position in index..chars.len() {
            let ch = chars[position];
            if escape {
                escape = false;
                continue;
            }
            if ch == '\\' && in_string {
                escape = true;
                continue;
            }
            if ch == '"' {
                in_string = !in_string;
                continue;
            }
            if in_string {
                continue;
            }
            if ch == '{' {
                depth += 1;
            } else if ch == '}' {
                depth -= 1;
                if depth == 0 {
                    end = Some(position);
                    break;
                }
            }
        }
        match end {
            Some(end) => {
                candidates.push(chars[index..=end].iter().collect());
                index = end + 1;
            }
            None => break,
        }
    }
    candidates
}

fn try_parse_verdict_json(raw: &str) -> Option<JudgeVerdict> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let ok = value.get("ok")?.as_bool()?;
    let reason = value.get("reason")?.as_str()?.to_string();
    let impossible = value
        .get("impossible")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Some(JudgeVerdict {
        ok: !impossible && ok,
        impossible,
        reason,
    })
}

async fn request_verdict(
    llm: &dyn LLM,
    transcript: &[LLMMessage],
    system_prompt: &str,
    user_prompt: String,
    cancel: Option<CancellationToken>,
    agent_id: &str,
    emit: Option<&(dyn Fn(serde_json::Value) + Send + Sync)>,
    max_output_tokens: u32,
) -> Result<String, String> {
    let mut messages = Vec::with_capacity(transcript.len() + 2);
    messages.push(LLMMessage::system(system_prompt));
    messages.extend_from_slice(transcript);
    messages.push(LLMMessage::user(user_prompt));
    let params = LLMChatParams {
        messages: Arc::from(messages),
        tools: Arc::from(Vec::new()),
        cancel: cancel.clone(),
        used_context_tokens: None,
        max_context_tokens: None,
        max_output_tokens: Some(max_output_tokens),
        max_output_size: None,
    };
    if let Some(emit) = emit {
        emit(crate::session::agent_records::llm_request_record(
            llm,
            &params,
            crate::session::agent_records::LlmRequestSpec {
                agent_id,
                kind: "loop",
                turn_step: None,
                tool_select: false,
                dropped_count: None,
            },
        ));
    }
    let response = match cancel {
        Some(token) => tokio::select! {
            biased;
            _ = token.cancelled() => return Err("cancelled".into()),
            response = llm.chat(params) => response,
        },
        None => llm.chat(params).await,
    };
    response
        .map(|response| response.content.trim().to_string())
        .map_err(|error| error.to_string())
}

pub async fn evaluate_transcript(
    llm: &dyn LLM,
    transcript: &[LLMMessage],
    goal: &GoalSnapshot,
    cancel: Option<CancellationToken>,
) -> JudgeVerdict {
    evaluate_transcript_with_events(
        llm,
        transcript,
        goal,
        cancel,
        crate::callbacks::MAIN_AGENT_ID,
        None,
    )
    .await
}

pub async fn evaluate_transcript_with_events(
    llm: &dyn LLM,
    transcript: &[LLMMessage],
    goal: &GoalSnapshot,
    cancel: Option<CancellationToken>,
    agent_id: &str,
    emit: Option<&(dyn Fn(serde_json::Value) + Send + Sync)>,
) -> JudgeVerdict {
    let judge_user = build_judge_user_prompt(&goal.objective, goal.completion_criterion.as_deref());
    let response = match request_verdict(
        llm,
        transcript,
        JUDGE_SYSTEM_PROMPT,
        judge_user,
        cancel.clone(),
        agent_id,
        emit,
        4096,
    )
    .await
    {
        Ok(content) => content,
        Err(_) => {
            return JudgeVerdict::allow(
                "Judge evaluation failed (network/timeout) — allowing completion.",
            );
        }
    };
    if let Some(verdict) = parse_verdict(&response) {
        return verdict;
    }
    let retry_prompt = build_retry_user_prompt(goal);
    if let Ok(retry) = request_verdict(
        llm,
        transcript,
        RETRY_SYSTEM_PROMPT,
        retry_prompt,
        cancel,
        agent_id,
        emit,
        512,
    )
    .await
    {
        if let Some(verdict) = parse_verdict(&retry) {
            return verdict;
        }
    }
    let preview: String = response.chars().take(200).collect();
    JudgeVerdict::allow(format!(
        "Judge response could not be parsed after retry — allowing completion. Response: {preview}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::{GoalBudgetReport, GoalStatus};
    use std::sync::Mutex;

    fn goal() -> GoalSnapshot {
        GoalSnapshot {
            goal_id: "goal-1".into(),
            objective: "Ship the feature".into(),
            completion_criterion: Some("Tests pass".into()),
            status: GoalStatus::Active,
            turns_used: 1,
            tokens_used: 10,
            input_tokens_used: 6,
            output_tokens_used: 4,
            wall_clock_ms: 1_000,
            budget: GoalBudgetReport {
                token_budget: None,
                turn_budget: None,
                wall_clock_budget_ms: None,
                remaining_tokens: None,
                remaining_turns: None,
                remaining_wall_clock_ms: None,
                token_budget_reached: false,
                turn_budget_reached: false,
                wall_clock_budget_reached: false,
                over_budget: false,
                input_tokens_used: 6,
                output_tokens_used: 4,
            },
            terminal_reason: None,
            created_at: 1,
            updated_at: 2,
        }
    }

    struct ScriptedLlm {
        answers: Mutex<Vec<Result<String, String>>>,
        requests: Mutex<Vec<Vec<LLMMessage>>>,
    }

    impl ScriptedLlm {
        fn new(answers: Vec<Result<String, String>>) -> Self {
            Self {
                answers: Mutex::new(answers),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl LLM for ScriptedLlm {
        fn system_prompt(&self) -> &str {
            "agent prompt"
        }
        fn model_name(&self) -> &str {
            "scripted"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> crate::rpc::types::BoxFuture<
            '_,
            Result<
                crate::turn_loop::types::LLMChatResponse,
                Box<dyn std::error::Error + Send + Sync>,
            >,
        > {
            self.requests.lock().unwrap().push(params.messages.to_vec());
            let answer = self.answers.lock().unwrap().remove(0);
            Box::pin(async move {
                match answer {
                    Ok(content) => Ok(crate::turn_loop::types::LLMChatResponse {
                        content,
                        thinking: vec![],
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: crate::rpc::types::TokenUsage::default(),
                        timing: None,
                    }),
                    Err(error) => Err(error.into()),
                }
            })
        }
    }

    #[test]
    fn direct_json_fenced_json_and_embedded_json_all_parse() {
        assert_eq!(
            parse_verdict(r#"{"ok": true, "reason": "done"}"#),
            Some(JudgeVerdict {
                ok: true,
                impossible: false,
                reason: "done".into()
            })
        );
        assert_eq!(
            parse_verdict("```json\n{\"ok\": false, \"reason\": \"nope\"}\n```"),
            Some(JudgeVerdict {
                ok: false,
                impossible: false,
                reason: "nope".into()
            })
        );
        assert_eq!(
            parse_verdict(
                "thinking... {\"note\":\"x\"} then {\"ok\": false, \"impossible\": true, \"reason\": \"no\"}"
            ),
            Some(JudgeVerdict {
                ok: false,
                impossible: true,
                reason: "no".into()
            })
        );
        assert_eq!(parse_verdict("no json here"), None);
        assert_eq!(parse_verdict(r#"{"ok": "yes", "reason": "x"}"#), None);
    }

    #[test]
    fn the_user_prompt_names_the_objective_and_criterion() {
        let prompt = build_judge_user_prompt("Do X", Some("Y passes"));
        assert!(prompt.contains("## Goal Objective\nDo X"));
        assert!(prompt.contains("## Completion Criterion\nY passes"));
        assert!(
            prompt.contains("If any condition lacks direct evidence, the goal is NOT complete.")
        );

        let no_criterion = build_judge_user_prompt("Do X", None);
        assert!(no_criterion.contains("No explicit completion criterion was provided."));
        assert!(!no_criterion.contains("## Completion Criterion"));
    }

    #[tokio::test]
    async fn a_rejecting_verdict_is_returned_verbatim() {
        let llm = ScriptedLlm::new(vec![Ok(
            r#"{"ok": false, "reason": "tests were never run"}"#.into(),
        )]);
        let verdict = evaluate_transcript(&llm, &[LLMMessage::user("hi")], &goal(), None).await;
        assert!(!verdict.ok);
        assert_eq!(verdict.reason, "tests were never run");
        let requests = llm.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0][0].role, "system");
        assert!(requests[0][0].content.contains("independent judge"));
        assert_eq!(requests[0].last().unwrap().role, "user");
    }

    #[tokio::test]
    async fn an_unparseable_answer_retries_once_with_the_strict_prompt() {
        let llm = ScriptedLlm::new(vec![
            Ok("I think it is done".into()),
            Ok(r#"{"ok": false, "reason": "missing evidence"}"#.into()),
        ]);
        let verdict = evaluate_transcript(&llm, &[], &goal(), None).await;
        assert!(!verdict.ok);
        assert_eq!(verdict.reason, "missing evidence");
        let requests = llm.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1][0].content.contains("Return ONLY a JSON object"));
    }

    #[tokio::test]
    async fn transport_and_parse_failures_fail_open() {
        let llm = ScriptedLlm::new(vec![Err("network down".into())]);
        let verdict = evaluate_transcript(&llm, &[], &goal(), None).await;
        assert!(verdict.ok, "a judge outage must not trap the goal");
        assert!(verdict.reason.contains("allowing completion"));

        let llm = ScriptedLlm::new(vec![Ok("still not json".into()), Ok("nor this".into())]);
        let verdict = evaluate_transcript(&llm, &[], &goal(), None).await;
        assert!(verdict.ok);
        assert!(verdict.reason.contains("could not be parsed after retry"));
    }

    #[tokio::test]
    async fn the_judge_request_is_emitted_with_the_v2_fields() {
        let llm = ScriptedLlm::new(vec![Ok("{\"ok\":true,\"reason\":\"done\"}".into())]);
        let events = std::sync::Arc::new(Mutex::new(Vec::new()));
        let emit = {
            let events = events.clone();
            move |event: serde_json::Value| events.lock().unwrap().push(event)
        };
        let verdict = evaluate_transcript_with_events(
            &llm,
            &[LLMMessage::user("hi")],
            &goal(),
            None,
            "agent-2",
            Some(&emit),
        )
        .await;
        assert!(verdict.ok);
        let events = events.lock().unwrap();
        let request = events
            .iter()
            .find(|event| event["type"] == "llm.request")
            .expect("the judge request must be recorded");
        assert_eq!(request["kind"], "loop");
        assert_eq!(request["agent_id"], "agent-2");
        assert_eq!(request["toolSelect"], false);
        assert_eq!(request["messageCount"], 2);
        assert_eq!(request["systemPrompt"], JUDGE_SYSTEM_PROMPT);
        assert_eq!(
            request["systemPromptHash"],
            crate::session::agent_records::sha256_hex(JUDGE_SYSTEM_PROMPT)
        );
        assert!(request.get("turnStep").is_none());
        assert_eq!(request["maxOutputTokens"], 4096);
    }
}
