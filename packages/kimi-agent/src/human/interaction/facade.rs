use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Weak};
use std::time::Duration;

use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::human::interaction::interaction::{
    INTERACTION_TAG_AGENT_ID, INTERACTION_TAG_SESSION_ID, Interaction, InteractionCancellation,
    InteractionCancellationReason, InteractionPendingChangedEvent, InteractionQuery,
    InteractionRequest, InteractionResolution, InteractionTagValue, InteractionTags,
};
use crate::human::interaction::machine::{
    InteractionActor, InteractionEmitted, InteractionRecord, InteractionSubscription,
    create_interaction_machine,
};
use crate::wire::record::WireAsyncError;

const RECENTLY_RESOLVED_TTL_MS: u64 = 60_000;
const RECENTLY_RESOLVED_MAX: usize = 256;

pub type InteractionAgentDispatch = Arc<dyn Fn(InteractionEmitted) + Send + Sync>;
pub type PendingListener = Arc<dyn Fn(InteractionPendingChangedEvent) + Send + Sync>;
pub type ResolveListener = Arc<dyn Fn(InteractionResolution) + Send + Sync>;

#[derive(Debug)]
struct InteractionError(String);

impl Display for InteractionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InteractionError {}

fn error(message: impl Into<String>) -> WireAsyncError {
    Box::new(InteractionError(message.into()))
}

struct Waiter {
    id: u64,
    sender: oneshot::Sender<Value>,
}

#[derive(Default)]
pub struct InteractionFacadeInput {
    pub now: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
}

struct FacadeInner {
    actor: Arc<InteractionActor>,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    waiters: Mutex<HashMap<String, Vec<Waiter>>>,
    recently_resolved: Mutex<IndexMap<String, u64>>,
    change_listeners: Mutex<IndexMap<u64, PendingListener>>,
    resolve_listeners: Mutex<IndexMap<u64, ResolveListener>>,
    agent_dispatchers: Mutex<HashMap<String, InteractionAgentDispatch>>,
    next_id: AtomicU64,
    next_listener_id: AtomicU64,
    next_waiter_id: AtomicU64,
    subscriptions: Mutex<Vec<InteractionSubscription>>,
}

pub struct InteractionFacade {
    inner: Arc<FacadeInner>,
}

fn matches(record: &InteractionRecord, query: &InteractionQuery) -> bool {
    if query.id.as_ref().is_some_and(|id| id != record.id()) {
        return false;
    }
    if query.kind.is_some_and(|kind| kind != record.interaction.kind) {
        return false;
    }
    if query.resolved.is_some_and(|resolved| resolved != record.resolved) {
        return false;
    }
    if let Some(tags) = &query.tags {
        for (key, value) in tags {
            if record.interaction.tags.get(key) != Some(value) {
                return false;
            }
        }
    }
    true
}

fn tag_string(value: Option<&InteractionTagValue>) -> Option<String> {
    match value {
        Some(InteractionTagValue::String(value)) => Some(value.clone()),
        Some(InteractionTagValue::Number(value)) => Some(value.to_string()),
        None => None,
    }
}

fn agent_key(session_id: &str, agent_id: &str) -> String {
    format!("{session_id}:{agent_id}")
}

impl FacadeInner {
    fn records(&self) -> IndexMap<String, InteractionRecord> {
        self.actor.get_snapshot().context.records
    }

    fn pending_ids(&self) -> Vec<String> {
        self.records()
            .values()
            .filter(|record| !record.resolved)
            .map(|record| record.id().to_string())
            .collect()
    }

    fn fire_pending_changed(&self) {
        let event = InteractionPendingChangedEvent {
            pending: self.pending_ids(),
        };
        for listener in self.change_listeners.lock().values() {
            listener(event.clone());
        }
    }

    fn settle_waiters(&self, id: &str, response: &Value) {
        let Some(entries) = self.waiters.lock().remove(id) else {
            return;
        };
        for waiter in entries {
            let _ = waiter.sender.send(response.clone());
        }
    }

    fn evict_resolved(&self, id: &str) {
        self.recently_resolved.lock().remove(id);
        self.actor.send(crate::human::interaction::machine::InteractionEvent::Evict {
            id: id.to_string(),
        });
    }

    fn remember_resolved(&self, id: &str) {
        let at = (self.now)();
        let expired = self
            .recently_resolved
            .lock()
            .iter()
            .filter(|(_, resolved_at)| at.saturating_sub(**resolved_at) > RECENTLY_RESOLVED_TTL_MS)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in expired {
            self.evict_resolved(&id);
        }
        while self.recently_resolved.lock().len() >= RECENTLY_RESOLVED_MAX {
            let oldest = self
                .recently_resolved
                .lock()
                .keys()
                .next()
                .cloned();
            let Some(oldest) = oldest else {
                break;
            };
            self.evict_resolved(&oldest);
        }
        self.recently_resolved.lock().insert(id.to_string(), at);
    }

