use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use futures_util::future::join_all;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::human::agent::slices::agent_slices;
use crate::human::eventStore::eventStore::{EventStoreImpl, EventStoreOptions, create_event_store};
use crate::human::eventStore::events::ExternalEvent;
use crate::human::eventStore::journal::{StoreJournal, journal_from_branch};
use crate::human::session::events::{
    SESSION_LOG_BRANCH, agent_closed, agent_opened, agent_switched,
};
use crate::human::session::slices::session_slices;
use crate::human::store::backend::backend::StoreBackend;
use crate::human::store::tree::Tree;
use crate::human::store::types::{BranchRef, StoreError};
use crate::wire::record::WireAsyncError;

pub type SessionStore = Arc<EventStoreImpl>;
pub type AgentEventStore = Arc<EventStoreImpl>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UndoErrorReason {
    UnknownAgent,
    InvalidCount,
    Insufficient,
}

#[derive(Debug)]
pub struct UndoError {
    pub reason: UndoErrorReason,
    pub message: String,
}

impl UndoError {
    pub fn new(reason: UndoErrorReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

impl fmt::Display for UndoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for UndoError {}

#[derive(Clone, Debug)]
pub struct BranchIdResult {
    pub branch_id: String,
}

pub struct SwitchBranchOptions {
    pub reason: String,
    pub stats: Option<HashMap<String, f64>>,
    pub seed: Vec<ExternalEvent>,
}

pub fn is_valid_undo_count(count: f64) -> bool {
    count.is_finite()
        && count.fract() == 0.0
        && count > 0.0
        && count <= 9_007_199_254_740_991.0
}

fn js_number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value == f64::INFINITY {
        return "Infinity".to_string();
    }
    if value == f64::NEG_INFINITY {
        return "-Infinity".to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }
    let magnitude = value.abs();
    if !(1e-6..1e21).contains(&magnitude) {
        let scientific = format!("{value:e}");
        if let Some((mantissa, exponent)) = scientific.split_once('e') {
            let exponent = exponent.parse::<i32>().unwrap_or(0);
            return format!("{mantissa}e{exponent:+}");
        }
    }
    format!("{value}")
}

pub fn fresh_branch_name(tree: &Tree, agent_id: &str) -> String {
    if !tree.has(agent_id) {
        return agent_id.to_string();
    }
    let mut n = 2;
    while tree.has(&format!("{agent_id}~{n}")) {
        n += 1;
    }
    format!("{agent_id}~{n}")
}

fn undo_fork_ref(tree: &Tree, start: &BranchRef) -> Result<Option<BranchRef>, StoreError> {
    if start.seq > 0 {
        return Ok(Some(BranchRef {
            branch: start.branch.clone(),
            seq: start.seq - 1,
        }));
    }
    let header = &tree.open_branch(&start.branch)?.header;
    Ok(match (&header.parent_branch, header.parent_seq) {
        (Some(branch), Some(seq)) => Some(BranchRef {
            branch: branch.clone(),
            seq,
        }),
        _ => None,
    })
}

fn external(event: impl Serialize) -> Result<ExternalEvent, StoreError> {
    serde_json::to_value(event)
        .map_err(|error| StoreError::new("schema", error.to_string()))?
        .as_object()
        .cloned()
        .ok_or_else(|| StoreError::new("schema", "event is not an object"))
}

pub struct SessionStores {
    pub tree: Arc<Tree>,
    pub backend: StoreBackend,
    agents: Mutex<HashMap<String, AgentEventStore>>,
    session_store: tokio::sync::Mutex<Option<SessionStore>>,
}

impl SessionStores {
    pub fn new(tree: Arc<Tree>, backend: StoreBackend) -> Self {
        Self {
            tree,
            backend,
            agents: Mutex::new(HashMap::new()),
            session_store: tokio::sync::Mutex::new(None),
        }
    }

    pub fn get(&self, agent_id: &str) -> Option<AgentEventStore> {
        self.agents.lock().get(agent_id).cloned()
    }

    pub async fn session(&self) -> Result<SessionStore, StoreError> {
        let mut current = self.session_store.lock().await;
        if let Some(store) = current.as_ref() {
            return Ok(store.clone());
        }
        let branch = if self.tree.has(SESSION_LOG_BRANCH) {
            self.tree.open_branch(SESSION_LOG_BRANCH)?
        } else {
            self.tree.create_branch(SESSION_LOG_BRANCH, None)?
        };
        let store = create_event_store(EventStoreOptions {
            journal: journal_from_branch(branch, self.tree.clone()),
            slices: session_slices(),
            drain_limit: None,
            on_error: None,
            mutable_replay: None,
        })
        .await?;
        *current = Some(store.clone());
        Ok(store)
    }

    pub async fn open(
        &self,
        agent_id: &str,
        from: Option<BranchRef>,
    ) -> Result<AgentEventStore, StoreError> {
        if let Some(existing) = self.get(agent_id) {
            return Ok(existing);
        }
        let existed = self.tree.has(agent_id);
        let branch = if existed {
            self.tree.open_branch(agent_id)?
        } else {
            self.tree.create_branch(agent_id, from)?
        };
        let engine = create_event_store(EventStoreOptions {
            journal: journal_from_branch(branch.clone(), self.tree.clone()),
            slices: agent_slices(),
            drain_limit: None,
            on_error: None,
            mutable_replay: None,
        })
        .await?;
        self.agents.lock().insert(agent_id.to_string(), engine.clone());
        if !existed {
            self.session()
                .await?
                .dispatch(vec![external(agent_opened(agent_id, &branch.name))?])
                .await?;
        }
        Ok(engine)
    }

