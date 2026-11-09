//! Session title composition and the managed `chat_title` call.
//!
//! Mirrors v2's `sessionTitleService` + `agentTitlePromptSourceService`: the
//! deterministic sources (`first_turn` / `user_prompts`) derive a title from
//! the history, and `digest` asks the managed platform for one through the
//! `chat_title` tool. The wire shape is the one `packages/oauth`'s
//! `fetchChatTitle` already speaks — `POST {base}/tools` with
//! `{method, params:{chat_content}}` answering `{title}`.
//!
//! The credential comes from the LLM's managed seam
//! ([`crate::turn_loop::types::LLM::media_target`] +
//! [`crate::turn_loop::types::LLM::media_upload_credential`]): both are gated
//! on `auth_provider`, which is the engine's marker for v2's
//! `modelSource === 'oauth-catalog'`, and together they carry exactly what a
//! managed call needs — the base URL and a refreshed bearer token. Media
//! upload is the other caller of that seam; the name is media-shaped but the
//! gate and the payload are the same.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::turn_loop::types::{LLM, LLMChatParams, LLMMessage};

/// v2 `MAX_GENERATED_TITLE_LENGTH`.
const MAX_GENERATED_TITLE_LENGTH: usize = 200;
/// v2 `MAX_TITLE_INPUT_LENGTH`.
const MAX_TITLE_INPUT_LENGTH: usize = 1000;
/// v2 `MAX_TITLE_PROMPTS`.
const MAX_TITLE_PROMPTS: usize = 3;
/// v2 `MAX_TITLE_USER_SEGMENT`.
const MAX_TITLE_USER_SEGMENT: usize = 400;
/// v2 `MAX_TITLE_FIRST_TURN_ASSISTANT`.
const MAX_TITLE_FIRST_TURN_ASSISTANT: usize = 300;
/// v2 `MAX_TITLE_DIGEST_USER_SEGMENT`.
const MAX_TITLE_DIGEST_USER_SEGMENT: usize = 200;
/// v2 `MAX_TITLE_DIGEST_ASSISTANT`.
const MAX_TITLE_DIGEST_ASSISTANT: usize = 200;
/// v2 `MAX_TITLE_DIGEST_INPUT_LENGTH`.
const MAX_TITLE_DIGEST_INPUT_LENGTH: usize = 3000;
/// v2 `TITLE_DIGEST_ELISION_MARKER`.
const TITLE_DIGEST_ELISION_MARKER: &str = "...";
/// v2 `fetchChatTitle`'s default request timeout.
const CHAT_TITLE_TIMEOUT: Duration = Duration::from_secs(8);

/// Truncate to at most `max` characters.
///
/// v2 slices by UTF-16 code unit; a character count is the closest safe
/// equivalent. `String::truncate` counts bytes and panics when the cut lands
/// inside a code point, which a CJK prompt reaches immediately.
fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// The indices of the natural-language user prompts in `history`.
///
/// v2's `isNaturalLanguagePrompt` also excludes messages whose origin is not
/// `user`; the fork's tool results carry their own `tool` role, so the role
/// check is the whole predicate here.
fn user_prompt_indexes(history: &[LLMMessage]) -> Vec<usize> {
    history
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == "user" && !message.content.trim().is_empty())
        .map(|(index, _)| index)
        .collect()
}

/// The last assistant text in `span` (v2 `finalAssistantText`).
fn final_assistant_text(span: &[LLMMessage]) -> Option<String> {
    span.iter()
        .rev()
        .filter(|message| message.role == "assistant")
        .map(|message| message.content.trim())
        .find(|text| !text.is_empty())
        .map(str::to_string)
}

