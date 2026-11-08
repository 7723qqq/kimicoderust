//! The slice against a real session, and the one translation the bus needs.
//!
//! The first test runs everything below the terminal for real — the session and
//! its pump, the event bus, the projector, the transport — with only the model
//! stubbed. It pins what a typed prompt actually produces: a turn opens on
//! `turn.prompt`, closes on `turn.ended`, and the user's line is in the store
//! the widgets read.
//!
//! The second test pins the other direction of the same fold: the transport's
//! `llm.delta` reaches the assistant entry. It is separate because a stub
//! answers without streaming, and streaming is the transport's own behaviour —
//! what belongs to this crate is that the event lands in the transcript.
//!
//! Neither test can supply a TTY; `kimi-tui-app`'s `render_slice` paints the
//! same state into a textual-rs test terminal.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kimi_agent::config::{KimiConfig, ResolvedNativeLlm};
use kimi_agent::events::EventBus;
use kimi_agent::rpc::types::TokenUsage;
use kimi_agent::turn_loop::types::{LLM, LLMChatParams, LLMChatResponse};
use kimi_tui_app::{RecordingTransport, UiState};
use kimi_ui_core::{EntryKind, InputEvent, Key, KeyEvent};

/// A model that answers once and says nothing else.
struct StubLlm;

impl LLM for StubLlm {
    fn system_prompt(&self) -> &str {
        "stub"
    }

    fn model_name(&self) -> &str {
        "stub-model"
    }

    fn is_retryable_error(&self, _error: &str) -> bool {
        false
    }

    fn chat(
        &self,
        _params: LLMChatParams,
    ) -> kimi_agent::rpc::types::BoxFuture<
        '_,
        Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>,
    > {
        Box::pin(async {
            Ok(LLMChatResponse {
                content: "the build is two steps".into(),
                finish_reason: Some("stop".into()),
                usage: TokenUsage {
                    input_tokens: 3,
                    output_tokens: 5,
                    total_tokens: 8,
                    ..Default::default()
                },
                ..Default::default()
            })
        })
    }
}

/// A workspace that removes itself, so a test run leaves no `target`-adjacent
/// state behind.
struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kimi-tui-host-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp workspace");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The provider entry a resolved model needs; only `capabilities` is read once
/// the transport is injected.
fn resolved() -> ResolvedNativeLlm {
    serde_json::from_value(serde_json::json!({
        "protocol": "openai",
        "base_url": "http://example.test",
        "api_key": "test-key",
        "model": "stub-model",
    }))
    .expect("the provider entry decodes")
}

/// `(kind, text, sealed)` for everything on screen, in entry order.
fn entries(state: &Arc<UiState>) -> Vec<(EntryKind, String, bool)> {
    state.with_store(|store| {
        store
            .entries()
            .map(|(id, content)| (content.kind, content.text.clone(), store.is_sealed(id)))
            .collect()
    })
}

