//! Per-session prompt queue: one active prompt per session, submitted prompts
//! that arrive while a turn runs wait behind it, and `steer` moves queued
//! prompts into the running turn. Mirrors the kap-server prompt-queue state
//! machine (`running` / `queued` / `blocked`) that the REST contract exposes.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::events::EngineEvent;
use crate::rpc::types::ContentBlock;
use crate::server::engine::{ServerEngine, TurnReport};
use crate::server::hub::EventHub;
use crate::session::sqlite_store::SqliteSessionStore;
use crate::session::wake::{WakePayload, WakeReplayHandler, WakeReplayStatus};

/// The model input handed back to the turn loop when a prompt is admitted or
/// promoted.
pub struct RunInput {
    pub prompt_id: String,
    pub prompt: String,
    pub blocks: Vec<ContentBlock>,
    /// The request's `metadata` as `[metadata]` (v2 `clientMetadata`), `None`
    /// when the client sent none. Persisted as the turn's origin payload.
    pub origin: Option<Value>,
    /// Whether this run settles a client-visible prompt. A task-notification
    /// wake (v2 `loop.notify`) has no prompt behind it, so the driver must
    /// not publish `prompt.completed` for it.
    pub prompt_events: bool,
}

enum PromptAdvance {
    Run(RunInput),
    Replay(WakePayload, WakeReplayHandler),
    Idle,
}

#[derive(Default)]
struct State {
    active: HashMap<String, Entry>,
    queued: HashMap<String, Vec<Entry>>,
    /// Rich wakes interrupted by user input, replayed after the prompt FIFO
    /// drains. Ordinary task notifications keep their existing semantics.
    deferred_wakes: HashMap<String, Vec<WakePayload>>,
    wake_replay_handlers: HashMap<(String, String), WakeReplayHandler>,
    wake_replay_inflight: HashSet<(String, String)>,
    wake_replay_errors: HashMap<(String, String), String>,
}

/// One active prompt plus a FIFO of queued prompts, keyed by session.
#[derive(Default)]
pub struct PromptQueue {
    state: Mutex<State>,
}

/// What [`PromptQueue::cancel`] found.
pub enum CancelOutcome {
    /// The prompt was active: the caller cancels its turn through the engine.
    Active,
    /// The prompt was queued and has been removed; it will never run.
    Queued,
}

struct Entry {
    item: Value,
    prompt: String,
    blocks: Vec<ContentBlock>,
    origin: Option<Value>,
    /// See [`RunInput::prompt_events`].
    prompt_events: bool,
    /// The turn number the live `event.message.created` id names
    /// (`msg-u{turn}`), stamped by the driver when the turn starts. `None`
    /// until then: a queued prompt has no turn yet.
    turn_number: Option<u32>,
    /// The abort route settled this prompt: the turn driver must not publish
    /// a second terminal event (`prompt.completed`) on top of the
    /// `prompt.aborted` the route already published — v2's `cancelWaiter`
    /// settles the item exactly once (loopService.ts:735-750).
    cancelled: bool,
    /// Present only for caller-rendered rich wakes. The payload survives an
    /// automatic user-prompt interruption so it can be replayed verbatim.
    wake: Option<WakePayload>,
}

