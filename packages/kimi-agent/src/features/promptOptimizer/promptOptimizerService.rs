//! Prompt optimizer service, ported from v2
//! `features/promptOptimizer/promptOptimizerService.ts`.
//!
//! The optimizer runs one throwaway agent bound to the `promptOptimizer` profile
//! over the assembled input and returns its final message as the rewritten
//! prompt. v2 reaches that agent through its DI scope (`agentLifecycle.fork` +
//! `subagents.run` + `agentLifecycle.remove`); the engine has no scope, so the
//! subagent manager, the caller id, the flag and the configured timeout are
//! constructor arguments here, and the caller hands in the parent's conversation
//! history that v2 would inherit at fork time.
//!
//! Two v2 bindings have no per-instance carrier in the engine and are carried on
//! the config surface instead: `thinking` (effort) follows the model's configured
//! `reasoning_effort`, and the fork's `labels` are recorded by the run's own
//! event origin.

use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use kimi_core::primitives::errors::{Error2, Error2Options};
use kimi_core::primitives::errors::INTERNAL_CODE;
use kimi_core::primitives::errors::domains::prompt_optimizer::{
    PROMPT_OPTIMIZER_BUSY, PROMPT_OPTIMIZER_DISABLED, PROMPT_OPTIMIZER_EMPTY_DRAFT,
    PROMPT_OPTIMIZER_INPUT_TOO_LONG, PROMPT_OPTIMIZER_NO_OUTPUT,
    PROMPT_OPTIMIZER_OUTPUT_TOO_LONG, PROMPT_OPTIMIZER_TIMEOUT,
};

use crate::subagent::manager::{
    ForegroundTurnOutcome, SubagentManager, final_assistant_summary,
};
use crate::turn_loop::types::LLMMessage;

use super::promptOptimizer::{
    PROMPT_OPTIMIZER_AGENT_HELPER, PROMPT_OPTIMIZER_COMPRESS_INSTRUCTION,
    PROMPT_OPTIMIZER_DRAFT_CLOSE, PROMPT_OPTIMIZER_DRAFT_OPEN,
    PROMPT_OPTIMIZER_MAX_CONTEXT_LENGTH, PROMPT_OPTIMIZER_MAX_CONTEXT_TURNS,
    PROMPT_OPTIMIZER_MAX_CONTEXT_TURN_CHARS, PROMPT_OPTIMIZER_MAX_INPUT_LENGTH,
    PROMPT_OPTIMIZER_MAX_OUTPUT_LENGTH, PROMPT_OPTIMIZER_PROFILE_NAME,
    PROMPT_OPTIMIZER_TIMEOUT_MS, PROMPT_OPTIMIZER_TRUNCATION_MARKER, PromptOptimizerContext,
};

pub struct PromptOptimizerService {
    subagents: Arc<SubagentManager>,
    main_agent_id: String,
    flag_enabled: bool,
    timeout_ms: Option<u64>,
    thinking: Option<String>,
    in_flight: StdMutex<HashSet<String>>,
}

impl PromptOptimizerService {
    pub fn new(subagents: Arc<SubagentManager>, main_agent_id: impl Into<String>) -> Self {
        Self {
            subagents,
            main_agent_id: main_agent_id.into(),
            flag_enabled: false,
            timeout_ms: None,
            thinking: None,
            in_flight: StdMutex::new(HashSet::new()),
        }
    }

    pub fn with_flag_enabled(mut self, enabled: bool) -> Self {
        self.flag_enabled = enabled;
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }

    pub fn with_thinking(mut self, thinking: Option<String>) -> Self {
        self.thinking = thinking;
        self
    }

    pub async fn optimize(
        &self,
        text: &str,
        context: &PromptOptimizerContext,
        inherited_history: Option<Vec<LLMMessage>>,
    ) -> Result<String, Error2> {
        if !self.flag_enabled {
            return Err(Error2::new(
                PROMPT_OPTIMIZER_DISABLED,
                "The prompt optimizer is disabled. Turn on the prompt_optimizer experimental flag to use it.",
            ));
        }
        let timeout_ms = context
            .timeout_ms
            .or(self.timeout_ms)
            .unwrap_or(PROMPT_OPTIMIZER_TIMEOUT_MS);
        let draft = trim_js(text);
        if draft.is_empty() {
            return Err(Error2::new(
                PROMPT_OPTIMIZER_EMPTY_DRAFT,
                "There is no prompt to rewrite.",
            ));
        }
        let draft_length = draft.encode_utf16().count();
        if draft_length > PROMPT_OPTIMIZER_MAX_INPUT_LENGTH {
            return Err(Error2::with_options(
                PROMPT_OPTIMIZER_INPUT_TOO_LONG,
                format!(
                    "The draft is {draft_length} characters, over the {PROMPT_OPTIMIZER_MAX_INPUT_LENGTH} limit. Shorten it and try again."
                ),
                length_details(draft_length, PROMPT_OPTIMIZER_MAX_INPUT_LENGTH),
            ));
        }

        let _guard = InFlightGuard::acquire(self, context.session_key.clone())?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let explicit_turns = normalized_turns(context.recent_turns.as_deref());
        let agent_id = self
            .subagents
            .spawn_child(
                &self.main_agent_id,
                PROMPT_OPTIMIZER_PROFILE_NAME,
                PROMPT_OPTIMIZER_AGENT_HELPER,
            )
            .await
            .map_err(|error| {
                Error2::new(
                    INTERNAL_CODE,
                    format!("The prompt optimizer could not start: {error}"),
                )
            })?;

        let result = self
            .rewrite(
                &agent_id,
                draft,
                context,
                explicit_turns.as_deref(),
                inherited_history,
                timeout_ms,
                deadline,
            )
            .await;
        let _ = self.subagents.kill(&agent_id).await;
        result
    }