    fn dispatch_to_agent(&self, tags: &InteractionTags, event: InteractionEmitted) {
        let (Some(agent_id), Some(session_id)) = (
            tags.get(INTERACTION_TAG_AGENT_ID).and_then(|value| match value {
                InteractionTagValue::String(value) => Some(value),
                _ => None,
            }),
            tags.get(INTERACTION_TAG_SESSION_ID).and_then(|value| match value {
                InteractionTagValue::String(value) => Some(value),
                _ => None,
            }),
        ) else {
            return;
        };
        if let Some(dispatch) = self
            .agent_dispatchers
            .lock()
            .get(&agent_key(session_id, agent_id))
            .cloned()
        {
            dispatch(event);
        }
    }
}

impl InteractionFacade {
    pub async fn request(&self, req: InteractionRequest) -> Result<Value, WireAsyncError> {
        let interaction = self.enqueue(req)?;
        self.wait(&interaction.id, None).await
    }

    pub fn enqueue(&self, req: InteractionRequest) -> Result<Interaction, WireAsyncError> {
        let id = match req.id {
            Some(id) => id,
            None => {
                let next = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
                let agent_id = req
                    .tags
                    .as_ref()
                    .and_then(|tags| tags.get(INTERACTION_TAG_AGENT_ID));
                match agent_id {
                    Some(agent_id) => format!(
                        "{}:interaction-{next}",
                        tag_string(Some(agent_id)).unwrap_or_default()
                    ),
                    None => format!("interaction-{next}"),
                }
            }
        };
        if self
            .inner
            .records()
            .get(&id)
            .is_some_and(|existing| !existing.resolved)
        {
            return Err(error(format!("Interaction \"{id}\" is already pending")));
        }
        let interaction = Interaction {
            id,
            kind: req.kind,
            payload: req.payload,
            tags: req.tags.unwrap_or_default(),
            created_at: (self.inner.now)(),
        };
        self.inner.actor.send(
            crate::human::interaction::machine::InteractionEvent::Request {
                record: InteractionRecord {
                    interaction: interaction.clone(),
                    resolved: false,
                    response: None,
                },
            },
        );
        self.inner.fire_pending_changed();
        Ok(interaction)
    }

    pub fn respond(&self, id: &str, response: Value) -> bool {
        let Some(record) = self.inner.records().get(id).cloned() else {
            return false;
        };
        if record.resolved {
            return false;
        }
        self.inner.actor.send(
            crate::human::interaction::machine::InteractionEvent::Resolve {
                id: id.to_string(),
                response,
            },
        );
        self.inner.fire_pending_changed();
        true
    }

    pub fn find_all(&self, query: InteractionQuery) -> Vec<Interaction> {
        self.inner
            .records()
            .values()
            .filter(|record| matches(record, &query))
            .map(|record| record.interaction.clone())
            .collect()
    }

    pub fn find_one(&self, query: InteractionQuery) -> Option<Interaction> {
        self.inner
            .records()
            .values()
            .find(|record| matches(record, &query))
            .map(|record| record.interaction.clone())
    }

