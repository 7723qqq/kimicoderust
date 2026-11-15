use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::{Value, json};

use super::*;

pub const WAKE_BATCH_LIMIT: usize = 6;

pub const WAKE_TRACKED_KEY_LIMIT: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeItem {
    pub key: String,
    pub source: String,
    pub subject: String,
    pub message_id: Option<String>,
    pub file: Option<String>,
    pub sent_at: Option<String>,
    pub mission_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeGateDigest {
    pub mission_id: String,
    pub status: String,
    pub reasons: Vec<String>,
    pub binding: Option<String>,
}

pub type WakeReplayHandler = Arc<
    dyn Fn(WakePayload) -> futures_util::future::BoxFuture<'static, Result<(), String>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeReplayStatus {
    pub variant: String,
    pub pending: bool,
    pub error: Option<String>,
}

pub trait TowerWakeDriver: Send + Sync {
    fn enqueue(&self, session_id: &str, payload: WakePayload) -> Result<(), String>;
    fn drop_wakes(&self, session_id: &str, variant: &str) -> bool;
    fn set_replay_handler(&self, session_id: &str, variant: &str, handler: WakeReplayHandler) {
        let _ = (session_id, variant, handler);
    }
    fn wake_replay_status(&self, session_id: &str, variant: &str) -> Option<WakeReplayStatus> {
        let _ = (session_id, variant);
        None
    }
}

#[derive(Debug, Clone)]
pub struct WakePayload {
    pub variant: String,
    pub message: LLMMessage,
    pub items: Vec<WakeItem>,
    pub omitted: usize,
    pub gate_digests: Vec<WakeGateDigest>,
}

impl WakePayload {
    #[must_use]
    pub fn new(variant: impl Into<String>, message: LLMMessage) -> Self {
        Self {
            variant: variant.into(),
            message,
            items: Vec::new(),
            omitted: 0,
            gate_digests: Vec::new(),
        }
    }

    #[must_use]
    pub fn origin(&self) -> Value {
        json!({ "kind": "injection", "variant": self.variant })
    }

    pub(crate) fn input(&self) -> Value {
        json!([{ "type": "text", "text": self.message.content }])
    }
}

#[derive(Debug)]
pub struct WakeBuffer {
    variant: String,
    items: Vec<WakeItem>,
    omitted: usize,
    legacy_sequence: u64,
    recent_keys: HashSet<String>,
    recent_order: VecDeque<String>,
}

impl WakeBuffer {
    #[must_use]
    pub fn new(variant: impl Into<String>) -> Self {
        Self {
            variant: variant.into(),
            items: Vec::new(),
            omitted: 0,
            legacy_sequence: 0,
            recent_keys: HashSet::new(),
            recent_order: VecDeque::new(),
        }
    }

    pub fn push(&mut self, mut item: WakeItem) -> bool {
        if item.key.is_empty() {
            self.legacy_sequence += 1;
            item.key = format!("legacy:{}", self.legacy_sequence);
        }
        self.enqueue(item, false)
    }

    fn enqueue(&mut self, item: WakeItem, replay: bool) -> bool {
        if self.items.iter().any(|candidate| candidate.key == item.key) {
            return false;
        }
        if !replay && self.recent_keys.contains(&item.key) {
            return false;
        }
        self.remember(item.key.clone());
        self.items.push(item);
        if self.items.len() > WAKE_BATCH_LIMIT {
            self.items.remove(0);
            self.omitted += 1;
        }
        true
    }

    fn remember(&mut self, key: String) {
        if self.recent_keys.insert(key.clone()) {
            self.recent_order.push_back(key);
        }
        while self.recent_order.len() > WAKE_TRACKED_KEY_LIMIT {
            if let Some(oldest) = self.recent_order.pop_front() {
                self.recent_keys.remove(&oldest);
            }
        }
    }

    #[must_use]
    pub fn items(&self) -> &[WakeItem] {
        &self.items
    }

    #[must_use]
    pub fn omitted(&self) -> usize {
        self.omitted
    }

    pub fn take(
        &mut self,
        message: LLMMessage,
        gate_digests: Vec<WakeGateDigest>,
    ) -> Option<WakePayload> {
        if self.items.is_empty() {
            return None;
        }
        Some(WakePayload {
            variant: self.variant.clone(),
            message,
            items: std::mem::take(&mut self.items),
            omitted: std::mem::take(&mut self.omitted),
            gate_digests,
        })
    }

    pub fn requeue(&mut self, payload: WakePayload) {
        assert_eq!(payload.variant, self.variant);
        for item in payload.items {
            self.enqueue(item, true);
        }
        self.omitted += payload.omitted;
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.omitted = 0;
        self.recent_keys.clear();
        self.recent_order.clear();
    }
}