    async fn rewrite(
        &self,
        agent_id: &str,
        draft: &str,
        context: &PromptOptimizerContext,
        explicit_turns: Option<&[String]>,
        inherited_history: Option<Vec<LLMMessage>>,
        timeout_ms: u64,
        deadline: tokio::time::Instant,
    ) -> Result<String, Error2> {
        let prompt = build_optimizer_input(draft, context, explicit_turns);
        // v2 `inheritedHistory: { maxUserTurns: 0 }` when the caller passes
        // turns explicitly: they are inlined into the prompt instead.
        let history = if explicit_turns.is_some() {
            None
        } else {
            inherited_history
        };
        let mut rewritten = self
            .run_once(agent_id, &prompt, history, timeout_ms, deadline)
            .await?;
        if rewritten.is_empty() {
            return Err(Error2::new(
                PROMPT_OPTIMIZER_NO_OUTPUT,
                "The optimizer returned no text.",
            ));
        }
        if rewritten.encode_utf16().count() > PROMPT_OPTIMIZER_MAX_OUTPUT_LENGTH {
            let compress_prompt = format!("{prompt}\n\n{PROMPT_OPTIMIZER_COMPRESS_INSTRUCTION}");
            rewritten = self
                .run_once(agent_id, &compress_prompt, None, timeout_ms, deadline)
                .await?;
            if rewritten.is_empty() {
                return Err(Error2::new(
                    PROMPT_OPTIMIZER_NO_OUTPUT,
                    "The optimizer returned no text.",
                ));
            }
            let length = rewritten.encode_utf16().count();
            if length > PROMPT_OPTIMIZER_MAX_OUTPUT_LENGTH {
                return Err(Error2::with_options(
                    PROMPT_OPTIMIZER_OUTPUT_TOO_LONG,
                    format!(
                        "The rewrite is {length} characters, over the {PROMPT_OPTIMIZER_MAX_OUTPUT_LENGTH} limit, after one retry. Shorten the draft."
                    ),
                    length_details(length, PROMPT_OPTIMIZER_MAX_OUTPUT_LENGTH),
                ));
            }
        }
        Ok(rewritten)
    }

    async fn run_once(
        &self,
        agent_id: &str,
        prompt: &str,
        history: Option<Vec<LLMMessage>>,
        timeout_ms: u64,
        deadline: tokio::time::Instant,
    ) -> Result<String, Error2> {
        let run = self
            .subagents
            .run_foreground_turn_with_history(agent_id, prompt, history, None);
        let outcome = match tokio::time::timeout_at(deadline, run).await {
            Err(error) => {
                return Err(Error2::with_options(
                    PROMPT_OPTIMIZER_TIMEOUT,
                    format!("The rewrite did not finish within {timeout_ms}ms."),
                    Error2Options {
                        details: serde_json::json!({ "timeoutMs": timeout_ms })
                            .as_object()
                            .cloned(),
                        cause: Some(Box::new(error)),
                        ..Error2Options::default()
                    },
                ));
            }
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => {
                return Err(Error2::new(
                    INTERNAL_CODE,
                    format!("The prompt rewrite failed: {error}"),
                ));
            }
        };
        match outcome {
            ForegroundTurnOutcome::Completed(turn) => {
                Ok(trim_js(&final_assistant_summary(&turn.messages)).to_string())
            }
            ForegroundTurnOutcome::ParentCancelled => Err(Error2::new(
                INTERNAL_CODE,
                "The prompt rewrite was cancelled.".to_string(),
            )),
        }
    }
}

fn normalized_turns(turns: Option<&[String]>) -> Option<Vec<String>> {
    let turns = turns?;
    let trimmed: Vec<&str> = turns
        .iter()
        .map(|turn| trim_js(turn))
        .filter(|turn| !turn.is_empty())
        .collect();
    let start = trimmed.len().saturating_sub(PROMPT_OPTIMIZER_MAX_CONTEXT_TURNS);
    let kept: Vec<String> = trimmed[start..]
        .iter()
        .map(|turn| clamp(turn, PROMPT_OPTIMIZER_MAX_CONTEXT_TURN_CHARS))
        .collect();
    if kept.is_empty() {
        None
    } else {
        Some(kept)
    }
}

