//! The steer chain against the real transport, hop by hop.
//!
//! This test exists because the other three steer tests each skip a different
//! middle: the host slice tests inject an `LLM` implementation (no
//! `NativeHttpLlm`, no SSE, no sink), the engine UI-slice test feeds recorded
//! events (no session, pump or turn loop), and the TUI flow tests use a fake
//! session (no SDK). Here one process runs the whole chain:
//!
//! `UiState::handle_input` → `UiCommand::Steer` → `drive_commands` →
//! `enqueue_turn_with_id` → admission/steer queue → pump → turn loop →
//! `drain_steers` → `TurnEvent::Steer` → event bus → `Bridge` → `UiState`
//! (op trace) → `EntryStore` → `render::entry_lines`.
//!
//! The model is a local OpenAI-shaped SSE stub, so the requests it receives are
//! the real wire bodies and the text it streams is parsed by the real
//! `NativeHttpLlm`. Every hop is recorded two ways: the engine/host
//! `SteerJournal` (ordered stages under one prompt id) and the applied-op trace.
//!
//! Not exercised: `App::run()` against a TTY — this environment cannot drive an
//! interactive terminal. The paint hop is therefore `render::entry_lines` over
//! the real store (the same function the textual-rs screen paints), not a
//! mounted widget.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kimi_agent::config::{KimiConfig, ResolvedNativeLlm};
use kimi_agent::events::{EngineEvent, EventBus};
use kimi_agent::llm::http::NativeHttpLlm;
use kimi_tui_app::UiState;
use kimi_ui_core::{
    EntryKind, EntryOp, InputEvent, Key, KeyEvent, SteerStage, SubmissionState, render::entry_lines,
};

// ── OpenAI-shaped SSE stub ──────────────────────────────────────────────────

/// What the stub observed and how far it got.
struct Stub {
    addr: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    /// Index of each request as it arrives, before the response is written.
    arrived: tokio::sync::mpsc::UnboundedReceiver<usize>,
    /// Release for the first response: the test submits the steer while it is
    /// held, so "running turn, message not yet read" is deterministic.
    release: tokio::sync::oneshot::Sender<()>,
}

/// One SSE body. The shape is what the real transport parses: `data:` frames,
/// a `finish_reason`, and a terminal `[DONE]`.
fn sse(chunks: &[serde_json::Value]) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str("data: ");
        body.push_str(&chunk.to_string());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

/// Step one: the model calls a native tool, so the turn continues to a second
/// step — the step where the steer queue is drained.
fn tool_call_sse() -> String {
    sse(&[
        serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_glob","type":"function","function":{"name":"Glob","arguments":"{\"pattern\":\"*.rs\"}"}}]},"finish_reason":null}]}),
        serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        serde_json::json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
    ])
}

/// Step two: the answer the model gives after reading the steer.
fn answer_sse() -> String {
    sse(&[
        serde_json::json!({"choices":[{"delta":{"content":"answer "},"finish_reason":null}]}),
        serde_json::json!({"choices":[{"delta":{"content":"after "},"finish_reason":null}]}),
        serde_json::json!({"choices":[{"delta":{"content":"the steer"},"finish_reason":"stop"}]}),
        serde_json::json!({"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":3,"total_tokens":23}}),
    ])
}

async fn spawn_openai_stub() -> Stub {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the SSE stub");
    let addr = listener.local_addr().expect("stub address");
    let requests: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let (arrived_tx, arrived) = tokio::sync::mpsc::unbounded_channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut release = Some(release_rx);
        let mut index = 0usize;
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            // Read the request head + body; reqwest sends a content-length.
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                let read = sock.read(&mut buf).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..read]);
                let Some(header_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let header_end = header_end + 4;
                let headers = String::from_utf8_lossy(&raw[..header_end]).to_ascii_lowercase();
                let content_length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if raw.len() >= header_end + content_length {
                    break;
                }
            }
            let body_start = raw
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|at| at + 4)
                .unwrap_or(raw.len());
            let body: serde_json::Value =
                serde_json::from_slice(&raw[body_start..]).unwrap_or(serde_json::Value::Null);
            captured
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(body);
            let current = index;
            index += 1;
            let _ = arrived_tx.send(current);
            if current == 0
                && let Some(release) = release.take()
            {
                let _ = release.await;
            }
            let body = if current == 0 {
                tool_call_sse()
            } else {
                answer_sse()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.flush().await;
        }
    });
    Stub {
        addr,
        requests,
        arrived,
        release,
    }
}

