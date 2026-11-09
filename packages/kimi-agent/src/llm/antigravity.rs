//! The antigravity protocol: a subprocess bridge to the Google Cloud Code
//! `agy` CLI.
//!
//! This is not an HTTP wire format. The reference implementation
//! (`agent-core-v2/src/human/llm/requester/bases/antigravity/`) spawns the
//! locally installed `agy` binary, reads its NDJSON stdout, and polls a
//! transcript file for the CLI's internal tool activity. There is no endpoint,
//! no auth header and no request body: the CLI owns its credentials, and the
//! engine only forwards a prompt string plus a whitelisted environment.
//!
//! Two deliberate hardenings over the reference:
//!
//! * every wait is bounded ([`crate::primitives::deadline`]); the reference
//!   has no timeouts at all, only an abort signal;
//! * malformed lines, unknown events and empty streams are counted and
//!   reported, where the reference silently drops them.
//!
//! Everything else — prompt assembly, model aliasing, stream-event semantics,
//! usage bucketing, transcript rendering — follows the reference so the
//! observable behaviour matches.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::time::Instant;

use crate::llm::LlmTiming;
use crate::llm::http::EventSink;
use crate::llm::usage::classify_deepseek_cache_split;
use crate::llm::wire::StreamDelta;
use crate::primitives::deadline::{Deadline, bounded};
use crate::rpc::types::{AntigravityLimits, BoxFuture, ContentBlock, NativeLlmConfig, TokenUsage};
use kimi_core::types::{LLM, LLMChatParams, LLMChatResponse, LLMMessage};

/// Poll cadence for the transcript side channel (reference: 250ms).
pub const TRANSCRIPT_POLL: Duration = Duration::from_millis(250);
/// Bound for one transcript side-channel read. This provider has no
/// counterpart in the reference tree, so the value is local to this module.
const TRANSCRIPT_READ_WAIT: Duration = Duration::from_secs(30);
/// Bound for waiting out the provider process once it was asked to exit.
const AGY_EXIT_WAIT: Duration = Duration::from_secs(15);
/// How much stderr is kept for the exit report (reference: 64 KiB).
pub const STDERR_LIMIT: usize = 64 * 1024;

const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "SYSTEMROOT",
    "COMSPEC",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_CLOUD_LOCATION",
];

// Error prefixes. Stable strings: the retry layer classifies on them and the
// host shows them verbatim.
const ERR_SPAWN: &str = "antigravity.spawn";
const ERR_AUTH: &str = "antigravity.auth";
const ERR_QUOTA: &str = "antigravity.quota";
const ERR_MODEL_NOT_FOUND: &str = "antigravity.model_not_found";
const ERR_PROTOCOL: &str = "antigravity.protocol";
const ERR_TIMEOUT: &str = "antigravity.timeout";

/// Cancellation message shared with the native HTTP transport, so the retry
/// layer's `is_cancelled_error` recognizes it.
const CANCELLED: &str = "llm cancelled: antigravity request aborted";

// ── Process abstraction ────────────────────────────────────────────────────

/// One `agy` invocation, fully resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    pub binary: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

/// A spawned `agy` process as the transport sees it.
#[async_trait]
pub trait AntigravityChild: Send {
    /// The next stdout chunk, `None` on EOF.
    async fn recv_chunk(&mut self) -> Option<Result<Vec<u8>, String>>;
    /// Wait for the process to exit.
    async fn wait_exit(&mut self) -> Result<ExitInfo, String>;
    /// The last [`STDERR_LIMIT`] bytes the process wrote.
    fn stderr_tail(&self) -> String;
    /// Terminate the process (idempotent).
    fn kill(&mut self);
}

/// Starts `agy`. Split out so tests can script a process without a real
/// binary or a network round-trip.
#[async_trait]
pub trait AntigravityRunner: Send + Sync {
    async fn spawn(&self, spec: SpawnSpec) -> Result<Box<dyn AntigravityChild>, String>;
}

/// A process exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitInfo {
    /// The exit code, when the platform reports one (a signal-killed child has
    /// none).
    pub code: Option<i32>,
    /// The signal that killed the child, when there was one.
    pub signal: Option<i32>,
}

/// The production runner: a real `tokio::process::Child`.
#[derive(Debug, Default)]
pub struct TokioRunner;

#[async_trait]
impl AntigravityRunner for TokioRunner {
    async fn spawn(&self, spec: SpawnSpec) -> Result<Box<dyn AntigravityChild>, String> {
        use std::process::Stdio;

        let mut command = tokio::process::Command::new(&spec.binary);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // An abandoned turn must not leave an `agy` process behind.
            .kill_on_drop(true);
        command.env_clear();
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("{ERR_SPAWN}: failed to start {}: {error}", spec.binary))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("{ERR_SPAWN}: {} produced no stdout pipe", spec.binary))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| format!("{ERR_SPAWN}: {} produced no stderr pipe", spec.binary))?;
        Ok(Box::new(TokioChild::new(child, stdout, stderr)))
    }
}

struct TokioChild {
    child: tokio::process::Child,
    stdout: tokio::process::ChildStdout,
    stderr_tail: Arc<Mutex<String>>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
}

impl TokioChild {
    fn new(
        child: tokio::process::Child,
        stdout: tokio::process::ChildStdout,
        mut stderr: tokio::process::ChildStderr,
    ) -> Self {
        let tail = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&tail);
        let handle = tokio::spawn(async move {
            let mut buffer = [0u8; 8192];
            loop {
                match stderr.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        let text = String::from_utf8_lossy(&buffer[..read]);
                        let mut tail = sink.lock().unwrap_or_else(|error| error.into_inner());
                        tail.push_str(&text);
                        if tail.len() > STDERR_LIMIT {
                            let cut = tail.len() - STDERR_LIMIT;
                            let cut = tail
                                .char_indices()
                                .map(|(index, _)| index)
                                .find(|index| *index >= cut)
                                .unwrap_or(tail.len());
                            tail.drain(..cut);
                        }
                    }
                }
            }
        });
        Self {
            child,
            stdout,
            stderr_tail: tail,
            stderr_task: Some(handle),
        }
    }
}

#[async_trait]
impl AntigravityChild for TokioChild {
    async fn recv_chunk(&mut self) -> Option<Result<Vec<u8>, String>> {
        let mut buffer = vec![0u8; 8192];
        match self.stdout.read(&mut buffer).await {
            Ok(0) => None,
            Ok(read) => {
                buffer.truncate(read);
                Some(Ok(buffer))
            }
            Err(error) => Some(Err(format!(
                "{ERR_PROTOCOL}: failed reading {} output: {error}",
                "agy"
            ))),
        }
    }

    async fn wait_exit(&mut self) -> Result<ExitInfo, String> {
        let status = self
            .child
            .wait()
            .await
            .map_err(|error| format!("{ERR_PROTOCOL}: failed waiting for agy: {error}"))?;
        // The stderr reader sees EOF once the child is gone; joining it makes
        // the tail complete before the exit is classified.
        if let Some(handle) = self.stderr_task.take() {
            let _ = handle.await;
        }
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        Ok(ExitInfo {
            code: status.code(),
            signal,
        })
    }

    fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn kill(&mut self) {
        let _ = self.child.start_kill();
    }
}

// ── Stream frames ──────────────────────────────────────────────────────────