fn build_optimizer_input(
    draft: &str,
    context: &PromptOptimizerContext,
    turns: Option<&[String]>,
) -> String {
    let mut sections = vec![format!("Working directory: {}", context.cwd)];
    if let Some(turns) = turns {
        let block = turns
            .iter()
            .map(|turn| format!("- {turn}"))
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!(
            "Recent user turns, oldest first:\n{}",
            clamp(&block, PROMPT_OPTIMIZER_MAX_CONTEXT_LENGTH)
        ));
    }
    let draft = draft
        .replace(PROMPT_OPTIMIZER_DRAFT_CLOSE, "&lt;/draft&gt;")
        .replace(PROMPT_OPTIMIZER_DRAFT_OPEN, "&lt;draft&gt;");
    sections.push(format!(
        "{PROMPT_OPTIMIZER_DRAFT_OPEN}\n{draft}\n{PROMPT_OPTIMIZER_DRAFT_CLOSE}"
    ));
    sections.join("\n\n")
}

fn clamp(text: &str, max: usize) -> String {
    if text.encode_utf16().count() <= max {
        return text.to_string();
    }
    let prefix = String::from_utf16_lossy(&text.encode_utf16().take(max).collect::<Vec<_>>());
    format!("{prefix}{PROMPT_OPTIMIZER_TRUNCATION_MARKER}")
}

fn length_details(length: usize, limit: usize) -> Error2Options {
    Error2Options {
        details: serde_json::json!({ "length": length, "limit": limit })
            .as_object()
            .cloned(),
        ..Error2Options::default()
    }
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

struct InFlightGuard<'a> {
    service: &'a PromptOptimizerService,
    key: Option<String>,
}

impl<'a> InFlightGuard<'a> {
    fn acquire(
        service: &'a PromptOptimizerService,
        key: Option<String>,
    ) -> Result<Self, Error2> {
        let Some(key) = key else {
            return Ok(Self {
                service,
                key: None,
            });
        };
        let mut in_flight = service
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if in_flight.contains(&key) {
            return Err(Error2::new(
                PROMPT_OPTIMIZER_BUSY,
                "A prompt rewrite is already running for this session.",
            ));
        }
        in_flight.insert(key.clone());
        Ok(Self {
            service,
            key: Some(key),
        })
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        if let Some(key) = &self.key {
            self.service
                .in_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> PromptOptimizerService {
        PromptOptimizerService::new(Arc::new(SubagentManager::new()), "main")
    }

    fn context(session_key: Option<&str>) -> PromptOptimizerContext {
        PromptOptimizerContext {
            cwd: "repo".to_string(),
            recent_turns: None,
            session_key: session_key.map(str::to_string),
            timeout_ms: None,
            thinking: None,
        }
    }

    #[tokio::test]
    async fn a_disabled_flag_is_refused_before_the_draft_is_read() {
        let error = service()
            .optimize("   ", &context(None), None)
            .await
            .unwrap_err();
        assert_eq!(error.code().as_str(), PROMPT_OPTIMIZER_DISABLED);
    }

    #[tokio::test]
    async fn a_blank_draft_is_refused() {
        let error = service()
            .with_flag_enabled(true)
            .optimize("   ", &context(None), None)
            .await
            .unwrap_err();
        assert_eq!(error.code().as_str(), PROMPT_OPTIMIZER_EMPTY_DRAFT);
    }

    #[tokio::test]
    async fn a_draft_over_the_input_limit_is_refused_with_both_numbers() {
        let error = service()
            .with_flag_enabled(true)
            .optimize(
                &"x".repeat(PROMPT_OPTIMIZER_MAX_INPUT_LENGTH + 1),
                &context(None),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(error.code().as_str(), PROMPT_OPTIMIZER_INPUT_TOO_LONG);
        let limit = PROMPT_OPTIMIZER_MAX_INPUT_LENGTH.to_string();
        let message = error.message();
        assert!(message.contains(&limit), "{message}");
    }

    #[tokio::test]
    async fn a_second_rewrite_for_the_same_session_is_busy() {
        let service = service().with_flag_enabled(true);
        service
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert("session-1".to_string());
        let error = service
            .optimize("fix the bug", &context(Some("session-1")), None)
            .await
            .unwrap_err();
        assert_eq!(error.code().as_str(), PROMPT_OPTIMIZER_BUSY);
        assert!(
            service
                .in_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains("session-1"),
            "a refused acquire must not clear the holder's entry"
        );
    }

    #[tokio::test]
    async fn the_profile_is_registered_with_the_optimizer_prompt_and_read_only_tools() {
        let catalog = crate::prompt::ProfileCatalog::with_builtins();
        let profile = catalog
            .get(PROMPT_OPTIMIZER_PROFILE_NAME)
            .expect("the promptOptimizer profile is a builtin");
        assert_eq!(
            profile.role_additional,
            crate::features::promptOptimizer::promptOptimizer::PROMPT_OPTIMIZER_SYSTEM_PROMPT
        );
        assert_eq!(
            profile.tools,
            crate::features::promptOptimizer::promptOptimizer::PROMPT_OPTIMIZER_TOOLS
        );
    }
}
