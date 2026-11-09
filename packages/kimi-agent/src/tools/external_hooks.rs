use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::permission::HookDef;
use crate::rpc::types::{ContentBlock, ToolExecuteRequest};

const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 30;

const MAX_HOOK_TIMEOUT_SECS: u64 = 600;

const TIMED_OUT: &str = "Permission hook timed out";

const MAX_HOOK_OUTPUT_CHARS: usize = 64 * 1024;
const OUTPUT_TRUNCATED_MARKER: &str = "\n[hook output truncated]";

const MAX_HOOK_OUTPUT_BYTES: usize = MAX_HOOK_OUTPUT_CHARS * 4;

#[derive(Clone, Default)]
pub struct HooksSource {
    hooks: Arc<std::sync::RwLock<Option<Vec<HookDef>>>>,
}

impl HooksSource {
    pub fn new(hooks: Vec<HookDef>) -> Self {
        Self {
            hooks: Arc::new(std::sync::RwLock::new(Some(hooks))),
        }
    }

    pub fn snapshot(&self) -> Option<Vec<HookDef>> {
        self.hooks
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn replace(&self, hooks: Vec<HookDef>) -> Option<Vec<HookDef>> {
        std::mem::replace(
            &mut *self
                .hooks
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            Some(hooks),
        )
    }
}

#[derive(Debug, Clone)]
pub struct InvalidMatcher {
    pub event: String,
    pub matcher: String,
    pub error: String,
}

pub struct HookGuard {
    hooks: Vec<HookDef>,

    matchers: Vec<Matcher>,
    invalid: Vec<InvalidMatcher>,

    telemetry: std::sync::OnceLock<TelemetrySink>,

    hook_result: std::sync::OnceLock<HookResultSink>,

    session_cwd: Option<String>,

    failure_warned: Arc<std::sync::atomic::AtomicBool>,
}

pub type TelemetrySink = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;

pub struct PostToolUse<'a> {
    pub agent_id: &'a str,
    pub turn_id: &'a str,
    pub tool_name: &'a str,
    pub tool_call_id: &'a str,
    pub args: &'a Value,
    pub content: &'a str,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookResultMeta {
    pub agent_id: String,
    pub turn_id: Option<String>,
}

impl HookResultMeta {
    pub fn new(agent_id: impl Into<String>, turn_id: impl Into<Option<String>>) -> Self {
        Self {
            agent_id: agent_id.into(),
            turn_id: turn_id.into(),
        }
    }
}

pub type HookResultSink = Arc<dyn Fn(&HookResultMeta, &str, &str, bool) + Send + Sync>;

#[derive(Clone)]
struct HookReporter {
    sink: Option<HookResultSink>,
    session_cwd: Option<String>,
    warned: Arc<std::sync::atomic::AtomicBool>,
}

impl HookReporter {
    fn warn(&self, meta: &HookResultMeta, event: &str, message: &str) {
        if meta.agent_id != crate::callbacks::MAIN_AGENT_ID {
            return;
        }
        if self.warned.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        tracing::warn!(hook_event = event, %message, "external hook failed to execute");
        if let Some(sink) = &self.sink {
            let content = format!(
                "A {event} hook failed to execute: {message}. Hooks always run on this local machine; check that the hook command works locally. Later hook failures in this session are logged without another warning."
            );
            sink(meta, event, &content, false);
        }
    }
}

struct CappedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

impl CappedOutput {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        let remaining = MAX_HOOK_OUTPUT_BYTES.saturating_sub(self.bytes.len());
        if chunk.len() > remaining {
            self.bytes.extend_from_slice(&chunk[..remaining]);
            self.truncated = true;
        } else {
            self.bytes.extend_from_slice(chunk);
        }
    }

    fn finish(self) -> (String, bool) {
        let text = String::from_utf8_lossy(&self.bytes);
        if !self.truncated && text.chars().count() <= MAX_HOOK_OUTPUT_CHARS {
            return (text.into_owned(), false);
        }
        let mut capped: String = text.chars().take(MAX_HOOK_OUTPUT_CHARS).collect();
        capped.push_str(OUTPUT_TRUNCATED_MARKER);
        (capped, true)
    }
}

#[derive(Debug, Default)]
pub struct UserPromptSubmitOutcome {
    pub block: Option<String>,
    pub append: Vec<ContentBlock>,
}

enum Matcher {
    All,
    Regex(regex::Regex),
    Never,
}

impl Matcher {
    fn matches(&self, target: &str) -> bool {
        match self {
            Matcher::All => true,
            Matcher::Regex(re) => re.is_match(target),
            Matcher::Never => false,
        }
    }
}

impl HookGuard {
    pub fn new(hooks: Vec<HookDef>) -> Self {
        let mut invalid = Vec::new();
        let matchers = hooks
            .iter()
            .map(|hook| {
                if hook.matcher.is_empty() {
                    Matcher::All
                } else {
                    match regex::Regex::new(&hook.matcher) {
                        Ok(re) => Matcher::Regex(re),
                        Err(error) => {




                            tracing::warn!(
                                event = %hook.event,
                                matcher = %hook.matcher,
                                %error,
                                "external hook matcher is not a valid regex; the hook will never match"
                            );
                            invalid.push(InvalidMatcher {
                                event: hook.event.clone(),
                                matcher: hook.matcher.clone(),
                                error: error.to_string(),
                            });
                            Matcher::Never
                        }
                    }
                }
            })
            .collect();
        Self {
            hooks,
            matchers,
            invalid,
            telemetry: std::sync::OnceLock::new(),
            hook_result: std::sync::OnceLock::new(),
            session_cwd: None,
            failure_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn reporter(&self) -> HookReporter {
        HookReporter {
            sink: self.hook_result.get().cloned(),
            session_cwd: self.session_cwd.clone(),
            warned: Arc::clone(&self.failure_warned),
        }
    }

    pub fn with_session_cwd(mut self, cwd: Option<String>) -> Self {
        self.session_cwd = cwd.filter(|dir| !dir.is_empty());
        self
    }

    fn payload_cwd(&self) -> String {
        self.session_cwd.clone().unwrap_or_else(|| {
            std::env::current_dir()
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or_default()
        })
    }

    pub fn with_telemetry(self: &Arc<Self>, sink: TelemetrySink) {
        let _ = self.telemetry.set(sink);
    }

    pub fn with_hook_result(self: &Arc<Self>, sink: HookResultSink) {
        let _ = self.hook_result.set(sink);
    }

    pub fn invalid_matchers(&self) -> &[InvalidMatcher] {
        &self.invalid
    }

    fn matched_hooks(&self, event: &str, target: &str) -> Vec<HookDef> {
        let mut matched: Vec<HookDef> = Vec::new();
        let mut seen_commands = std::collections::HashSet::new();
        for (hook, matcher) in self.hooks.iter().zip(self.matchers.iter()) {
            if hook.event != event {
                continue;
            }
            if !matcher.matches(target) {
                continue;
            }

            if !seen_commands.insert((hook.cwd.clone(), hook.command.clone())) {
                continue;
            }
            matched.push(hook.clone());
        }
        matched
    }

    pub async fn denial(&self, request: &ToolExecuteRequest) -> Option<String> {
        let matched = self.matched_hooks("PreToolUse", &request.tool_name);
        if matched.is_empty() {
            return None;
        }
        let payload = hook_payload(request, &self.payload_cwd());
        let results = futures_util::future::join_all(
            matched
                .iter()
                .map(|hook| run_pre_tool_use_hook(hook, &payload, self.session_cwd.as_deref())),
        )
        .await;
        let meta = HookResultMeta::new(
            request.agent_id.clone(),
            Some(request.turn_id.clone()).filter(|turn| !turn.is_empty()),
        );

        if let Some(sink) = self.hook_result.get() {
            for outcome in &results {
                sink(
                    &meta,
                    "PreToolUse",
                    &outcome.stdout,
                    outcome.reason.is_some(),
                );
            }
        }
        let reporter = self.reporter();
        for outcome in &results {
            if let Some(message) = &outcome.failure {
                reporter.warn(&meta, "PreToolUse", message);
            }
        }

        let matched_count = results.len();
        let failed_count = results.iter().filter(|r| r.failure.is_some()).count();
        if let Some(sink) = self.telemetry.get() {
            let action = if results.iter().any(|outcome| outcome.reason.is_some()) {
                "block"
            } else {
                "allow"
            };
            sink(
                "external_hook_resolved",
                serde_json::json!({
                    "hook_event": "PreToolUse",
                    "action": action,
                    "matched_count": matched_count,
                    "failed_count": failed_count,
                }),
            );
        }
        results.into_iter().find_map(|outcome| outcome.reason)
    }

    pub async fn notify_post_tool_use(&self, trigger: PostToolUse<'_>) {
        let PostToolUse {
            agent_id,
            turn_id,
            tool_name,
            tool_call_id,
            args,
            content,
            is_error,
        } = trigger;
        let event_type = if is_error {
            "PostToolUseFailure"
        } else {
            "PostToolUse"
        };
        let matched = self.matched_hooks(event_type, tool_name);
        if matched.is_empty() {
            return;
        }

        let output_slice: String = content.chars().take(2000).collect();
        let payload = serde_json::json!({
            "tool_name": tool_name,
            "tool_call_id": tool_call_id,
            "tool_input": args,
            "is_error": is_error,
            "tool_output": if !is_error { Some(output_slice) } else { None },
            "error": if is_error { Some(content) } else { None },
        });

        spawn_hooks(
            "PostToolUse",
            matched,
            payload,
            self.reporter(),
            HookResultMeta::new(
                agent_id,
                Some(turn_id.to_string()).filter(|turn| !turn.is_empty()),
            ),
        );
    }

    pub async fn run_user_prompt_submit(
        &self,
        agent_id: &str,
        turn_id: &str,
        prompt: &str,
    ) -> UserPromptSubmitOutcome {
        let matched = self.matched_hooks("UserPromptSubmit", prompt);
        if matched.is_empty() {
            return UserPromptSubmitOutcome::default();
        }
        let payload = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "turn_id": turn_id,
            "prompt": prompt,
            "cwd": self.payload_cwd(),
        });
        let results = futures_util::future::join_all(
            matched
                .iter()
                .map(|hook| run_prompt_submit_hook(hook, &payload, self.session_cwd.as_deref())),
        )
        .await;

        let reporter = self.reporter();
        let meta = HookResultMeta::new(agent_id, Some(turn_id.to_string()));
        for result in &results {
            if let Some(message) = &result.failure {
                reporter.warn(&meta, "UserPromptSubmit", message);
            }
        }

        if let Some(block) = results
            .iter()
            .find(|result| result.action == PromptHookAction::Block)
        {
            let message = block.block_message();
            if let Some(sink) = self.hook_result.get() {
                sink(
                    &HookResultMeta::new(agent_id, Some(turn_id.to_string())),
                    "UserPromptSubmit",
                    &message,
                    true,
                );
            }
            return UserPromptSubmitOutcome {
                block: Some(render_hook_result("UserPromptSubmit", &message)),
                append: Vec::new(),
            };
        }

        let messages: Vec<String> = results
            .iter()
            .filter_map(PromptHookResult::append_message)
            .collect();
        if let Some(sink) = self.hook_result.get() {
            let meta = HookResultMeta::new(agent_id, Some(turn_id.to_string()));
            for message in &messages {
                sink(&meta, "UserPromptSubmit", message, false);
            }
        }
        UserPromptSubmitOutcome {
            block: None,
            append: messages
                .iter()
                .map(|message| user_prompt_submit_hook_part(message))
                .collect(),
        }
    }