/// One decoded stdout frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Init { conversation_id: String },
    StepUpdate(StepUpdate),
    Result(StreamResult),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepUpdate {
    pub text_delta: Option<String>,
    pub thinking_delta: Option<String>,
    pub usage: Option<RawUsage>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamResult {
    pub response: Option<String>,
    pub usage: Option<RawUsage>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct RawUsage {
    #[serde(default)]
    pub input_tokens: Option<u32>,
    #[serde(default)]
    pub output_tokens: Option<u32>,
    #[serde(default)]
    pub cache_read_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct RawFrame {
    event: Option<String>,
    conversation_id: Option<String>,
    step_update: Option<RawStepUpdate>,
    result: Option<RawResult>,
}

#[derive(Deserialize, Default)]
struct RawStepUpdate {
    text_delta: Option<String>,
    thinking_delta: Option<String>,
    usage: Option<RawUsage>,
}

#[derive(Deserialize, Default)]
struct RawResult {
    response: Option<String>,
    usage: Option<RawUsage>,
}

/// What the decoder had to ignore, kept so a silent stream cannot hide
/// protocol drift.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameTrouble {
    pub malformed: usize,
    pub unknown: usize,
    pub first_malformed: Option<String>,
    pub first_unknown: Option<String>,
}

impl FrameTrouble {
    pub fn is_empty(&self) -> bool {
        self.malformed == 0 && self.unknown == 0
    }

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.malformed > 0 {
            parts.push(format!(
                "{} malformed line(s), first: {}",
                self.malformed,
                self.first_malformed.as_deref().unwrap_or("<empty>")
            ));
        }
        if self.unknown > 0 {
            parts.push(format!(
                "{} unknown event(s), first: {}",
                self.unknown,
                self.first_unknown.as_deref().unwrap_or("<unnamed>")
            ));
        }
        parts.join("; ")
    }
}

/// Incremental line decoder for the NDJSON stdout stream.
///
/// A chunk may end mid-line (half frame) or contain several lines (coalesced
/// frames); both are handled by carrying the residual bytes between pushes.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
    pub trouble: FrameTrouble,
}

impl FrameDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one stdout chunk and return every frame it completed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Frame> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffer.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            self.decode_line(&line, &mut frames);
        }
        frames
    }

    /// Flush a residual line at EOF (the reference's readline emits it too).
    pub fn finish(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        if self.buffer.is_empty() {
            return frames;
        }
        let line = std::mem::take(&mut self.buffer);
        self.decode_line(&line, &mut frames);
        frames
    }

    fn decode_line(&mut self, line: &[u8], frames: &mut Vec<Frame>) {
        let trimmed = trim_ascii(line);
        if trimmed.is_empty() {
            return;
        }
        if !trimmed.starts_with(b"{") {
            self.record_malformed(&String::from_utf8_lossy(trimmed));
            return;
        }
        let Ok(raw) = serde_json::from_slice::<RawFrame>(trimmed) else {
            self.record_malformed(&String::from_utf8_lossy(trimmed));
            return;
        };
        match raw.event.as_deref() {
            Some("init") => {
                if let Some(conversation_id) = raw.conversation_id.filter(|id| !id.is_empty()) {
                    frames.push(Frame::Init { conversation_id });
                } else {
                    self.record_malformed(&String::from_utf8_lossy(trimmed));
                }
            }
            Some("step_update") => {
                let update = raw.step_update.unwrap_or_default();
                frames.push(Frame::StepUpdate(StepUpdate {
                    text_delta: update.text_delta,
                    thinking_delta: update.thinking_delta,
                    usage: update.usage,
                }));
            }
            Some("result") => {
                let result = raw.result.unwrap_or_default();
                frames.push(Frame::Result(StreamResult {
                    response: result.response,
                    usage: result.usage,
                }));
            }
            other => {
                self.trouble.unknown += 1;
                if self.trouble.first_unknown.is_none() {
                    self.trouble.first_unknown =
                        Some(other.unwrap_or("<missing event>").to_string());
                }
            }
        }
    }

    fn record_malformed(&mut self, sample: &str) {
        self.trouble.malformed += 1;
        if self.trouble.first_malformed.is_none() {
            let sample = sample.trim();
            let sample = if sample.len() > 200 {
                sample.chars().take(200).collect::<String>()
            } else {
                sample.to_string()
            };
            self.trouble.first_malformed = Some(sample);
        }
    }
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
}

// ── Prompt / model / env ───────────────────────────────────────────────────

/// Model alias mapping (v2 `mapModelToAntigravity`).
#[must_use]
pub fn map_model(model: &str, effort: Option<&str>) -> String {
    let normalized = model.to_lowercase().replace(['_', ' ', '.'], "-");
    let effort_suffix = match effort {
        Some("low") => "low",
        Some("medium") => "medium",
        _ => "high",
    };
    if normalized.contains("3-7") || normalized.contains("37") {
        return format!("gemini-3.7-flash-{effort_suffix}");
    }
    if normalized.contains("3-1") || normalized.contains("31") {
        return if effort == Some("low") {
            "gemini-3.1-pro-low".to_string()
        } else {
            "gemini-3.1-pro-high".to_string()
        };
    }
    if normalized.contains("3-6") || normalized.contains("36") {
        return format!("gemini-3.6-flash-{effort_suffix}");
    }
    if normalized.contains("3-5") || normalized.contains("35") {
        return format!("gemini-3.5-flash-{effort_suffix}");
    }
    if normalized.contains("2-5-pro")
        || normalized.contains("2-0-pro")
        || normalized.contains("1-5-pro")
    {
        return "gemini-3.1-pro-high".to_string();
    }
    if normalized.contains("2-5-flash")
        || normalized.contains("2-0-flash")
        || normalized.contains("1-5-flash")
    {
        return "gemini-3.7-flash-high".to_string();
    }
    if normalized.contains("sonnet") {
        return "claude-sonnet-4-6".to_string();
    }
    if normalized.contains("opus") {
        return "claude-opus-4-6-thinking".to_string();
    }
    if normalized.contains("gpt-oss")
        || normalized.contains("gptoss")
        || normalized.contains("120b")
    {
        return "gpt-oss-120b-medium".to_string();
    }
    model.to_string()
}