impl PromptQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a submitted prompt and stamp its wire `status`. Returns the
    /// wire item, and the run input when this prompt becomes active (the caller
    /// must start a turn); `None` means it was queued behind the active one.
    pub fn admit(
        &self,
        session_id: &str,
        item: Value,
        prompt: String,
        blocks: Vec<ContentBlock>,
        origin: Option<Value>,
    ) -> (Value, Option<RunInput>) {
        let mut state = self.lock();
        Self::defer_rich_wakes_locked(&mut state, session_id);
        let mut item = item;
        if state.active.contains_key(session_id) {
            item["status"] = json!("queued");
            state
                .queued
                .entry(session_id.to_string())
                .or_default()
                .push(Entry {
                    item: item.clone(),
                    prompt,
                    blocks,
                    origin,
                    prompt_events: true,
                    turn_number: None,
                    cancelled: false,
                    wake: None,
                });
            (item, None)
        } else {
            item["status"] = json!("running");
            let run = RunInput {
                prompt_id: item_id(&item),
                prompt: prompt.clone(),
                blocks: blocks.clone(),
                origin: origin.clone(),
                prompt_events: true,
            };
            state.active.insert(
                session_id.to_string(),
                Entry {
                    item: item.clone(),
                    prompt,
                    blocks,
                    origin,
                    prompt_events: true,
                    turn_number: None,
                    cancelled: false,
                    wake: None,
                },
            );
            (item, Some(run))
        }
    }

    /// Admit a synthetic task-notification turn (v2 `loop.notify`
    /// `turnScoped: false`): the model sees a background task's completion as
    /// input even though no user prompt is pending. The entry carries no
    /// prompt events, and its origin is the task origin the transcript folds.
    ///
    /// Returns the run when this wake became the session's active turn;
    /// `None` means it queued behind an active prompt and the running driver
    /// promotes it through [`PromptQueue::advance`].
    pub fn admit_wake(&self, session_id: &str, text: String, origin: Value) -> Option<RunInput> {
        self.admit_wake_entry(session_id, text, Vec::new(), Some(origin), None)
            .1
    }

    /// Admit a caller-rendered rich wake. Unlike an ordinary task notification,
    /// this entry participates in user-prompt interruption and replay.
    pub fn admit_rich_wake(&self, session_id: &str, payload: WakePayload) -> Option<RunInput> {
        self.admit_rich_wake_with_id(session_id, payload).1
    }

    /// Variant that also returns the wake id for explicit cancellation through
    /// [`PromptQueue::cancel`]. Explicit cancellation discards; only automatic
    /// user-prompt interruption requeues the payload.
    pub fn admit_rich_wake_with_id(
        &self,
        session_id: &str,
        payload: WakePayload,
    ) -> (String, Option<RunInput>) {
        let origin = payload.origin();
        self.admit_wake_entry(
            session_id,
            payload.message.content.clone(),
            payload.message.blocks.clone(),
            Some(origin),
            Some(payload),
        )
    }

    fn admit_wake_entry(
        &self,
        session_id: &str,
        prompt: String,
        blocks: Vec<ContentBlock>,
        origin: Option<Value>,
        wake: Option<WakePayload>,
    ) -> (String, Option<RunInput>) {
        let mut state = self.lock();
        let entry = Self::new_wake_entry(prompt, blocks, origin, wake);
        let wake_id = item_id(&entry.item);
        if state.active.contains_key(session_id) {
            state
                .queued
                .entry(session_id.to_string())
                .or_default()
                .push(entry);
            return (wake_id, None);
        }
        let run = Self::run_input(&entry);
        let mut entry = entry;
        entry.item["status"] = json!("running");
        state.active.insert(session_id.to_string(), entry);
        (wake_id, Some(run))
    }

    fn new_wake_entry(
        prompt: String,
        blocks: Vec<ContentBlock>,
        origin: Option<Value>,
        wake: Option<WakePayload>,
    ) -> Entry {
        Entry {
            item: json!({
                "prompt_id": format!("wake-{}", fastrand::u64(..)),
                "status": "queued",
                "kind": "notification",
                "created_at": chrono::Utc::now().to_rfc3339(),
            }),
            prompt,
            blocks,
            origin,
            prompt_events: false,
            turn_number: None,
            cancelled: false,
            wake,
        }
    }

    fn run_input(entry: &Entry) -> RunInput {
        RunInput {
            prompt_id: item_id(&entry.item),
            prompt: entry.prompt.clone(),
            blocks: entry.blocks.clone(),
            origin: entry.origin.clone(),
            prompt_events: entry.prompt_events,
        }
    }

    fn defer_rich_wakes_locked(state: &mut State, session_id: &str) {
        let deferred = state
            .deferred_wakes
            .entry(session_id.to_string())
            .or_default();
        if let Some(active) = state.active.get_mut(session_id)
            && active.wake.is_some()
            && !active.cancelled
        {
            active.cancelled = true;
            active.item["status"] = json!("aborted");
            deferred.push(active.wake.clone().expect("checked above"));
        }
        if let Some(queue) = state.queued.get_mut(session_id) {
            let mut ordinary = Vec::with_capacity(queue.len());
            for entry in queue.drain(..) {
                if let Some(payload) = entry.wake {
                    deferred.push(payload);
                } else {
                    ordinary.push(entry);
                }
            }
            *queue = ordinary;
        }
    }

    /// The expected server turn number of an active wake interrupted by a user
    /// prompt. The caller passes it to `ServerEngine::cancel_turn_at`, which
    /// refuses to cancel a newer turn if the wake ended in the meantime.
    pub fn interrupted_wake_turn(&self, session_id: &str) -> Option<u32> {
        let state = self.lock();
        let active = state.active.get(session_id)?;
        (active.wake.is_some() && active.cancelled)
            .then_some(active.turn_number)
            .flatten()
    }

    /// Whether the active rich wake was interrupted before its driver started
    /// the turn. The run loop skips execution and immediately promotes the
    /// queued user prompt in this case.
    pub fn active_is_interrupted_wake(&self, session_id: &str) -> bool {
        self.lock()
            .active
            .get(session_id)
            .is_some_and(|entry| entry.wake.is_some() && entry.cancelled)
    }

    /// Discard a producer's whole wake channel, including deferred replays.
    /// Returns whether anything was found and the active turn that now needs
    /// cancellation, if it had already started.
    pub fn drop_rich_wakes(&self, session_id: &str, variant: &str) -> (bool, Option<u32>) {
        let mut state = self.lock();
        let mut found = false;
        let mut active_turn = None;
        if let Some(active) = state.active.get_mut(session_id)
            && active
                .wake
                .as_ref()
                .is_some_and(|payload| payload.variant == variant)
        {
            found = true;
            if !active.cancelled {
                active.cancelled = true;
                active.item["status"] = json!("aborted");
                active_turn = active.turn_number;
            }
        }
        if let Some(queue) = state.queued.get_mut(session_id) {
            let before = queue.len();
            queue.retain(|entry| {
                entry
                    .wake
                    .as_ref()
                    .is_none_or(|payload| payload.variant != variant)
            });
            found |= queue.len() != before;
        }
        if let Some(deferred) = state.deferred_wakes.get_mut(session_id) {
            let before = deferred.len();
            deferred.retain(|payload| payload.variant != variant);
            found |= deferred.len() != before;
        }
        if found {
            state
                .wake_replay_errors
                .remove(&(session_id.to_string(), variant.to_string()));
        }
        (found, active_turn)
    }

    /// `(active, queued)` wire items for the list route.
    pub fn snapshot(&self, session_id: &str) -> (Option<Value>, Vec<Value>) {
        let state = self.lock();
        let active = state.active.get(session_id).map(|entry| entry.item.clone());
        let queued = state
            .queued
            .get(session_id)
            .map(|entries| entries.iter().map(|entry| entry.item.clone()).collect())
            .unwrap_or_default();
        (active, queued)
    }

    /// The active prompt's wire item, when one is running.
    pub fn active_item(&self, session_id: &str) -> Option<Value> {
        self.lock()
            .active
            .get(session_id)
            .map(|entry| entry.item.clone())
    }

    /// Replace the replay owner for one session + variant. Deferred payloads
    /// use the handler on their next advance; unregistered variants retain the
    /// original direct replay.
    pub fn set_wake_replay_handler(
        &self,
        session_id: &str,
        variant: &str,
        handler: WakeReplayHandler,
    ) {
        self.lock()
            .wake_replay_handlers
            .insert((session_id.to_string(), variant.to_string()), handler);
    }

    /// Inspect one channel's deferred/in-flight replay and last handler error.
    pub fn wake_replay_status(&self, session_id: &str, variant: &str) -> Option<WakeReplayStatus> {
        let state = self.lock();
        let key = (session_id.to_string(), variant.to_string());
        let pending = state.wake_replay_inflight.contains(&key)
            || state
                .deferred_wakes
                .get(session_id)
                .is_some_and(|deferred| deferred.iter().any(|payload| payload.variant == variant));
        let error = state.wake_replay_errors.get(&key).cloned();
        (pending || error.is_some()).then(|| WakeReplayStatus {
            variant: variant.to_string(),
            pending,
            error,
        })
    }

    /// Select the next runnable turn, or a delegated replay when its variant
    /// has a handler. The queue lock is released before `advance_after_turn`
    /// invokes that handler.
    fn advance(&self, session_id: &str) -> PromptAdvance {
        let mut state = self.lock();
        let queued = match state.queued.get_mut(session_id) {
            Some(queue) if !queue.is_empty() => Some(queue.remove(0)),
            _ => None,
        };
        let next = match queued {
            Some(entry) => entry,
            None => {
                let Some(payload) = state
                    .deferred_wakes
                    .get_mut(session_id)
                    .filter(|deferred| !deferred.is_empty())
                    .map(|deferred| deferred.remove(0))
                else {
                    return PromptAdvance::Idle;
                };
                let key = (session_id.to_string(), payload.variant.clone());
                if let Some(handler) = state.wake_replay_handlers.get(&key).cloned() {
                    state.wake_replay_inflight.insert(key);
                    return PromptAdvance::Replay(payload, handler);
                }
                let origin = payload.origin();
                Self::new_wake_entry(
                    payload.message.content.clone(),
                    payload.message.blocks.clone(),
                    Some(origin),
                    Some(payload),
                )
            }
        };
        let run = Self::run_input(&next);
        let mut next = next;
        next.item["status"] = json!("running");
        state.active.insert(session_id.to_string(), next);
        PromptAdvance::Run(run)
    }

    /// Advance after a completed turn, awaiting any delegated replay. Handler
    /// success transfers ownership of re-enqueue to the handler. On failure the
    /// original payload is restored and the error remains queryable; returning
    /// `None` stops the current driver instead of spinning on the failure.
    async fn advance_after_turn(&self, session_id: &str) -> Option<RunInput> {
        loop {
            let (payload, handler) = match self.advance(session_id) {
                PromptAdvance::Run(run) => return Some(run),
                PromptAdvance::Idle => return None,
                PromptAdvance::Replay(payload, handler) => (payload, handler),
            };
            let variant = payload.variant.clone();
            let result = handler(payload.clone()).await;
            let mut state = self.lock();
            state
                .wake_replay_inflight
                .remove(&(session_id.to_string(), variant.clone()));
            match result {
                Ok(()) => {
                    state
                        .wake_replay_errors
                        .remove(&(session_id.to_string(), variant));
                }
                Err(error) => {
                    state
                        .deferred_wakes
                        .entry(session_id.to_string())
                        .or_default()
                        .insert(0, payload);
                    state
                        .wake_replay_errors
                        .insert((session_id.to_string(), variant), error);
                    return None;
                }
            }
        }
    }

    /// Settle a prompt by id (v2 `cancelWaiter`, loopService.ts:735-750): a
    /// queued prompt is removed from the queue so the driver never promotes
    /// it, and the active prompt is flagged so the driver publishes no second
    /// terminal event on top of the `prompt.aborted` the route published.
    /// `None` means no active or queued prompt carries the id.
    pub fn cancel(&self, session_id: &str, prompt_id: &str) -> Option<CancelOutcome> {
        Self::cancel_locked(&mut self.lock(), session_id, prompt_id)
    }

    fn cancel_locked(
        state: &mut State,
        session_id: &str,
        prompt_id: &str,
    ) -> Option<CancelOutcome> {
        if let Some(entry) = state.active.get_mut(session_id)
            && item_id(&entry.item) == prompt_id
        {
            entry.cancelled = true;
            entry.item["status"] = json!("aborted");
            return Some(CancelOutcome::Active);
        }
        if let Some(queue) = state.queued.get_mut(session_id) {
            let before = queue.len();
            queue.retain(|entry| item_id(&entry.item) != prompt_id);
            if queue.len() != before {
                return Some(CancelOutcome::Queued);
            }
        }
        None
    }

    /// Record the turn number the driver is about to run for the active
    /// prompt, so the live message id (`msg-u{turn}`) can be resolved back to
    /// it. Called by `run_prompt_loop` once per turn.
    pub fn stamp_active_turn(&self, session_id: &str, turn_number: u32) {
        if let Some(entry) = self.lock().active.get_mut(session_id) {
            entry.turn_number = Some(turn_number);
        }
    }

    /// The turn number stamped for the active prompt, when the driver has
    /// started it — the `turnId` a `turn.steer` payload carries so the fold
    /// can pair the steer with its turn (v2 `turnSteerSchema`).
    pub fn active_turn_number(&self, session_id: &str) -> Option<u32> {
        self.lock()
            .active
            .get(session_id)
            .and_then(|entry| entry.turn_number)
    }

    /// Settle a prompt by the *user message id* a client holds, returning the
    /// resolved prompt id alongside the outcome. Two schemes name the same
    /// message and neither is the prompt id:
    ///
    /// - `msg-u{turn}` — the live `event.message.created` id
    ///   (`message_events.rs`), resolved through the active prompt's stamped
    ///   turn. This is what the web UI's abort button sends (ROADMAP §7.6:
    ///   the raw prompt-id lookup missed it and the route answered 404).
    /// - `msg-{prompt_id}` — the prompt item's own `user_message_id`.
    ///
    /// A turn scheme that names a turn the active prompt is not running falls
    /// through to the prompt-id scheme, so a client-chosen prompt id that
    /// happens to start with `u` still resolves.
    pub fn cancel_by_user_message_id(
        &self,
        session_id: &str,
        user_message_id: &str,
    ) -> Option<(String, CancelOutcome)> {
        let mut state = self.lock();
        if let Some(turn) = user_message_id
            .strip_prefix("msg-u")
            .and_then(|rest| rest.parse::<u32>().ok())
            && let Some(entry) = state.active.get(session_id)
            && entry.turn_number == Some(turn)
        {
            let prompt_id = item_id(&entry.item);
            let outcome = Self::cancel_locked(&mut state, session_id, &prompt_id)?;
            return Some((prompt_id, outcome));
        }
        let prompt_id = user_message_id.strip_prefix("msg-")?;
        let outcome = Self::cancel_locked(&mut state, session_id, prompt_id)?;
        Some((prompt_id.to_string(), outcome))
    }

    /// Whether the abort route settled the active prompt.
    pub fn active_is_cancelled(&self, session_id: &str) -> bool {
        self.lock()
            .active
            .get(session_id)
            .is_some_and(|entry| entry.cancelled)
    }

    /// Overwrite the active prompt's status (used to surface `blocked`).
    pub fn set_active_status(&self, session_id: &str, status: &str) {
        if let Some(entry) = self.lock().active.get_mut(session_id) {
            entry.item["status"] = json!(status);
        }
    }

    /// Drop the active slot once a session has no more prompts to run.
    pub fn clear_active(&self, session_id: &str) {
        self.lock().active.remove(session_id);
    }

    /// Remove queued prompts by id, returning them for steering. Ids that are
    /// not queued are absent from the result.
    pub fn take_queued(
        &self,
        session_id: &str,
        ids: &[String],
    ) -> Vec<(Value, String, Vec<ContentBlock>, Option<Value>)> {
        let mut state = self.lock();
        let mut taken = Vec::new();
        if let Some(queue) = state.queued.get_mut(session_id) {
            let mut remaining = Vec::new();
            for entry in queue.drain(..) {
                if ids.iter().any(|wanted| wanted == &item_id(&entry.item)) {
                    taken.push((entry.item, entry.prompt, entry.blocks, entry.origin));
                } else {
                    remaining.push(entry);
                }
            }
            *queue = remaining;
        }
        taken
    }

    /// Whether a prompt id is active or queued.
    pub fn contains(&self, session_id: &str, prompt_id: &str) -> bool {
        let state = self.lock();
        state
            .active
            .get(session_id)
            .is_some_and(|entry| item_id(&entry.item) == prompt_id)
            || state
                .queued
                .get(session_id)
                .is_some_and(|queue| queue.iter().any(|entry| item_id(&entry.item) == prompt_id))
    }

    /// Whether the active prompt has this id.
    pub fn active_is(&self, session_id: &str, prompt_id: &str) -> bool {
        self.lock()
            .active
            .get(session_id)
            .is_some_and(|entry| item_id(&entry.item) == prompt_id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn item_id(item: &Value) -> String {
    item.get("prompt_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Publish one prompt-lifecycle event on the session's lane.
pub fn publish_prompt_event(hub: &EventHub, session_id: &str, event: Value) {
    hub.bus_for(session_id).publish(&EngineEvent::Custom(event));
}

/// Drive a session's prompts to completion: run the admitted prompt, then
/// promote and run each queued one in turn until the queue drains. Emits
/// `prompt.completed` per prompt (reason `failed` when the turn errored, the
/// prompt then surfaces as `blocked`).
pub async fn run_prompt_loop(
    engine: Option<Arc<ServerEngine>>,
    store: Arc<SqliteSessionStore>,
    queue: Arc<PromptQueue>,
    hub: Arc<EventHub>,
    session_id: String,
    run: RunInput,
) {
    let Some(engine) = engine else {
        queue.clear_active(&session_id);
        return;
    };
    let mut run = run;
    loop {
        // A user prompt can interrupt an admitted wake before this driver gets
        // its first poll. Such a wake never starts an engine turn; its payload
        // is already deferred and replayed after the user queue drains.
        if queue.active_is_interrupted_wake(&session_id) {
            match queue.advance_after_turn(&session_id).await {
                Some(next) => {
                    run = next;
                    continue;
                }
                None => {
                    queue.clear_active(&session_id);
                    break;
                }
            }
        }
        let history = store.load_session_history(&session_id).unwrap_or_default();
        let turn_number = store.next_turn_number(&session_id).unwrap_or(1);
        // The live `event.message.created` id names this turn (`msg-u{turn}`);
        // stamping it lets the abort route resolve that id back to the prompt
        // (ROADMAP §7.6).
        queue.stamp_active_turn(&session_id, turn_number);
        let outcome = engine
            .run_turn_with_media(
                &session_id,
                turn_number,
                history,
                &run.prompt,
                run.blocks.clone(),
                run.origin.clone(),
            )
            .await;
        if outcome.is_err() {
            queue.set_active_status(&session_id, "blocked");
        }
        // A prompt the abort route already settled has its terminal event
        // (`prompt.aborted`); publishing `prompt.completed` as well would
        // settle it twice — v2's `cancelWaiter` settles exactly once.
        // `agentId` is deliberately the literal `main`, not a carried-over id:
        // the prompt queue drives the session's main turn only — a queued
        // prompt, a steer, and an abort all target that one turn. This is a
        // different thing from the subagent identity plumbing, where an event's
        // owner is threaded explicitly through
        // `RunTurnInput.agent_id` → `ToolExecuteRequest.agent_id` and a
        // task-local read would silently degrade to `main` across the
        // scheduler's `tokio::spawn`. Do not "fix" this into a lookup.
        if run.prompt_events && !queue.active_is_cancelled(&session_id) {
            publish_prompt_event(
                &hub,
                &session_id,
                json!({
                    "type": "prompt.completed",
                    "agentId": "main",
                    "sessionId": session_id,
                    "promptId": run.prompt_id,
                    "finishedAt": chrono::Utc::now().to_rfc3339(),
                    "reason": if outcome.is_ok() { "completed" } else { "failed" },
                }),
            );
        }
        // v2 `launchContinuationTurn`: a cleanly completed turn of an active
        // goal owes the model another prompt — without it a goal never
        // progresses past one response, because no host loop re-prompts on
        // this entry point. The continuation runs before the queue advances,
        // so a queued user prompt cannot interleave with the goal's own
        // turns; a cancel stops the chain, the way v2 pauses the goal on an
        // interrupted turn.
        let mut outcome = outcome;
        while !queue.active_is_cancelled(&session_id)
            && outcome.as_ref().is_ok_and(turn_owes_goal_continuation)
        {
            let step_capped = outcome.as_ref().is_ok_and(turn_step_capped);
            let Some(text) = engine
                .pending_goal_continuation(&session_id, step_capped)
                .await
            else {
                break;
            };
            let history = store.load_session_history(&session_id).unwrap_or_default();
            let turn_number = store.next_turn_number(&session_id).unwrap_or(1);
            queue.stamp_active_turn(&session_id, turn_number);
            outcome = engine
                .run_turn_with_media(
                    &session_id,
                    turn_number,
                    history,
                    &text,
                    Vec::new(),
                    Some(json!({ "kind": "system_trigger", "name": "goal_continuation" })),
                )
                .await;
        }
        match queue.advance_after_turn(&session_id).await {
            Some(next) => run = next,
            None => {
                queue.clear_active(&session_id);
                break;
            }
        }
    }
}

/// Drain one session's queued task notifications into the wake turn's prompt
/// and origin, or `None` when nothing is queued.
pub fn take_task_wake_prompt(
    runner: &crate::storage::TaskRunner,
    session_id: &str,
) -> Option<(String, Value)> {
    let notifications = runner.take_pending_notifications(Some(session_id));
    let first = notifications.first()?;
    let text = crate::session::render_task_notifications(&notifications);
    let origin = json!({
        "kind": "task",
        "taskId": &first.task_id,
        "status": first.status.as_str(),
        "notificationId": format!("{}-{}", first.task_id, first.ended_at),
    });
    Some((text, origin))
}

/// Whether a turn ended cleanly enough to owe a goal continuation. v2
/// continues on a completed turn (a max-tokens finish included); a
/// cancelled, filtered, or step-capped turn settles the goal instead.
pub(crate) fn turn_completed(report: &TurnReport) -> bool {
    matches!(
        report.stop_reason.as_str(),
        "EndTurn" | "MaxTokens" | "RepeatBreaker"
    )
}

/// A turn after which the goal may owe another turn: a clean completion, or
/// the per-turn step budget running out (v2 `isMaxStepsTurnFailure`, which
/// continues with the step-cap prompt instead of pausing).
pub(crate) fn turn_owes_goal_continuation(report: &TurnReport) -> bool {
    turn_completed(report) || turn_step_capped(report)
}

/// Whether the turn stopped because it ran out of steps inside its goal
/// (v2 `isMaxStepsTurnFailure`).
pub(crate) fn turn_step_capped(report: &TurnReport) -> bool {
    report.stop_reason == "MaxSteps"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> Value {
        json!({
            "prompt_id": id,
            "user_message_id": format!("msg-{id}"),
            "content": [{ "type": "text", "text": id }],
            "created_at": "2026-01-01T00:00:00Z",
        })
    }

    fn advance(queue: &PromptQueue, session_id: &str) -> Option<RunInput> {
        match queue.advance(session_id) {
            PromptAdvance::Run(run) => Some(run),
            PromptAdvance::Idle => None,
            PromptAdvance::Replay(_, _) => panic!(),
        }
    }

    #[test]
    fn first_prompt_runs_second_queues() {
        let queue = PromptQueue::new();
        let (first, run) = queue.admit("s1", item("p1"), "p1".into(), Vec::new(), None);
        assert_eq!(first["status"], "running");
        assert_eq!(run.unwrap().prompt_id, "p1");

        let (second, run) = queue.admit("s1", item("p2"), "p2".into(), Vec::new(), None);
        assert_eq!(second["status"], "queued");
        assert!(run.is_none());

        let (active, queued) = queue.snapshot("s1");
        assert_eq!(active.unwrap()["prompt_id"], "p1");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0]["prompt_id"], "p2");
    }

    /// v2 `loop.notify` on the server: a task-notification wake is a queued
    /// turn with no prompt behind it. It runs when the session is idle and
    /// queues behind an active prompt otherwise, and the driver is told not to
    /// settle a client prompt for it.
    #[test]
    fn a_task_wake_runs_when_idle_and_queues_behind_a_prompt() {
        let queue = PromptQueue::new();
        let origin = json!({
            "kind": "task",
            "taskId": "t1",
            "status": "completed",
            "notificationId": "t1-1",
        });
        let (_, run) = queue.admit("s1", item("p1"), "p1".into(), Vec::new(), None);
        assert!(run.is_some());

        assert!(
            queue
                .admit_wake("s1", "task done".into(), origin.clone())
                .is_none(),
            "a busy session queues the wake"
        );
        assert_eq!(queue.snapshot("s1").1.len(), 1);

        let next = advance(&queue, "s1").expect("the queued wake is promoted");
        assert_eq!(next.prompt, "task done");
        assert!(!next.prompt_events, "a wake settles no client prompt");
        assert_eq!(next.origin.unwrap()["kind"], "task");
        queue.clear_active("s1");

        let run = queue
            .admit_wake("s1", "task done".into(), origin)
            .expect("an idle session admits the wake immediately");
        assert_eq!(run.prompt, "task done");
        assert!(!run.prompt_events);
        assert_eq!(queue.snapshot("s1").0.unwrap()["kind"], "notification");
    }

    /// ROADMAP §7.6: the web UI's abort button sends the live
    /// `event.message.created` id (`msg-u{turn}`, `message_events.rs`), which
    /// names the turn's user message — not the prompt. The raw prompt-id
    /// lookup misses it and the route answered 404; resolving the message id
    /// to the prompt whose turn it names is what keeps the button working.
    #[test]
    fn abort_resolves_the_live_user_message_id_to_the_active_prompt() {
        let queue = PromptQueue::new();
        let (_item, run) = queue.admit("s1", item("p1"), "p1".into(), Vec::new(), None);
        run.unwrap();
        queue.stamp_active_turn("s1", 2);

        assert!(
            queue.cancel("s1", "msg-u2").is_none(),
            "the live message id is not a prompt id — the raw lookup misses"
        );

        let (resolved, outcome) = queue
            .cancel_by_user_message_id("s1", "msg-u2")
            .expect("the live message id resolves to the active prompt");
        assert_eq!(resolved, "p1");
        assert!(matches!(outcome, CancelOutcome::Active));
        assert!(queue.active_is_cancelled("s1"));
    }

    /// The prompt item's own `user_message_id` (`msg-{prompt_id}`) is the
    /// other scheme a client can hold; it resolves active and queued prompts
    /// alike. An id that names neither is still a miss.
    #[test]
    fn abort_resolves_the_prompt_items_own_user_message_id() {
        let queue = PromptQueue::new();
        let (_first, run) = queue.admit("s1", item("p1"), "p1".into(), Vec::new(), None);
        run.unwrap();
        let (_second, none) = queue.admit("s1", item("p2"), "p2".into(), Vec::new(), None);
        assert!(none.is_none(), "p2 queues behind the active prompt");

        let (resolved, outcome) = queue
            .cancel_by_user_message_id("s1", "msg-p2")
            .expect("the item's own scheme resolves a queued prompt");
        assert_eq!(resolved, "p2");
        assert!(matches!(outcome, CancelOutcome::Queued));

        let (resolved, outcome) = queue
            .cancel_by_user_message_id("s1", "msg-p1")
            .expect("the item's own scheme resolves the active prompt");
        assert_eq!(resolved, "p1");
        assert!(matches!(outcome, CancelOutcome::Active));

        assert!(queue.cancel_by_user_message_id("s1", "msg-nope").is_none());
        assert!(
            queue.cancel_by_user_message_id("s1", "p1").is_none(),
            "a bare prompt id is not a message id"
        );
        assert!(
            queue.cancel_by_user_message_id("s1", "msg-u9").is_none(),
            "a turn the active prompt is not running does not resolve"
        );
    }

    #[test]
    fn client_metadata_rides_the_item_and_the_run() {
        // #3764: the route wraps the request's `metadata` as a one-element
        // `clientMetadata` array inside the origin; the item echoes it for the
        // REST response and the run carries the origin to persistence.
        let queue = PromptQueue::new();
        let mut base = item("p1");
        base["metadata"] = json!({ "surface": "web", "threadId": "abc" });
        let origin = json!({
            "kind": "user",
            "clientMetadata": [{ "surface": "web", "threadId": "abc" }],
        });
        let (admitted, run) = queue.admit("s1", base, "p1".into(), Vec::new(), Some(origin));
        assert_eq!(admitted["metadata"]["threadId"], "abc");
        let run = run.unwrap();
        let origin = run.origin.unwrap();
        assert_eq!(origin["kind"], "user");
        assert_eq!(origin["clientMetadata"][0]["surface"], "web");

        // A queued prompt keeps its origin until it is promoted.
        let mut queued_base = item("p2");
        queued_base["metadata"] = json!({ "surface": "web" });
        let origin2 = json!({ "kind": "user", "clientMetadata": [{ "surface": "web" }] });
        let (_queued, none) =
            queue.admit("s1", queued_base, "p2".into(), Vec::new(), Some(origin2));
        assert!(none.is_none());

        let next = advance(&queue, "s1").expect("p2 promoted");
        let origin = next.origin.unwrap();
        assert_eq!(origin["clientMetadata"][0]["surface"], "web");
    }

    #[test]
    fn advancing_promotes_then_idles() {
        let queue = PromptQueue::new();
        queue.admit("s1", item("p1"), "p1".into(), Vec::new(), None);
        queue.admit("s1", item("p2"), "p2".into(), Vec::new(), None);

        let next = advance(&queue, "s1").expect("p2 promoted");
        assert_eq!(next.prompt, "p2");
        assert!(queue.active_is("s1", "p2"));
        assert_eq!(queue.snapshot("s1").0.unwrap()["status"], "running");
        assert!(advance(&queue, "s1").is_none());

        queue.clear_active("s1");
        assert!(queue.snapshot("s1").0.is_none());
    }

    #[test]
    fn take_queued_moves_only_requested_ids() {
        let queue = PromptQueue::new();
        queue.admit("s1", item("p1"), "p1".into(), Vec::new(), None);
        queue.admit("s1", item("p2"), "p2".into(), Vec::new(), None);
        queue.admit("s1", item("p3"), "p3".into(), Vec::new(), None);

        let taken = queue.take_queued("s1", &["p2".to_string()]);
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].0["prompt_id"], "p2");
        assert!(!queue.contains("s1", "p2"));
        assert!(queue.contains("s1", "p3"));
        assert!(queue.active_is("s1", "p1"));
    }

    #[test]
    fn set_active_status_marks_blocked() {
        let queue = PromptQueue::new();
        queue.admit("s1", item("p1"), "p1".into(), Vec::new(), None);
        queue.set_active_status("s1", "blocked");
        assert_eq!(queue.active_item("s1").unwrap()["status"], "blocked");
    }

    /// v2's `cancelWaiter` (loopService.ts:735-750): a queued prompt is
    /// removed so the driver never promotes it — an aborted prompt must not
    /// run after the abort.
    #[test]
    fn cancel_removes_a_queued_prompt_so_it_never_runs() {
        let queue = PromptQueue::new();
        queue.admit("s1", item("p1"), "p1".into(), Vec::new(), None);
        queue.admit("s1", item("p2"), "p2".into(), Vec::new(), None);

        assert!(matches!(
            queue.cancel("s1", "p2"),
            Some(CancelOutcome::Queued)
        ));
        assert!(!queue.contains("s1", "p2"));
        // The active prompt is untouched and still runs.
        assert!(queue.active_is("s1", "p1"));
        assert!(!queue.active_is_cancelled("s1"));
        // Promotion skips the removed entry entirely.
        assert!(advance(&queue, "s1").is_none());
    }

    /// The active prompt is flagged, not removed: its turn is still running
    /// and must be cancelled through the engine, and the driver must publish
    /// no second terminal event on top of the route's `prompt.aborted`.
    #[test]
    fn cancel_flags_the_active_prompt() {
        let queue = PromptQueue::new();
        queue.admit("s1", item("p1"), "p1".into(), Vec::new(), None);

        assert!(matches!(
            queue.cancel("s1", "p1"),
            Some(CancelOutcome::Active)
        ));
        assert!(queue.active_is_cancelled("s1"));
        assert_eq!(queue.active_item("s1").unwrap()["status"], "aborted");
        // Still present: the driver settles it after the turn returns.
        assert!(queue.contains("s1", "p1"));
    }

    #[test]
    fn cancel_reports_an_unknown_prompt() {
        let queue = PromptQueue::new();
        queue.admit("s1", item("p1"), "p1".into(), Vec::new(), None);
        assert!(queue.cancel("s1", "nope").is_none());
        assert!(queue.cancel("nope", "p1").is_none());
    }

    #[test]
    fn user_prompt_interrupts_active_rich_wake_and_requeues_it_afterward() {
        let queue = PromptQueue::new();
        let payload = crate::session::wake::WakePayload::new(
            "tower_inbox",
            crate::turn_loop::types::LLMMessage {
                role: "user".into(),
                content: "caller-rendered wake".into(),
                ..Default::default()
            },
        );
        let wake = queue
            .admit_rich_wake("s1", payload)
            .expect("an idle session starts the wake");
        assert!(!wake.prompt_events);
        let origin = wake.origin.expect("the wake carries its injection origin");
        assert_eq!(origin["kind"], "injection");
        assert_eq!(origin["variant"], "tower_inbox");
        queue.stamp_active_turn("s1", 7);

        let (_, user) = queue.admit("s1", item("p1"), "user".into(), Vec::new(), None);
        assert!(
            user.is_none(),
            "the user queues behind the interrupted wake"
        );
        assert_eq!(queue.interrupted_wake_turn("s1"), Some(7));
        assert!(queue.active_is_cancelled("s1"));

        let user = advance(&queue, "s1").expect("the user turn follows the wake");
        assert_eq!(user.prompt_id, "p1");
        let replay = advance(&queue, "s1").expect("the wake is requeued after the user turn");
        assert_eq!(replay.prompt, "caller-rendered wake");
        assert_eq!(replay.origin.unwrap()["variant"], "tower_inbox");
        assert!(!replay.prompt_events);
    }

    #[test]
    fn user_prompt_removes_queued_rich_wake_and_replays_it_after_user_turns() {
        let queue = PromptQueue::new();
        queue.admit("s1", item("p1"), "first".into(), Vec::new(), None);
        let payload = crate::session::wake::WakePayload::new(
            "tower_inbox",
            crate::turn_loop::types::LLMMessage {
                role: "user".into(),
                content: "queued wake".into(),
                ..Default::default()
            },
        );
        assert!(queue.admit_rich_wake("s1", payload).is_none());
        queue.admit("s1", item("p2"), "second".into(), Vec::new(), None);

        let second = advance(&queue, "s1").expect("the second user turn is next");
        assert_eq!(second.prompt_id, "p2");
        let replay = advance(&queue, "s1").expect("the queued wake replays after both user turns");
        assert_eq!(replay.prompt, "queued wake");
        assert!(!replay.prompt_events);
    }

    #[test]
    fn drop_rich_wakes_discards_deferred_replay_by_variant() {
        let queue = PromptQueue::new();
        queue.admit("s1", item("p1"), "first".into(), Vec::new(), None);
        let payload = crate::session::wake::WakePayload::new(
            "tower_inbox",
            crate::turn_loop::types::LLMMessage {
                role: "user".into(),
                content: "discarded wake".into(),
                ..Default::default()
            },
        );
        assert!(queue.admit_rich_wake("s1", payload).is_none());
        queue.admit("s1", item("p2"), "second".into(), Vec::new(), None);
        assert_eq!(advance(&queue, "s1").unwrap().prompt_id, "p2");

        assert_eq!(queue.drop_rich_wakes("s1", "tower_inbox"), (true, None));
        assert_eq!(queue.drop_rich_wakes("s1", "tower_inbox"), (false, None));
        assert!(advance(&queue, "s1").is_none());
    }

    #[tokio::test]
    async fn replay_handler_receives_original_payload_without_direct_requeue() {
        let queue = PromptQueue::new();
        let payload = crate::session::wake::WakePayload::new(
            "tower_inbox",
            crate::turn_loop::types::LLMMessage {
                role: "user".into(),
                content: "original server wake".into(),
                ..Default::default()
            },
        );
        let expected = payload.clone();
        queue.admit_rich_wake("s1", payload).unwrap();
        queue.stamp_active_turn("s1", 7);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let handler_seen = seen.clone();
        queue.set_wake_replay_handler(
            "s1",
            "tower_inbox",
            Arc::new(move |payload| {
                let handler_seen = handler_seen.clone();
                Box::pin(async move {
                    handler_seen.lock().unwrap().push(payload);
                    Ok(())
                })
            }),
        );
        queue.admit("s1", item("p1"), "user".into(), Vec::new(), None);

        let user = queue
            .advance_after_turn("s1")
            .await
            .expect("the user turn follows the interrupted wake");
        assert_eq!(user.prompt_id, "p1");
        assert!(
            queue.advance_after_turn("s1").await.is_none(),
            "the handler, not the queue, owns the replay"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].message.content, expected.message.content);
        assert_eq!(seen[0].items, expected.items);
        assert_eq!(seen[0].gate_digests, expected.gate_digests);
    }

    #[tokio::test]
    async fn replay_handler_error_retains_server_payload_and_exposes_status() {
        let queue = PromptQueue::new();
        let payload = crate::session::wake::WakePayload::new(
            "tower_inbox",
            crate::turn_loop::types::LLMMessage {
                role: "user".into(),
                content: "server wake retained after failure".into(),
                ..Default::default()
            },
        );
        queue.admit_rich_wake("s1", payload).unwrap();
        queue.stamp_active_turn("s1", 7);
        queue.set_wake_replay_handler(
            "s1",
            "tower_inbox",
            Arc::new(|_payload| Box::pin(async { Err("server replay failed".to_string()) })),
        );
        queue.admit("s1", item("p1"), "user".into(), Vec::new(), None);
        assert_eq!(
            queue.advance_after_turn("s1").await.unwrap().prompt_id,
            "p1"
        );
        assert!(queue.advance_after_turn("s1").await.is_none());

        let status = queue
            .wake_replay_status("s1", "tower_inbox")
            .expect("the failed server replay remains observable");
        assert!(status.pending);
        assert_eq!(status.error.as_deref(), Some("server replay failed"));
    }

    #[tokio::test]
    async fn http_wake_driver_preserves_enqueue_error_kind() {
        use crate::session::wake::TowerWakeDriver as _;

        let server = Arc::new(crate::server::HttpServer::in_memory().unwrap());
        server.store().create_session("s1", None).unwrap();
        let driver = crate::server::HttpServerWakeDriver::new(server.clone());
        let error = driver
            .enqueue(
                "s1",
                crate::session::wake::WakePayload::new(
                    "tower_inbox",
                    crate::turn_loop::types::LLMMessage::default(),
                ),
            )
            .unwrap_err();
        assert_eq!(
            error,
            format!("{:?}", crate::server::WakeEnqueueError::EngineUnavailable)
        );
        let error = driver
            .enqueue(
                "missing",
                crate::session::wake::WakePayload::new(
                    "tower_inbox",
                    crate::turn_loop::types::LLMMessage::default(),
                ),
            )
            .unwrap_err();
        assert_eq!(
            error,
            format!("{:?}", crate::server::WakeEnqueueError::SessionNotFound)
        );
    }

    #[tokio::test]
    async fn http_driver_forwards_replay_handler_and_status() {
        use crate::session::wake::TowerWakeDriver as _;

        let server = Arc::new(crate::server::HttpServer::in_memory().unwrap());
        let driver = crate::server::HttpServerWakeDriver::new(server.clone());
        driver.set_replay_handler(
            "s1",
            "tower_inbox",
            Arc::new(|_payload| Box::pin(async { Err("http driver replay failed".to_string()) })),
        );
        let payload = crate::session::wake::WakePayload::new(
            "tower_inbox",
            crate::turn_loop::types::LLMMessage {
                role: "user".into(),
                content: "http driver retained wake".into(),
                ..Default::default()
            },
        );
        server.prompt_queue.admit_rich_wake("s1", payload).unwrap();
        server.prompt_queue.stamp_active_turn("s1", 7);
        server
            .prompt_queue
            .admit("s1", item("p1"), "user".into(), Vec::new(), None);
        assert_eq!(
            server
                .prompt_queue
                .advance_after_turn("s1")
                .await
                .unwrap()
                .prompt_id,
            "p1"
        );
        assert!(server.prompt_queue.advance_after_turn("s1").await.is_none());

        let status = driver
            .wake_replay_status("s1", "tower_inbox")
            .expect("the HTTP driver forwards replay status");
        assert!(status.pending);
        assert_eq!(status.error.as_deref(), Some("http driver replay failed"));
        assert_eq!(
            status,
            server.wake_replay_status("s1", "tower_inbox").unwrap()
        );
    }
}
