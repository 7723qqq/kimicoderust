use std::collections::{HashMap, VecDeque};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::{Stream, StreamExt, TryStreamExt};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::human::llm::capability::ModelCapability;
use crate::human::llm::finish_reason::FinishReason;
use crate::human::llm::message::{ContentPart, Message, Role, StreamedMessagePart, ToolDescription};
use crate::human::llm::thinking::ThinkingEffort;
use crate::human::llm::usage::TokenUsage;
use crate::wire::record::WireAsyncError;

pub const ANTIGRAVITY_STDERR_LIMIT: usize = 64 * 1024;

#[derive(Clone, Default)]
pub struct AntigravityGenerateOptions {
    pub signal: Option<CancellationToken>,
    pub conversation_id: Option<String>,
    pub on_request_sent: Option<Arc<dyn Fn() + Send + Sync>>,
}

pub struct AntigravityStreamedMessage {
    shared: Arc<StreamShared>,
    stream: Mutex<Option<Pin<Box<dyn Stream<Item = Result<StreamedMessagePart, WireAsyncError>> + Send>>>>,
}

impl AntigravityStreamedMessage {
    pub fn id(&self) -> Option<String> {
        self.shared.id.lock().clone()
    }

    pub fn usage(&self) -> Option<TokenUsage> {
        self.shared.usage.lock().clone()
    }

    pub fn finish_reason(&self) -> Option<FinishReason> {
        self.shared.finish_reason.lock().clone()
    }

    pub fn raw_finish_reason(&self) -> Option<String> {
        self.shared.raw_finish_reason.lock().clone()
    }

    pub fn set_usage(&self, usage: TokenUsage) {
        *self.shared.usage.lock() = Some(usage);
    }

    pub fn set_id(&self, id: &str) {
        *self.shared.id.lock() = Some(id.to_string());
    }

    pub fn set_finish_reason(&self, reason: FinishReason, raw: Option<&str>) {
        *self.shared.finish_reason.lock() = Some(reason);
        *self.shared.raw_finish_reason.lock() = raw.map(str::to_string);
    }

    pub fn stream(&self) -> Pin<Box<dyn Stream<Item = Result<StreamedMessagePart, WireAsyncError>> + Send>> {
        self.stream.lock().take().unwrap_or_else(|| Box::pin(futures_util::stream::empty()))
    }
}

use std::pin::Pin;

pub struct AntigravityOptions {
    pub model: String,
    pub binary_path: Option<String>,
    pub thinking_effort: Option<ThinkingEffort>,
}

pub fn detect_antigravity_binary() -> Option<String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "undefined".to_string());
    let candidates = [
        format!("{home}/.local/bin/agy"),
        "/usr/local/bin/agy".to_string(),
        "/usr/bin/agy".to_string(),
    ];
    candidates
        .into_iter()
        .find(|candidate| !candidate.is_empty() && Path::new(candidate).exists())
}

const ANTIGRAVITY_ENV_ALLOWLIST: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "TEMP", "TMP",
    "USERPROFILE", "APPDATA", "LOCALAPPDATA", "SYSTEMROOT", "COMSPEC",
];
const ANTIGRAVITY_AUTH_ENV_ALLOWLIST: &[&str] = &[
    "GOOGLE_API_KEY", "GEMINI_API_KEY", "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_LOCATION",
];

pub fn build_antigravity_env() -> HashMap<String, String> {
    let allowed = ANTIGRAVITY_ENV_ALLOWLIST
        .iter()
        .chain(ANTIGRAVITY_AUTH_ENV_ALLOWLIST)
        .copied()
        .collect::<std::collections::HashSet<_>>();
    let mut output = HashMap::new();
    for (key, value) in std::env::vars() {
        let normalized = if cfg!(windows) {
            key.to_uppercase()
        } else {
            key
        };
        if allowed.contains(normalized.as_str()) {
            output.insert(normalized, value);
        }
    }
    output
}

pub fn map_model_to_antigravity(model: &str, effort: Option<&str>) -> String {
    let normalized = regex::Regex::new(r"[_\s.]+")
        .unwrap()
        .replace_all(&model.to_lowercase(), "-")
        .into_owned();
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
    if normalized.contains("gpt-oss") || normalized.contains("gptoss") || normalized.contains("120b") {
        return "gpt-oss-120b-medium".to_string();
    }
    model.to_string()
}

#[derive(Debug, Clone)]
pub struct AntigravityPromptPlan {
    pub prompt_text: String,
    pub use_conversation_id: Option<String>,
}