    pub async fn fork(
        &self,
        source_id: &str,
        agent_id: &str,
    ) -> Result<AgentEventStore, StoreError> {
        let Some(source) = self.get(source_id) else {
            return Err(StoreError::new(
                "unknown-agent",
                format!("unknown agent '{source_id}'"),
            ));
        };
        let source_branch = self.tree.open_branch(&source.ref_().branch)?;
        let from = source_branch.head().map(|head| BranchRef {
            branch: source_branch.name.clone(),
            seq: head as u64,
        });
        self.open(agent_id, from).await
    }

    pub async fn close(&self, agent_id: &str) -> Result<(), StoreError> {
        let store = self.agents.lock().remove(agent_id);
        let Some(store) = store else {
            return Ok(());
        };
        store.close().await?;
        self.session()
            .await?
            .dispatch(vec![external(agent_closed(agent_id))?])
            .await?;
        Ok(())
    }

    pub async fn undo(
        &self,
        agent_id: &str,
        turns: f64,
    ) -> Result<BranchIdResult, WireAsyncError> {
        let Some(store) = self.get(agent_id) else {
            return Err(Box::new(UndoError::new(
                UndoErrorReason::UnknownAgent,
                format!("unknown agent: '{agent_id}'"),
            )));
        };
        if !is_valid_undo_count(turns) {
            return Err(Box::new(UndoError::new(
                UndoErrorReason::InvalidCount,
                format!("invalid undo count: {}", js_number_to_string(turns)),
            )));
        }
        let count = turns as usize;
        let index = store.slice("turnIndex").unwrap_or(Value::Null);
        let turn_list = index
            .get("turns")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let Some(cut) = turn_list.len().checked_sub(count).and_then(|index| turn_list.get(index)) else {
            return Err(Box::new(UndoError::new(
                UndoErrorReason::Insufficient,
                format!("cannot undo {turns} turn(s): not enough turns"),
            )));
        };
        let start = cut.get("start").cloned().unwrap_or(Value::Null);
        let start = BranchRef {
            branch: start
                .get("branch")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            seq: start.get("seq").and_then(Value::as_u64).unwrap_or(0),
        };
        let Some(from) = undo_fork_ref(&self.tree, &start)? else {
            return Err(Box::new(UndoError::new(
                UndoErrorReason::Insufficient,
                format!("cannot undo {turns} turn(s): no earlier history"),
            )));
        };
        let branch_id = fresh_branch_name(&self.tree, agent_id);
        let branch = self.tree.create_branch(&branch_id, Some(from))?;
        store
            .reset(journal_from_branch(branch, self.tree.clone()))
            .await?;
        self.session()
            .await?
            .dispatch(vec![external(agent_switched(
                agent_id,
                &branch_id,
                Some("undo".to_string()),
                None,
            ))?])
            .await?;
        Ok(BranchIdResult { branch_id })
    }

    pub async fn switch_branch(
        &self,
        agent_id: &str,
        options: SwitchBranchOptions,
    ) -> Result<BranchIdResult, StoreError> {
        let Some(store) = self.get(agent_id) else {
            return Err(StoreError::new(
                "unknown-agent",
                format!("unknown agent '{agent_id}'"),
            ));
        };
        let branch_id = fresh_branch_name(&self.tree, agent_id);
        let branch = self.tree.create_branch(&branch_id, None)?;
        let journal = journal_from_branch(branch, self.tree.clone());
        let seed_store = create_event_store(EventStoreOptions {
            journal: journal.clone(),
            slices: agent_slices(),
            drain_limit: None,
            on_error: None,
            mutable_replay: None,
        })
        .await?;
        let seeded = async {
            seed_store.dispatch(options.seed).await?;
            seed_store.flush().await
        }
        .await;
        let closed = seed_store.close().await;
        if let Err(error) = closed {
            return Err(error);
        }
        seeded?;
        store.reset(journal).await?;
        self.session()
            .await?
            .dispatch(vec![external(agent_switched(
                agent_id,
                &branch_id,
                Some(options.reason),
                options.stats,
            ))?])
            .await?;
        Ok(BranchIdResult { branch_id })
    }

    pub async fn flush(&self) -> Result<(), StoreError> {
        let agents = self.agents.lock().values().cloned().collect::<Vec<_>>();
        let results = join_all(agents.into_iter().map(|store| async move {
            store.flush().await
        }))
        .await;
        for result in results {
            result?;
        }
        if let Some(session) = self.session_store.lock().await.clone() {
            session.flush().await?;
        }
        Ok(())
    }

    pub async fn dispose(&self) -> Result<(), StoreError> {
        let agents = self.agents.lock().values().cloned().collect::<Vec<_>>();
        let results = join_all(agents.into_iter().map(|store| async move {
            store.close().await
        }))
        .await;
        for result in results {
            result?;
        }
        self.agents.lock().clear();
        if let Some(session) = self.session_store.lock().await.take() {
            session.close().await?;
        }
        Ok(())
    }
}