/// Wait for `done` to hold, polling the store the way a frame would read it.
fn wait_until(
    state: &Arc<UiState>,
    done: impl Fn(&[(EntryKind, String, bool)]) -> bool,
) -> Vec<(EntryKind, String, bool)> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let current = entries(state);
        if done(&current) {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the transcript; saw {current:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_typed_prompt_opens_and_closes_a_turn() {
    let workspace = TempWorkspace::new("prompt");
    let built = kimi_tui_host::build_session(
        KimiConfig::default(),
        workspace.path().to_path_buf(),
        resolved(),
        Arc::new(StubLlm),
        Arc::new(EventBus::new()),
    )
    .await
    .expect("the session builds from a default config");

    let session = built.session.clone();
    let state = Arc::clone(&built.state);
    tokio::spawn(kimi_tui_host::drive_commands(
        session.clone(),
        built.commands,
        Arc::clone(&built.bridge),
        Arc::clone(&built.state),
        built.steer_trace.clone(),
    ));

    // Typing and Enter is the whole input path a user takes: the paste lands in
    // the prompt, Enter turns it into `UiCommand::Prompt` on the real transport.
    state.handle_input(InputEvent::Paste("explain the build".into()));
    state.handle_input(InputEvent::Key(KeyEvent::new(Key::Enter)));
    assert_eq!(state.prompt(), "", "submitting clears the prompt line");

    let seen = wait_until(&state, |entries| {
        entries
            .iter()
            .any(|(kind, _, sealed)| *kind == EntryKind::UserMsg && *sealed)
    });

    session.shutdown();

    let prompt: Vec<_> = seen
        .iter()
        .filter(|(kind, _, _)| *kind == EntryKind::UserMsg)
        .collect();
    assert_eq!(prompt.len(), 1, "one prompt, one entry: {seen:?}");
    assert_eq!(prompt[0].1, "explain the build");
    assert!(
        state.with_store(|store| store.violations().is_empty()),
        "the projection contract held"
    );
}

#[tokio::test]
async fn a_stopped_turn_leaves_a_status_message() {
    let bus = EventBus::new();
    let state = UiState::new(RecordingTransport::new());
    let _feed = kimi_tui_host::transport::project_events(
        &bus,
        Arc::clone(&state),
        kimi_ui_core::SteerJournal::new(),
    );

    bus.publish_json(serde_json::json!({
        "type": "turn.prompt",
        "turnId": 1,
        "input": [{ "type": "text", "text": "go on" }],
        "origin": { "kind": "user" },
    }));
    // A hook-blocked turn: the reference TUI puts this on the status line and
    // leaves the transcript alone.
    bus.publish_json(serde_json::json!({
        "type": "turn.ended",
        "turnId": 1,
        "reason": "blocked",
    }));

    assert_eq!(
        state.status().as_deref(),
        Some("Turn stopped: prompt hook blocked the request."),
        "a blocked turn says so on the status line"
    );

    // The next turn clears it.
    bus.publish_json(serde_json::json!({
        "type": "turn.prompt",
        "turnId": 2,
        "input": [{ "type": "text", "text": "try again" }],
        "origin": { "kind": "user" },
    }));
    assert_eq!(state.status(), None, "a new turn clears the stale message");
}

#[tokio::test]
async fn a_streamed_delta_reaches_the_assistant_entry() {
    let bus = EventBus::new();
    let state = UiState::new(RecordingTransport::new());
    let _feed = kimi_tui_host::transport::project_events(
        &bus,
        Arc::clone(&state),
        kimi_ui_core::SteerJournal::new(),
    );

    // The order the engine emits in: the durable prompt event first (it is the
    // one that carries the text), then the transport's streamed fragments, then
    // the durable end.
    bus.publish_json(serde_json::json!({
        "type": "turn.prompt",
        "turnId": 1,
        "input": [{ "type": "text", "text": "explain the build" }],
        "origin": { "kind": "user" },
    }));
    for text in ["the build is ", "two steps"] {
        bus.publish_json(serde_json::json!({
            "type": "llm.delta",
            "part": { "type": "text", "text": text },
        }));
    }
    bus.publish_json(serde_json::json!({
        "type": "turn.ended",
        "turnId": 1,
        "reason": "completed",
    }));

    let seen = entries(&state);
    assert_eq!(
        seen.iter()
            .map(|(kind, text, _)| (*kind, text.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (EntryKind::UserMsg, "explain the build"),
            (EntryKind::AssistantMsg, "the build is two steps"),
        ],
        "the prompt and the streamed answer are both entries"
    );
    assert!(
        state.with_store(|store| store.violations().is_empty()),
        "delta spans continue and the turn closes them: {:?}",
        state.with_store(|store| store.violations().to_vec())
    );
}

/// A model that parks its first call until the test releases it, then asks for
/// one native tool so the turn advances to a second step — the step where a
/// steered message is drained.
struct GatedToolLlm {
    started: tokio::sync::mpsc::UnboundedSender<()>,
    release: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl LLM for GatedToolLlm {
    fn system_prompt(&self) -> &str {
        "stub"
    }

    fn model_name(&self) -> &str {
        "stub-model"
    }

    fn is_retryable_error(&self, _error: &str) -> bool {
        false
    }

    fn chat(
        &self,
        _params: LLMChatParams,
    ) -> kimi_agent::rpc::types::BoxFuture<
        '_,
        Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>,
    > {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let started = self.started.clone();
        let release = if call == 0 {
            self.release
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
        } else {
            None
        };
        Box::pin(async move {
            if let Some(release) = release {
                let _ = started.send(());
                let _ = release.await;
            }
            if call == 0 {
                Ok(LLMChatResponse {
                    content: String::new(),
                    tool_calls: vec![kimi_agent::turn_loop::types::ToolCall {
                        id: "c1".into(),
                        name: "Glob".into(),
                        arguments: serde_json::json!({ "pattern": "*.rs" }),
                        extras: None,
                        args_parse_error: None,
                    }],
                    finish_reason: Some("tool_calls".into()),
                    ..Default::default()
                })
            } else {
                Ok(LLMChatResponse {
                    content: "answer after the steer".into(),
                    finish_reason: Some("stop".into()),
                    ..Default::default()
                })
            }
        })
    }
}

/// The id of the first user entry whose text is `text`.
fn user_entry(state: &Arc<UiState>, text: &str) -> Option<kimi_ui_core::EntryId> {
    state.with_store(|store| {
        store
            .entries()
            .find(|(_, content)| content.kind == EntryKind::UserMsg && content.text == text)
            .map(|(id, _)| id.clone())
    })
}

/// v2 #4627 end to end: Enter steers while a turn is live, the row is dimmed
/// until the engine drains the message into the turn, and the post-steer answer
/// is a separate entry painted after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steer_is_dimmed_until_the_engine_reads_it() {
    let workspace = TempWorkspace::new("steer");
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let llm = Arc::new(GatedToolLlm {
        started: started_tx,
        release: std::sync::Mutex::new(Some(release_rx)),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    // Yolo: the tool call must run without an approval dialog the test cannot
    // answer — the steer is what this test is about.
    let config = KimiConfig {
        yolo: Some(true),
        ..KimiConfig::default()
    };
    let built = kimi_tui_host::build_session(
        config,
        workspace.path().to_path_buf(),
        resolved(),
        llm,
        Arc::new(EventBus::new()),
    )
    .await
    .expect("the session builds from a default config");

    let session = built.session.clone();
    let state = Arc::clone(&built.state);
    tokio::spawn(kimi_tui_host::drive_commands(
        session.clone(),
        built.commands,
        Arc::clone(&built.bridge),
        Arc::clone(&built.state),
        built.steer_trace.clone(),
    ));

    state.handle_input(InputEvent::Paste("first question".into()));
    state.handle_input(InputEvent::Key(KeyEvent::new(Key::Enter)));
    started_rx
        .recv()
        .await
        .expect("the first request reached the model");
    assert!(state.turn_running(), "the turn lifecycle marks a live turn");

    // A turn is in flight: the same key steers instead of opening a turn.
    state.handle_input(InputEvent::Paste("steer me".into()));
    state.handle_input(InputEvent::Key(KeyEvent::new(Key::Enter)));

    let deadline = Instant::now() + Duration::from_secs(20);
    let steer = loop {
        if let Some(id) = user_entry(&state, "steer me") {
            break id;
        }
        assert!(Instant::now() < deadline, "the steer row never appeared");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        state.with_store(|store| store.is_pending(&steer)),
        "a submitted steer stays dimmed until the model reads it"
    );

    let _ = release_tx.send(());
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if !state.with_store(|store| store.is_pending(&steer)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the engine never acknowledged the steer"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if !state.turn_running() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the turn never finished after the steer; entries={:?}",
            entries(&state)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    session.shutdown();

    // The stub model is not a streamer, so the answer is read where it is
    // authoritative: the conversation the engine handed the model. It proves
    // the steer joined the turn and the next request followed it.
    let history = session.snapshot_history();
    let contents: Vec<&str> = history
        .iter()
        .map(|message| message.content.as_str())
        .collect();
    assert!(
        contents.contains(&"steer me"),
        "the engine consumed the steer into the turn: {contents:?}"
    );
    assert!(
        contents.contains(&"answer after the steer"),
        "the model answered after the steer: {contents:?}"
    );
    assert!(
        state.with_store(|store| store.violations().is_empty()),
        "the projection contract held: {:?}",
        state.with_store(|store| store.violations().to_vec())
    );
}