pub fn build_antigravity_prompt(
    history: &[Message],
    last_conversation_id: Option<&str>,
) -> AntigravityPromptPlan {
    let extract = |message: &Message| {
        let content = match message {
            Message::System(message) => &message.content,
            Message::User(message) => &message.content,
            Message::Assistant(message) => &message.content,
            Message::Tool(message) => &message.content,
        };
        content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut prompt_text;
    let mut use_conversation_id = None;
    if last_conversation_id.is_some_and(|id| !id.is_empty()) && history.len() > 1 {
        prompt_text = history.last().map(&extract).unwrap_or_default();
        use_conversation_id = last_conversation_id.map(str::to_string);
    } else if history.len() <= 1 {
        prompt_text = history.last().map(&extract).unwrap_or_default();
    } else {
        let mut parts = Vec::new();
        for message in history {
            let text = extract(message);
            if text.trim().is_empty() {
                continue;
            }
            match message {
                Message::User(_) => parts.push(format!("[User]:\n{text}")),
                Message::Assistant(_) => parts.push(format!("[Assistant]:\n{text}")),
                Message::Tool(_) => parts.push(format!("[Tool Result]:\n{text}")),
                Message::System(_) => {}
            }
        }
        prompt_text = parts.join("\n\n");
    }
    if prompt_text.trim().is_empty() {
        prompt_text = "hi".to_string();
    }
    AntigravityPromptPlan {
        prompt_text,
        use_conversation_id,
    }
}

pub struct AntigravityChatProvider {
    pub name: String,
    pub model_name: String,
    pub thinking_effort: Option<ThinkingEffort>,
    binary_path: String,
    last_conversation_id: Arc<Mutex<Option<String>>>,
}

impl AntigravityChatProvider {
    pub fn new(options: AntigravityOptions) -> Self {
        Self {
            name: "AntigravityChatProvider".to_string(),
            model_name: options.model,
            thinking_effort: Some(options.thinking_effort.unwrap_or_else(|| "high".to_string())),
            binary_path: options
                .binary_path
                .or_else(detect_antigravity_binary)
                .unwrap_or_else(|| "agy".to_string()),
            last_conversation_id: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn generate(
        &self,
        _system_prompt: &str,
        _tools: &[ToolDescription],
        history: Vec<Message>,
        options: AntigravityGenerateOptions,
    ) -> Result<AntigravityStreamedMessage, WireAsyncError> {
        let model = map_model_to_antigravity(&self.model_name, self.thinking_effort.as_deref());
        let previous_conversation = options
            .conversation_id
            .clone()
            .or_else(|| self.last_conversation_id.lock().clone());
        let plan = build_antigravity_prompt(&history, previous_conversation.as_deref());
        let mut args = Vec::new();
        if let Some(conversation) = &plan.use_conversation_id {
            args.push("--conversation".to_string());
            args.push(conversation.clone());
        }
        args.extend([
            "-p".to_string(),
            plan.prompt_text,
            "--model".to_string(),
            model,
            "--disable-slash-commands".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ]);
        if let Some(on_sent) = &options.on_request_sent {
            on_sent();
        }
        let child = Command::new(&self.binary_path)
            .args(&args)
            .current_dir(std::env::temp_dir())
            .env_clear()
            .envs(build_antigravity_env())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|error| {
                std::io::Error::other(format!(
                    "Failed to spawn Antigravity client ({}): {error}",
                    self.binary_path
                ))
            })?;
        let pid = child.id().unwrap_or(0);
        let child = Arc::new(tokio::sync::Mutex::new(child));
        let shared = Arc::new(StreamShared::default());
        let generation = Arc::new(GenerationState {
            shared: shared.clone(),
            aborted: AtomicBool::new(false),
            conversation_id: Mutex::new(None),
            transcript_offset: Mutex::new(0),
            transcript_pending: Mutex::new(Vec::new()),
            last_seen_thinking_length: Mutex::new(0),
            awaiting_tool_result: Mutex::new(false),
            transcript_lock: tokio::sync::Mutex::new(()),
        });
        if let Some(signal) = &options.signal {
            if signal.is_cancelled() {
                generation.aborted.store(true, Ordering::Relaxed);
                shared.aborted.store(true, Ordering::Relaxed);
                let _ = child.lock().await.kill().await;
                return Err("The operation was aborted.".into());
            }
            let signal = signal.clone();
            let generation = generation.clone();
            let shared = shared.clone();
            tokio::spawn(async move {
                signal.cancelled().await;
                generation.aborted.store(true, Ordering::Relaxed);
                shared.aborted.store(true, Ordering::Relaxed);
                kill_process_by_id(pid);
            });
        }
        spawn_stdout_reader(child.clone(), generation.clone(), self.last_conversation_id.clone());
        spawn_stderr_reader(child.clone(), shared.clone());
        spawn_transcript_poller(generation.clone());
        spawn_exit_waiter(child, generation.clone());
        let stream = Box::pin(futures_util::stream::try_unfold(
            (shared.clone(), false),
            |(shared, mut finished)| async move {
                loop {
                    if shared.aborted() {
                        return Err("The operation was aborted.".into());
                    }
                    if let Some(part) = shared.queue.lock().pop_front() {
                        if matches!(&part, StreamedMessagePart::Content(ContentPart::Text { text, .. }) if !text.trim().is_empty()) {
                            *shared.yielded_any_text.lock() = true;
                        }
                        return Ok(Some((part, (shared, finished))));
                    }
                    if let Some(error) = shared.error.lock().clone() {
                        if !*shared.yielded_any_text.lock()
                            && !shared.final_response_text.lock().is_empty()
                        {
                            *shared.yielded_any_text.lock() = true;
                            let text = shared.final_response_text.lock().clone();
                            return Ok(Some((StreamedMessagePart::Content(ContentPart::Text {
                                text,
                                meta: None,
                            }), (shared, finished))));
                        }
                        if finished {
                            return Ok(None);
                        }
                        finished = true;
                        return Err(std::io::Error::other(error).into());
                    }
                    if shared.is_done.load(Ordering::Relaxed) {
                        if !*shared.yielded_any_text.lock() {
                            *shared.yielded_any_text.lock() = true;
                            let text = {
                                let final_text = shared.final_response_text.lock().clone();
                                if final_text.is_empty() { " ".to_string() } else { final_text }
                            };
                            return Ok(Some((StreamedMessagePart::Content(ContentPart::Text {
                                text,
                                meta: None,
                            }), (shared, finished))));
                        }
                        return Ok(None);
                    }
                    shared.notify.notified().await;
                }
            }),
        );
        Ok(AntigravityStreamedMessage {
            shared,
            stream: Mutex::new(Some(stream)),
        })
    }
}

struct StreamShared {
    id: Mutex<Option<String>>,
    usage: Mutex<Option<TokenUsage>>,
    finish_reason: Mutex<Option<FinishReason>>,
    raw_finish_reason: Mutex<Option<String>>,
    queue: Mutex<VecDeque<StreamedMessagePart>>,
    notify: tokio::sync::Notify,
    is_done: AtomicBool,
    error: Mutex<Option<String>>,
    final_response_text: Mutex<String>,
    stderr_text: Mutex<String>,
    yielded_any_text: Mutex<bool>,
    aborted: AtomicBool,
}

impl Default for StreamShared {
    fn default() -> Self {
        Self {
            id: Mutex::new(None),
            usage: Mutex::new(None),
            finish_reason: Mutex::new(Some(FinishReason::Completed)),
            raw_finish_reason: Mutex::new(Some("STOP".to_string())),
            queue: Mutex::new(VecDeque::new()),
            notify: tokio::sync::Notify::new(),
            is_done: AtomicBool::new(false),
            error: Mutex::new(None),
            final_response_text: Mutex::new(String::new()),
            stderr_text: Mutex::new(String::new()),
            yielded_any_text: Mutex::new(false),
            aborted: AtomicBool::new(false),
        }
    }
}

impl StreamShared {
    fn push(&self, part: StreamedMessagePart) {
        self.queue.lock().push_back(part);
        self.notify.notify_one();
    }

    fn aborted(&self) -> bool {
        self.aborted.load(Ordering::Relaxed)
    }
}

struct GenerationState {
    shared: Arc<StreamShared>,
    aborted: AtomicBool,
    conversation_id: Mutex<Option<String>>,
    transcript_offset: Mutex<u64>,
    transcript_pending: Mutex<Vec<u8>>,
    last_seen_thinking_length: Mutex<usize>,
    awaiting_tool_result: Mutex<bool>,
    transcript_lock: tokio::sync::Mutex<()>,
}

fn spawn_stdout_reader(
    child: Arc<tokio::sync::Mutex<Child>>,
    generation: Arc<GenerationState>,
    last_conversation_id: Arc<Mutex<Option<String>>>,
) {
    tokio::spawn(async move {
        let stdout = child.lock().await.stdout.take();
        let Some(stdout) = stdout else {
            return;
        };
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if !trimmed.starts_with('{') {
                continue;
            }
            let Ok(payload) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            match payload.get("event").and_then(Value::as_str) {
                Some("init") => {
                    if let Some(id) = payload.get("conversation_id").and_then(Value::as_str) {
                        *generation.conversation_id.lock() = Some(id.to_string());
                        *last_conversation_id.lock() = Some(id.to_string());
                        *generation.shared.id.lock() = Some(id.to_string());
                        sync_transcript(&generation, false).await;
                    }
                }
                Some("step_update") => {
                    sync_transcript(&generation, false).await;
                    if let Some(update) = payload.get("step_update") {
                        if let Some(text) = update.get("text_delta").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            generation.shared.push(StreamedMessagePart::Content(ContentPart::Text {
                                text: text.to_string(),
                                meta: None,
                            }));
                        }
                        if let Some(think) = update.get("thinking_delta").and_then(Value::as_str)
                            && !think.is_empty()
                        {
                            generation.shared.push(StreamedMessagePart::Content(ContentPart::Think {
                                think: think.to_string(),
                                encrypted: None,
                                details_index: None,
                                hidden: None,
                                reasoning_key: None,
                            }));
                        }
                        if let Some(usage) = update.get("usage") {
                            *generation.shared.usage.lock() = Some(TokenUsage {
                                input_other: usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
                                output: usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
                                input_cache_read: usage.get("cache_read_tokens").and_then(Value::as_u64).unwrap_or(0),
                                input_cache_creation: 0,
                                raw: None,
                            });
                        }
                    }
                }
                Some("result") => {
                    sync_transcript(&generation, false).await;
                    if let Some(result) = payload.get("result") {
                        if let Some(response) = result.get("response").and_then(Value::as_str)
                            && !response.is_empty()
                        {
                            *generation.shared.final_response_text.lock() = response.to_string();
                        }
                        if let Some(usage) = result.get("usage") {
                            *generation.shared.usage.lock() = Some(TokenUsage {
                                input_other: usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
                                output: usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
                                input_cache_read: usage.get("cache_read_tokens").and_then(Value::as_u64).unwrap_or(0),
                                input_cache_creation: 0,
                                raw: None,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    });
}

fn spawn_stderr_reader(child: Arc<tokio::sync::Mutex<Child>>, shared: Arc<StreamShared>) {
    tokio::spawn(async move {
        let stderr = child.lock().await.stderr.take();
        let Some(mut stderr) = stderr else {
            return;
        };
        let mut output = Vec::new();
        let mut buffer = [0; 4096];
        while let Ok(read) = stderr.read(&mut buffer).await {
            if read == 0 {
                break;
            }
            output.extend(&buffer[..read]);
            if output.len() > ANTIGRAVITY_STDERR_LIMIT {
                output = output[output.len() - ANTIGRAVITY_STDERR_LIMIT..].to_vec();
            }
        }
        if !output.is_empty() {
            *shared.stderr_text.lock() = String::from_utf8_lossy(&output).into_owned();
        }
    });
}

fn spawn_transcript_poller(generation: Arc<GenerationState>) {
    tokio::spawn(async move {
        loop {
            if generation.shared.is_done.load(Ordering::Relaxed) {
                return;
            }
            sync_transcript(&generation, false).await;
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    });
}

fn spawn_exit_waiter(child: Arc<tokio::sync::Mutex<Child>>, generation: Arc<GenerationState>) {
    tokio::spawn(async move {
        let status = child.lock().await.wait().await;
        sync_transcript(&generation, true).await;
        if let Ok(status) = status {
            if !status.success() && !generation.aborted.load(Ordering::Relaxed) {
                let code = status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "null".to_string());
                *generation.shared.finish_reason.lock() = Some(FinishReason::Error);
                *generation.shared.raw_finish_reason.lock() = Some(code.clone());
                *generation.shared.error.lock() = Some(format!(
                    "Antigravity exited with code {code}: {}",
                    generation.shared.stderr_text.lock()
                ));
            }
        } else if let Err(error) = status
            && !generation.aborted.load(Ordering::Relaxed)
        {
            *generation.shared.error.lock() = Some(format!("Antigravity error: {error}"));
        }
        generation.shared.is_done.store(true, Ordering::Relaxed);
        generation.shared.notify.notify_one();
    });
}

async fn sync_transcript(generation: &Arc<GenerationState>, flush_pending: bool) {
    let _guard = generation.transcript_lock.lock().await;
    let Some(conversation_id) = generation.conversation_id.lock().clone() else {
        return;
    };
    let path = home_dir().join(format!(
        ".gemini/antigravity-cli/brain/{conversation_id}/.system_generated/logs/transcript.jsonl"
    ));
    let Ok(size) = std::fs::metadata(&path).map(|metadata| metadata.len()) else {
        return;
    };
    let mut offset = *generation.transcript_offset.lock();
    if size < offset {
        offset = 0;
        *generation.transcript_offset.lock() = 0;
        generation.transcript_pending.lock().clear();
    }
    let mut chunk = Vec::new();
    if size > offset {
        let read = async {
            let mut file = tokio::fs::File::open(&path).await?;
            file.seek(SeekFrom::Start(offset)).await?;
            let mut chunk = Vec::new();
            file.take(size - offset).read_to_end(&mut chunk).await?;
            Ok::<_, std::io::Error>(chunk)
        }
        .await;
        let Ok(read_chunk) = read else {
            return;
        };
        *generation.transcript_offset.lock() = offset + read_chunk.len() as u64;
        chunk = read_chunk;
    } else if !flush_pending {
        return;
    }
    let mut combined = generation.transcript_pending.lock().clone();
    combined.extend(chunk);
    let boundary = if flush_pending {
        combined.len()
    } else {
        match combined.iter().rposition(|byte| *byte == b'\n') {
            Some(boundary) => boundary,
            None => {
                *generation.transcript_pending.lock() = combined;
                return;
            }
        }
    };
    let complete = String::from_utf8_lossy(&combined[..boundary]).into_owned();
    *generation.transcript_pending.lock() = combined[(boundary + 1).min(combined.len())..].to_vec();
    for raw_line in complete.split('\n') {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match entry.get("type").and_then(Value::as_str) {
            Some("PLANNER_RESPONSE") => {
                if let Some(thinking) = entry.get("thinking").and_then(Value::as_str) {
                    let mut last = generation.last_seen_thinking_length.lock();
                    if thinking.len() > *last {
                        let delta = thinking[*last..].to_string();
                        *last = thinking.len();
                        drop(last);
                        generation.shared.push(StreamedMessagePart::Content(ContentPart::Think {
                            think: delta,
                            encrypted: None,
                            details_index: None,
                            hidden: None,
                            reasoning_key: None,
                        }));
                    }
                }
                if let Some(calls) = entry.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        let Some(name) = call.get("name").and_then(Value::as_str) else {
                            continue;
                        };
                        let args = format_tool_args(call.get("args").and_then(Value::as_object));
                        generation.shared.push(StreamedMessagePart::Content(ContentPart::Think {
                            think: format!("\n🔧 {name}({args})"),
                            encrypted: None,
                            details_index: None,
                            hidden: None,
                            reasoning_key: None,
                        }));
                        *generation.awaiting_tool_result.lock() = true;
                    }
                }
                if let Some(content) = entry.get("content").and_then(Value::as_str)
                    && !content.is_empty()
                    && generation.shared.final_response_text.lock().is_empty()
                {
                    *generation.shared.final_response_text.lock() = content.to_string();
                }
            }
            Some("GENERIC") => {
                if !*generation.awaiting_tool_result.lock() {
                    continue;
                }
                *generation.awaiting_tool_result.lock() = false;
                if let Some(content) = entry.get("content").and_then(Value::as_str) {
                    let first_line = content
                        .split('\n')
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or("");
                    let snippet = first_line.chars().take(160).collect::<String>();
                    if !snippet.is_empty() {
                        generation.shared.push(StreamedMessagePart::Content(ContentPart::Think {
                            think: format!("\n   ↳ {snippet}"),
                            encrypted: None,
                            details_index: None,
                            hidden: None,
                            reasoning_key: None,
                        }));
                    }
                }
            }
            _ => {}
        }
    }
}

fn format_tool_args(args: Option<&serde_json::Map<String, Value>>) -> String {
    let Some(args) = args else {
        return String::new();
    };
    args.iter()
        .filter(|(key, _)| !["toolAction", "toolSummary"].contains(&key.as_str()))
        .take(3)
        .map(|(key, value)| {
            let value = match value {
                Value::String(value) => value.chars().take(120).collect::<String>(),
                other => serde_json::to_string(other)
                    .unwrap()
                    .chars()
                    .take(120)
                    .collect(),
            };
            format!("{key}: {value}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn home_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default()
    } else {
        std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
    }
}

fn kill_process_by_id(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(unix)]
    {
        if unsafe { libc::kill(-(pid as i32), libc::SIGTERM) } != 0 {
            let _ = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .spawn();
    }
}

pub fn get_antigravity_model_capability() -> ModelCapability {
    ModelCapability {
        image_in: false,
        pdf_in: Some(false),
        video_in: false,
        audio_in: false,
        thinking: true,
        tool_use: false,
        dynamically_loaded_tools: None,
        unknown_capability: false,
    }
}