    pub async fn wait(
        &self,
        id: &str,
        timeout_ms: Option<u64>,
    ) -> Result<Value, WireAsyncError> {
        let Some(record) = self.inner.records().get(id).cloned() else {
            return Err(error(format!("Interaction \"{id}\" does not exist")));
        };
        if record.resolved {
            return Ok(record.response.unwrap_or(Value::Null));
        }
        let waiter_id = self.inner.next_waiter_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.inner.waiters.lock().entry(id.to_string()).or_default().push(Waiter {
            id: waiter_id,
            sender,
        });
        tokio::pin!(receiver);
        if let Some(timeout_ms) = timeout_ms {
            tokio::select! {
                result = &mut receiver => {
                    result.map_err(|_| error("interaction waiter was dropped"))
                }
                _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
                    let mut waiters = self.inner.waiters.lock();
                    if let Some(entries) = waiters.get_mut(id) {
                        entries.retain(|waiter| waiter.id != waiter_id);
                        if entries.is_empty() {
                            waiters.remove(id);
                        }
                    }
                    Err(error(format!("Timed out waiting for interaction \"{id}\"")))
                }
            }
        } else {
            receiver
                .await
                .map_err(|_| error("interaction waiter was dropped"))
        }
    }

    pub fn on_did_change_pending(
        &self,
        listener: PendingListener,
    ) -> Arc<dyn Fn() + Send + Sync> {
        let id = self.inner.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.inner.change_listeners.lock().insert(id, listener);
        let inner = Arc::downgrade(&self.inner);
        Arc::new(move || {
            if let Some(inner) = Weak::upgrade(&inner) {
                inner.change_listeners.lock().remove(&id);
            }
        })
    }

    pub fn on_did_resolve(
        &self,
        listener: ResolveListener,
    ) -> Arc<dyn Fn() + Send + Sync> {
        let id = self.inner.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.inner.resolve_listeners.lock().insert(id, listener);
        let inner = Arc::downgrade(&self.inner);
        Arc::new(move || {
            if let Some(inner) = Weak::upgrade(&inner) {
                inner.resolve_listeners.lock().remove(&id);
            }
        })
    }

    pub fn attach_agent(
        &self,
        agent_id: &str,
        session_id: &str,
        dispatch: InteractionAgentDispatch,
    ) {
        self.inner
            .agent_dispatchers
            .lock()
            .insert(agent_key(session_id, agent_id), dispatch);
    }

    pub fn detach_agent(&self, agent_id: &str, session_id: &str) {
        self.inner
            .agent_dispatchers
            .lock()
            .remove(&agent_key(session_id, agent_id));
    }

    pub fn purge_session(&self, session_id: &str) {
        let purged = self
            .inner
            .records()
            .values()
            .filter(|record| {
                record.interaction.tags.get(INTERACTION_TAG_SESSION_ID)
                    == Some(&InteractionTagValue::String(session_id.to_string()))
            })
            .cloned()
            .collect::<Vec<_>>();
        for record in &purged {
            if !record.resolved {
                self.respond(
                    record.id(),
                    serde_json::to_value(InteractionCancellation {
                        cancelled: true,
                        reason: InteractionCancellationReason::AgentClosed,
                    })
                    .unwrap(),
                );
            }
        }
        self.inner.actor.send(
            crate::human::interaction::machine::InteractionEvent::Purge {
                session_id: session_id.to_string(),
            },
        );
        for record in &purged {
            self.inner.recently_resolved.lock().remove(record.id());
        }
        self.inner
            .agent_dispatchers
            .lock()
            .retain(|key, _| !key.starts_with(&format!("{session_id}:")));
    }

    pub fn stop(&self) {
        for record in self.inner.records().values() {
            if !record.resolved {
                self.inner.actor.send(
                    crate::human::interaction::machine::InteractionEvent::Resolve {
                        id: record.id().to_string(),
                        response: json!({
                            "cancelled": true,
                            "reason": "agent_closed",
                        }),
                    },
                );
            }
        }
        for subscription in self.inner.subscriptions.lock().drain(..) {
            subscription.unsubscribe();
        }
        self.inner.change_listeners.lock().clear();
        self.inner.resolve_listeners.lock().clear();
        self.inner.agent_dispatchers.lock().clear();
    }
}

pub fn create_interaction_facade(
    actor: Arc<InteractionActor>,
    input: Option<InteractionFacadeInput>,
) -> Arc<InteractionFacade> {
    let inner = Arc::new(FacadeInner {
        actor,
        now: input
            .and_then(|input| input.now)
            .unwrap_or_else(|| Arc::new(|| chrono::Utc::now().timestamp_millis() as u64)),
        waiters: Mutex::new(HashMap::new()),
        recently_resolved: Mutex::new(IndexMap::new()),
        change_listeners: Mutex::new(IndexMap::new()),
        resolve_listeners: Mutex::new(IndexMap::new()),
        agent_dispatchers: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(0),
        next_listener_id: AtomicU64::new(0),
        next_waiter_id: AtomicU64::new(0),
        subscriptions: Mutex::new(Vec::new()),
    });
    let requested = inner.actor.on(
        "interaction.requested",
        Arc::new({
            let inner = inner.clone();
            move |event| {
                let InteractionEmitted::Requested { record } = event else {
                    return;
                };
                inner.dispatch_to_agent(&record.interaction.tags, InteractionEmitted::Requested { record });
            }
        }),
    );
    let resolved = inner.actor.on(
        "interaction.resolved",
        Arc::new({
            let inner = inner.clone();
            move |event| {
                let InteractionEmitted::Resolved { id, response, record } = event else {
                    return;
                };
                inner.settle_waiters(&id, &response);
                inner.remember_resolved(&id);
                inner.dispatch_to_agent(
                    &record.interaction.tags,
                    InteractionEmitted::Resolved {
                        id: id.clone(),
                        response: response.clone(),
                        record,
                    },
                );
                let resolution = InteractionResolution { id, response };
                for listener in inner.resolve_listeners.lock().values() {
                    listener(resolution.clone());
                }
            }
        }),
    );
    inner.subscriptions.lock().extend([requested, resolved]);
    Arc::new(InteractionFacade { inner })
}

fn create_default_interaction_facade() -> Arc<InteractionFacade> {
    let actor = create_interaction_machine().start();
    create_interaction_facade(actor, None)
}

pub static INTERACTIONS: LazyLock<Arc<InteractionFacade>> =
    LazyLock::new(create_default_interaction_facade);

pub fn interactions() -> Arc<InteractionFacade> {
    INTERACTIONS.clone()
}