    pub async fn notify_pre_compact(&self, turn_id: &str, message_count: usize, token_count: u32) {
        let matched = self.matched_hooks("PreCompact", "auto");
        if matched.is_empty() {
            return;
        }

        let payload = serde_json::json!({
            "turn_id": turn_id,
            "message_count": message_count,
            "trigger": "auto",
            "token_count": token_count,
        });
        spawn_hooks(
            "PreCompact",
            matched,
            payload,
            self.reporter(),
            HookResultMeta::new(crate::callbacks::MAIN_AGENT_ID, None),
        );
    }

    pub async fn notify_session_lifecycle(&self, event: &str, matcher: &str, payload: Value) {
        let matched = self.matched_hooks(event, matcher);
        if matched.is_empty() {
            return;
        }
        spawn_hooks(
            event,
            matched,
            payload,
            self.reporter(),
            HookResultMeta::new(crate::callbacks::MAIN_AGENT_ID, None),
        );
    }

    pub fn has_hooks_for(&self, event: &str) -> bool {
        self.hooks.iter().any(|hook| hook.event == event)
    }

    pub async fn notify_permission_request(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        turn_id: &str,
        tool_input: &Value,
        reason: Option<&str>,
    ) {
        let matched = self.matched_hooks("PermissionRequest", tool_name);
        if matched.is_empty() {
            return;
        }
        let mut payload = serde_json::json!({
            "tool_name": tool_name,
            "tool_call_id": tool_call_id,
            "turn_id": turn_id,
            "tool_input": tool_input,
        });
        if let Some(reason) = reason {
            payload["reason"] = Value::String(reason.to_string());
        }
        spawn_hooks(
            "PermissionRequest",
            matched,
            payload,
            self.reporter(),
            HookResultMeta::new(crate::callbacks::MAIN_AGENT_ID, None),
        );
    }

    pub async fn notify_permission_result(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        turn_id: &str,
        tool_input: &Value,
        decision: &str,
        feedback: Option<&str>,
    ) {
        let matched = self.matched_hooks("PermissionResult", tool_name);
        if matched.is_empty() {
            return;
        }
        let mut payload = serde_json::json!({
            "tool_name": tool_name,
            "tool_call_id": tool_call_id,
            "turn_id": turn_id,
            "tool_input": tool_input,
            "decision": decision,
        });
        if let Some(feedback) = feedback {
            payload["feedback"] = Value::String(feedback.to_string());
        }
        spawn_hooks(
            "PermissionResult",
            matched,
            payload,
            self.reporter(),
            HookResultMeta::new(crate::callbacks::MAIN_AGENT_ID, None),
        );
    }

    pub async fn notify_turn_started(&self, turn_id: u64, origin: &Value, prompt: &str) {
        let origin_kind = origin
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let matched = self.matched_hooks("TurnStarted", origin_kind);
        if matched.is_empty() {
            return;
        }
        let mut payload = serde_json::json!({
            "turn_id": turn_id,
            "origin_kind": origin_kind,
            "prompt": prompt,
        });
        if let Some(name) = origin.get("name").and_then(Value::as_str) {
            payload["origin_name"] = Value::String(name.to_string());
        }
        spawn_hooks(
            "TurnStarted",
            matched,
            payload,
            self.reporter(),
            HookResultMeta::new(crate::callbacks::MAIN_AGENT_ID, None),
        );
    }

    pub async fn notify_interrupt(&self, turn_id: u64) {
        let matched = self.matched_hooks("Interrupt", "");
        if matched.is_empty() {
            return;
        }
        spawn_hooks(
            "Interrupt",
            matched,
            serde_json::json!({ "turn_id": turn_id, "reason": "cancelled" }),
            self.reporter(),
            HookResultMeta::new(crate::callbacks::MAIN_AGENT_ID, None),
        );
    }

    pub async fn notify_stop_failure(&self, error_type: &str, error_message: &str) {
        let matched = self.matched_hooks("StopFailure", error_type);
        if matched.is_empty() {
            return;
        }
        spawn_hooks(
            "StopFailure",
            matched,
            serde_json::json!({
                "error_type": error_type,
                "error_message": error_message,
            }),
            self.reporter(),
            HookResultMeta::new(crate::callbacks::MAIN_AGENT_ID, None),
        );
    }

    pub async fn notify_user_prompt_queued(
        &self,
        prompt_id: u64,
        prompt: &str,
        queue_length: usize,
    ) {
        let matched = self.matched_hooks("UserPromptQueued", prompt);
        if matched.is_empty() {
            return;
        }
        spawn_hooks(
            "UserPromptQueued",
            matched,
            serde_json::json!({
                "prompt_id": prompt_id,
                "prompt": prompt,
                "queue_length": queue_length,
            }),
            self.reporter(),
            HookResultMeta::new(crate::callbacks::MAIN_AGENT_ID, None),
        );
    }

    pub async fn notify_post_compact(
        &self,
        turn_id: &str,
        trigger: &str,
        estimated_token_count: u64,
    ) {
        let matched = self.matched_hooks("PostCompact", trigger);
        if matched.is_empty() {
            return;
        }
        spawn_hooks(
            "PostCompact",
            matched,
            serde_json::json!({
                "turn_id": turn_id,
                "trigger": trigger,
                "estimated_token_count": estimated_token_count,
            }),
            self.reporter(),
            HookResultMeta::new(crate::callbacks::MAIN_AGENT_ID, None),
        );
    }