// ── Fixtures ────────────────────────────────────────────────────────────────

struct TempWorkspace(std::path::PathBuf);

impl TempWorkspace {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kimi-tui-real-steer-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp workspace");
        Self(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn resolved(base_url: String) -> ResolvedNativeLlm {
    serde_json::from_value(serde_json::json!({
        "protocol": "openai",
        "base_url": base_url,
        "api_key": "test-key",
        "model": "stub-model",
    }))
    .expect("the provider entry decodes")
}

/// `(kind, text)` for everything on screen, in entry order.
fn entries(state: &Arc<UiState>) -> Vec<(EntryKind, String)> {
    state.with_store(|store| {
        store
            .entries()
            .map(|(_, content)| (content.kind, content.text.clone()))
            .collect()
    })
}

fn wait_for<T>(mut read: impl FnMut() -> Option<T>, what: &str) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(value) = read() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ── The test ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steer_walks_every_hop_against_the_real_transport() {
    let stub = spawn_openai_stub().await;
    let base_url = format!("http://{}/v1", stub.addr);
    let workspace = TempWorkspace::new("chain");

    let config = KimiConfig {
        yolo: Some(true),
        ..KimiConfig::default()
    };
    let resolved = resolved(base_url);
    let native_llm = config.native_llm_config(&resolved);
    let bus = Arc::new(EventBus::new());
    // The real sink the engine's transport publishes through; build_engine
    // wires exactly this pair.
    let sink_bus = Arc::clone(&bus);
    let llm = Arc::new(
        NativeHttpLlm::new(native_llm, "You are a test assistant.".to_string())
            .with_sink(Arc::new(move |event| sink_bus.publish_json(event))),
    );

    // Record every bus payload in arrival order. The bridge subscribes inside
    // build_session; this second subscriber is the observation point.
    let bus_log: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&bus_log);
    let _record = bus.subscribe(move |event| {
        let value = match event {
            EngineEvent::Custom(raw) => raw.clone(),
            other => serde_json::to_value(other).unwrap_or(serde_json::Value::Null),
        };
        log.lock().unwrap_or_else(|e| e.into_inner()).push(value);
    });

    let built = kimi_tui_host::build_session(
        config,
        workspace.path().to_path_buf(),
        resolved,
        llm,
        Arc::clone(&bus),
    )
    .await
    .expect("the session builds");

    let session = built.session.clone();
    let state = Arc::clone(&built.state);
    let steer_trace = built.steer_trace.clone();
    let mut arrived = stub.arrived;
    tokio::spawn(kimi_tui_host::drive_commands(
        session.clone(),
        built.commands,
        Arc::clone(&built.bridge),
        Arc::clone(&built.state),
        built.steer_trace.clone(),
    ));

    // Hop 1: the keystroke becomes a prompt through the real UI state.
    state.handle_input(InputEvent::Paste("first question".into()));
    state.handle_input(InputEvent::Key(KeyEvent::new(Key::Enter)));
    assert_eq!(
        arrived.recv().await,
        Some(0),
        "the first request reached the stub"
    );

    // Hop 2: with the turn live, Enter steers.
    assert!(state.turn_running(), "the turn lifecycle marks a live turn");
    state.handle_input(InputEvent::Paste("steer me".into()));
    state.handle_input(InputEvent::Key(KeyEvent::new(Key::Enter)));

    // The row appears, dimmed, and its id comes out of the op trace.
    let steer_entry = wait_for(
        || {
            state.with_store(|store| {
                store
                    .entries()
                    .find(|(_, content)| {
                        content.kind == EntryKind::UserMsg && content.text == "steer me"
                    })
                    .map(|(id, _)| id.clone())
            })
        },
        "the steer row",
    );
    assert!(
        state.with_store(|store| store.is_pending(&steer_entry)),
        "the row is pending until the engine reads it"
    );
    let prompt_id = state
        .last_steer_prompt_id()
        .expect("the receipt carries the host-minted prompt id");
    assert!(
        steer_trace
            .stages_of(&prompt_id)
            .contains(&SteerStage::RowAllocated {
                entry: steer_entry.clone()
            }),
        "the journal names the row that was allocated"
    );

    // The paint policy on the real store: the steer row is dimmed while pending.
    let pending_line = state.with_store(|store| {
        entry_lines(store, 80)
            .into_iter()
            .find(|(line, _)| line.contains("steer me"))
            .expect("the steer row is painted")
    });
    assert_eq!(
        pending_line.1,
        kimi_ui_core::Style::DEFAULT.dim(),
        "a pending steer row is dimmed"
    );

    // Release step one; the tool runs, step two drains the steer.
    stub.release.send(()).expect("release the first response");

    // Hop 3: the second request is the model reading the steer.
    assert_eq!(
        arrived.recv().await,
        Some(1),
        "the second request reached the stub"
    );
    // The engine acknowledged the steer before asking for the answer.
    wait_for(
        || {
            let stages = steer_trace.stages_of(&prompt_id);
            stages.contains(&SteerStage::Acknowledged).then_some(())
        },
        "turn.steer acknowledgement",
    );
    assert!(
        !state.with_store(|store| store.is_pending(&steer_entry)),
        "the acknowledged row is no longer pending"
    );

    // Let the turn finish, then read the final state.
    wait_for(|| (!state.turn_running()).then_some(()), "the turn to end");
    session.shutdown();

    // ── Evidence: requests ────────────────────────────────────────────────
    let requests = stub
        .requests
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(
        requests.len(),
        2,
        "one request per step, no retry: {requests:#?}"
    );
    let first = &requests[0];
    assert!(
        first["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == "Glob")),
        "the model was offered the native tool table: {first:#?}"
    );
    let second = &requests[1];
    let messages = second["messages"]
        .as_array()
        .expect("the second request carries messages");
    let role = |message: &serde_json::Value| message["role"].as_str().unwrap_or("").to_string();
    let assistant_tool_at = messages
        .iter()
        .position(|message| {
            role(message) == "assistant"
                && message["tool_calls"]
                    .as_array()
                    .is_some_and(|c| !c.is_empty())
        })
        .expect("the assistant tool call is in the history");
    let tool_at = messages
        .iter()
        .position(|message| role(message) == "tool")
        .expect("the tool result is in the history");
    let steer_at = messages
        .iter()
        .position(|message| role(message) == "user" && message["content"] == "steer me")
        .expect("the steer is in the second request");
    assert!(
        assistant_tool_at < tool_at && tool_at < steer_at,
        "the steer sits after the tool call and its result: {:?}",
        messages.iter().map(role).collect::<Vec<_>>()
    );
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["content"] == "steer me")
            .count(),
        1,
        "the steer appears once"
    );