/// The visible text of a message, with thinking and media dropped
/// (v2's `extractText`).
fn message_text(message: &LLMMessage) -> String {
    if message.blocks.is_empty() {
        return message.content.clone();
    }
    message
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn same_message(left: &LLMMessage, right: &LLMMessage) -> bool {
    left.role == right.role
        && left.content == right.content
        && left.blocks == right.blocks
        && left.tool_calls == right.tool_calls
        && left.tool_call_id == right.tool_call_id
}

/// Whether the new history strictly extends the remembered one
/// (v2 `extendsMessages`).
fn extends_messages(previous: &[LLMMessage], current: &[LLMMessage]) -> bool {
    previous.len() <= current.len()
        && previous
            .iter()
            .zip(current.iter())
            .all(|(left, right)| same_message(left, right))
}

/// Build the prompt string and the conversation id to resume.
///
/// * resumed: only the last message, the thread lives in the CLI;
/// * one message: just that message;
/// * otherwise: the whole history with role prefixes.
#[must_use]
pub fn build_prompt(history: &[LLMMessage], remembered: Option<&str>) -> (String, Option<String>) {
    if let Some(id) = remembered
        && history.len() > 1
    {
        return (non_empty_prompt(last_text(history)), Some(id.to_string()));
    }
    if history.len() <= 1 {
        return (non_empty_prompt(last_text(history)), None);
    }
    let mut parts = Vec::new();
    for message in history {
        let prefix = match message.role.as_str() {
            "user" => "[User]:\n",
            "assistant" => "[Assistant]:\n",
            "tool" => "[Tool Result]:\n",
            _ => continue,
        };
        let text = message_text(message);
        if text.trim().is_empty() {
            continue;
        }
        parts.push(format!("{prefix}{text}"));
    }
    (non_empty_prompt(parts.join("\n\n")), None)
}

fn last_text(history: &[LLMMessage]) -> String {
    history.last().map(message_text).unwrap_or_default()
}

/// v2 sends `"hi"` when there is nothing to say, so the CLI never receives an
/// empty prompt.
fn non_empty_prompt(prompt: String) -> String {
    if prompt.trim().is_empty() {
        "hi".to_string()
    } else {
        prompt
    }
}

fn detect_binary() -> String {
    if let Some(home) = crate::default_workspace::user_home_dir() {
        let candidate = home.join(".local/bin/agy");
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    for candidate in ["/usr/local/bin/agy", "/usr/bin/agy"] {
        if Path::new(candidate).is_file() {
            return candidate.to_string();
        }
    }
    "agy".to_string()
}

fn child_env() -> Vec<(String, String)> {
    let allowed = ENV_ALLOWLIST;
    std::env::vars_os()
        .filter_map(|(key, value)| {
            let key = key.to_string_lossy().into_owned();
            let value = value.to_string_lossy().into_owned();
            #[cfg(windows)]
            {
                let normalized = key.to_uppercase();
                allowed
                    .contains(&normalized.as_str())
                    .then_some((normalized, value))
            }
            #[cfg(not(windows))]
            {
                allowed.contains(&key.as_str()).then_some((key, value))
            }
        })
        .collect()
}

// ── Usage ──────────────────────────────────────────────────────────────────

/// Map one antigravity usage object onto the engine's bucket convention.
///
/// The CLI reports the uncached input (`input_tokens`) and the cached read
/// (`cache_read_tokens`) separately, with no prompt total. The prompt total is
/// therefore their sum, and [`classify_deepseek_cache_split`] keeps the bucket
/// arithmetic identical to every other provider.
#[must_use]
pub fn parse_usage(raw: &RawUsage) -> TokenUsage {
    let miss = raw.input_tokens.unwrap_or(0);
    let cached = raw.cache_read_tokens.unwrap_or(0);
    let prompt = miss.saturating_add(cached);
    let split = classify_deepseek_cache_split(Some(cached), Some(miss), prompt, 0);
    let input_tokens = split.input_other(prompt);
    let input_cache_read = split.cached();
    let output_tokens = raw.output_tokens.unwrap_or(0);
    TokenUsage {
        input_tokens,
        output_tokens,
        // Same convention as every other transport: the wire total is the two
        // headline buckets, not the provider's raw prompt total.
        total_tokens: input_tokens.saturating_add(output_tokens),
        input_cache_read,
        input_cache_creation: 0,
    }
}

// ── Error classification ───────────────────────────────────────────────────

/// Classify a non-zero exit by its stderr.
///
/// The reference has no classification at all — every failure is
/// `Antigravity exited with code N: <stderr>`. The Rust engine needs
/// auth/quota/model-not-found to be distinguishable, so the stderr is
/// scanned for the phrases those failures carry. Anything unrecognized stays
/// a protocol failure (the bridge produced no usable result).
#[must_use]
pub fn classify_exit(exit: &ExitInfo, stderr: &str) -> String {
    let haystack = stderr.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| haystack.contains(needle));
    let code = exit
        .code
        .map_or_else(|| "signal".to_string(), |code| code.to_string());
    let detail = stderr.trim();
    let detail = if detail.len() > 400 {
        detail.chars().take(400).collect::<String>()
    } else {
        detail.to_string()
    };
    if has(&[
        "unauthorized",
        "unauthenticated",
        "permission denied",
        "invalid api key",
        "api key not valid",
        "credential",
        "401",
        "403",
    ]) {
        return format!("{ERR_AUTH}: agy exited with code {code}: {detail}");
    }
    if has(&[
        "quota",
        "rate limit",
        "rate_limit",
        "resource_exhausted",
        "too many requests",
        "429",
    ]) {
        return format!("{ERR_QUOTA}: agy exited with code {code}: {detail}");
    }
    if has(&[
        "model not found",
        "unsupported model",
        "unknown model",
        "invalid model",
        "no such model",
        "404",
    ]) {
        return format!("{ERR_MODEL_NOT_FOUND}: agy exited with code {code}: {detail}");
    }
    format!("{ERR_PROTOCOL}: agy exited with code {code}: {detail}")
}

fn is_retryable(error: &str) -> bool {
    error.starts_with(ERR_QUOTA) || error.starts_with(ERR_TIMEOUT)
}

// ── Transcript side channel ────────────────────────────────────────────────

#[derive(Deserialize)]
struct TranscriptEntry {
    #[serde(rename = "type")]
    kind: Option<String>,
    thinking: Option<String>,
    tool_calls: Option<Vec<TranscriptToolCall>>,
    content: Option<String>,
}

#[derive(Deserialize)]
struct TranscriptToolCall {
    name: Option<String>,
    #[serde(default)]
    args: serde_json::Value,
}

/// The CLI's internal activity, rendered as thinking text.
///
/// `root` is `None` until `init` names a conversation; the reader is a no-op
/// before that. Offsets and a pending buffer make partial lines safe, and a
/// shrunken file resets both.
#[derive(Debug, Default)]
pub struct TranscriptReader {
    root: Option<PathBuf>,
    offset: u64,
    pending: Vec<u8>,
    last_thinking_len: usize,
    awaiting_tool_result: bool,
    /// Last-resort response text, mirroring v2's `finalResponseText`.
    pub final_text: Option<String>,
    pub io_errors: usize,
}

impl TranscriptReader {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root: Some(root),
            ..Self::default()
        }
    }

    pub fn set_conversation(&mut self, root: PathBuf) {
        self.root = Some(root);
    }

    /// Read the new tail and return the thinking fragments it produced.
    ///
    /// `flush` treats an unterminated last line as complete (process exit).
    pub async fn poll(&mut self, flush: bool) -> Result<Vec<String>, String> {
        let Some(root) = self.root.clone() else {
            return Ok(Vec::new());
        };
        let path = root
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) => metadata,
            // The transcript is created by the child after it starts, so an
            // absent file is the normal "nothing written yet" state.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            // Anything else is a read failure, not evidence that the
            // transcript does not exist — the remaining reads treat it the
            // same way.
            Err(error) => {
                self.io_errors += 1;
                return Err(format!("reading transcript metadata: {error}"));
            }
        };
        let size = metadata.len();
        if size < self.offset {
            self.offset = 0;
            self.pending.clear();
        }
        let mut chunk = Vec::new();
        if size > self.offset {
            let mut file = match tokio::fs::File::open(&path).await {
                Ok(file) => file,
                Err(error) => {
                    self.io_errors += 1;
                    return Err(format!("opening transcript: {error}"));
                }
            };
            if let Err(error) = file.seek(std::io::SeekFrom::Start(self.offset)).await {
                self.io_errors += 1;
                return Err(format!("seeking transcript: {error}"));
            }
            if let Err(error) = file.read_to_end(&mut chunk).await {
                self.io_errors += 1;
                return Err(format!("reading transcript: {error}"));
            }
            self.offset += chunk.len() as u64;
        } else if !flush {
            return Ok(Vec::new());
        }

        let mut combined = std::mem::take(&mut self.pending);
        combined.extend_from_slice(&chunk);
        let boundary = if flush {
            combined.len()
        } else {
            combined
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |index| index + 1)
        };
        if boundary == 0 && !combined.is_empty() {
            self.pending = combined;
            return Ok(Vec::new());
        }
        let complete = combined[..boundary].to_vec();
        self.pending = combined[boundary..].to_vec();

        let mut thoughts = Vec::new();
        for line in complete.split(|byte| *byte == b'\n') {
            let line = trim_ascii(line);
            if line.is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_slice::<TranscriptEntry>(line) else {
                continue;
            };
            match entry.kind.as_deref() {
                Some("PLANNER_RESPONSE") => {
                    if let Some(thinking) = entry.thinking
                        && thinking.len() > self.last_thinking_len
                    {
                        let delta = thinking[self.last_thinking_len..].to_string();
                        self.last_thinking_len = thinking.len();
                        if !delta.is_empty() {
                            thoughts.push(delta);
                        }
                    }
                    if let Some(calls) = entry.tool_calls {
                        for call in calls {
                            let Some(name) = call.name else { continue };
                            let args = format_tool_args(&call.args);
                            thoughts.push(format!("\n🔧 {name}({args})"));
                            self.awaiting_tool_result = true;
                        }
                    }
                    if let Some(content) = entry.content
                        && !content.is_empty()
                        && self.final_text.is_none()
                    {
                        self.final_text = Some(content);
                    }
                }
                Some("GENERIC") if self.awaiting_tool_result => {
                    self.awaiting_tool_result = false;
                    if let Some(content) = entry.content {
                        let first = content
                            .split('\n')
                            .find(|line| !line.trim().is_empty())
                            .unwrap_or("");
                        let snippet = first.chars().take(160).collect::<String>();
                        if !snippet.is_empty() {
                            thoughts.push(format!("\n   ↳ {snippet}"));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(thoughts)
    }
}

fn format_tool_args(args: &serde_json::Value) -> String {
    let Some(object) = args.as_object() else {
        return String::new();
    };
    let parts = object
        .iter()
        .filter(|(key, _)| key.as_str() != "toolAction" && key.as_str() != "toolSummary")
        .take(3)
        .map(|(key, value)| {
            let rendered = match value {
                serde_json::Value::String(text) => text.chars().take(120).collect::<String>(),
                other => other.to_string().chars().take(120).collect::<String>(),
            };
            format!("{key}: {rendered}")
        })
        .collect::<Vec<_>>();
    parts.join(", ")
}

// ── The transport ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Remembered {
    id: String,
    messages: Vec<LLMMessage>,
}

/// The antigravity LLM transport.
pub struct AntigravityLlm {
    config: NativeLlmConfig,
    system_prompt: String,
    sink: Option<EventSink>,
    stream_agent_id: Mutex<Option<String>>,
    turn_watch: Mutex<Option<Arc<kimi_core::watch::TurnWatchState>>>,
    remembered: Mutex<Option<Remembered>>,
    transcript_root: Option<PathBuf>,
    runner: Arc<dyn AntigravityRunner>,
    limits: AntigravityLimits,
    binary: String,
}

impl AntigravityLlm {
    #[must_use]
    pub fn new(config: NativeLlmConfig, system_prompt: String) -> Self {
        let limits = config.antigravity.clone().unwrap_or_default();
        let binary = config
            .binary_path
            .clone()
            .filter(|path| !path.is_empty())
            .unwrap_or_else(detect_binary);
        Self {
            config,
            system_prompt,
            sink: None,
            stream_agent_id: Mutex::new(None),
            turn_watch: Mutex::new(None),
            remembered: Mutex::new(None),
            transcript_root: None,
            runner: Arc::new(TokioRunner),
            limits,
            binary,
        }
    }

    #[must_use]
    pub fn with_sink(mut self, sink: EventSink) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Root of the transcript tree (`<root>/<conversation>/.system_generated/
    /// logs/transcript.jsonl`), injectable so tests need no real CLI state.
    #[must_use]
    pub fn with_transcript_root(mut self, root: PathBuf) -> Self {
        self.transcript_root = Some(root);
        self
    }

    #[must_use]
    pub fn with_runner(mut self, runner: Arc<dyn AntigravityRunner>) -> Self {
        self.runner = runner;
        self
    }

    #[must_use]
    pub fn with_limits(mut self, limits: AntigravityLimits) -> Self {
        self.limits = limits;
        self
    }

    #[must_use]
    pub fn with_binary(mut self, binary: String) -> Self {
        self.binary = binary;
        self
    }

    fn emit_delta(&self, delta: StreamDelta) {
        if let Some(watch) = self
            .turn_watch
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            watch.note_partial();
        }
        if let Some(sink) = &self.sink {
            let mut event = serde_json::json!({
                "type": "llm.delta",
                "part": delta.to_part(),
            });
            if let Some(agent_id) = self
                .stream_agent_id
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_deref()
            {
                event["agent_id"] = serde_json::Value::String(agent_id.to_string());
            }
            sink(event);
        }
    }

    fn transcript_path(&self, conversation_id: &str) -> PathBuf {
        match &self.transcript_root {
            Some(root) => root.join(conversation_id),
            None => crate::default_workspace::user_home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".gemini")
                .join("antigravity-cli")
                .join("brain")
                .join(conversation_id),
        }
    }

    async fn chat_impl(
        &self,
        params: LLMChatParams,
    ) -> Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>> {
        let started_at = Instant::now();
        let history: &[LLMMessage] = &params.messages;

        let remembered = self
            .remembered
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        // v2 drops the remembered thread as soon as the history is not a
        // strict extension, so a rewritten history cannot resume stale state.
        if let Some(state) = &remembered
            && !extends_messages(&state.messages, history)
        {
            self.remembered
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
        }
        let resume = remembered
            .as_ref()
            .filter(|state| extends_messages(&state.messages, history))
            .map(|state| state.id.clone());
        let (prompt, conversation_id) = build_prompt(history, resume.as_deref());

        let model = map_model(
            &self.config.model,
            if self.config.adaptive_thinking == Some(true) {
                Some("high")
            } else {
                None
            },
        );
        let mut args = Vec::new();
        if let Some(id) = &conversation_id {
            args.push("--conversation".to_string());
            args.push(id.clone());
        }
        args.push("-p".to_string());
        args.push(prompt);
        args.push("--model".to_string());
        args.push(model);
        args.push("--disable-slash-commands".to_string());
        args.push("--output-format".to_string());
        args.push("stream-json".to_string());
        args.push("--dangerously-skip-permissions".to_string());

        let spec = SpawnSpec {
            binary: self.binary.clone(),
            args,
            cwd: std::env::temp_dir(),
            env: child_env(),
        };
        let total = Deadline::new(
            "antigravity request",
            Duration::from_millis(self.limits.total_ms),
        );
        let spawn_limit = Duration::from_millis(self.limits.spawn_ms).min(total.remaining());
        let child = bounded(
            "antigravity request",
            "waiting for the agy process to spawn",
            spawn_limit,
            self.runner.spawn(spec),
        )
        .await
        .map_err(|timeout| format!("{ERR_TIMEOUT}: {timeout}"))?
        .map_err(|error| {
            if error.starts_with("antigravity.") {
                error
            } else {
                format!("{ERR_SPAWN}: {error}")
            }
        })?;
        let mut child = child;
        let sent_ms = started_at.elapsed().as_millis() as i64;
        let mut first_event_ms = None;

        let mut decoder = FrameDecoder::new();
        let mut transcript = TranscriptReader::default();
        if let Some(root) = &self.transcript_root {
            transcript.set_conversation(root.clone());
        }

        let mut content = String::new();
        let mut thinking = String::new();
        let mut final_text = String::new();
        let mut usage = TokenUsage::default();
        let mut saw_output = false;
        let mut frames_seen = 0usize;
        let mut yielded_text = false;
        let mut conversation = conversation_id.clone();

        let mut ticker = tokio::time::interval(TRANSCRIPT_POLL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Stream progress marks: a transcript tick must not reset the stream
        // timeout, so the wait is measured from the last stdout chunk, not from
        // the last loop iteration.
        let mut last_output_at = Instant::now();

        let mut stdout_closed = false;
        while !stdout_closed {
            let (stage, stage_limit) = if saw_output {
                (
                    "waiting for the next stream frame",
                    Duration::from_millis(self.limits.idle_ms)
                        .saturating_sub(last_output_at.elapsed()),
                )
            } else {
                (
                    "waiting for the first stream frame",
                    Duration::from_millis(self.limits.first_frame_ms)
                        .saturating_sub(started_at.elapsed()),
                )
            };
            let remaining = total.remaining();
            let (stage, stage_limit) = if remaining < stage_limit {
                (
                    "waiting for the request to finish within the total budget",
                    remaining,
                )
            } else {
                (stage, stage_limit)
            };
            if stage_limit.is_zero() {
                child.kill();
                return Err(format!(
                    "{ERR_TIMEOUT}: antigravity request timed out while {stage} (limit 0ms)"
                )
                .into());
            }

            let cancel = params.cancel.clone();
            let chunk = tokio::select! {
                biased;
                _ = cancel_future(cancel) => {
                    child.kill();
                    return Err(CANCELLED.into());
                }
                chunk = bounded(
                    "antigravity stream",
                    stage,
                    stage_limit,
                    child.recv_chunk(),
                ) => match chunk {
                    Ok(chunk) => chunk,
                    Err(timeout) => {
                        child.kill();
                        return Err(format!("{ERR_TIMEOUT}: {timeout}").into());
                    }
                },
                _ = ticker.tick() => {
                    if let Some(root) = conversation.as_deref().map(|id| self.transcript_path(id)) {
                        transcript.set_conversation(root);
                        match bounded(
                            "antigravity transcript",
                            "reading the transcript side channel",
                            TRANSCRIPT_READ_WAIT,
                            transcript.poll(false),
                        )
                        .await
                        {
                            Ok(Ok(thoughts)) => {
                                for thought in thoughts {
                                    if first_event_ms.is_none() {
                                        first_event_ms = Some(started_at.elapsed().as_millis() as i64);
                                    }
                                    thinking.push_str(&thought);
                                    self.emit_delta(StreamDelta::Think(thought));
                                }
                            }
                            Ok(Err(error)) => {
                                tracing::warn!(target: "llm.antigravity", %error, "transcript read failed");
                            }
                            Err(timeout) => {
                                transcript.io_errors += 1;
                                tracing::warn!(
                                    target: "llm.antigravity",
                                    %timeout,
                                    "transcript read timed out"
                                );
                            }
                        }
                    }
                    continue;
                }
            };

            let Some(chunk) = chunk else {
                stdout_closed = true;
                continue;
            };
            saw_output = true;
            last_output_at = Instant::now();
            let bytes = match chunk {
                Ok(bytes) => bytes,
                Err(error) => {
                    child.kill();
                    return Err(error.into());
                }
            };
            for frame in decoder.push(&bytes) {
                frames_seen += 1;
                if first_event_ms.is_none() {
                    first_event_ms = Some(started_at.elapsed().as_millis() as i64);
                }
                match frame {
                    Frame::Init {
                        conversation_id: id,
                    } => {
                        conversation = Some(id.clone());
                        transcript.set_conversation(self.transcript_path(&id));
                    }
                    Frame::StepUpdate(update) => {
                        if let Some(text) = update.text_delta
                            && !text.is_empty()
                        {
                            if !text.trim().is_empty() {
                                yielded_text = true;
                            }
                            content.push_str(&text);
                            self.emit_delta(StreamDelta::Text(text));
                        }
                        if let Some(delta) = update.thinking_delta
                            && !delta.is_empty()
                        {
                            thinking.push_str(&delta);
                            self.emit_delta(StreamDelta::Think(delta));
                        }
                        if let Some(raw) = update.usage {
                            usage = parse_usage(&raw);
                        }
                    }
                    Frame::Result(result) => {
                        if let Some(response) = result.response
                            && !response.is_empty()
                        {
                            final_text = response;
                        }
                        if let Some(raw) = result.usage {
                            usage = parse_usage(&raw);
                        }
                    }
                }
            }
        }

        for frame in decoder.finish() {
            frames_seen += 1;
            match frame {
                Frame::Init {
                    conversation_id: id,
                } => {
                    conversation = Some(id.clone());
                    transcript.set_conversation(self.transcript_path(&id));
                }
                Frame::StepUpdate(update) => {
                    if let Some(text) = update.text_delta
                        && !text.is_empty()
                    {
                        if !text.trim().is_empty() {
                            yielded_text = true;
                        }
                        content.push_str(&text);
                        self.emit_delta(StreamDelta::Text(text));
                    }
                    if let Some(delta) = update.thinking_delta
                        && !delta.is_empty()
                    {
                        thinking.push_str(&delta);
                        self.emit_delta(StreamDelta::Think(delta));
                    }
                    if let Some(raw) = update.usage {
                        usage = parse_usage(&raw);
                    }
                }
                Frame::Result(result) => {
                    if let Some(response) = result.response
                        && !response.is_empty()
                    {
                        final_text = response;
                    }
                    if let Some(raw) = result.usage {
                        usage = parse_usage(&raw);
                    }
                }
            }
        }
        if let Some(root) = conversation.as_deref().map(|id| self.transcript_path(id)) {
            transcript.set_conversation(root);
            match bounded(
                "antigravity transcript",
                "flushing the transcript side channel",
                TRANSCRIPT_READ_WAIT,
                transcript.poll(true),
            )
            .await
            {
                Ok(Ok(thoughts)) => {
                    for thought in thoughts {
                        thinking.push_str(&thought);
                        self.emit_delta(StreamDelta::Think(thought));
                    }
                }
                Ok(Err(error)) => {
                    tracing::warn!(target: "llm.antigravity", %error, "transcript flush failed");
                }
                Err(timeout) => {
                    transcript.io_errors += 1;
                    tracing::warn!(target: "llm.antigravity", %timeout, "transcript flush timed out");
                }
            }
        }

        let exit = bounded(
            "antigravity process",
            "waiting for the agy process to exit",
            AGY_EXIT_WAIT,
            child.wait_exit(),
        )
        .await
        .map_err(|timeout| {
            child.kill();
            format!("{ERR_TIMEOUT}: {timeout}")
        })?;
        let exit = match exit {
            Ok(exit) => exit,
            Err(error) => {
                child.kill();
                return Err(error.into());
            }
        };

        if !decoder.trouble.is_empty() {
            tracing::warn!(
                target: "llm.antigravity",
                malformed = decoder.trouble.malformed,
                unknown = decoder.trouble.unknown,
                first_malformed = decoder.trouble.first_malformed.as_deref().unwrap_or(""),
                first_unknown = decoder.trouble.first_unknown.as_deref().unwrap_or(""),
                "antigravity stream contained frames the decoder had to ignore: {}",
                decoder.trouble.describe()
            );
        }
        if transcript.io_errors > 0 {
            tracing::warn!(
                target: "llm.antigravity",
                io_errors = transcript.io_errors,
                "antigravity transcript side channel had read failures"
            );
        }
        // A stream that yielded no frames at all is as suspicious as one with
        // frames the decoder could not use: a clean exit would otherwise hide
        // a CLI that printed nothing.
        let mut stream_notes = Vec::new();
        if !decoder.trouble.is_empty() {
            stream_notes.push(decoder.trouble.describe());
        }
        if frames_seen == 0 {
            tracing::warn!(
                target: "llm.antigravity",
                "antigravity stream produced no frames"
            );
            stream_notes.push("the stream produced no frames".to_string());
        }

        if final_text.is_empty()
            && let Some(text) = transcript.final_text.clone()
        {
            final_text = text;
        }

        let succeeded = exit.code == Some(0);
        if !succeeded {
            // v2 yields whatever text it already produced before throwing, so
            // a truncated answer is not lost.
            if !yielded_text && !final_text.is_empty() {
                content.push_str(&final_text);
                self.emit_delta(StreamDelta::Text(final_text.clone()));
            }
            self.remembered
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            let mut error = classify_exit(&exit, &child.stderr_tail());
            if !stream_notes.is_empty() {
                error.push_str(&format!(" (stream: {})", stream_notes.join("; ")));
            }
            return Err(error.into());
        }

        if !yielded_text {
            // v2 substitutes a single space so an empty answer never becomes
            // an empty assistant message.
            let fallback = if final_text.is_empty() {
                " ".to_string()
            } else {
                final_text
            };
            content.push_str(&fallback);
            self.emit_delta(StreamDelta::Text(fallback));
        }

        if let Some(id) = conversation {
            *self
                .remembered
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(Remembered {
                id,
                messages: history.to_vec(),
            });
        }

        let mut thinking_blocks = Vec::new();
        if !thinking.trim().is_empty() {
            thinking_blocks.push(ContentBlock::Think {
                think: thinking,
                encrypted: None,
                details_index: None,
                reasoning_key: None,
                details_summary: None,
            });
        }

        let ended_ms = started_at.elapsed().as_millis() as i64;
        let timing = LlmTiming::from_marks(0, Some(sent_ms), first_event_ms, ended_ms);
        Ok(LLMChatResponse {
            content,
            thinking: thinking_blocks,
            tool_calls: Vec::new(),
            finish_reason: Some("stop".to_string()),
            usage,
            timing: Some(timing),
        })
    }
}

async fn cancel_future(cancel: Option<tokio_util::sync::CancellationToken>) {
    match cancel {
        Some(token) => token.cancelled().await,
        None => std::future::pending().await,
    }
}

impl LLM for AntigravityLlm {
    fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    fn model_name(&self) -> &str {
        &self.config.model
    }

    fn is_retryable_error(&self, error: &str) -> bool {
        is_retryable(error)
    }

    fn transport(&self) -> &'static str {
        "native-antigravity"
    }

    fn model_alias(&self) -> Option<&str> {
        self.config.alias.as_deref()
    }

    fn provider_type(&self) -> Option<&str> {
        self.config.provider_type.as_deref()
    }

    fn protocol(&self) -> Option<&str> {
        Some("antigravity")
    }

    fn thinking_effort(&self) -> Option<&str> {
        self.config.reasoning_effort.as_deref()
    }

    fn media_target(&self) -> Option<crate::llm::media_resolver::MediaTarget> {
        Some(crate::llm::media_resolver::MediaTarget {
            protocol: "antigravity".to_string(),
            base_url: String::new(),
            provider_key: self
                .config
                .provider_type
                .clone()
                .unwrap_or_else(|| "antigravity".to_string()),
            provider_type: self.config.provider_type.clone().unwrap_or_default(),
            // v2 declares image/video/audio false for this provider.
            image_in: false,
            video_in: false,
            audio_in: false,
            uploads_media: false,
            inline_video: false,
        })
    }

    fn set_stream_agent_id(&self, agent_id: &str) {
        *self
            .stream_agent_id
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(agent_id.to_string());
    }

    fn set_turn_watch(&self, watch: Option<Arc<kimi_core::watch::TurnWatchState>>) {
        *self
            .turn_watch
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = watch;
    }

    fn chat(
        &self,
        params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        Box::pin(self.chat_impl(params))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};

    use kimi_core::types::ToolInfo;

    // ── Scripted process ───────────────────────────────────────────────────

    #[derive(Clone)]
    enum Step {
        Chunk(&'static str),
        Delay(Duration),
    }

    struct ScriptedChild {
        steps: VecDeque<Step>,
        exit: ExitInfo,
        stderr: String,
        killed: Arc<AtomicBool>,
    }

    impl ScriptedChild {
        fn new(steps: Vec<Step>, code: i32, stderr: &str) -> Self {
            Self {
                steps: steps.into(),
                exit: ExitInfo {
                    code: Some(code),
                    signal: None,
                },
                stderr: stderr.to_string(),
                killed: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    #[async_trait]
    impl AntigravityChild for ScriptedChild {
        async fn recv_chunk(&mut self) -> Option<Result<Vec<u8>, String>> {
            // A cancelled `recv_chunk` must not consume the pending step: the
            // transport drops the read future on every timeout stage retry.
            let step = self.steps.front()?.clone();
            match step {
                Step::Chunk(text) => {
                    self.steps.pop_front();
                    Some(Ok(text.as_bytes().to_vec()))
                }
                Step::Delay(duration) => {
                    tokio::time::sleep(duration).await;
                    self.steps.pop_front();
                    None
                }
            }
        }

        async fn wait_exit(&mut self) -> Result<ExitInfo, String> {
            Ok(self.exit.clone())
        }

        fn stderr_tail(&self) -> String {
            self.stderr.clone()
        }

        fn kill(&mut self) {
            self.killed.store(true, Ordering::SeqCst);
        }
    }

    struct ScriptedRunner {
        children: Mutex<VecDeque<ScriptedChild>>,
        specs: Mutex<Vec<SpawnSpec>>,
    }

    impl ScriptedRunner {
        fn new(children: Vec<ScriptedChild>) -> Arc<Self> {
            Arc::new(Self {
                children: Mutex::new(children.into()),
                specs: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl AntigravityRunner for ScriptedRunner {
        async fn spawn(&self, spec: SpawnSpec) -> Result<Box<dyn AntigravityChild>, String> {
            self.specs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(spec);
            let child = self
                .children
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pop_front()
                .ok_or_else(|| "no scripted child left".to_string())?;
            Ok(Box::new(child))
        }
    }

    fn base_config() -> NativeLlmConfig {
        NativeLlmConfig {
            protocol: "antigravity".to_string(),
            alias: None,
            provider_type: Some("antigravity".to_string()),
            base_url: String::new(),
            api_key: String::new(),
            api_key_env: None,
            model: "gemini-3.7-flash".to_string(),
            max_tokens: None,
            custom_headers: std::collections::HashMap::new(),
            reasoning_effort: None,
            thinking_budget: None,
            auth_provider: None,
            thinking_keep: None,
            beta_api: false,
            capabilities: None,
            system_prompt: None,
            max_input_size: None,
            adaptive_thinking: None,
            reasoning_key: None,
            off_effort: None,
            response_format: None,
            sampling: None,
            determinism: None,
            extra_params: None,
            binary_path: Some("agy".to_string()),
            antigravity: None,
        }
    }

    fn params(messages: Vec<LLMMessage>) -> LLMChatParams {
        LLMChatParams {
            messages: Arc::from(messages),
            tools: Arc::<[ToolInfo]>::from(Vec::new()),
            cancel: None,
            used_context_tokens: None,
            max_context_tokens: None,
        }
    }

    fn user(text: &str) -> LLMMessage {
        LLMMessage::new("user", text)
    }

    fn limited(
        spawn_ms: u64,
        first_frame_ms: u64,
        idle_ms: u64,
        total_ms: u64,
    ) -> AntigravityLimits {
        AntigravityLimits {
            spawn_ms,
            first_frame_ms,
            idle_ms,
            total_ms,
        }
    }

    // ── Frame decoding ─────────────────────────────────────────────────────

    #[test]
    fn decoder_handles_half_and_coalesced_frames() {
        let mut decoder = FrameDecoder::new();
        let init = r#"{"event":"init","conversation_id":"conv-1"}"#;
        let step = r#"{"event":"step_update","step_update":{"text_delta":"hello"}}"#;
        let combined = format!("{init}\n{step}\n");

        // One byte at a time: every frame is split across pushes.
        let mut frames = Vec::new();
        for byte in combined.as_bytes() {
            frames.extend(decoder.push(&[*byte]));
        }
        assert_eq!(
            frames,
            vec![
                Frame::Init {
                    conversation_id: "conv-1".to_string()
                },
                Frame::StepUpdate(StepUpdate {
                    text_delta: Some("hello".to_string()),
                    thinking_delta: None,
                    usage: None,
                }),
            ]
        );

        // Coalesced: two complete lines plus a half one in one push; the half
        // line is a trailing line with no newline, so EOF flushes it.
        let mut decoder = FrameDecoder::new();
        let coalesced = format!("{init}\n{step}\n{step}");
        let split = coalesced.len() - 4;
        let frames = decoder.push(coalesced[..split].as_bytes());
        assert_eq!(frames.len(), 2);
        let frames = decoder.push(coalesced[split..].as_bytes());
        assert!(frames.is_empty());
        assert_eq!(decoder.finish().len(), 1);
        assert!(decoder.trouble.is_empty());
    }

    #[test]
    fn decoder_counts_malformed_and_unknown_lines_with_the_first_sample() {
        let mut decoder = FrameDecoder::new();
        let frames = decoder.push(
            b"not json\n{\"event\":\"brand_new_event\"}\n\n{\"event\":\"even_newer\"}\n{\"oops\":\n",
        );
        assert!(frames.is_empty());
        assert_eq!(decoder.trouble.malformed, 2);
        assert_eq!(decoder.trouble.unknown, 2);
        assert_eq!(decoder.trouble.first_malformed.as_deref(), Some("not json"));
        assert_eq!(
            decoder.trouble.first_unknown.as_deref(),
            Some("brand_new_event")
        );
        let described = decoder.trouble.describe();
        assert!(described.contains("malformed"));
        assert!(described.contains("unknown"));
    }

    // ── Prompt / model / usage ─────────────────────────────────────────────

    #[test]
    fn model_mapping_matches_the_reference_aliases() {
        assert_eq!(map_model("gemini-3.7-flash", None), "gemini-3.7-flash-high");
        assert_eq!(
            map_model("gemini-3.7-flash", Some("low")),
            "gemini-3.7-flash-low"
        );
        assert_eq!(
            map_model("Gemini 3.1 Pro", Some("low")),
            "gemini-3.1-pro-low"
        );
        assert_eq!(map_model("gemini-3.1-pro", None), "gemini-3.1-pro-high");
        assert_eq!(map_model("gemini-2.5-pro", None), "gemini-3.1-pro-high");
        assert_eq!(map_model("gemini-1.5-flash", None), "gemini-3.7-flash-high");
        assert_eq!(map_model("claude-sonnet-4", None), "claude-sonnet-4-6");
        assert_eq!(map_model("claude-opus-4", None), "claude-opus-4-6-thinking");
        assert_eq!(map_model("gpt-oss-120b", None), "gpt-oss-120b-medium");
        assert_eq!(map_model("kimi-k2", None), "kimi-k2");
    }

    #[test]
    fn prompt_resumes_only_an_extended_multi_turn_history() {
        let first = vec![user("first")];
        assert_eq!(build_prompt(&first, None), ("first".to_string(), None));
        assert_eq!(
            build_prompt(&first, Some("conv-1")),
            ("first".to_string(), None)
        );

        let two = vec![
            user("first"),
            LLMMessage::new("assistant", "reply"),
            user("second"),
        ];
        assert_eq!(
            build_prompt(&two, Some("conv-1")),
            ("second".to_string(), Some("conv-1".to_string()))
        );
        let (prompt, id) = build_prompt(&two, None);
        assert_eq!(id, None);
        assert!(prompt.starts_with("[User]:\nfirst"));
        assert!(prompt.contains("[Assistant]:\nreply"));
        assert!(prompt.ends_with("[User]:\nsecond"));

        assert_eq!(build_prompt(&[], None), ("hi".to_string(), None));
    }

    #[test]
    fn usage_reuses_the_shared_cache_split() {
        let usage = parse_usage(&RawUsage {
            input_tokens: Some(100),
            output_tokens: Some(50),
            cache_read_tokens: Some(30),
        });
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.input_cache_read, 30);
        assert_eq!(usage.input_cache_creation, 0);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.total_tokens, 150);

        let empty = parse_usage(&RawUsage::default());
        assert_eq!(empty.total_tokens, 0);
    }

    #[test]
    fn error_classification_covers_the_six_classes() {
        let exit = |code: i32| ExitInfo {
            code: Some(code),
            signal: None,
        };
        assert!(classify_exit(&exit(1), "401 Unauthorized: invalid API key").starts_with(ERR_AUTH));
        assert!(
            classify_exit(&exit(1), "429 RESOURCE_EXHAUSTED: quota exceeded")
                .starts_with(ERR_QUOTA)
        );
        assert!(
            classify_exit(&exit(1), "model not found: gemini-x").starts_with(ERR_MODEL_NOT_FOUND)
        );
        assert!(classify_exit(&exit(2), "boom").starts_with(ERR_PROTOCOL));
        assert!(is_retryable(&format!("{ERR_QUOTA}: x")));
        assert!(is_retryable(&format!("{ERR_TIMEOUT}: x")));
        assert!(!is_retryable(&format!("{ERR_AUTH}: x")));
        assert!(!is_retryable(&format!("{ERR_PROTOCOL}: x")));
    }

    // ── Chat against a scripted process ────────────────────────────────────

    #[tokio::test]
    async fn chat_streams_text_thinking_and_usage() {
        let child = ScriptedChild::new(
            vec![
                Step::Chunk("{\"event\":\"init\",\"conversation_id\":\"conv-1\"}\n"),
                Step::Chunk(
                    "{\"event\":\"step_update\",\"step_update\":{\"thinking_delta\":\"why\",\"text_delta\":\"hel\"}}\n",
                ),
                Step::Chunk(
                    "{\"event\":\"step_update\",\"step_update\":{\"text_delta\":\"lo\",\"usage\":{\"input_tokens\":100,\"output_tokens\":50,\"cache_read_tokens\":30}}}\n",
                ),
                Step::Chunk("{\"event\":\"result\",\"result\":{\"response\":\"hello\"}}\n"),
            ],
            0,
            "",
        );
        let runner = ScriptedRunner::new(vec![child]);
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let sink_events = Arc::clone(&emitted);
        let llm = AntigravityLlm::new(base_config(), "sys".to_string())
            .with_runner(Arc::clone(&runner) as Arc<dyn AntigravityRunner>)
            .with_sink(Arc::new(move |event: serde_json::Value| {
                sink_events
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(event);
            }));

        let response = llm.chat(params(vec![user("hi")])).await.unwrap();
        assert_eq!(response.content, "hello");
        assert_eq!(response.finish_reason.as_deref(), Some("stop"));
        assert!(response.tool_calls.is_empty());
        assert_eq!(response.usage.total_tokens, 150);
        match &response.thinking[0] {
            ContentBlock::Think { think, .. } => assert_eq!(think, "why"),
            other => panic!("unexpected thinking block: {other:?}"),
        }
        let events = emitted.lock().unwrap();
        assert!(events.iter().any(|event| event["part"]["text"] == "hel"));
        assert!(events.iter().any(|event| event["part"]["think"] == "why"));

        let specs = runner.specs.lock().unwrap();
        assert_eq!(specs.len(), 1);
        let args = &specs[0].args;
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"hi".to_string()));
        assert!(args.contains(&"--output-format".to_string()));
        assert!(args.contains(&"stream-json".to_string()));
        assert!(!args.contains(&"--conversation".to_string()));
    }

    #[tokio::test]
    async fn chat_returns_a_single_space_for_an_empty_stream() {
        let child = ScriptedChild::new(vec![], 0, "");
        let runner = ScriptedRunner::new(vec![child]);
        let llm = AntigravityLlm::new(base_config(), "sys".to_string())
            .with_runner(Arc::clone(&runner) as Arc<dyn AntigravityRunner>);
        let response = llm.chat(params(vec![user("hi")])).await.unwrap();
        assert_eq!(response.content, " ");
    }

    #[tokio::test]
    async fn chat_preserves_streamed_text_on_a_non_zero_exit() {
        let child = ScriptedChild::new(
            vec![Step::Chunk(
                "{\"event\":\"step_update\",\"step_update\":{\"text_delta\":\"partial\"}}\n",
            )],
            1,
            "429 RESOURCE_EXHAUSTED: quota exceeded",
        );
        let runner = ScriptedRunner::new(vec![child]);
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let sink_events = Arc::clone(&emitted);
        let llm = AntigravityLlm::new(base_config(), "sys".to_string())
            .with_runner(Arc::clone(&runner) as Arc<dyn AntigravityRunner>)
            .with_sink(Arc::new(move |event: serde_json::Value| {
                sink_events
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(event);
            }));

        let error = llm.chat(params(vec![user("hi")])).await.unwrap_err();
        let message = error.to_string();
        assert!(message.starts_with(ERR_QUOTA), "{message}");
        assert!(llm.is_retryable_error(&message));
        // The already-streamed text reached the sink before the failure.
        let events = emitted.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|event| event["part"]["text"] == "partial")
        );
    }

    #[tokio::test]
    async fn chat_reports_malformed_lines_in_the_failure() {
        let child = ScriptedChild::new(
            vec![Step::Chunk("garbage\n{\"event\":\"who_knows\"}\n")],
            1,
            "boom",
        );
        let runner = ScriptedRunner::new(vec![child]);
        let llm = AntigravityLlm::new(base_config(), "sys".to_string())
            .with_runner(Arc::clone(&runner) as Arc<dyn AntigravityRunner>);
        let error = llm
            .chat(params(vec![user("hi")]))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.starts_with(ERR_PROTOCOL), "{error}");
        assert!(error.contains("malformed"), "{error}");
        assert!(error.contains("unknown"), "{error}");
        assert!(error.contains("garbage"), "{error}");
        assert!(!llm.is_retryable_error(&error));
    }

    #[tokio::test]
    async fn chat_first_frame_timeout_is_bounded_retryable_and_kills_the_child() {
        let killed = Arc::new(AtomicBool::new(false));
        let mut child = ScriptedChild::new(vec![Step::Delay(Duration::from_secs(5))], 0, "");
        child.killed = Arc::clone(&killed);
        let runner = ScriptedRunner::new(vec![child]);
        let llm = AntigravityLlm::new(base_config(), "sys".to_string())
            .with_runner(Arc::clone(&runner) as Arc<dyn AntigravityRunner>)
            .with_limits(limited(1_000, 30, 1_000, 5_000));

        let error = llm
            .chat(params(vec![user("hi")]))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.starts_with(ERR_TIMEOUT), "{error}");
        assert!(error.contains("first stream frame"), "{error}");
        assert!(error.contains("limit"), "{error}");
        assert!(llm.is_retryable_error(&error));
        assert!(killed.load(Ordering::SeqCst), "the child must be killed");
    }

    #[tokio::test]
    async fn chat_idle_timeout_counts_from_the_last_frame() {
        let child = ScriptedChild::new(
            vec![
                Step::Chunk("{\"event\":\"init\",\"conversation_id\":\"conv-1\"}\n"),
                Step::Delay(Duration::from_secs(5)),
            ],
            0,
            "",
        );
        let runner = ScriptedRunner::new(vec![child]);
        let llm = AntigravityLlm::new(base_config(), "sys".to_string())
            .with_runner(Arc::clone(&runner) as Arc<dyn AntigravityRunner>)
            .with_limits(limited(1_000, 1_000, 30, 5_000));

        let error = llm
            .chat(params(vec![user("hi")]))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.starts_with(ERR_TIMEOUT), "{error}");
        assert!(error.contains("next stream frame"), "{error}");
        assert!(llm.is_retryable_error(&error));
    }

    #[tokio::test]
    async fn chat_remembers_the_conversation_for_the_next_turn() {
        let first = ScriptedChild::new(
            vec![
                Step::Chunk("{\"event\":\"init\",\"conversation_id\":\"conv-1\"}\n"),
                Step::Chunk("{\"event\":\"result\",\"result\":{\"response\":\"one\"}}\n"),
            ],
            0,
            "",
        );
        // A rewritten history must not resume: the second spawn gets a fresh
        // prompt, but the first (extended) one resumes.
        let second = ScriptedChild::new(
            vec![Step::Chunk(
                "{\"event\":\"result\",\"result\":{\"response\":\"two\"}}\n",
            )],
            0,
            "",
        );
        let runner = ScriptedRunner::new(vec![first, second]);
        let llm = AntigravityLlm::new(base_config(), "sys".to_string())
            .with_runner(Arc::clone(&runner) as Arc<dyn AntigravityRunner>);

        llm.chat(params(vec![user("first")])).await.unwrap();
        let extended = vec![
            user("first"),
            LLMMessage::new("assistant", "one"),
            user("second"),
        ];
        llm.chat(params(extended)).await.unwrap();

        let specs = runner.specs.lock().unwrap();
        assert_eq!(specs.len(), 2);
        assert!(!specs[0].args.contains(&"--conversation".to_string()));
        let at = specs[1]
            .args
            .iter()
            .position(|arg| arg == "--conversation")
            .expect("second turn resumes");
        assert_eq!(specs[1].args[at + 1], "conv-1");
    }

    #[tokio::test]
    async fn chat_does_not_resume_when_the_history_was_rewritten() {
        let first = ScriptedChild::new(
            vec![
                Step::Chunk("{\"event\":\"init\",\"conversation_id\":\"conv-1\"}\n"),
                Step::Chunk("{\"event\":\"result\",\"result\":{\"response\":\"one\"}}\n"),
            ],
            0,
            "",
        );
        let second = ScriptedChild::new(
            vec![Step::Chunk(
                "{\"event\":\"result\",\"result\":{\"response\":\"two\"}}\n",
            )],
            0,
            "",
        );
        let runner = ScriptedRunner::new(vec![first, second]);
        let llm = AntigravityLlm::new(base_config(), "sys".to_string())
            .with_runner(Arc::clone(&runner) as Arc<dyn AntigravityRunner>);

        llm.chat(params(vec![user("first")])).await.unwrap();
        let rewritten = vec![user("something else"), user("second")];
        llm.chat(params(rewritten)).await.unwrap();

        let specs = runner.specs.lock().unwrap();
        assert!(!specs[1].args.contains(&"--conversation".to_string()));
    }

    // ── Transcript side channel ────────────────────────────────────────────

    fn transcript_file(root: &Path, conversation: &str) -> PathBuf {
        root.join(conversation)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl")
    }

    #[test]
    fn transcript_reader_polls_incrementally_and_renders_tool_calls() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("brain");
        let path = transcript_file(&root, "conv-1");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        let mut reader = TranscriptReader::new(root);
        reader.set_conversation(dir.path().join("brain").join("conv-1"));

        // A partial line yields nothing and is carried over.
        std::fs::write(
            &path,
            "{\"type\":\"PLANNER_RESPONSE\",\"thinking\":\"thinking\"}",
        )
        .unwrap();
        let thoughts = tokio_test_block_on(reader.poll(false)).unwrap();
        assert!(thoughts.is_empty());

        // The newline completes the line; only the new suffix is emitted.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(
            &mut file,
            b"\n{\"type\":\"PLANNER_RESPONSE\",\"thinking\":\"thinking more\"}\n\
              {\"type\":\"PLANNER_RESPONSE\",\"tool_calls\":[{\"name\":\"read_file\",\"args\":{\"path\":\"a.rs\",\"toolAction\":\"ignored\"}}]}\n",
        )
        .unwrap();
        drop(file);
        let thoughts = tokio_test_block_on(reader.poll(false)).unwrap();
        assert_eq!(thoughts[0], "thinking");
        assert_eq!(thoughts[1], " more");
        assert!(
            thoughts
                .iter()
                .any(|thought| thought == "\n🔧 read_file(path: a.rs)")
        );

        // A tool result line renders a short summary.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(
            &mut file,
            b"{\"type\":\"GENERIC\",\"content\":\"line one\\nline two\"}\n",
        )
        .unwrap();
        drop(file);
        let thoughts = tokio_test_block_on(reader.poll(false)).unwrap();
        assert_eq!(thoughts, vec!["\n   ↳ line one".to_string()]);
    }

    #[test]
    fn transcript_reader_flushes_an_unterminated_line_and_resets_on_shrink() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("brain");
        let conversation = root.join("conv-1");
        let path = transcript_file(&root, "conv-1");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\"type\":\"PLANNER_RESPONSE\",\"thinking\":\"hello\"}\n",
        )
        .unwrap();

        let mut reader = TranscriptReader::new(root.clone());
        reader.set_conversation(conversation.clone());
        let thoughts = tokio_test_block_on(reader.poll(false)).unwrap();
        assert_eq!(thoughts, vec!["hello".to_string()]);

        // A trailing line without a newline is only emitted on flush.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(
            &mut file,
            b"{\"type\":\"PLANNER_RESPONSE\",\"thinking\":\"hello world\"}",
        )
        .unwrap();
        drop(file);
        assert!(tokio_test_block_on(reader.poll(false)).unwrap().is_empty());
        let flushed = tokio_test_block_on(reader.poll(true)).unwrap();
        assert_eq!(flushed, vec![" world".to_string()]);

        // A shrunken file resets the offset instead of reading nothing; the
        // cumulative-thinking slice still starts at the last seen length.
        std::fs::write(
            &path,
            "{\"type\":\"PLANNER_RESPONSE\",\"thinking\":\"short again and longer\"}\n",
        )
        .unwrap();
        let reset = tokio_test_block_on(reader.poll(false)).unwrap();
        assert_eq!(reset, vec![" and longer".to_string()]);
    }

    /// Minimal block-on for the non-async transcript tests: a current-thread
    /// runtime drives the `tokio::fs` calls to completion.
    fn tokio_test_block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }
}