impl EngineSession {
    pub fn enqueue_wake(&self, payload: WakePayload) -> Result<TurnReceipt, String> {
        let (outcome_tx, outcome_rx) = oneshot::channel();
        let request = TurnRequest {
            prompt: payload.message.clone(),
            admission: Admission::NewTurn,
            input: payload.input(),
            origin: payload.origin(),
        };
        let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        let turn_id = if core.quiescence_depth > 0 {
            let turn_id = core.next_turn_id;
            core.next_turn_id += 1;
            core.held.push(HeldTurn {
                turn_id,
                prompt_id: Some(format!("wake-{turn_id}")),
                request,
                cancel: Arc::new(AtomicBool::new(false)),
                outcome: Some(outcome_tx),
                wake: Some(payload),
            });
            turn_id
        } else {
            Self::admit_locked(
                &mut core,
                request,
                outcome_tx,
                None,
                TurnPlacement {
                    preallocated_id: None,
                    wake: Some(payload),
                },
                AdmissionHandles {
                    steer_queue: &self.steer_queue,
                    steer_slot: &self.steer_slot,
                    wakeup: &self.wakeup,
                    journal: self.steer_trace.as_ref(),
                },
            )?
        };
        Ok(TurnReceipt {
            turn_id,
            prompt_id: format!("wake-{turn_id}"),
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            state: PromptSubmissionState::Running,
            steered: false,
            outcome: outcome_rx,
        })
    }

    pub fn set_wake_replay_handler(&self, variant: &str, handler: WakeReplayHandler) {
        self.core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .wake_replay_handlers
            .insert(variant.to_string(), handler);
    }

    pub fn wake_replay_status(&self, variant: &str) -> Option<WakeReplayStatus> {
        let core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        let pending = core.wake_replay_inflight.contains_key(variant)
            || core
                .deferred_wakes
                .iter()
                .any(|payload| payload.variant == variant);
        let error = core.wake_replay_errors.get(variant).cloned();
        (pending || error.is_some()).then(|| WakeReplayStatus {
            variant: variant.to_string(),
            pending,
            error,
        })
    }

    pub(super) fn interrupt_wakes_locked(core: &mut Core) -> WakeInterruption {
        let mut interruption = WakeInterruption::default();
        if let Some(payload) = core.active_wake.take() {
            if let Some(cancel) = &core.active_cancel {
                cancel.store(true, Ordering::SeqCst);
            }
            interruption.active = core.active_turn_id;
            core.deferred_wakes.push(payload);
        }
        let mut pending = Vec::with_capacity(core.pending.len());
        for mut entry in std::mem::take(&mut core.pending) {
            if let Some(payload) = entry.wake.take() {
                entry.cancel.store(true, Ordering::SeqCst);
                if let Some(tx) = entry.outcome.take() {
                    if tx.send(Ok(TurnOutcome::CancelledBeforeStart)).is_err() {
                        tracing::debug!(
                            turn_id = entry.turn_id,
                            "a cancelled turn's waiter had already gone"
                        );
                    }
                }
                interruption.queued.push(entry.turn_id);
                core.deferred_wakes.push(payload);
            } else {
                pending.push(entry);
            }
        }
        core.pending = pending;
        let mut held = Vec::with_capacity(core.held.len());
        for mut entry in std::mem::take(&mut core.held) {
            if let Some(payload) = entry.wake.take() {
                entry.cancel.store(true, Ordering::SeqCst);
                if let Some(tx) = entry.outcome.take() {
                    if tx.send(Ok(TurnOutcome::CancelledBeforeStart)).is_err() {
                        tracing::debug!(
                            turn_id = entry.turn_id,
                            "a cancelled turn's waiter had already gone"
                        );
                    }
                }
                interruption.queued.push(entry.turn_id);
                core.deferred_wakes.push(payload);
            } else {
                held.push(entry);
            }
        }
        core.held = held;
        if interruption.active.is_some() || !interruption.queued.is_empty() {
            core.wake_waiting_for_user = true;
        }
        interruption
    }

    pub(super) fn emit_wake_interruption(&self, interruption: WakeInterruption) {
        if interruption.active.is_some()
            && let Some(slot) = &self.agent_cancel_slot
            && let Some(signal) = slot.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        {
            signal.trigger();
        }
        if let Some(turn_id) = interruption.active {
            self.callbacks.turn_event(TurnEvent::Cancel {
                turn_id: Some(turn_id),
                target: Some(TurnCancelTarget::Active),
                reason: Some(TurnCancelReason::Aborted),
            });
        }
        for turn_id in interruption.queued {
            self.callbacks.turn_event(TurnEvent::Cancel {
                turn_id: Some(turn_id),
                target: Some(TurnCancelTarget::Queued),
                reason: Some(TurnCancelReason::Aborted),
            });
        }
    }

