//! The engine → UI direction: engine events become projected entries.
//!
//! The projector lives in the engine crate (`kimi_agent::events::Projector`),
//! so the fold that turns `EngineEvent`s into `EntryOp`s runs here, on the
//! composition root. That is the layering the design settled on: the UI crate
//! consumes `EntryOp` and never sees a raw engine event.

use std::sync::Arc;
use std::sync::Mutex;

use kimi_agent::callbacks::MAIN_AGENT_ID;
use kimi_agent::events::{EngineEvent, EventBus, Projector, Subscription};
use kimi_agent::i18n::LocalizedText;
use kimi_agent::session::{Admission, EngineSession, PromptSubmissionState, TurnRequest};
use kimi_agent::turn_loop::types::LLMMessage;
use kimi_tui_app::{UiCommand, UiState, UiTransport};
use kimi_ui_core::{CoalescingPolicy, EntryId, EntryOp, SubmissionState};

/// The UI's end of the command channel.
///
/// `UiState` owns it, so a widget's `send` is a channel send and nothing on the
/// render path ever waits for the engine.
pub struct EngineTransport {
    commands: tokio::sync::mpsc::UnboundedSender<UiCommand>,
}

impl EngineTransport {
    /// A transport plus the receiving end [`drive_commands`] drains.
    pub fn channel() -> (Self, tokio::sync::mpsc::UnboundedReceiver<UiCommand>) {
        let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
        (Self { commands }, receiver)
    }
}

impl UiTransport for EngineTransport {
    fn send(&mut self, command: UiCommand) {
        // A closed channel means the driver is gone, which only happens while
        // the process is shutting down; the widget has nothing to do about it.
        let _ = self.commands.send(command);
    }
}

/// The fold from bus events to entries, plus the two translations the bus needs.
///
/// The bus carries two dialects, and neither decodes cleanly into the
/// projector's [`EngineEvent`].
///
/// The turn lifecycle is published as a `TurnEvent` (`turn.prompt` /
/// `turn.started` / `turn.steer` / `turn.ended`). Its wire shape (`turnId`,
/// `input`) is not an `EngineEvent`, so it reaches a subscriber as
/// [`EngineEvent::Custom`]. The projector's vocabulary is the UI contract, so
/// the durable lifecycle events are translated here: `turn.prompt` opens the
/// turn — it is the event that carries the prompt — and `turn.ended` closes
/// it. `turn.started` is dropped on purpose. The projector allocates its entry
/// ids when a turn opens, so a second opener for the same turn would restart
/// that allocation under entries that already exist. `turn.steer` is the
/// engine's read acknowledgement for a steered message: it clears the row's
/// pending state and produces no entry of its own.
///
/// Content is published by the transport's sink as `llm.delta` with a `part`
/// and nothing else. That is one field short of [`EngineEvent::LlmDelta`],
/// which also names the turn and the step, so it too arrives as `Custom`. The
/// open turn is the only one a fragment can belong to.
pub struct Bridge {
    projector: Projector,
    state: Arc<UiState>,
    open_turn: Option<String>,
    steer_trace: kimi_ui_core::SteerJournal,
}

impl Bridge {
    pub fn new(state: Arc<UiState>, steer_trace: kimi_ui_core::SteerJournal) -> Self {
        Self {
            projector: Projector::new(CoalescingPolicy::default()),
            state,
            open_turn: None,
            steer_trace,
        }
    }

    /// The host submitted a steer into `turn_id`: allocate its row through the
    /// projector (so it orders between what streamed before and after it) and
    /// return the entry id with the ops to apply.
    pub fn note_steer(
        &mut self,
        turn_id: &str,
        text: &str,
    ) -> Option<(Vec<EntryId>, Vec<EntryOp>)> {
        let (id, ops) = self.projector.note_user_entry(turn_id, text)?;
        Some((vec![id], ops))
    }

    fn feed(&mut self, event: &EngineEvent) {
        let translated = match event {
            EngineEvent::Custom(raw) => self.translate(raw),
            _ => None,
        };
        if let Some(translated) = translated {
            self.feed(&translated);
            return;
        }
        for op in self.projector.project(event.clone()) {
            self.state.apply(op);
        }
    }