    // ── Evidence: the stage journal for this prompt ───────────────────────
    let stages = steer_trace.stages_of(&prompt_id);
    let expected = vec![
        SteerStage::CommandAccepted,
        SteerStage::QueuePushed,
        SteerStage::Admitted {
            turn_id: 0,
            steered: true,
        },
        SteerStage::Receipt {
            turn_id: 0,
            state: SubmissionState::Running,
            steered: true,
        },
        SteerStage::RowAllocated {
            entry: steer_entry.clone(),
        },
        SteerStage::Drained,
        SteerStage::Acknowledged,
    ];
    assert_eq!(
        stages, expected,
        "the steer walked exactly these hops, in this order; \
         a detour (park, re-admission, another agent) would show up here"
    );
    assert_eq!(
        steer_trace.current(&prompt_id),
        Some(SteerStage::Acknowledged),
        "where the prompt ended up"
    );

    // ── Evidence: the bus order ───────────────────────────────────────────
    let log = bus_log.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let index_of = |predicate: &dyn Fn(&serde_json::Value) -> bool, what: &str| {
        log.iter()
            .position(predicate)
            .unwrap_or_else(|| panic!("no {what} on the bus: {log:#?}"))
    };
    let prompt_at = index_of(&|event| event["type"] == "turn.prompt", "turn.prompt");
    let started_at = index_of(&|event| event["type"] == "turn.started", "turn.started");
    let steer_at = index_of(&|event| event["type"] == "turn.steer", "turn.steer");
    let answer_at = index_of(
        &|event| {
            event["type"] == "llm.delta"
                && event["part"]["type"] == "text"
                && event["part"]["text"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("answer"))
        },
        "the answer delta",
    );
    let ended_at = index_of(&|event| event["type"] == "turn.ended", "turn.ended");
    assert!(
        prompt_at < started_at && started_at < steer_at,
        "the turn opens before the steer is reported"
    );
    assert!(
        steer_at < answer_at,
        "the steer is consumed before the answer streams"
    );
    assert!(
        answer_at < ended_at,
        "the answer streams before the turn ends"
    );
    assert_eq!(
        log.iter()
            .filter(|event| event["type"] == "turn.steer")
            .count(),
        1,
        "exactly one read report"
    );
    let steer_event = &log[steer_at];
    assert_eq!(
        steer_event["promptIds"],
        serde_json::json!([prompt_id.clone()]),
        "the event names the receipt's prompt id"
    );