    pub fn drop_wakes(&self, variant: &str) -> bool {
        let mut interruption = WakeInterruption::default();
        let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        if core
            .active_wake
            .as_ref()
            .is_some_and(|payload| payload.variant == variant)
        {
            core.active_wake = None;
            if let Some(cancel) = &core.active_cancel {
                cancel.store(true, Ordering::SeqCst);
            }
            interruption.active = core.active_turn_id;
        }
        let mut pending = Vec::with_capacity(core.pending.len());
        for mut entry in std::mem::take(&mut core.pending) {
            if entry
                .wake
                .as_ref()
                .is_some_and(|payload| payload.variant == variant)
            {
                entry.cancel.store(true, Ordering::SeqCst);
                if let Some(tx) = entry.outcome.take() {
                    if tx.send(Ok(TurnOutcome::CancelledBeforeStart)).is_err() {
                        tracing::debug!(
                            turn_id = entry.turn_id,
                            "a cancelled turn's waiter had already gone"
                        );
                    }
                }
                interruption.queued.push(entry.turn_id);
            } else {
                pending.push(entry);
            }
        }
        core.pending = pending;
        let mut held = Vec::with_capacity(core.held.len());
        for mut entry in std::mem::take(&mut core.held) {
            if entry
                .wake
                .as_ref()
                .is_some_and(|payload| payload.variant == variant)
            {
                entry.cancel.store(true, Ordering::SeqCst);
                if let Some(tx) = entry.outcome.take() {
                    if tx.send(Ok(TurnOutcome::CancelledBeforeStart)).is_err() {
                        tracing::debug!(
                            turn_id = entry.turn_id,
                            "a cancelled turn's waiter had already gone"
                        );
                    }
                }
                interruption.queued.push(entry.turn_id);
            } else {
                held.push(entry);
            }
        }
        core.held = held;
        let deferred_before = core.deferred_wakes.len();
        core.deferred_wakes
            .retain(|payload| payload.variant != variant);
        let dropped = interruption.active.is_some()
            || !interruption.queued.is_empty()
            || core.deferred_wakes.len() != deferred_before;
        if dropped {
            core.wake_replay_errors.remove(variant);
            core.wake_waiting_for_user = false;
        }
        maybe_settle_locked(&mut core);
        drop(core);
        self.emit_wake_interruption(interruption);
        dropped
    }
}

pub(super) fn spawn_notification_waiter(mut receipt: TurnReceipt) {
    tokio::spawn(async move {
        let turn_id = receipt.turn_id;

        match receipt.outcome().await {
            Ok(TurnOutcome::Ran(_)) => {}
            Ok(TurnOutcome::CancelledBeforeStart) => {
                tracing::warn!(
                    turn_id,
                    "a notification turn was cancelled before it started"
                );
            }
            Err(error) => {
                tracing::warn!(
                    turn_id,
                    %error,
                    "a notification turn's waiter was dropped without an outcome"
                );
            }
        }
    });
}

fn enqueue_notification_locked(
    core: &mut Core,
    wakeup: &Notify,
    message: LLMMessage,
    origin: serde_json::Value,
    wake: Option<WakePayload>,
) -> TurnReceipt {
    let (outcome_tx, outcome_rx) = oneshot::channel();
    let turn_id = core.next_turn_id;
    core.next_turn_id += 1;
    let input = serde_json::json!([{
        "type": "text",
        "text": message.content.clone(),
    }]);
    core.pending.push(PendingTurn {
        turn_id,
        prompt_id: None,
        prompt: message,
        input,
        origin,
        cancel: Arc::new(AtomicBool::new(false)),
        outcome: Some(outcome_tx),
        wake,
    });
    wakeup.notify_one();
    TurnReceipt {
        turn_id,
        prompt_id: format!("wake-{turn_id}"),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        state: PromptSubmissionState::Running,
        steered: false,
        outcome: outcome_rx,
    }
}

pub(super) fn enqueue_replayed_wake_locked(
    core: &mut Core,
    wakeup: &Notify,
    payload: WakePayload,
) -> TurnReceipt {
    let origin = payload.origin();
    enqueue_notification_locked(core, wakeup, payload.message.clone(), origin, Some(payload))
}

pub(super) fn enqueue_task_notification_turn(
    ctx: &SessionContext,
    core: &mut Core,
    wakeup: &Notify,
) -> Option<TurnReceipt> {
    let runner = ctx.task_runner.as_ref()?;
    let session_id = ctx.session_id.as_deref();
    if runner.pending_notification_count(session_id) == 0 {
        return None;
    }
    let notifications = runner.take_pending_notifications(session_id);
    let first = notifications.first()?;
    let text = render_task_notifications(&notifications);
    let origin = serde_json::json!({
        "kind": "task",
        "taskId": &first.task_id,
        "status": first.status.as_str(),
        "notificationId": format!("{}-{}", first.task_id, first.ended_at),
    });
    Some(enqueue_notification_locked(
        core,
        wakeup,
        LLMMessage {
            role: kimi_core::types::Role::User,
            content: text,
            ..LLMMessage::default()
        },
        origin,
        None,
    ))
}