    /// A raw bus payload in the projector's vocabulary, or `None` for one the
    /// transcript has no entry for.
    fn translate(&mut self, raw: &serde_json::Value) -> Option<EngineEvent> {
        match raw.get("type")?.as_str()? {
            "turn.prompt" => {
                let turn_id = turn_id_of(raw)?;
                let prompt_id = prompt_id_of(raw);
                // A prompt naming the turn already open is a steer, not a new
                // turn: re-opening would restart the entry allocator under
                // entries that are already on screen.
                if self.open_turn.as_deref() == Some(turn_id.as_str()) {
                    // Legacy shape: the steer arrives as a second prompt for
                    // the open turn, and this is its read acknowledgement.
                    self.state.steer_acknowledge_started(prompt_id.as_deref());
                    return None;
                }
                // A new turn clears whatever the last one left on the status
                // line; a stale "turn stopped" over a running turn reads as a
                // contradiction.
                self.state.set_status(None);
                self.state.set_turn_running(true);
                self.open_turn = Some(turn_id.clone());
                Some(EngineEvent::TurnStarted {
                    agent_id: MAIN_AGENT_ID.to_string(),
                    prompt: Some(content_text(raw.get("input")?)),
                    turn_id,
                })
            }
            "turn.started" => {
                // Observable only for the transcript, but it carries the prompt
                // id a queued steer waits for: its own turn starting is the
                // read acknowledgement (v2 #4627).
                self.state.set_turn_running(true);
                self.state
                    .steer_acknowledge_started(prompt_id_of(raw).as_deref());
                None
            }
            "turn.steer" => {
                // v2 #4627 `TurnSteer`: the engine consumed these steers into
                // the running turn's context. The row stops being pending; no
                // entry is created here — the host made it at submission.
                let prompt_ids: Vec<String> = raw
                    .get("promptIds")
                    .and_then(|ids| ids.as_array())
                    .map(|ids| {
                        ids.iter()
                            .filter_map(|id| id.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                self.state.steer_acknowledge(&prompt_ids);
                for prompt_id in &prompt_ids {
                    self.steer_trace
                        .record(prompt_id.clone(), kimi_ui_core::SteerStage::Acknowledged);
                }
                None
            }
            "turn.ended" => {
                let turn_id = turn_id_of(raw)?;
                // A turn that never opened (cancelled while queued) has no
                // entries to close.
                if self.open_turn.as_deref() != Some(turn_id.as_str()) {
                    return None;
                }
                self.open_turn = None;
                self.state.set_turn_running(false);
                let reason = raw
                    .get("reason")
                    .and_then(|reason| reason.as_str())
                    .unwrap_or("completed");
                self.state.set_status(status_for(reason, raw.get("error")));
                Some(EngineEvent::TurnEnded {
                    agent_id: MAIN_AGENT_ID.to_string(),
                    reason: reason.to_string(),
                    turn_id,
                })
            }
            // The transport's streamed fragment (`llm.delta`) carries the part
            // and nothing else: no turn, no step, so it does not decode as an
            // [`EngineEvent`] and arrives here as `Custom`. The open turn is
            // the only one a fragment can belong to, and the projector's delta
            // arm keys on the turn rather than the step.
            "llm.delta" => Some(EngineEvent::LlmDelta {
                agent_id: raw
                    .get("agent_id")
                    .and_then(|agent| agent.as_str())
                    .unwrap_or(MAIN_AGENT_ID)
                    .to_string(),
                turn_id: self.open_turn.clone()?,
                step: 0,
                part: raw.get("part")?.clone(),
            }),
            _ => None,
        }
    }
}

/// The `turn-<n>` identity the projector namespaces entries by.
fn turn_id_of(raw: &serde_json::Value) -> Option<String> {
    Some(format!("turn-{}", raw.get("turnId")?.as_u64()?))
}

/// The prompt id a turn lifecycle event carries (v2 #4627), when present.
fn prompt_id_of(raw: &serde_json::Value) -> Option<String> {
    raw.get("promptId")
        .and_then(|id| id.as_str())
        .map(str::to_string)
}

/// A prompt id the host mints before admission, so the journal can file every
/// hop under one key (v2 #4627: callers may submit under an id they know).
fn host_prompt_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "prompt-host-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// The receipt's state in the UI contract's vocabulary.
fn submission_state(state: PromptSubmissionState) -> SubmissionState {
    match state {
        PromptSubmissionState::Running => SubmissionState::Running,
        PromptSubmissionState::Queued => SubmissionState::Queued,
        PromptSubmissionState::Blocked => SubmissionState::Blocked,
    }
}

/// The status line a finished turn leaves behind, if any.
///
/// The vocabulary and the wording are the reference TUI's
/// (`D:/kimi/kimi-code`'s `session-event-handler.ts`): a provider-filtered
/// response and a prompt-hook block get a sentence, a clean finish gets nothing,
/// and a cancel reads as the user's own interruption rather than a failure.
fn status_for(reason: &str, error: Option<&serde_json::Value>) -> Option<String> {
    match reason {
        "failed"
            if error
                .and_then(|error| error.get("code"))
                .and_then(|code| code.as_str())
                == Some("provider.filtered") =>
        {
            Some(LocalizedText::new("engine.tui.turnStoppedFiltered").render())
        }
        "blocked" => Some(LocalizedText::new("engine.tui.turnStoppedBlocked").render()),
        "cancelled" => Some(LocalizedText::new("engine.tui.interruptedByUser").render()),
        _ => None,
    }
}

/// The text of a `ContentPart[]` prompt.
/// Text parts are joined; a part with no text (an image) contributes nothing
/// rather than a placeholder, because the transcript shows what the user typed
/// and the entry vocabulary has no media row yet.
fn content_text(input: &serde_json::Value) -> String {
    input
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(|text| text.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Subscribe the UI state to the engine's events.
///
/// The returned subscription keeps the handler alive; dropping it stops the
/// feed.
pub fn project_events(
    bus: &EventBus,
    state: Arc<UiState>,
    steer_trace: kimi_ui_core::SteerJournal,
) -> (Subscription, Arc<Mutex<Bridge>>) {
    let bridge = Arc::new(Mutex::new(Bridge::new(state, steer_trace)));
    let shared = Arc::clone(&bridge);
    let subscription = bus.subscribe(move |event| {
        // A poisoned bridge is not a reason to stop painting: the UI keeps the
        // entries it has, and the next event re-folds from whatever state the
        // projector is in.
        shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .feed(event);
    });
    (subscription, bridge)
}

/// Turn the UI's commands into session operations.
///
/// One task owns the session's prompt queue: a prompt is enqueued and the turn
/// runs on the engine's pump, so a turn that takes a minute does not block a
/// cancellation arriving behind it.
pub async fn drive_commands(
    session: EngineSession,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<UiCommand>,
    bridge: Arc<Mutex<Bridge>>,
    state: Arc<UiState>,
    steer_trace: kimi_ui_core::SteerJournal,
) {
    while let Some(command) = commands.recv().await {
        match command {
            UiCommand::Prompt(text) => {
                let message = LLMMessage {
                    role: "user".into(),
                    content: text,
                    ..Default::default()
                };
                // The receipt is dropped: the pump owns the turn, and the
                // outcome reaches the UI as `turn.ended` on the event bus. The
                // one thing worth logging here is an engine that refuses the
                // prompt at all, which is what `Err` means.
                if let Err(reason) =
                    session.enqueue_turn(TurnRequest::user(message, Admission::NewTurn))
                {
                    eprintln!("kimi-tui: cannot start a turn: {reason}");
                }
            }
            UiCommand::Steer(text) => {
                let message = LLMMessage {
                    role: "user".into(),
                    content: text.clone(),
                    ..Default::default()
                };
                // v2 #4627: the host mints the id, so every hop from this
                // keystroke to the row is filed under one key.
                let prompt_id = host_prompt_id();
                steer_trace.record(prompt_id.clone(), kimi_ui_core::SteerStage::CommandAccepted);
                match session.enqueue_turn_with_id(
                    TurnRequest::user(message, Admission::ActiveOrNewTurn),
                    Some(prompt_id.clone()),
                ) {
                    Ok(receipt) => {
                        steer_trace.record(
                            prompt_id,
                            kimi_ui_core::SteerStage::Receipt {
                                turn_id: receipt.turn_id,
                                state: submission_state(receipt.state),
                                steered: receipt.steered,
                            },
                        );
                        // Only a steer that actually joined the running turn
                        // needs an optimistic row; one that launched its own
                        // turn is rendered by `turn.prompt`, and one still
                        // queued has no open turn to allocate into yet.
                        if !receipt.steered {
                            continue;
                        }
                        let turn_id = format!("turn-{}", receipt.turn_id);
                        let noted = bridge
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .note_steer(&turn_id, &text);
                        if let Some((entries, ops)) = noted {
                            for entry in &entries {
                                steer_trace.record(
                                    receipt.prompt_id.clone(),
                                    kimi_ui_core::SteerStage::RowAllocated {
                                        entry: entry.clone(),
                                    },
                                );
                            }
                            state.apply_all(ops);
                            state.steer_submitted(entries.clone());
                            state.steer_receipt(
                                &entries,
                                &receipt.prompt_id,
                                submission_state(receipt.state),
                                receipt.steered,
                            );
                        }
                    }
                    Err(reason) => {
                        steer_trace.record(prompt_id, kimi_ui_core::SteerStage::Rejected);
                        eprintln!("kimi-tui: cannot steer: {reason}");
                    }
                }
            }
            UiCommand::Cancel => {
                // `None` cancels the turn in flight; a queued turn is
                // cancelled by the same call.
                session.cancel_turn(None);
            }
        }
    }
}