    pub async fn notify_task_started(
        &self,
        task_id: &str,
        kind: &str,
        description: &str,
        status: &str,
        started_at: u64,
    ) {
        let matched = self.matched_hooks("TaskStarted", kind);
        if matched.is_empty() {
            return;
        }
        spawn_hooks(
            "TaskStarted",
            matched,
            serde_json::json!({
                "task_id": task_id,
                "kind": kind,
                "description": description,
                "status": status,
                "started_at": started_at,
            }),
            self.reporter(),
            HookResultMeta::new(crate::callbacks::MAIN_AGENT_ID, None),
        );
    }

    pub async fn notify_task_notification(
        &self,
        task_id: &str,
        description: &str,
        status: &str,
        started_at: u64,
        ended_at: u64,
    ) {
        let matched = self.matched_hooks("Notification", status);
        if matched.is_empty() {
            return;
        }
        spawn_hooks(
            "Notification",
            matched,
            serde_json::json!({
                "sink": "context",
                "task_id": task_id,
                "description": description,
                "status": status,
                "started_at": started_at,
                "ended_at": ended_at,
            }),
            self.reporter(),
            HookResultMeta::new(crate::callbacks::MAIN_AGENT_ID, None),
        );
    }

    pub async fn notify_subagent_start(&self, agent_name: &str, prompt: &str) {
        let matched = self.matched_hooks("SubagentStart", agent_name);
        if matched.is_empty() {
            return;
        }
        spawn_hooks(
            "SubagentStart",
            matched,
            serde_json::json!({
                "agent_name": agent_name,
                "prompt": prompt,
                "session_title": "",
            }),
            self.reporter(),
            HookResultMeta::new(crate::callbacks::MAIN_AGENT_ID, None),
        );
    }

    pub async fn notify_subagent_stop(&self, agent_name: &str, response: &str) {
        let matched = self.matched_hooks("SubagentStop", agent_name);
        if matched.is_empty() {
            return;
        }
        spawn_hooks(
            "SubagentStop",
            matched,
            serde_json::json!({
                "agent_name": agent_name,
                "response": response,
                "session_title": "",
            }),
            self.reporter(),
            HookResultMeta::new(crate::callbacks::MAIN_AGENT_ID, None),
        );
    }

    pub async fn notify_stop(
        &self,
        agent_id: &str,
        turn_id: &str,
        tool_name: &str,
        tool_call_id: &str,
        reason: &str,
    ) -> Option<String> {
        let matched = self.matched_hooks("Stop", tool_name);
        if matched.is_empty() {
            return None;
        }
        let payload = serde_json::json!({
            "tool_name": tool_name,
            "tool_call_id": tool_call_id,
            "stop_reason": reason,
            "cwd": self.payload_cwd(),
        });
        let results = futures_util::future::join_all(
            matched
                .iter()
                .map(|hook| run_stop_hook(hook, &payload, self.session_cwd.as_deref())),
        )
        .await;
        let meta = HookResultMeta::new(agent_id, Some(turn_id.to_string()));
        if let Some(sink) = self.hook_result.get() {
            for outcome in &results {
                sink(&meta, "Stop", &outcome.stdout, outcome.reason.is_some());
            }
        }
        let reporter = self.reporter();
        for outcome in &results {
            if let Some(message) = &outcome.failure {
                reporter.warn(&meta, "Stop", message);
            }
        }

        results.into_iter().find_map(|outcome| outcome.reason)
    }
}

fn spawn_hooks(
    event: &str,
    matched: Vec<HookDef>,
    payload: Value,
    reporter: HookReporter,
    meta: HookResultMeta,
) {
    let payload_cwd = reporter.session_cwd.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .map(|d| d.to_string_lossy().to_string())
            .unwrap_or_default()
    });
    for hook in matched {
        let mut p = payload.clone();
        if let Some(obj) = p.as_object_mut() {
            obj.insert("hook_event_name".into(), Value::String(event.to_string()));
            obj.entry("cwd".to_string())
                .or_insert_with(|| Value::String(payload_cwd.clone()));
        }
        let reporter = reporter.clone();
        let event = event.to_string();
        let meta = meta.clone();
        tokio::spawn(async move {
            let outcome =
                run_hook_with_denial(&hook, &p, &event, reporter.session_cwd.as_deref(), false)
                    .await;

            if let Some(sink) = &reporter.sink {
                sink(&meta, &event, &outcome.stdout, outcome.reason.is_some());
            }
            if let Some(message) = &outcome.failure {
                reporter.warn(&meta, &event, message);
            }
        });
    }
}

struct HookOutcome {
    reason: Option<String>,
    stdout: String,
    failure: Option<String>,
}

async fn run_stop_hook(hook: &HookDef, payload: &Value, session_cwd: Option<&str>) -> HookOutcome {
    run_hook_with_denial(hook, payload, "Stop", session_cwd, true).await
}

fn hook_payload(request: &ToolExecuteRequest, cwd: &str) -> Value {
    let tool_input = request
        .arguments
        .as_object()
        .map(|obj| Value::Object(obj.clone()))
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": request.turn_id,
        "cwd": cwd,
        "client_type": platform_string(),
        "session_title": "",
        "tool_name": request.tool_name,
        "tool_input": tool_input,
        "tool_call_id": request.tool_call_id,
    })
}

fn platform_string() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

async fn run_pre_tool_use_hook(
    hook: &HookDef,
    payload: &Value,
    session_cwd: Option<&str>,
) -> HookOutcome {
    run_hook_with_denial(hook, payload, "PreToolUse", session_cwd, true).await
}

async fn run_hook_with_denial(
    hook: &HookDef,
    payload: &Value,
    event: &str,
    session_cwd: Option<&str>,
    fail_closed: bool,
) -> HookOutcome {
    let result = run_hook_process(hook, payload, session_cwd).await;
    match &result.failure {
        Some(HookProcessFailure::TimedOut) if fail_closed => HookOutcome {
            reason: Some(TIMED_OUT.into()),
            stdout: String::new(),
            failure: None,
        },
        Some(failure) => HookOutcome {
            reason: None,
            stdout: result.stdout,
            failure: Some(failure.message(hook.timeout.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS))),
        },
        None => {
            let reason = result
                .status
                .as_ref()
                .and_then(|status| evaluate_hook(event, status, &result.stdout, &result.stderr));
            HookOutcome {
                reason,
                stdout: result.stdout,
                failure: exit_code_failure(result.status.as_ref()),
            }
        }
    }
}

fn exit_code_failure(status: Option<&ExitStatus>) -> Option<String> {
    let code = status?.code()?;
    if code == 0 || code == 2 {
        return None;
    }
    Some(format!("exited with exit code {code}"))
}

#[derive(Debug)]
enum HookProcessFailure {
    Spawn(String),
    Errored(String),
    TimedOut,
}

impl HookProcessFailure {
    fn message(&self, timeout_secs: u64) -> String {
        match self {
            Self::Spawn(error) => error.clone(),
            Self::Errored(error) => error.clone(),
            Self::TimedOut => format!("timed out after {timeout_secs}s"),
        }
    }
}

#[derive(Debug)]
struct HookProcessResult {
    status: Option<ExitStatus>,
    stdout: String,
    stderr: String,
    failure: Option<HookProcessFailure>,
}