    // ── Evidence: the rows ────────────────────────────────────────────────
    let rows = entries(&state);
    assert_eq!(
        rows.iter()
            .filter(|(kind, text)| *kind == EntryKind::UserMsg && text == "steer me")
            .count(),
        1,
        "one steer row, no duplicate: {rows:?}"
    );
    assert!(
        rows.iter().any(
            |(kind, text)| *kind == EntryKind::AssistantMsg && text == "answer after the steer"
        ),
        "the SSE text was projected into an assistant entry: {rows:?}"
    );
    // The cleared row is the created one.
    assert_eq!(
        steer_entry.turn_id.as_deref(),
        Some("turn-0"),
        "the row belongs to the first turn"
    );
    let cleared_line = state.with_store(|store| {
        entry_lines(store, 80)
            .into_iter()
            .find(|(line, _)| line.contains("steer me"))
            .expect("the steer row is still painted")
    });
    assert_eq!(
        cleared_line.1,
        kimi_ui_core::Style::DEFAULT,
        "after acknowledgement the row is painted at full intensity"
    );
    assert!(
        state.with_store(|store| store.violations().is_empty()),
        "the projection contract held: {:?}",
        state.with_store(|store| store.violations().to_vec())
    );
}

/// The same chain, started from `build_engine` — the function the terminal's
/// `main` calls — instead of assembling the transport by hand. `build_engine`
/// resolves the provider and builds the `NativeHttpLlm` + sink itself, so this
/// test covers the one seam the `build_session` test supplies directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn build_engine_wires_the_same_steer_chain() {
    let stub = spawn_openai_stub().await;
    let base_url = format!("http://{}/v1", stub.addr);
    let workspace = TempWorkspace::new("build-engine");

    let config: KimiConfig = serde_json::from_value(serde_json::json!({
        "yolo": true,
        "default_model": "local/a",
        "providers": {
            "local": { "type": "openai", "base_url": base_url, "api_key": "test-key" }
        },
        "models": {
            "local/a": { "provider": "local", "model": "stub-model", "max_context_size": 200000 }
        },
    }))
    .expect("the config decodes");

    let built = kimi_tui_host::build_engine(config, workspace.path().to_path_buf(), None, None)
        .await
        .expect("build_engine resolves the provider and builds the transport");
    let session = built.session.clone();
    let state = Arc::clone(&built.state);
    let steer_trace = built.steer_trace.clone();
    let mut arrived = stub.arrived;
    tokio::spawn(kimi_tui_host::drive_commands(
        session.clone(),
        built.commands,
        Arc::clone(&built.bridge),
        Arc::clone(&built.state),
        built.steer_trace.clone(),
    ));

    state.handle_input(InputEvent::Paste("first question".into()));
    state.handle_input(InputEvent::Key(KeyEvent::new(Key::Enter)));
    assert_eq!(
        arrived.recv().await,
        Some(0),
        "the first request reached the stub"
    );

    state.handle_input(InputEvent::Paste("steer me".into()));
    state.handle_input(InputEvent::Key(KeyEvent::new(Key::Enter)));
    let steer_entry = wait_for(
        || {
            state.with_store(|store| {
                store
                    .entries()
                    .find(|(_, content)| {
                        content.kind == EntryKind::UserMsg && content.text == "steer me"
                    })
                    .map(|(id, _)| id.clone())
            })
        },
        "the steer row",
    );
    let prompt_id = state
        .last_steer_prompt_id()
        .expect("the receipt carries the host-minted prompt id");
    assert!(state.with_store(|store| store.is_pending(&steer_entry)));

    stub.release.send(()).expect("release the first response");
    assert_eq!(
        arrived.recv().await,
        Some(1),
        "the second request reached the stub"
    );
    wait_for(
        || {
            steer_trace
                .stages_of(&prompt_id)
                .contains(&SteerStage::Acknowledged)
                .then_some(())
        },
        "turn.steer acknowledgement",
    );
    wait_for(|| (!state.turn_running()).then_some(()), "the turn to end");
    session.shutdown();

    // The same stage sequence as the hand-built session: the configuration path
    // adds no hop and skips none.
    assert_eq!(
        steer_trace.stages_of(&prompt_id),
        vec![
            SteerStage::CommandAccepted,
            SteerStage::QueuePushed,
            SteerStage::Admitted {
                turn_id: 0,
                steered: true
            },
            SteerStage::Receipt {
                turn_id: 0,
                state: SubmissionState::Running,
                steered: true,
            },
            SteerStage::RowAllocated {
                entry: steer_entry.clone()
            },
            SteerStage::Drained,
            SteerStage::Acknowledged,
        ],
        "build_engine walks exactly the same hops"
    );

    // The applied-op trace: one row created, dimmed, then cleared — the cleared
    // op names the row the allocation op named.
    let ops = state.ops_snapshot();
    let upserts = ops
        .iter()
        .filter(|op| matches!(op, EntryOp::Upsert { id, .. } if *id == steer_entry))
        .count();
    assert_eq!(upserts, 1, "the steer row is created once: {ops:?}");
    let pending_flags: Vec<bool> = ops
        .iter()
        .filter_map(|op| match op {
            EntryOp::SetPending { id, pending } if *id == steer_entry => Some(*pending),
            _ => None,
        })
        .collect();
    assert_eq!(
        pending_flags,
        vec![true, false],
        "the row dims once and clears once, on the same id"
    );

    let requests = stub
        .requests
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(requests.len(), 2, "one request per step");
    let messages = requests[1]["messages"]
        .as_array()
        .expect("the second request carries messages");
    let position = |needle: &str| {
        messages
            .iter()
            .position(|message| message.to_string().contains(needle))
    };
    let tool_at = position("call_glob").expect("the tool call survives into step two");
    let steer_at = messages
        .iter()
        .position(|message| message["role"] == "user" && message["content"] == "steer me")
        .expect("the steer is in the second request");
    assert!(tool_at < steer_at, "the steer follows step one's tool call");
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["content"] == "steer me")
            .count(),
        1,
        "the steer appears once"
    );

    let rows = entries(&state);
    assert_eq!(
        rows.iter()
            .filter(|(kind, text)| *kind == EntryKind::UserMsg && text == "steer me")
            .count(),
        1,
        "one steer row, no duplicate: {rows:?}"
    );
    assert!(
        rows.iter().any(
            |(kind, text)| *kind == EntryKind::AssistantMsg && text == "answer after the steer"
        ),
        "the SSE text was projected into an assistant entry: {rows:?}"
    );
    assert!(
        !state.with_store(|store| store.is_pending(&steer_entry)),
        "the acknowledged row is no longer pending"
    );
    assert!(
        state.with_store(|store| store.violations().is_empty()),
        "the projection contract held: {:?}",
        state.with_store(|store| store.violations().to_vec())
    );
}