#[derive(Clone)]
pub struct EngineSessionWakeDriver {
    session: EngineSession,
    receipt_tasks: Arc<Mutex<HashMap<u64, tokio::task::JoinHandle<()>>>>,
}

impl EngineSessionWakeDriver {
    #[must_use]
    pub fn new(session: EngineSession) -> Self {
        Self {
            session,
            receipt_tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[must_use]
    pub fn pending_receipt_count(&self) -> usize {
        self.receipt_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

impl TowerWakeDriver for EngineSessionWakeDriver {
    fn enqueue(&self, _session_id: &str, payload: WakePayload) -> Result<(), String> {
        let mut receipt = self.session.enqueue_wake(payload)?;
        let turn_id = receipt.turn_id;
        let receipt_tasks = self.receipt_tasks.clone();

        let mut pending = self.receipt_tasks.lock().unwrap_or_else(|e| e.into_inner());
        let task = tokio::spawn(async move {
            if let Err(error) = receipt.outcome().await {
                tracing::warn!(
                    turn_id,
                    %error,
                    "an enqueued wake turn's waiter was dropped without an outcome"
                );
            }
            receipt_tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&turn_id);
        });
        pending.insert(turn_id, task);
        Ok(())
    }

    fn drop_wakes(&self, _session_id: &str, variant: &str) -> bool {
        self.session.drop_wakes(variant)
    }

    fn set_replay_handler(&self, _session_id: &str, variant: &str, handler: WakeReplayHandler) {
        self.session.set_wake_replay_handler(variant, handler);
    }

    fn wake_replay_status(&self, _session_id: &str, variant: &str) -> Option<WakeReplayStatus> {
        self.session.wake_replay_status(variant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_loop::types::LLMMessage;

    fn message(text: &str) -> LLMMessage {
        LLMMessage {
            role: kimi_core::types::Role::User,
            content: text.into(),
            ..LLMMessage::default()
        }
    }

    fn rich_item(key: &str) -> WakeItem {
        WakeItem {
            key: key.into(),
            source: "worker-a".into(),
            subject: "rendered by the caller".into(),
            message_id: Some("message-1".into()),
            file: Some("inbox/message-1.md".into()),
            sent_at: Some("2026-11-04T00:00:00Z".into()),
            mission_id: Some("mission-1".into()),
        }
    }

    #[test]
    fn wake_buffer_dedups_rich_items_and_requeues_a_dropped_batch() {
        let mut buffer = WakeBuffer::new("tower_inbox");
        let item = rich_item("id:message-1");
        assert!(buffer.push(item.clone()));
        assert!(!buffer.push(item.clone()), "a live key is deduplicated");

        let digest = WakeGateDigest {
            mission_id: "mission-1".into(),
            status: "blocked".into(),
            reasons: vec!["review-pending".into()],
            binding: Some("review-1".into()),
        };
        let payload = buffer
            .take(message("caller-rendered wake"), vec![digest.clone()])
            .expect("one item forms a batch");
        assert_eq!(payload.variant, "tower_inbox");
        assert_eq!(payload.items, vec![item.clone()]);
        assert_eq!(payload.gate_digests, vec![digest]);
        assert_eq!(payload.omitted, 0);

        buffer.requeue(payload);
        assert!(!buffer.push(item.clone()), "replay stays deduplicated");
        let replayed = buffer
            .take(message("rendered again"), Vec::new())
            .expect("the dropped batch is queued again");
        assert_eq!(replayed.items, vec![item]);
        assert!(replayed.gate_digests.is_empty());
    }

    #[test]
    fn wake_buffer_bounds_a_batch_and_counts_omitted_items() {
        let mut buffer = WakeBuffer::new("tower_inbox");
        for index in 0..=WAKE_BATCH_LIMIT {
            let mut item = rich_item(&format!("id:message-{index}"));
            item.message_id = Some(format!("message-{index}"));
            assert!(buffer.push(item));
        }
        assert_eq!(buffer.items().len(), WAKE_BATCH_LIMIT);
        assert_eq!(buffer.omitted(), 1);
        assert_eq!(buffer.items()[0].key, "id:message-1");

        let payload = buffer
            .take(message("bounded"), Vec::new())
            .expect("the bounded batch is available");
        assert_eq!(payload.items.len(), WAKE_BATCH_LIMIT);
        assert_eq!(payload.omitted, 1);
        assert!(!buffer.push(rich_item("id:message-0")));
    }
}
