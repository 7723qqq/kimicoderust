use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::callbacks::MAIN_AGENT_ID;
use crate::goal::GoalSnapshot;
use crate::goal::judge::{JUDGE_SYSTEM_PROMPT, JudgeVerdict};
use crate::turn_loop::types::{LLM, LLMChatParams, LLMMessage};

use super::judgePrompt::build_judge_user_prompt;

const RETRY_SYSTEM_PROMPT: &str = "You are a judge. Return ONLY a JSON object with fields \"ok\" (boolean) and \"reason\" (string). No other text.";

pub struct AgentGoalJudgeService {
    llm: Arc<dyn LLM>,
    history: Arc<dyn Fn() -> Vec<LLMMessage> + Send + Sync>,
    max_output_size: Arc<dyn Fn() -> Option<u32> + Send + Sync>,
    agent_id: String,
}

impl AgentGoalJudgeService {
    pub fn new(
        llm: Arc<dyn LLM>,
        history: Arc<dyn Fn() -> Vec<LLMMessage> + Send + Sync>,
        max_output_size: Arc<dyn Fn() -> Option<u32> + Send + Sync>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            llm,
            history,
            max_output_size,
            agent_id: agent_id.into(),
        }
    }

    pub async fn evaluate(
        &self,
        goal: &GoalSnapshot,
        signal: Option<CancellationToken>,
    ) -> JudgeVerdict {
        if self.agent_id != MAIN_AGENT_ID {
            return JudgeVerdict {
                ok: true,
                impossible: false,
                reason: "Judge skipped: not main agent.".into(),
            };
        }
        if let Some(verdict) = self.evaluate_via_subagent(goal, signal.clone()).await {
            return verdict;
        }
        self.evaluate_from_transcript(goal, signal).await
    }

    async fn evaluate_via_subagent(
        &self,
        goal: &GoalSnapshot,
        _signal: Option<CancellationToken>,
    ) -> Option<JudgeVerdict> {
        tracing::debug!(goal_id = %goal.goal_id, "goal.judge.subagent.start");
        tracing::debug!(
            goal_id = %goal.goal_id,
            reason = "subagent-by-profile not yet wired in fork",
            "goal.judge.subagent.skipped"
        );
        None
    }

    async fn evaluate_from_transcript(
        &self,
        goal: &GoalSnapshot,
        signal: Option<CancellationToken>,
    ) -> JudgeVerdict {
        let history = (self.history)();
        let judge_user =
            build_judge_user_prompt(&goal.objective, goal.completion_criterion.as_deref());
        let max_output_size = (self.max_output_size)().unwrap_or(4096).min(4096);
        tracing::debug!(
            goal_id = %goal.goal_id,
            message_count = history.len() + 1,
            "goal.judge.transcript.start"
        );

        let response = self
            .request(
                &history,
                JUDGE_SYSTEM_PROMPT,
                judge_user,
                max_output_size,
                signal.clone(),
            )
            .await;
        let response_text = match response {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(goal_id = %goal.goal_id, %error, "goal.judge.transcript.error");
                return JudgeVerdict {
                    ok: true,
                    impossible: false,
                    reason: "Judge evaluation failed (network/timeout) — allowing completion."
                        .into(),
                };
            }
        };

        if let Some(verdict) = parse_verdict(&response_text) {
            tracing::debug!(
                goal_id = %goal.goal_id,
                ok = verdict.ok,
                reason = %verdict.reason.chars().take(200).collect::<String>(),
                "goal.judge.transcript.result"
            );
            return verdict;
        }

        tracing::warn!(
            goal_id = %goal.goal_id,
            preview = %response_text.chars().take(200).collect::<String>(),
            "goal.judge.transcript.parseFailed"
        );

        let retry_prompt = build_retry_user_prompt(goal);
        if let Ok(retry_text) = self
            .request(&history, RETRY_SYSTEM_PROMPT, retry_prompt, 512, signal)
            .await
            && let Some(verdict) = parse_verdict(&retry_text)
        {
            return verdict;
        }

        tracing::warn!(goal_id = %goal.goal_id, "goal.judge.transcript.retryFailed");
        JudgeVerdict {
            ok: true,
            impossible: false,
            reason: format!(
                "Judge response could not be parsed after retry — allowing completion. Response: {}",
                response_text.chars().take(200).collect::<String>()
            ),
        }
    }

    async fn request(
        &self,
        transcript: &[LLMMessage],
        system_prompt: &str,
        user_prompt: String,
        max_output_size: u32,
        signal: Option<CancellationToken>,
    ) -> Result<String, String> {
        let mut messages = Vec::with_capacity(transcript.len() + 2);
        messages.push(LLMMessage::system(system_prompt));
        messages.extend_from_slice(transcript);
        messages.push(LLMMessage::user(user_prompt));
        let params = LLMChatParams {
            messages: Arc::from(messages),
            tools: Arc::from(Vec::new()),
            cancel: signal.clone(),
            used_context_tokens: None,
            max_context_tokens: None,
            max_output_tokens: None,
            max_output_size: Some(max_output_size),
        };
        let response = match signal {
            Some(token) => tokio::select! {
                biased;
                _ = token.cancelled() => return Err("cancelled".into()),
                response = self.llm.chat(params) => response,
            },
            None => self.llm.chat(params).await,
        };
        response
            .map(|response| response.content.trim().to_string())
            .map_err(|error| error.to_string())
    }
}

fn build_retry_user_prompt(goal: &GoalSnapshot) -> String {
    let mut text = format!("The goal objective is: {}\n", goal.objective);
    if let Some(criterion) = goal
        .completion_criterion
        .as_deref()
        .filter(|criterion| !criterion.is_empty())
    {
        text.push_str(&format!("Completion criterion: {criterion}\n"));
    }
    text.push_str(
        "Based on the conversation transcript above, is this goal complete? Return ONLY {\"ok\": true/false, \"reason\": \"...\"}",
    );
    text
}

fn parse_verdict(text: &str) -> Option<JudgeVerdict> {
    if let Some(verdict) = try_parse_verdict_json(text) {
        return Some(verdict);
    }
    if let Some(fenced) = fenced_json(text)
        && let Some(verdict) = try_parse_verdict_json(fenced.trim())
    {
        return Some(verdict);
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
    let after = after.strip_prefix("json").unwrap_or(after);
    let body = after.trim_start_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}');
    let end = body.find("```")?;
    Some(&body[..end])
}

fn extract_json_candidates(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut candidates = Vec::new();
    for index in 0..chars.len() {
        if chars[index] != '{' {
            continue;
        }
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escape = false;
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
                    candidates.push(chars[index..=position].iter().collect());
                    break;
                }
            }
        }
    }
    candidates
}

fn try_parse_verdict_json(raw: &str) -> Option<JudgeVerdict> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let ok = value.get("ok")?.as_bool()?;
    let reason = value.get("reason")?.as_str()?.to_string();
    let impossible = value.get("impossible").and_then(serde_json::Value::as_bool) == Some(true);
    Some(JudgeVerdict {
        ok: !impossible && ok,
        impossible,
        reason,
    })
}