/// The `chat_content` for `source`, or `None` when the history cannot supply
/// one (v2 `composeTitleInput`).
pub fn compose_title_input(history: &[LLMMessage], source: &str) -> Option<String> {
    let prompts = user_prompt_indexes(history);
    match source {
        "first_turn" => {
            let first = *prompts.first()?;
            let span_end = prompts.get(1).copied().unwrap_or(history.len());
            let user = history.get(first)?.content.trim();
            let assistant = final_assistant_text(&history[first + 1..span_end])?;
            Some(format!(
                "user: {}\nassistant: {}",
                truncate_chars(user, MAX_TITLE_USER_SEGMENT),
                truncate_chars(&assistant, MAX_TITLE_FIRST_TURN_ASSISTANT),
            ))
        }
        "digest" => {
            let mut turns: Vec<Vec<String>> = Vec::new();
            for (position, index) in prompts.iter().enumerate() {
                let span_end = prompts.get(position + 1).copied().unwrap_or(history.len());
                let user = history.get(*index)?.content.trim();
                let mut group = vec![format!(
                    "user: {}",
                    truncate_chars(user, MAX_TITLE_DIGEST_USER_SEGMENT)
                )];
                if let Some(assistant) = final_assistant_text(&history[index + 1..span_end]) {
                    group.push(format!(
                        "assistant: {}",
                        truncate_chars(&assistant, MAX_TITLE_DIGEST_ASSISTANT)
                    ));
                }
                turns.push(group);
            }
            elide_digest_turns(&turns)
        }
        _ => {
            if prompts.is_empty() {
                return None;
            }
            let joined = prompts
                .iter()
                .take(MAX_TITLE_PROMPTS)
                .filter_map(|index| history.get(*index))
                .map(|message| {
                    format!(
                        "user: {}",
                        truncate_chars(message.content.trim(), MAX_TITLE_USER_SEGMENT)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Some(truncate_chars(&joined, MAX_TITLE_INPUT_LENGTH))
        }
    }
}

/// Keep the head and the tail of a long digest, eliding the middle
/// (v2 `elideTitleDigestTurns`).
fn elide_digest_turns(turns: &[Vec<String>]) -> Option<String> {
    if turns.is_empty() {
        return None;
    }
    let joined = turns
        .iter()
        .flatten()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if joined.chars().count() <= MAX_TITLE_DIGEST_INPUT_LENGTH {
        return Some(joined);
    }
    // The marker plus the two newlines that join it to the head and the tail.
    let mut budget = MAX_TITLE_DIGEST_INPUT_LENGTH - TITLE_DIGEST_ELISION_MARKER.len() - 2;
    let mut head: Vec<String> = Vec::new();
    for line in &turns[0] {
        let cost = line.chars().count() + 1;
        if budget < cost {
            break;
        }
        head.push(line.clone());
        budget -= cost;
    }
    let mut tail: Vec<String> = Vec::new();
    for group in turns.iter().skip(1).rev() {
        let cost: usize = group.iter().map(|line| line.chars().count() + 1).sum();
        if budget < cost {
            break;
        }
        for line in group.iter().rev() {
            tail.insert(0, line.clone());
        }
        budget -= cost;
    }
    let mut lines = head;
    lines.push(TITLE_DIGEST_ELISION_MARKER.to_string());
    lines.extend(tail);
    Some(lines.join("\n"))
}

/// The title for `history` (v2 `generateTitle`).
///
/// Every source is summarized by a model:
/// - a managed OAuth catalog provider uses the hosted `chat_title` tool;
/// - every other provider uses an ordinary local generation request.
///
/// The deterministic composition only picks the excerpt that is sent; it never
/// becomes the title itself (the old "first prompt cut to 60 chars" behavior is
/// v2's fallback excerpt, not its title).
/// `Ok(None)` means the history cannot supply an input — not a failure.
pub async fn generate_session_title(
    history: &[LLMMessage],
    source: Option<&str>,
    llm: &dyn LLM,
) -> Result<Option<String>, String> {
    let source = source.unwrap_or("user_prompts");
    let Some(input) = compose_title_input(history, source) else {
        return Ok(None);
    };
    // v2 `generateAndApply`: the hosted channel is used only when the managed
    // provider exists. A failure there is logged and yields no title; it does
    // **not** silently fall back to a local request, because the two channels
    // can bill/behave differently.
    let managed = llm.media_target().filter(|target| target.uploads_media);
    if managed.is_some() {
        let Some(target) = managed else {
            unreachable!()
        };
        let Some(credential) = llm.media_upload_credential() else {
            tracing::debug!(
                "chat_title request unavailable: no managed credential for this session's model"
            );
            return Ok(None);
        };
        let token = match credential.await {
            Ok(token) => token,
            Err(error) => {
                tracing::debug!("chat_title credential unavailable: {error}");
                return Ok(None);
            }
        };
        return match fetch_chat_title_with_headers(
            &target.base_url,
            &token,
            &input,
            &custom_title_headers(),
        )
        .await
        {
            Ok(title) => Ok(Some(title)),
            // v2 force-refreshes the OAuth token once on a 401 and retries the
            // same request. The credential seam hands out whatever the provider
            // currently holds; asking it again after the 401 is the engine's
            // equivalent of `getAccessToken({ force: true })`.
            Err(error) if error.status == Some(401) => {
                let Some(retry_credential) = llm.media_upload_credential() else {
                    tracing::debug!("chat_title request failed: {}", error.message);
                    return Ok(None);
                };
                let retry_token = match retry_credential.await {
                    Ok(token) => token,
                    Err(error) => {
                        tracing::debug!("chat_title credential unavailable: {error}");
                        return Ok(None);
                    }
                };
                match fetch_chat_title_with_headers(
                    &target.base_url,
                    &retry_token,
                    &input,
                    &custom_title_headers(),
                )
                .await
                {
                    Ok(title) => Ok(Some(title)),
                    Err(error) => {
                        tracing::debug!("chat_title request failed: {}", error.message);
                        Ok(None)
                    }
                }
            }
            Err(error) => {
                tracing::debug!("chat_title request failed: {}", error.message);
                Ok(None)
            }
        };
    }
    generate_local_title(&input, llm).await
}

/// v2 `generateLocalTitle` instruction, kept separate from the interpolated
/// excerpt so the literal stays a single catalog-visible sentence.
const LOCAL_TITLE_PROMPT: &str = "Write a short title for this conversation, at most 40 characters. Reply with the title only: no quotes, no preamble, no trailing period.";

/// The v2 `generateLocalTitle` prompt and cleanup: ask the session's own model
/// for a short title and strip the quoting/punctuation a model tends to add.
async fn generate_local_title(input: &str, llm: &dyn LLM) -> Result<Option<String>, String> {
    let prompt = format!("{LOCAL_TITLE_PROMPT}\n\n{input}");
    let params = LLMChatParams {
        messages: Arc::from([LLMMessage::user(prompt)]),
        ..Default::default()
    };
    let response = llm.chat(params).await.map_err(|e| e.to_string())?;
    // v2 trims, then strips one leading/trailing quote character, then cuts to
    // the maximum length. `take` avoids splitting a code point.
    let trimmed = response.content.trim();
    let mut unquoted = trimmed;
    if let Some(rest) = unquoted.strip_prefix(['"', '\'', '“']) {
        unquoted = rest;
    }
    if let Some(rest) = unquoted.strip_suffix(['"', '\'', '”']) {
        unquoted = rest;
    }
    let title = truncate_chars(unquoted.trim(), MAX_GENERATED_TITLE_LENGTH);
    Ok((!title.is_empty()).then_some(title))
}

/// One async lock per session, so two concurrent `generateTitle` calls for the
/// same session do not both run a model request (v2 `SessionTitleService`'s
/// `_shared` promise). The second caller waits, then observes the title the
/// first one applied and returns it without re-requesting.
static TITLE_LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Acquire the session's title-generation slot for the duration of one attempt.
pub async fn title_generation_slot(session_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = {
        let mut locks = TITLE_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
        locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    lock.lock_owned().await
}

/// v2 `MAX_TITLE_LENGTH` (promptMetadataText.ts:4): the first prompt becomes a
/// title of at most this many characters.
pub const MAX_PROMPT_TITLE_LENGTH: usize = 200;

/// v2 `MAX_LAST_PROMPT_LENGTH` (promptMetadataText.ts:5): the persisted
/// `lastPrompt` keeps at most this many characters.
pub const MAX_LAST_PROMPT_LENGTH: usize = 4000;

/// v2 `promptMetadataTextFromText`: redact secrets, collapse control characters
/// and whitespace, and cap the length. Returns `None` when nothing but
/// whitespace remains.
pub fn prompt_metadata_text(text: &str) -> Option<String> {
    static REDACTIONS: LazyLock<[regex::Regex; 5]> = LazyLock::new(|| {
        [
            regex::Regex::new(
                r"(?is)-----BEGIN [^-]*PRIVATE KEY-----.*?-----END [^-]*PRIVATE KEY-----",
            )
            .expect("private key pattern"),
            regex::Regex::new(r"(?i)\b(authorization)\s*:\s*bearer\s+\S+")
                .expect("authorization pattern"),
            regex::Regex::new(
                r#"(?i)\b(api[_-]?key|token|secret|password|passwd|pwd)\b\s*[:=]\s*(?:"[^"]*"|'[^']*'|\S+)"#,
            )
            .expect("secret pattern"),
            regex::Regex::new(r"\bsk-[A-Za-z0-9_-]{12,}\b").expect("sk pattern"),
            regex::Regex::new(r"\b[A-Za-z0-9][A-Za-z0-9+/=_-]{39,}\b").expect("token pattern"),
        ]
    });
    static CONTROL: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\p{Cc}+").expect("control pattern"));
    static WHITESPACE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\s+").expect("whitespace pattern"));

    let mut sanitized = REDACTIONS[0].replace_all(text, "[redacted]").into_owned();
    sanitized = REDACTIONS[1]
        .replace_all(&sanitized, "$1: Bearer [redacted]")
        .into_owned();
    sanitized = REDACTIONS[2]
        .replace_all(&sanitized, "$1=[redacted]")
        .into_owned();
    sanitized = REDACTIONS[3]
        .replace_all(&sanitized, "[redacted]")
        .into_owned();
    sanitized = REDACTIONS[4]
        .replace_all(&sanitized, "[redacted]")
        .into_owned();
    sanitized = CONTROL.replace_all(&sanitized, " ").into_owned();
    sanitized = WHITESPACE.replace_all(&sanitized, " ").into_owned();
    let sanitized = sanitized.trim();
    if sanitized.is_empty() {
        return None;
    }
    Some(truncate_chars(sanitized, MAX_LAST_PROMPT_LENGTH))
}

/// v2 `promptMetadataTextFromContentParts`: a client's own `display_text`
/// entries win over the rendered prompt text when every entry carries one.
pub fn prompt_metadata_text_from_parts(
    parts_text: &str,
    client_metadata: Option<&serde_json::Value>,
) -> Option<String> {
    let entries = client_metadata
        .and_then(serde_json::Value::as_array)
        .filter(|entries| !entries.is_empty());
    if let Some(entries) = entries {
        let display: Option<Vec<&str>> = entries
            .iter()
            .map(|entry| {
                entry
                    .get("display_text")
                    .and_then(serde_json::Value::as_str)
            })
            .collect();
        if let Some(display) = display {
            return prompt_metadata_text(&display.join("\n"));
        }
    }
    prompt_metadata_text(parts_text)
}

/// v2 `titleFromPromptMetadataText`: the sanitized first prompt, cut to the
/// title length. This is a *placeholder* title (`replaceable`) until a model
/// summarizes the conversation.
pub fn title_from_prompt_metadata_text(text: &str) -> String {
    truncate_chars(text, MAX_PROMPT_TITLE_LENGTH)
}

/// The telemetry sink for one title generation attempt (v2 `ITelemetryService`).
pub trait TitleTelemetrySink: Send + Sync {
    fn track(&self, event: serde_json::Value);
}

/// A title attempt's terminal outcome, in v2's `SessionTitleGenerationFinishedEvent`
/// vocabulary. `Ok` carries the applied title and whether it was truncated;
/// `Unavailable` carries the bounded reason; `Error` is an unexpected failure.
enum TitleAttemptOutcome {
    Success {
        title_chars: usize,
        title_outcome: &'static str,
    },
    Unavailable(&'static str),
    Error,
}

/// Generate a title and report the attempt once, at its terminal result (v2
/// `SessionTitleService.trackGeneration`). Skipped calls for an existing title
/// never reach here, so every call emits one event. `force` names the trigger:
/// a host-forced regeneration is `manual`, an ordinary one is `auto`.
pub async fn generate_session_title_with_telemetry(
    history: &[LLMMessage],
    source: Option<&str>,
    llm: &dyn LLM,
    force: bool,
    telemetry: &dyn TitleTelemetrySink,
) -> Result<Option<String>, String> {
    let started = Instant::now();
    let source = source.unwrap_or("user_prompts");
    let result = generate_session_title(history, Some(source), llm).await;
    let outcome = match &result {
        Ok(Some(title)) => TitleAttemptOutcome::Success {
            title_chars: title.encode_utf16().count(),
            title_outcome: "ok",
        },
        Ok(None) => TitleAttemptOutcome::Unavailable("no_input"),
        Err(_) => TitleAttemptOutcome::Error,
    };
    report_generation_event(telemetry, source, force, started, outcome);
    result
}

/// Ask the managed platform for a title (v2 `fetchChatTitle`).
pub async fn fetch_chat_title(
    base_url: &str,
    token: &str,
    chat_content: &str,
) -> Result<String, String> {
    fetch_chat_title_with_headers(base_url, token, chat_content, &[])
        .await
        .map_err(|error| error.message)
}

/// The failure of one hosted title request, with the HTTP status when the
/// server answered. v2 branches on `401` to force-refresh the token and retry
/// exactly once (sessionTitleService.ts:143-153); the status is what makes that
/// decision possible.
#[derive(Debug)]
pub struct ChatTitleError {
    pub message: String,
    pub status: Option<u16>,
}

/// v2 `parseKimiCodeCustomHeaders`: `KIMI_CODE_CUSTOM_HEADERS` holds
/// newline-separated `Name: value` lines applied to every managed request.
pub fn custom_title_headers() -> Vec<(String, String)> {
    let Ok(raw) = std::env::var(KIMI_CODE_CUSTOM_HEADERS_ENV) else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            let name = name.trim();
            (!name.is_empty()).then(|| (name.to_string(), value.trim().to_string()))
        })
        .collect()
}

/// v2 `KIMI_CODE_CUSTOM_HEADERS_ENV`.
pub const KIMI_CODE_CUSTOM_HEADERS_ENV: &str = "KIMI_CODE_CUSTOM_HEADERS";

/// Ask the managed platform for a title, with the caller's custom headers
/// merged in (v2 `fetchChatTitle(..., { headers })`).
pub async fn fetch_chat_title_with_headers(
    base_url: &str,
    token: &str,
    chat_content: &str,
    headers: &[(String, String)],
) -> Result<String, ChatTitleError> {
    let url = format!("{}/tools", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "method": "chat_title",
        "params": { "chat_content": chat_content },
    });
    let mut request = crate::llm::http::SHARED_HTTP_CLIENT
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .timeout(CHAT_TITLE_TIMEOUT);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request
        .json(&body)
        .send()
        .await
        .map_err(|e| ChatTitleError {
            message: format!("Failed to generate session title: {e}"),
            status: None,
        })?;
    if !response.status().is_success() {
        return Err(ChatTitleError {
            message: format!(
                "Failed to generate session title: HTTP {}",
                response.status().as_u16()
            ),
            status: Some(response.status().as_u16()),
        });
    }
    let payload: serde_json::Value = response.json().await.map_err(|e| ChatTitleError {
        message: format!("Failed to generate session title: {e}"),
        status: None,
    })?;
    let title = payload
        .get("title")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .ok_or_else(|| ChatTitleError {
            message: "Failed to generate session title: missing title.".to_string(),
            status: None,
        })?;
    Ok(truncate_chars(title, MAX_GENERATED_TITLE_LENGTH))
}

/// Build the terminal event and hand it to the sink, separated from the async
/// call so the error arm is testable without a failing provider.
fn report_generation_event(
    telemetry: &dyn TitleTelemetrySink,
    source: &str,
    force: bool,
    started: Instant,
    outcome: TitleAttemptOutcome,
) {
    let mut event = serde_json::json!({
        "event": "session_title_generation_finished",
        "trigger": if force { "manual" } else { "auto" },
        "source": source,
        "duration_ms": started.elapsed().as_millis() as u64,
    });
    match outcome {
        TitleAttemptOutcome::Success {
            title_chars,
            title_outcome,
        } => {
            event["outcome"] = "success".into();
            event["title_chars"] = title_chars.into();
            event["title_outcome"] = title_outcome.into();
        }
        TitleAttemptOutcome::Unavailable(reason) => {
            event["outcome"] = "unavailable".into();
            event["reason"] = reason.into();
        }
        TitleAttemptOutcome::Error => {
            event["outcome"] = "error".into();
            event["reason"] = "unexpected".into();
            event["error_type"] = "unexpected".into();
        }
    }
    telemetry.track(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn history(pairs: &[(&str, &str)]) -> Vec<LLMMessage> {
        let mut messages = Vec::new();
        for (user, assistant) in pairs {
            messages.push(LLMMessage::user(*user));
            if !assistant.is_empty() {
                messages.push(LLMMessage::assistant(*assistant));
            }
        }
        messages
    }

    #[test]
    fn user_prompts_takes_the_first_three_prompts() {
        let messages = history(&[("one", "a"), ("two", "b"), ("three", "c"), ("four", "d")]);
        assert_eq!(
            compose_title_input(&messages, "user_prompts").as_deref(),
            Some("user: one\nuser: two\nuser: three")
        );
    }

    #[test]
    fn first_turn_needs_both_sides() {
        let with_assistant = history(&[("hello", "hi there")]);
        assert_eq!(
            compose_title_input(&with_assistant, "first_turn").as_deref(),
            Some("user: hello\nassistant: hi there")
        );
        // A prompt with no assistant reply yet cannot title the session.
        let without_assistant = history(&[("hello", "")]);
        assert_eq!(compose_title_input(&without_assistant, "first_turn"), None);
    }

    #[test]
    fn digest_groups_each_prompt_with_its_final_assistant_reply() {
        let messages = vec![
            LLMMessage::user("first"),
            LLMMessage::assistant("draft"),
            LLMMessage::assistant("final"),
            LLMMessage::user("second"),
        ];
        assert_eq!(
            compose_title_input(&messages, "digest").as_deref(),
            Some("user: first\nassistant: final\nuser: second")
        );
    }

    #[test]
    fn digest_elides_the_middle_when_it_overflows() {
        let long = "x".repeat(200);
        let pairs: Vec<(&str, &str)> = (0..40).map(|_| (long.as_str(), long.as_str())).collect();
        let messages = history(&pairs);
        let input = compose_title_input(&messages, "digest").expect("a digest");
        assert!(input.contains(TITLE_DIGEST_ELISION_MARKER));
        assert!(
            input.chars().count() <= MAX_TITLE_DIGEST_INPUT_LENGTH,
            "elided digest is {} chars",
            input.chars().count()
        );
        // The head and the tail both survive.
        assert!(input.starts_with("user: xxx"));
        assert!(input.ends_with(&long));
    }

    #[test]
    fn prompt_metadata_redacts_secrets_before_persisting() {
        let text = "deploy with api_key=super-secret and token: abc123 \
                    Authorization: Bearer deadbeefsk-1234567890abcdef \
                    -----BEGIN RSA PRIVATE KEY-----\nMIIEsecret\n-----END RSA PRIVATE KEY-----\n\
                    then continue";
        let sanitized = prompt_metadata_text(text).expect("text remains after redaction");
        assert!(!sanitized.contains("super-secret"));
        assert!(!sanitized.contains("abc123"));
        assert!(!sanitized.contains("deadbeef"));
        assert!(!sanitized.contains("MIIEsecret"));
        assert!(!sanitized.contains('\n'));
        assert!(sanitized.contains("then continue"));
        assert_eq!(
            title_from_prompt_metadata_text(&sanitized),
            truncate_chars(&sanitized, MAX_PROMPT_TITLE_LENGTH)
        );
    }

    #[test]
    fn prompt_metadata_empty_input_is_not_persisted() {
        assert_eq!(prompt_metadata_text("   \n\t "), None);
    }

    #[test]
    fn a_clients_display_text_wins_over_the_rendered_prompt() {
        let metadata = serde_json::json!([
            { "display_text": "fix the login bug" },
            { "display_text": "add a test" }
        ]);
        assert_eq!(
            prompt_metadata_text_from_parts("rendered prompt", Some(&metadata)).as_deref(),
            Some("fix the login bug add a test")
        );

        let partial = serde_json::json!([{ "display_text": "only one" }, { "surface": "web" }]);
        assert_eq!(
            prompt_metadata_text_from_parts("rendered prompt", Some(&partial)).as_deref(),
            Some("rendered prompt")
        );
        assert_eq!(
            prompt_metadata_text_from_parts("rendered prompt", None).as_deref(),
            Some("rendered prompt")
        );
    }

    #[test]
    fn truncation_never_splits_a_code_point() {
        // 300 CJK characters is 900 bytes; a byte-wise cut at the 200-char
        // budget would land inside a code point and panic.
        let prompt = "汉".repeat(300);
        let messages = vec![LLMMessage::user(prompt)];
        let title = crate::session::sqlite_store::derive_session_title(&messages, None)
            .expect("a title")
            .expect("a prompt");
        assert_eq!(title.chars().count(), MAX_PROMPT_TITLE_LENGTH);
    }

    #[test]
    fn a_history_without_prompts_has_no_input() {
        let messages = vec![LLMMessage::assistant("only an answer")];
        assert_eq!(compose_title_input(&messages, "digest"), None);
        assert_eq!(compose_title_input(&messages, "user_prompts"), None);
    }

    /// Serve one HTTP response and hand back the base URL plus a handle that
    /// resolves to the request line and headers the client sent.
    async fn serve_once(status: &str, body: &str) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let status = status.to_string();
        let body = body.to_string();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 8192];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
            request
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn chat_title_posts_the_managed_tool_shape_and_reads_the_title() {
        let (base_url, request) = serve_once("200 OK", r#"{"title":"  A short title  "}"#).await;
        let title = fetch_chat_title(&base_url, "token-123", "user: hello")
            .await
            .expect("a 200 with a title");
        assert_eq!(title, "A short title");

        let request = request.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with("post /tools "), "{request}");
        assert!(
            request.contains("authorization: bearer token-123"),
            "{request}"
        );
        assert!(request.contains(r#""method":"chat_title""#), "{request}");
        assert!(
            request.contains(r#""chat_content":"user: hello""#),
            "{request}"
        );
    }

    #[tokio::test]
    async fn chat_title_reports_a_missing_title() {
        let (base_url, _request) = serve_once("200 OK", "{}").await;
        let error = fetch_chat_title(&base_url, "t", "user: hi")
            .await
            .expect_err("a payload without a title is a failure");
        assert!(error.contains("missing title"), "{error}");
    }

    #[tokio::test]
    async fn chat_title_reports_a_non_success_status() {
        let (base_url, _request) = serve_once("500 Internal Server Error", "{}").await;
        let error = fetch_chat_title(&base_url, "t", "user: hi")
            .await
            .expect_err("a 500 is a failure");
        assert!(error.contains("HTTP 500"), "{error}");
    }

    /// A provider with no managed seam that still answers ordinary chat calls
    /// — the local title-generation channel (v2 `generateLocalTitle`).
    struct LocalLlm;

    impl LLM for LocalLlm {
        fn system_prompt(&self) -> &str {
            ""
        }
        fn model_name(&self) -> &str {
            "local"
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }
        fn chat(
            &self,
            _params: crate::turn_loop::types::LLMChatParams,
        ) -> crate::rpc::types::BoxFuture<
            '_,
            Result<
                crate::turn_loop::types::LLMChatResponse,
                Box<dyn std::error::Error + Send + Sync>,
            >,
        > {
            Box::pin(async {
                Ok(crate::turn_loop::types::LLMChatResponse {
                    content: "  \"A generated title\"  ".to_string(),
                    ..Default::default()
                })
            })
        }
    }

    struct StaticLlm;

    impl LLM for StaticLlm {
        fn system_prompt(&self) -> &str {
            ""
        }
        fn model_name(&self) -> &str {
            "static"
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }
        fn chat(
            &self,
            _params: crate::turn_loop::types::LLMChatParams,
        ) -> crate::rpc::types::BoxFuture<
            '_,
            Result<
                crate::turn_loop::types::LLMChatResponse,
                Box<dyn std::error::Error + Send + Sync>,
            >,
        > {
            Box::pin(async { Err("unused in this test".into()) })
        }
    }

    #[tokio::test]
    async fn digest_uses_the_local_channel_without_a_managed_provider() {
        let messages = history(&[("hello", "hi there")]);
        assert_eq!(
            generate_session_title(&messages, Some("digest"), &LocalLlm)
                .await
                .expect("the local model answers"),
            Some("A generated title".to_string()),
            "without a managed provider every source is summarized locally"
        );
    }

    #[tokio::test]
    async fn every_source_is_summarized_by_the_session_model() {
        let messages = history(&[("hello", "hi there")]);
        for source in ["first_turn", "digest", "user_prompts"] {
            assert_eq!(
                generate_session_title(&messages, Some(source), &LocalLlm)
                    .await
                    .expect("the local model answers")
                    .as_deref(),
                Some("A generated title"),
                "source {source} must be summarized, not copied"
            );
        }
    }

    struct RecordingTelemetry {
        events: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    impl TitleTelemetrySink for RecordingTelemetry {
        fn track(&self, event: serde_json::Value) {
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(event);
        }
    }

    #[tokio::test]
    async fn a_successful_generation_reports_the_v2_finished_event() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = RecordingTelemetry {
            events: Arc::clone(&events),
        };
        let messages = history(&[("hello", "hi there")]);
        let title = generate_session_title_with_telemetry(
            &messages,
            Some("first_turn"),
            &LocalLlm,
            false,
            &sink,
        )
        .await
        .expect("the local model answers");
        assert_eq!(title.as_deref(), Some("A generated title"));
        let events = events.lock().unwrap();
        assert_eq!(
            events.len(),
            1,
            "one attempt reports exactly one terminal event"
        );
        let event = &events[0];
        assert_eq!(event["event"], "session_title_generation_finished");
        assert_eq!(event["trigger"], "auto");
        assert_eq!(event["source"], "first_turn");
        assert_eq!(event["outcome"], "success");
        assert_eq!(event["title_chars"], 17);
        assert_eq!(event["title_outcome"], "ok");
        assert!(event["duration_ms"].is_u64());
        assert!(event.get("reason").is_none());
        assert!(
            !event.to_string().contains("hello"),
            "the title text is not recorded"
        );
    }

    #[tokio::test]
    async fn a_manual_attempt_without_input_reports_unavailable() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = RecordingTelemetry {
            events: Arc::clone(&events),
        };
        let result =
            generate_session_title_with_telemetry(&[], Some("first_turn"), &StaticLlm, true, &sink)
                .await
                .expect("an empty history is an outcome, not a failure");
        assert_eq!(result, None);
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["trigger"], "manual");
        assert_eq!(events[0]["source"], "first_turn");
        assert_eq!(events[0]["outcome"], "unavailable");
        assert_eq!(events[0]["reason"], "no_input");
        assert!(events[0]["duration_ms"].is_u64());
        assert!(events[0].get("title_chars").is_none());
    }

    #[test]
    fn an_error_reports_unexpected_without_the_error_text() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = RecordingTelemetry {
            events: Arc::clone(&events),
        };
        report_generation_event(
            &sink,
            "user_prompts",
            true,
            Instant::now(),
            TitleAttemptOutcome::Error,
        );
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["outcome"], "error");
        assert_eq!(events[0]["reason"], "unexpected");
        assert_eq!(events[0]["error_type"], "unexpected");
        assert_eq!(events[0]["trigger"], "manual");
        assert!(events[0]["duration_ms"].is_u64());
        assert!(!events[0].to_string().contains("unexpected failure"));
    }
}