async fn run_hook_process(
    hook: &HookDef,
    payload: &Value,
    session_cwd: Option<&str>,
) -> HookProcessResult {
    let timeout = Duration::from_secs(
        hook.timeout
            .unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS)
            .clamp(1, MAX_HOOK_TIMEOUT_SECS),
    );
    let mut child = match spawn_hook_command(
        &hook.command,
        hook.cwd.as_deref().or(session_cwd),
        hook.env.as_ref(),
    ) {
        Ok(child) => child,
        Err(error) => {
            return HookProcessResult {
                status: None,
                stdout: String::new(),
                stderr: String::new(),
                failure: Some(HookProcessFailure::Spawn(error.to_string())),
            };
        }
    };

    let payload_json = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    let mut stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return HookProcessResult {
                status: None,
                stdout: String::new(),
                stderr: String::new(),
                failure: Some(HookProcessFailure::Errored(
                    "hook stdin was not available".into(),
                )),
            };
        }
    };

    let _ = stdin.write_all(payload_json.as_bytes()).await;
    drop(stdin);

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let out_buf = Arc::new(tokio::sync::Mutex::new(CappedOutput::new()));
    let err_buf = Arc::new(tokio::sync::Mutex::new(CappedOutput::new()));
    let out_drain = {
        let buf = Arc::clone(&out_buf);
        tokio::spawn(async move {
            if let Some(mut out) = stdout {
                use tokio::io::AsyncReadExt;
                let mut buf = buf.lock().await;
                let mut chunk = [0u8; 8192];
                while let Ok(read) = out.read(&mut chunk).await {
                    if read == 0 {
                        break;
                    }
                    buf.push(&chunk[..read]);
                }
            }
        })
    };
    let err_drain = {
        let buf = Arc::clone(&err_buf);
        tokio::spawn(async move {
            if let Some(mut err) = stderr {
                use tokio::io::AsyncReadExt;
                let mut buf = buf.lock().await;
                let mut chunk = [0u8; 8192];
                while let Ok(read) = err.read(&mut chunk).await {
                    if read == 0 {
                        break;
                    }
                    buf.push(&chunk[..read]);
                }
            }
        })
    };
    let (status, failure) = tokio::select! {
        status = child.wait() => match status {
            Ok(status) => (Some(status), None),
            Err(error) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                (None, Some(HookProcessFailure::Errored(error.to_string())))
            }
        },
        _ = tokio::time::sleep(timeout) => {


            let _ = child.kill().await;
            let _ = child.wait().await;
            (None, Some(HookProcessFailure::TimedOut))
        }
    };
    if failure.is_some() {
        out_drain.abort();
        err_drain.abort();
    }
    let _ = out_drain.await;
    let _ = err_drain.await;
    let (stdout, stdout_truncated) = take_output(&out_buf).await;
    let (stderr, stderr_truncated) = take_output(&err_buf).await;
    if stdout_truncated || stderr_truncated {
        tracing::warn!(
            stdout = stdout_truncated,
            stderr = stderr_truncated,
            "external hook output exceeded 64K characters and was truncated"
        );
    }
    HookProcessResult {
        status,
        stdout,
        stderr,
        failure,
    }
}

async fn take_output(buf: &Arc<tokio::sync::Mutex<CappedOutput>>) -> (String, bool) {
    let mut guard = buf.lock().await;
    std::mem::replace(&mut *guard, CappedOutput::new()).finish()
}

fn spawn_hook_command(
    command: &str,
    cwd: Option<&str>,
    env: Option<&std::collections::HashMap<String, String>>,
) -> std::io::Result<tokio::process::Child> {
    let mut cmd = if cfg!(windows) {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    };
    if let Some(dir) = cwd.filter(|dir| !dir.is_empty()) {
        cmd.current_dir(dir);
    }
    if let Some(vars) = env {
        cmd.envs(vars);
    }
    #[cfg(windows)]
    {
        cmd.creation_flags(0x08000000);
    }
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    cmd.spawn()
}

fn evaluate_hook(event: &str, status: &ExitStatus, stdout: &str, stderr: &str) -> Option<String> {
    let code = status.code()?;
    if code == 2 {
        return Some(fallback_reason(event, stderr.trim()));
    }
    if code == 0
        && let Some(reason) = json_deny_reason(stdout)
    {
        return Some(fallback_reason(event, &reason));
    }
    None
}

fn json_deny_reason(stdout: &str) -> Option<String> {
    let value: Value = serde_json::from_str(stdout.trim()).ok()?;
    let specific = value.get("hookSpecificOutput")?;
    if specific.get("permissionDecision").and_then(|v| v.as_str()) != Some("deny") {
        return None;
    }
    Some(
        specific
            .get("permissionDecisionReason")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string(),
    )
}

fn fallback_reason(event: &str, reason: &str) -> String {
    if reason.is_empty() {
        format!("Blocked by {event} hook")
    } else {
        reason.into()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptHookAction {
    Allow,
    Block,
}

#[derive(Debug)]
struct PromptHookResult {
    action: PromptHookAction,
    message: Option<String>,
    reason: Option<String>,
    stdout: String,
    exit_code: Option<i32>,
    exited: bool,
    timed_out: bool,
    structured_output: bool,

    failure: Option<String>,
}

impl PromptHookResult {
    fn from_process(hook: &HookDef, process: HookProcessResult) -> Self {
        let exit_code = process.status.as_ref().and_then(ExitStatus::code);
        let mut result = Self {
            action: PromptHookAction::Allow,
            message: None,
            reason: None,
            stdout: process.stdout,
            exit_code,
            exited: process.status.is_some(),
            timed_out: matches!(&process.failure, Some(HookProcessFailure::TimedOut)),
            structured_output: false,
            failure: process
                .failure
                .as_ref()
                .map(|failure| failure.message(hook.timeout.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS))),
        };
        if process.failure.is_some() {
            return result;
        }
        if result.failure.is_none() {
            result.failure = exit_code_failure(process.status.as_ref());
        }
        if exit_code == Some(2) {
            let message = process.stderr.trim().to_string();
            result.action = PromptHookAction::Block;
            result.message = Some(message.clone());
            result.reason = Some(message);
            return result;
        }
        if exit_code == Some(0)
            && let Some(structured) = structured_hook_output(&result.stdout)
        {
            result.structured_output = true;
            result.message = structured.message;
            if structured.deny {
                result.action = PromptHookAction::Block;
                result.reason = structured.reason;
            }
        }
        result
    }

    fn block_message(&self) -> String {
        non_empty_trimmed(self.message.as_deref())
            .or_else(|| non_empty_trimmed(self.reason.as_deref()))
            .unwrap_or_else(|| "Blocked by UserPromptSubmit hook".into())
    }

    fn append_message(&self) -> Option<String> {
        if self.action == PromptHookAction::Block
            || self.timed_out
            || self.exit_code.is_some_and(|code| code != 0)
            || (self.exited && self.exit_code.is_none())
        {
            return None;
        }
        if let Some(message) = non_empty_trimmed(self.message.as_deref()) {
            return Some(message);
        }

        if self.structured_output {
            return None;
        }
        non_empty_trimmed(Some(&self.stdout))
    }
}

struct StructuredHookOutput {
    message: Option<String>,
    deny: bool,
    reason: Option<String>,
}

fn structured_hook_output(stdout: &str) -> Option<StructuredHookOutput> {
    let value: Value = serde_json::from_str(stdout.trim()).ok()?;
    let object = value.as_object()?;
    let specific = object.get("hookSpecificOutput").and_then(Value::as_object);
    Some(StructuredHookOutput {
        message: optional_hook_string(object.get("message"))
            .or_else(|| optional_hook_string(specific.and_then(|output| output.get("message")))),
        deny: specific
            .and_then(|output| output.get("permissionDecision"))
            .and_then(Value::as_str)
            == Some("deny"),
        reason: specific
            .and_then(|output| output.get("permissionDecisionReason"))
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn optional_hook_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Number(number)) => Some(number.to_string()),
        Some(Value::Bool(flag)) => Some(flag.to_string()),
        _ => None,
    }
}

fn non_empty_trimmed(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

async fn run_prompt_submit_hook(
    hook: &HookDef,
    payload: &Value,
    session_cwd: Option<&str>,
) -> PromptHookResult {
    PromptHookResult::from_process(hook, run_hook_process(hook, payload, session_cwd).await)
}

fn render_hook_result(event: &str, message: &str) -> String {
    format!("<hook_result hook_event=\"{event}\">\n{message}\n</hook_result>")
}

fn user_prompt_submit_hook_part(message: &str) -> ContentBlock {
    let mut meta = serde_json::Map::new();
    meta.insert("contentType".into(), Value::String("text/xml".into()));
    meta.insert(
        "source".into(),
        Value::String("user prompt submit hook".into()),
    );
    ContentBlock::Text {
        text: render_hook_result("UserPromptSubmit", message),
        meta: Some(meta),
    }
}

#[cfg(test)]
pub(crate) mod capture {
    use serde_json::Value;

    use crate::permission::HookDef;

    pub fn dir() -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().ok()?;
        if dir.path().to_string_lossy().contains(' ') {
            return None;
        }
        Some(dir)
    }

    pub fn hook(event: &str, matcher: &str, target: &std::path::Path) -> HookDef {
        let command = if cfg!(windows) {
            format!(
                "%SystemRoot%\\System32\\more.com > {}",
                target.to_string_lossy()
            )
        } else {
            format!("cat > {}", target.to_string_lossy())
        };
        HookDef {
            event: event.into(),
            matcher: matcher.into(),
            command,
            timeout: None,
            cwd: None,
            env: None,
        }
    }

    const CAPTURE_WAIT: std::time::Duration =
        std::time::Duration::from_secs(super::DEFAULT_HOOK_TIMEOUT_SECS);

    pub async fn wait(target: &std::path::Path) -> Value {
        let deadline = std::time::Instant::now() + CAPTURE_WAIT;
        loop {
            let content = std::fs::read_to_string(target).unwrap_or_default();
            if let Ok(value) = serde_json::from_str(&content) {
                return value;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "no JSON payload captured at {} within {CAPTURE_WAIT:?}: {content:?}",
                    target.display()
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(tool_name: &str) -> ToolExecuteRequest {
        ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "turn-1".into(),
            tool_call_id: "call-1".into(),
            tool_name: tool_name.into(),
            arguments: json!({ "path": "a.txt" }),
        }
    }

    fn hook(event: &str, matcher: &str, command: &str) -> HookDef {
        HookDef {
            event: event.into(),
            matcher: matcher.into(),
            command: command.into(),
            timeout: None,
            cwd: None,
            env: None,
        }
    }

    fn exit_two_with_stderr() -> &'static str {
        if cfg!(windows) {
            "echo denied by test hook 1>&2 & exit /b 2"
        } else {
            "echo denied by test hook >&2; exit 2"
        }
    }

    fn exit_two_silent() -> &'static str {
        if cfg!(windows) { "exit /b 2" } else { "exit 2" }
    }

    fn exit_one() -> &'static str {
        if cfg!(windows) { "exit /b 1" } else { "exit 1" }
    }

    fn sleeper() -> &'static str {
        if cfg!(windows) {
            "%SystemRoot%\\System32\\ping.exe -n 3 127.0.0.1 >nul"
        } else {
            "sleep 3"
        }
    }

    fn json_deny_command(dir: &std::path::Path) -> String {
        let json_file = dir.join("deny.json");
        std::fs::write(
            &json_file,
            r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"blocked by json"}}"#,
        )
        .unwrap();
        if cfg!(windows) {
            format!("type {}", json_file.to_string_lossy())
        } else {
            format!("cat {}", json_file.to_string_lossy())
        }
    }

    fn json_top_level_deny_command(dir: &std::path::Path) -> String {
        let json_file = dir.join("top-deny.json");
        std::fs::write(
            &json_file,
            r#"{"permissionDecision":"deny","permissionDecisionReason":"must not win"}"#,
        )
        .unwrap();
        if cfg!(windows) {
            format!("type {}", json_file.to_string_lossy())
        } else {
            format!("cat {}", json_file.to_string_lossy())
        }
    }

    fn skip_if_path_has_spaces(path: &std::path::Path) -> bool {
        path.to_string_lossy().contains(' ')
    }

    #[tokio::test]
    async fn no_hooks_and_non_pretooluse_events_pass() {
        let guard = HookGuard::new(vec![
            hook("Stop", "", "exit 2"),
            hook("Notification", "", "exit 2"),
        ]);
        assert_eq!(guard.denial(&request("Read")).await, None);
        let empty = HookGuard::new(vec![]);
        assert_eq!(empty.denial(&request("Read")).await, None);
    }

    #[tokio::test]
    async fn hooks_source_replace_does_not_change_a_guard_already_built() {
        let source = HooksSource::new(vec![hook("PreToolUse", "", exit_two_silent())]);
        let guard = HookGuard::new(source.snapshot().unwrap_or_default());
        let old = source.replace(Vec::new());
        assert_eq!(
            old.map(|hooks| hooks.len()),
            Some(1),
            "replace must return the set it replaced"
        );
        assert!(source.snapshot().unwrap_or_default().is_empty());
        assert!(
            guard.denial(&request("Read")).await.is_some(),
            "the already-built guard must keep blocking after the reload"
        );
    }

    #[tokio::test]
    async fn denial_emits_the_usage_event_with_counts() {
        let events = Arc::new(std::sync::Mutex::new(
            Vec::<(String, serde_json::Value)>::new(),
        ));
        let log = events.clone();

        let guard = Arc::new(HookGuard::new(vec![
            hook("PreToolUse", "", exit_two_with_stderr()),
            hook("PreToolUse", "", exit_two_silent()),
        ]));
        guard.with_telemetry(Arc::new(move |event, payload| {
            log.lock().unwrap().push((event.to_string(), payload));
        }));

        let denial = guard.denial(&request("Write")).await;
        assert!(denial.is_some(), "the hooks still block");

        let log = events.lock().unwrap();
        assert_eq!(log.len(), 1, "one event per trigger");
        let (event, payload) = &log[0];
        assert_eq!(event, "external_hook_resolved");
        assert_eq!(payload["hook_event"], "PreToolUse");
        assert_eq!(payload["action"], "block");
        assert_eq!(payload["matched_count"], 2);

        assert_eq!(payload["failed_count"], 0);
    }

    #[tokio::test]
    async fn denial_emits_a_hook_result_per_hook() {
        let results = Arc::new(std::sync::Mutex::new(Vec::<(
            String,
            Option<String>,
            String,
            String,
            bool,
        )>::new()));
        let log = results.clone();
        let stdout_cmd = "echo hook says hi";
        let guard = Arc::new(HookGuard::new(vec![
            hook("PreToolUse", "", exit_two_with_stderr()),
            hook("PreToolUse", "", stdout_cmd),
        ]));
        guard.with_hook_result(Arc::new(move |meta, event, content, blocked| {
            log.lock().unwrap().push((
                meta.agent_id.clone(),
                meta.turn_id.clone(),
                event.to_string(),
                content.to_string(),
                blocked,
            ));
        }));

        let denial = guard.denial(&request("Write")).await;
        assert!(denial.is_some(), "the first hook still blocks");

        let log = results.lock().unwrap();
        assert_eq!(log.len(), 2, "one hook.result per hook run");
        assert!(log.iter().all(|(_, _, event, _, _)| event == "PreToolUse"));
        assert!(
            log.iter()
                .any(|(_, _, _, content, _)| content.contains("hook says hi")),
            "the hook's stdout reaches the host"
        );
        assert!(
            log.iter().any(|(_, _, _, _, blocked)| *blocked),
            "the blocking hook reports blocked"
        );

        assert!(
            log.iter().all(|(agent, turn, _, _, _)| {
                agent == &request("Write").agent_id && turn.as_deref() == Some("turn-1")
            }),
            "every hook.result carries the triggering agent and turn: {log:?}"
        );
    }

    #[tokio::test]
    async fn denial_without_a_sink_is_verdict_identical() {
        let guard = HookGuard::new(vec![hook("PreToolUse", "", exit_two_with_stderr())]);
        assert_eq!(
            guard.denial(&request("Write")).await.as_deref(),
            Some("denied by test hook")
        );
    }

    #[tokio::test]
    async fn matcher_filters_tools() {
        let guard = HookGuard::new(vec![hook("PreToolUse", "Wri", exit_two_with_stderr())]);
        assert!(
            guard.denial(&request("Write")).await.is_some(),
            "matcher hit must run the hook"
        );
        assert_eq!(
            guard.denial(&request("Read")).await,
            None,
            "matcher miss must skip the hook"
        );
    }

    #[tokio::test]
    async fn invalid_matcher_never_matches_but_is_reported() {
        let guard = HookGuard::new(vec![hook("PreToolUse", "[", exit_two_with_stderr())]);
        assert_eq!(guard.denial(&request("Write")).await, None);

        let invalid = guard.invalid_matchers();
        assert_eq!(invalid.len(), 1);
        assert_eq!(invalid[0].event, "PreToolUse");
        assert_eq!(invalid[0].matcher, "[");
        assert!(!invalid[0].error.is_empty());

        let healthy = HookGuard::new(vec![hook("PreToolUse", "Wri", exit_two_with_stderr())]);
        assert!(healthy.invalid_matchers().is_empty());
    }

    #[tokio::test]
    async fn exit_two_blocks_with_stderr_reason() {
        let guard = HookGuard::new(vec![hook("PreToolUse", "", exit_two_with_stderr())]);
        let denial = guard.denial(&request("Write")).await;
        assert_eq!(denial.as_deref(), Some("denied by test hook"));
    }

    #[tokio::test]
    async fn exit_two_with_empty_stderr_falls_back_to_default() {
        let guard = HookGuard::new(vec![hook("PreToolUse", "", exit_two_silent())]);
        assert_eq!(
            guard.denial(&request("Write")).await.as_deref(),
            Some("Blocked by PreToolUse hook")
        );
    }

    #[tokio::test]
    async fn json_deny_on_stdout_blocks() {
        let dir = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(dir.path()) {
            return;
        }
        let guard = HookGuard::new(vec![hook("PreToolUse", "", &json_deny_command(dir.path()))]);
        assert_eq!(
            guard.denial(&request("Write")).await.as_deref(),
            Some("blocked by json")
        );
    }

    #[tokio::test]
    async fn top_level_permission_decision_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(dir.path()) {
            return;
        }
        let guard = HookGuard::new(vec![hook(
            "PreToolUse",
            "",
            &json_top_level_deny_command(dir.path()),
        )]);
        assert_eq!(guard.denial(&request("Write")).await, None);
    }

    #[tokio::test]
    async fn other_exit_codes_allow() {
        let guard = HookGuard::new(vec![hook("PreToolUse", "", exit_one())]);
        assert_eq!(guard.denial(&request("Write")).await, None);
    }

    #[tokio::test]
    async fn timeout_fails_closed() {
        let guard = HookGuard::new(vec![HookDef {
            event: "PreToolUse".into(),
            matcher: String::new(),
            command: sleeper().into(),
            timeout: Some(1),
            cwd: None,
            env: None,
        }]);
        assert_eq!(
            guard.denial(&request("Write")).await.as_deref(),
            Some(TIMED_OUT)
        );
    }

    #[tokio::test]
    async fn hook_failures_fail_open_and_warn_once_per_session() {
        let warnings = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let log = warnings.clone();
        let second_failure = if cfg!(windows) { "exit /b 3" } else { "exit 3" };
        let guard = Arc::new(HookGuard::new(vec![
            hook("PreToolUse", "", exit_one()),
            hook("PreToolUse", "", second_failure),
        ]));
        guard.with_hook_result(Arc::new(move |_, _, content, blocked| {
            if !blocked && content.contains("hook failed to execute") {
                log.lock().unwrap().push(content.to_string());
            }
        }));

        assert_eq!(
            guard.denial(&request("Write")).await,
            None,
            "hook failures must fail open"
        );
        assert_eq!(guard.denial(&request("Write")).await, None);
        let warnings = warnings.lock().unwrap();
        assert_eq!(warnings.len(), 1, "one warning per session: {warnings:?}");
        assert!(
            warnings[0].contains("exited with exit code"),
            "the warning names the failure: {}",
            warnings[0]
        );
    }

    #[tokio::test]
    async fn hook_output_is_capped_at_64k_characters() {
        let dir = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(dir.path()) {
            return;
        }
        let file = dir.path().join("loud.txt");
        std::fs::write(&file, "a".repeat(MAX_HOOK_OUTPUT_CHARS + 4_000)).unwrap();
        let command = if cfg!(windows) {
            format!("type {}", file.to_string_lossy())
        } else {
            format!("cat {}", file.to_string_lossy())
        };
        let outcome = run_hook_process(
            &hook("PreToolUse", "", &command),
            &serde_json::json!({}),
            None,
        )
        .await;
        assert!(outcome.stdout.ends_with(OUTPUT_TRUNCATED_MARKER));
        assert_eq!(
            outcome.stdout.chars().count(),
            MAX_HOOK_OUTPUT_CHARS + OUTPUT_TRUNCATED_MARKER.chars().count()
        );
    }

    fn cwd_report_command() -> &'static str {
        if cfg!(windows) {
            "echo %CD% 1>&2 & exit /b 2"
        } else {
            "echo $PWD >&2; exit 2"
        }
    }

    fn env_report_command() -> &'static str {
        if cfg!(windows) {
            "echo %KIMI_HOOK_TEST_VALUE% 1>&2 & exit /b 2"
        } else {
            "echo $KIMI_HOOK_TEST_VALUE >&2; exit 2"
        }
    }

    #[tokio::test]
    async fn hook_cwd_reaches_the_child() {
        let dir = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(dir.path()) {
            return;
        }
        let guard = HookGuard::new(vec![HookDef {
            event: "PreToolUse".into(),
            matcher: String::new(),
            command: cwd_report_command().into(),
            timeout: None,
            cwd: Some(dir.path().to_string_lossy().to_string()),
            env: None,
        }]);
        let denial = guard.denial(&request("Write")).await;

        let name = dir.path().file_name().unwrap().to_string_lossy();
        assert!(
            denial
                .as_deref()
                .is_some_and(|reason| reason.contains(name.as_ref())),
            "cwd not observed in hook stderr: {denial:?}"
        );
    }

    #[tokio::test]
    async fn hooks_run_in_the_session_cwd_when_they_declare_none() {
        let dir = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(dir.path()) {
            return;
        }
        let guard = HookGuard::new(vec![HookDef {
            event: "PreToolUse".into(),
            matcher: String::new(),
            command: cwd_report_command().into(),
            timeout: None,
            cwd: None,
            env: None,
        }])
        .with_session_cwd(Some(dir.path().to_string_lossy().to_string()));
        let denial = guard.denial(&request("Write")).await;
        let name = dir.path().file_name().unwrap().to_string_lossy();
        assert!(
            denial
                .as_deref()
                .is_some_and(|reason| reason.contains(name.as_ref())),
            "session cwd not observed in hook stderr: {denial:?}"
        );
    }

    #[tokio::test]
    async fn a_hooks_own_cwd_wins_over_the_session_cwd() {
        let own = tempfile::tempdir().unwrap();
        let session = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(own.path()) || skip_if_path_has_spaces(session.path()) {
            return;
        }
        let guard = HookGuard::new(vec![HookDef {
            event: "PreToolUse".into(),
            matcher: String::new(),
            command: cwd_report_command().into(),
            timeout: None,
            cwd: Some(own.path().to_string_lossy().to_string()),
            env: None,
        }])
        .with_session_cwd(Some(session.path().to_string_lossy().to_string()));
        let denial = guard.denial(&request("Write")).await;
        let own_name = own.path().file_name().unwrap().to_string_lossy();
        let session_name = session.path().file_name().unwrap().to_string_lossy();
        assert!(
            denial
                .as_deref()
                .is_some_and(|reason| reason.contains(own_name.as_ref())
                    && !reason.contains(session_name.as_ref())),
            "the hook's own cwd must win: {denial:?}"
        );
    }

    #[tokio::test]
    async fn fire_and_forget_hooks_also_run_in_the_session_cwd() {
        let dir = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(dir.path()) {
            return;
        }

        let guard = HookGuard::new(vec![hook("PostToolUse", "Write", "echo 1 >> hook-ran.txt")])
            .with_session_cwd(Some(dir.path().to_string_lossy().to_string()));
        guard
            .notify_post_tool_use(PostToolUse {
                agent_id: "main",
                turn_id: "t",
                tool_name: "Write",
                tool_call_id: "c1",
                args: &serde_json::json!({}),
                content: "content",
                is_error: false,
            })
            .await;
        let marker = dir.path().join("hook-ran.txt");

        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(super::DEFAULT_HOOK_TIMEOUT_SECS);
        let ran = loop {
            if std::fs::read_to_string(&marker)
                .unwrap_or_default()
                .contains('1')
            {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        assert!(ran, "the hook never wrote into the session directory");
    }

    #[test]
    fn the_payload_cwd_is_the_session_cwd_or_the_process_cwd() {
        let guard = HookGuard::new(vec![]).with_session_cwd(Some("/session/dir".to_string()));
        assert_eq!(guard.payload_cwd(), "/session/dir");

        let guard = HookGuard::new(vec![]).with_session_cwd(Some(String::new()));
        assert!(!guard.payload_cwd().is_empty());
        let guard = HookGuard::new(vec![]);
        assert!(!guard.payload_cwd().is_empty());
    }

    #[tokio::test]
    async fn hook_env_reaches_the_child() {
        let guard = HookGuard::new(vec![HookDef {
            event: "PreToolUse".into(),
            matcher: String::new(),
            command: env_report_command().into(),
            timeout: None,
            cwd: None,
            env: Some(
                [(
                    "KIMI_HOOK_TEST_VALUE".to_string(),
                    "hook-env-ok".to_string(),
                )]
                .into_iter()
                .collect(),
            ),
        }]);
        assert_eq!(
            guard.denial(&request("Write")).await.as_deref(),
            Some("hook-env-ok")
        );
    }

    #[tokio::test]
    async fn commands_dedupe_accounts_for_cwd() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(dir_a.path()) || skip_if_path_has_spaces(dir_b.path()) {
            return;
        }

        let command = "echo 1 >> marker.txt".to_string();
        let guard = HookGuard::new(vec![
            HookDef {
                event: "PreToolUse".into(),
                matcher: String::new(),
                command: command.clone(),
                timeout: None,
                cwd: Some(dir_a.path().to_string_lossy().to_string()),
                env: None,
            },
            HookDef {
                event: "PreToolUse".into(),
                matcher: String::new(),
                command,
                timeout: None,
                cwd: Some(dir_b.path().to_string_lossy().to_string()),
                env: None,
            },
        ]);
        let _ = guard.denial(&request("Write")).await;
        let runs_a = std::fs::read_to_string(dir_a.path().join("marker.txt")).unwrap_or_default();
        let runs_b = std::fs::read_to_string(dir_b.path().join("marker.txt")).unwrap_or_default();
        assert_eq!(runs_a.lines().count(), 1, "first cwd runs once");
        assert_eq!(runs_b.lines().count(), 1, "second cwd runs once");
    }

    #[tokio::test]
    async fn commands_dedupe_within_one_trigger() {
        let dir = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(dir.path()) {
            return;
        }
        let marker = dir.path().join("marker.txt");
        let command = format!("echo 1 >> {}", marker.to_string_lossy());
        let guard = HookGuard::new(vec![
            hook("PreToolUse", "Wri", &command),
            hook("PreToolUse", ".*", &command),
        ]);
        let _ = guard.denial(&request("Write")).await;
        let runs = std::fs::read_to_string(&marker).unwrap_or_default();
        assert_eq!(
            runs.lines().count(),
            1,
            "identical commands must run once per trigger"
        );
    }

    #[test]
    fn payload_uses_the_snake_case_wire_shape() {
        let payload = hook_payload(&request("Write"), "/session/dir");
        assert_eq!(payload["hook_event_name"], "PreToolUse");
        assert_eq!(payload["session_id"], "turn-1");
        assert_eq!(payload["tool_name"], "Write");
        assert_eq!(payload["tool_call_id"], "call-1");
        assert_eq!(payload["tool_input"]["path"], "a.txt");
        assert_eq!(payload["cwd"], "/session/dir");
        assert!(
            payload["client_type"]
                .as_str()
                .is_some_and(|c| !c.is_empty())
        );
        assert_eq!(payload["session_title"], "");
    }

    #[test]
    fn non_object_tool_input_falls_back_to_empty_object() {
        let req = ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c".into(),
            tool_name: "Bash".into(),
            arguments: json!("just a string"),
        };
        assert_eq!(hook_payload(&req, "/session/dir")["tool_input"], json!({}));
    }

    #[tokio::test]
    async fn test_post_tool_use_hook_triggers() {
        let dir = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(dir.path()) {
            return;
        }
        let marker = dir.path().join("post_marker.txt");
        let command = format!("echo done >> {}", marker.to_string_lossy());
        let guard = HookGuard::new(vec![hook("PostToolUse", "Write", &command)]);
        guard
            .notify_post_tool_use(PostToolUse {
                agent_id: "main",
                turn_id: "t",
                tool_name: "Write",
                tool_call_id: "c1",
                args: &serde_json::json!({}),
                content: "content",
                is_error: false,
            })
            .await;

        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(super::DEFAULT_HOOK_TIMEOUT_SECS);
        let content = loop {
            let content = std::fs::read_to_string(&marker).unwrap_or_default();
            if content.contains("done") || std::time::Instant::now() >= deadline {
                break content;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        assert!(content.contains("done"));
    }

    fn capture_stdin_command(target: &std::path::Path) -> String {
        if cfg!(windows) {
            format!(
                "%SystemRoot%\\System32\\more.com > {}",
                target.to_string_lossy()
            )
        } else {
            format!("cat > {}", target.to_string_lossy())
        }
    }

    async fn wait_for_capture(path: &std::path::Path) -> String {
        let timeout = std::time::Duration::from_secs(super::DEFAULT_HOOK_TIMEOUT_SECS);
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let content = std::fs::read_to_string(path).unwrap_or_default();
            if serde_json::from_str::<Value>(&content).is_ok()
                || std::time::Instant::now() >= deadline
            {
                return content;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn session_start_matches_on_the_create_source_and_carries_the_event_name() {
        let dir = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(dir.path()) {
            return;
        }
        let captured = dir.path().join("session-start.json");
        let guard = HookGuard::new(vec![hook(
            "SessionStart",
            "startup",
            &capture_stdin_command(&captured),
        )]);

        guard
            .notify_session_lifecycle(
                "SessionStart",
                "resume",
                json!({ "source": "resume", "session_title": "" }),
            )
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            !captured.exists(),
            "the resume source must not match a startup hook"
        );

        guard
            .notify_session_lifecycle(
                "SessionStart",
                "startup",
                json!({ "source": "startup", "session_title": "" }),
            )
            .await;
        let payload: Value = serde_json::from_str(&wait_for_capture(&captured).await)
            .expect("the hook received a JSON payload");
        assert_eq!(payload["hook_event_name"], "SessionStart");
        assert_eq!(payload["source"], "startup");
    }

    #[tokio::test]
    async fn session_end_hooks_fire_observationally() {
        let dir = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(dir.path()) {
            return;
        }
        let captured = dir.path().join("session-end.json");

        let veto = hook("SessionEnd", "archive", exit_two_with_stderr());
        let guard = HookGuard::new(vec![
            veto,
            hook("SessionEnd", "archive", &capture_stdin_command(&captured)),
        ]);
        guard
            .notify_session_lifecycle(
                "SessionEnd",
                "archive",
                json!({ "reason": "archive", "session_title": "" }),
            )
            .await;

        let payload: Value = serde_json::from_str(&wait_for_capture(&captured).await)
            .expect("the hook received a JSON payload");
        assert_eq!(payload["hook_event_name"], "SessionEnd");
        assert_eq!(payload["reason"], "archive");
    }

    #[tokio::test]
    async fn turn_started_matches_the_origin_kind_and_only_carries_origin_name_when_present() {
        let dir = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(dir.path()) {
            return;
        }
        let captured = dir.path().join("turn-started.json");
        let guard = HookGuard::new(vec![hook(
            "TurnStarted",
            "system_trigger",
            &capture_stdin_command(&captured),
        )]);

        guard
            .notify_turn_started(7, &json!({ "kind": "user" }), "hello")
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!captured.exists());

        guard
            .notify_turn_started(
                7,
                &json!({ "kind": "system_trigger", "name": "cron" }),
                "tick",
            )
            .await;
        let payload: Value = serde_json::from_str(&wait_for_capture(&captured).await)
            .expect("the hook received a JSON payload");
        assert_eq!(payload["hook_event_name"], "TurnStarted");
        assert_eq!(payload["turn_id"], 7);
        assert_eq!(payload["origin_kind"], "system_trigger");
        assert_eq!(payload["origin_name"], "cron");
        assert_eq!(payload["prompt"], "tick");
    }

    #[tokio::test]
    async fn stop_failure_matches_against_the_error_type() {
        let dir = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(dir.path()) {
            return;
        }
        let captured = dir.path().join("stop-failure.json");
        let guard = HookGuard::new(vec![hook(
            "StopFailure",
            "provider.api_error",
            &capture_stdin_command(&captured),
        )]);

        guard.notify_stop_failure("permission.denied", "nope").await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!captured.exists());

        guard
            .notify_stop_failure("provider.api_error", "boom")
            .await;
        let payload: Value = serde_json::from_str(&wait_for_capture(&captured).await)
            .expect("the hook received a JSON payload");
        assert_eq!(payload["hook_event_name"], "StopFailure");
        assert_eq!(payload["error_type"], "provider.api_error");
        assert_eq!(payload["error_message"], "boom");
    }

    #[tokio::test]
    async fn user_prompt_queued_carries_the_queue_length_and_matches_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(dir.path()) {
            return;
        }
        let captured = dir.path().join("prompt-queued.json");
        let guard = HookGuard::new(vec![hook(
            "UserPromptQueued",
            "second thought",
            &capture_stdin_command(&captured),
        )]);

        guard.notify_user_prompt_queued(9, "unrelated", 2).await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!captured.exists());

        guard
            .notify_user_prompt_queued(9, "a second thought", 2)
            .await;
        let payload: Value = serde_json::from_str(&wait_for_capture(&captured).await)
            .expect("the hook received a JSON payload");
        assert_eq!(payload["hook_event_name"], "UserPromptQueued");
        assert_eq!(payload["prompt_id"], 9);
        assert_eq!(payload["queue_length"], 2);
        assert_eq!(payload["prompt"], "a second thought");
    }

    #[tokio::test]
    async fn permission_events_match_the_tool_and_carry_the_decision() {
        let dir = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(dir.path()) {
            return;
        }
        let request = dir.path().join("permission-request.json");
        let result = dir.path().join("permission-result.json");
        let guard = HookGuard::new(vec![
            hook(
                "PermissionRequest",
                "Bash",
                &capture_stdin_command(&request),
            ),
            hook("PermissionResult", "Bash", &capture_stdin_command(&result)),
        ]);
        let input = json!({ "command": "rm -rf /tmp/x" });

        guard
            .notify_permission_request("Bash", "c1", "t1", &input, Some("DangerousCommandAsk"))
            .await;
        guard
            .notify_permission_result("Bash", "c1", "t1", &input, "rejected", Some("not today"))
            .await;

        let request_payload: Value = serde_json::from_str(&wait_for_capture(&request).await)
            .expect("the request hook received a JSON payload");
        assert_eq!(request_payload["hook_event_name"], "PermissionRequest");
        assert_eq!(request_payload["tool_name"], "Bash");
        assert_eq!(request_payload["reason"], "DangerousCommandAsk");

        let result_payload: Value = serde_json::from_str(&wait_for_capture(&result).await)
            .expect("the result hook received a JSON payload");
        assert_eq!(result_payload["hook_event_name"], "PermissionResult");
        assert_eq!(result_payload["decision"], "rejected");
        assert_eq!(result_payload["feedback"], "not today");
    }

    #[tokio::test]
    async fn subagent_hooks_match_the_profile_name() {
        let dir = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(dir.path()) {
            return;
        }
        let started = dir.path().join("subagent-start.json");
        let stopped = dir.path().join("subagent-stop.json");
        let guard = HookGuard::new(vec![
            hook("SubagentStart", "explore", &capture_stdin_command(&started)),
            hook("SubagentStop", "explore", &capture_stdin_command(&stopped)),
        ]);

        guard.notify_subagent_start("general", "wrong prompt").await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!started.exists());

        guard
            .notify_subagent_start("explore", "list the files")
            .await;
        guard.notify_subagent_stop("explore", "done").await;

        let start_payload: Value = serde_json::from_str(&wait_for_capture(&started).await)
            .expect("the start hook received a JSON payload");
        assert_eq!(start_payload["hook_event_name"], "SubagentStart");
        assert_eq!(start_payload["agent_name"], "explore");
        assert_eq!(start_payload["prompt"], "list the files");

        let stop_payload: Value = serde_json::from_str(&wait_for_capture(&stopped).await)
            .expect("the stop hook received a JSON payload");
        assert_eq!(stop_payload["hook_event_name"], "SubagentStop");
        assert_eq!(stop_payload["response"], "done");
    }

    #[tokio::test]
    async fn task_and_compaction_events_match_on_kind_and_trigger() {
        let dir = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(dir.path()) {
            return;
        }
        let task = dir.path().join("task-started.json");
        let notify = dir.path().join("task-notification.json");
        let post = dir.path().join("post-compact.json");
        let guard = HookGuard::new(vec![
            hook("TaskStarted", "subagent", &capture_stdin_command(&task)),
            hook("Notification", "completed", &capture_stdin_command(&notify)),
            hook("PostCompact", "auto", &capture_stdin_command(&post)),
        ]);

        guard
            .notify_task_started("t0", "bash", "other", "running", 99)
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!task.exists());

        guard
            .notify_task_started("t1", "subagent", "run a helper", "running", 100)
            .await;
        guard
            .notify_task_notification("t1", "run a helper", "completed", 100, 200)
            .await;
        guard.notify_post_compact("turn-1", "auto", 42_000).await;

        let task_payload: Value = serde_json::from_str(&wait_for_capture(&task).await)
            .expect("the task hook received a JSON payload");
        assert_eq!(task_payload["hook_event_name"], "TaskStarted");
        assert_eq!(task_payload["kind"], "subagent");

        let notify_payload: Value = serde_json::from_str(&wait_for_capture(&notify).await)
            .expect("the notification hook received a JSON payload");
        assert_eq!(notify_payload["hook_event_name"], "Notification");
        assert_eq!(notify_payload["sink"], "context");
        assert_eq!(notify_payload["status"], "completed");

        let post_payload: Value = serde_json::from_str(&wait_for_capture(&post).await)
            .expect("the post-compact hook received a JSON payload");
        assert_eq!(post_payload["hook_event_name"], "PostCompact");
        assert_eq!(post_payload["trigger"], "auto");
        assert_eq!(post_payload["estimated_token_count"], 42_000);
    }

    #[tokio::test]
    async fn interrupt_matches_empty_and_stop_failure_error_type() {
        let guard = HookGuard::new(vec![
            hook("Interrupt", "", exit_two_silent()),
            hook("StopFailure", "permission.denied", exit_two_silent()),
        ]);

        guard.notify_interrupt(3).await;
        guard.notify_stop_failure("permission.denied", "nope").await;

        assert!(guard.has_hooks_for("Interrupt"));
        assert!(!guard.has_hooks_for("SessionHeartbeat"));
    }

    fn echo_reason_command(reason: &str) -> String {
        if cfg!(windows) {
            format!("echo {reason}")
        } else {
            format!("echo '{reason}'", reason = reason.replace('\'', "'\\''"))
        }
    }

    fn exit_two_with_reason(reason: &str) -> String {
        if cfg!(windows) {
            format!("echo {reason} 1>&2 & exit /b 2")
        } else {
            format!(
                "echo '{reason}' >&2; exit 2",
                reason = reason.replace('\'', "'\\''")
            )
        }
    }

    #[tokio::test]
    async fn stop_hook_exit_two_continues_with_stderr_reason() {
        let guard = HookGuard::new(vec![hook(
            "Stop",
            "",
            &exit_two_with_reason("user asked to keep going"),
        )]);
        let reason = guard.notify_stop("main", "t1", "", "", "stop").await;
        assert_eq!(reason.as_deref(), Some("user asked to keep going"));
    }

    #[tokio::test]
    async fn stop_hook_exit_two_empty_stderr_falls_back_to_default() {
        let guard = HookGuard::new(vec![hook("Stop", "", exit_two_silent())]);
        let reason = guard.notify_stop("main", "t1", "", "", "stop").await;
        assert_eq!(reason.as_deref(), Some("Blocked by Stop hook"));
    }

    #[tokio::test]
    async fn stop_hook_plain_stdout_does_not_continue() {
        let guard = HookGuard::new(vec![hook("Stop", "", &echo_reason_command("keep going"))]);
        let reason = guard.notify_stop("main", "t1", "", "", "stop").await;
        assert!(reason.is_none());
    }

    #[tokio::test]
    async fn stop_hook_json_deny_continues() {
        let dir = tempfile::tempdir().unwrap();
        if skip_if_path_has_spaces(dir.path()) {
            return;
        }
        let guard = HookGuard::new(vec![hook("Stop", "", &json_deny_command(dir.path()))]);
        let reason = guard.notify_stop("main", "t1", "", "", "stop").await;
        assert_eq!(reason.as_deref(), Some("blocked by json"));
    }

    #[tokio::test]
    async fn stop_hook_filters_by_event_and_matcher() {
        let guard = HookGuard::new(vec![
            hook("PreToolUse", "", &exit_two_with_reason("pretool")),
            hook("Stop", "Bash", &exit_two_with_reason("bash-only")),
        ]);

        assert!(
            guard
                .notify_stop("main", "t1", "", "", "stop")
                .await
                .is_none()
        );

        assert_eq!(
            guard
                .notify_stop("main", "t1", "Bash", "c2", "stop")
                .await
                .as_deref(),
            Some("bash-only")
        );
    }
}
