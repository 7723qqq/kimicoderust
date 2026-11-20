use crate::_base::di::instantiation::ServicesAccessorExt;
use crate::persistence::interface::atomic_document_store::I_ATOMIC_DOCUMENT_STORE;
use crate::app::bootstrap::bootstrap::I_BOOTSTRAP_SERVICE;
use crate::app::event::event::I_EVENT_SERVICE;
use crate::app::session_index::session_index::I_SESSION_INDEX;
use crate::app::session_index::session_index::I_SESSION_INDEX_MIRROR;
use crate::persistence::interface::session_lock::I_SESSION_LOCK_SERVICE;
use crate::app::session_manager::session_manager::I_SESSION_MANAGER;
use std::sync::Arc;

use futures_util::{StreamExt, stream};
use futures_util::future::BoxFuture;

use crate::_base::di::instantiation::ServicesAccessor;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::event::event::IEventService;
use crate::app::session_index::session_index::{ISessionIndex, ISessionIndexMirror};
use crate::app::session_index::session_index_source::build_session_summary;
use crate::app::session_manager::session_lookup::get_live_session_by_id;
use crate::app::session_manager::session_manager::ISessionManager;
use crate::persistence::interface::atomic_document_store::IAtomicDocumentStore;
use crate::persistence::interface::session_lock::ISessionLockService;
use crate::session::session_metadata::session_metadata::SessionMeta;
use crate::session::session_metadata::session_metadata_service::{
    encode_session_meta, normalize_session_meta,
};
use crate::wire::record::WireAsyncError;

use super::internal::addressing::{
    legacy_session_meta_scope_of, session_scope_of, workspace_persistence_scope,
};
use super::session_lifecycle_events::{SessionArchived, SessionRestored};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColdSessionArchiveOutcome {
    Updated,
    NotFound,
}

impl ColdSessionArchiveOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Updated => "updated",
            Self::NotFound => "not_found",
        }
    }
}

pub async fn set_cold_session_archived(
    accessor: Arc<dyn ServicesAccessor>,
    session_id: String,
    archived: bool,
) -> Result<ColdSessionArchiveOutcome, WireAsyncError> {
    let summary = accessor.get(&I_SESSION_INDEX).get(&session_id).await?;
    let Some(summary) = summary else {
        return Ok(ColdSessionArchiveOutcome::NotFound);
    };
    let lease = accessor.get(&I_SESSION_LOCK_SERVICE).lease(&session_id)?;
    let result = async {
        let docs = accessor.get(&I_ATOMIC_DOCUMENT_STORE);
        let meta_scope = session_scope_of(
            &workspace_persistence_scope(
                &accessor.get(&I_BOOTSTRAP_SERVICE).scope("sessions"),
                &summary.workspace_id,
            ),
            &session_id,
        );
        let mut raw = docs.get(&meta_scope, "state.json").await?;
        let mut legacy_meta_scope = None;
        if raw.is_none() {
            let legacy = legacy_session_meta_scope_of(&meta_scope);
            raw = docs.get(&legacy, "state.json").await?;
            legacy_meta_scope = Some(legacy);
        }
        let Some(raw) = raw else {
            return Ok(ColdSessionArchiveOutcome::NotFound);
        };
        let persisted = normalize_session_meta(raw, &session_id);
        let archived_at = archived.then(|| chrono::Utc::now().timestamp_millis());
        let next_meta = SessionMeta {
            archived,
            archived_at,
            ..persisted.clone()
        };
        docs.set(&meta_scope, "state.json", serde_json::to_value(encode_session_meta(&next_meta))?)
            .await?;
        if let Some(legacy_meta_scope) = legacy_meta_scope {
            docs.delete(&legacy_meta_scope, "state.json").await?;
        }
        accessor.get(&I_SESSION_INDEX_MIRROR).record(build_session_summary(
            crate::app::session_index::session_index_source::SessionSummaryInput {
                id: session_id.clone(),
                workspace_id: summary.workspace_id.clone(),
                cwd: next_meta.cwd.clone().or(summary.cwd.clone()),
                environment_id: next_meta.environment_id.clone(),
                environment_cwd: next_meta.environment_cwd.clone(),
                title: next_meta.title.clone(),
                last_prompt: next_meta.last_prompt.clone(),
                created_at: next_meta.created_at,
                updated_at: next_meta.updated_at,
                archived,
                archived_at,
                custom: next_meta.custom.clone(),
                last_turn_reason: next_meta.last_turn_reason.clone(),
            },
        ));
        if archived {
            accessor.get(&I_EVENT_SERVICE).publish(SessionArchived::new(
                super::session_lifecycle_events::SessionArchivedPayload {
                    session_id: session_id.clone(),
                    workspace_id: summary.workspace_id.clone(),
                },
            ).to_event2());
        } else if persisted.archived {
            accessor.get(&I_EVENT_SERVICE).publish(SessionRestored::new(
                super::session_lifecycle_events::SessionRestoredPayload {
                    session_id: session_id.clone(),
                    workspace_id: summary.workspace_id.clone(),
                },
            ).to_event2());
        }
        Ok(ColdSessionArchiveOutcome::Updated)
    }
    .await;
    lease.release();
    result
}

pub async fn set_session_archived(
    accessor: Arc<dyn ServicesAccessor>,
    session_id: String,
    archived: bool,
) -> Result<ColdSessionArchiveOutcome, WireAsyncError> {
    let manager = accessor.get(&I_SESSION_MANAGER);
    let _ = manager.when_resume_settled(&session_id).await;
    let accessor_for_work = accessor.clone();
    let session_id_for_work = session_id.clone();
    let output = manager
        .with_lifecycle_serialization(
            &session_id,
            Box::new(move |unguarded: std::sync::Arc<
                dyn crate::app::session_manager::session_manager::UnguardedSessionLifecycle,
            >| {
            Box::pin(async move {
            let outcome = if get_live_session_by_id(accessor_for_work.as_ref(), &session_id_for_work).is_some() {
                if archived {
                    unguarded.archive().await?;
                } else {
                    unguarded.restore(None).await?;
                }
                ColdSessionArchiveOutcome::Updated
            } else {
                set_cold_session_archived(accessor_for_work, session_id_for_work, archived).await?
            };
            Ok(Box::new(outcome) as Box<dyn std::any::Any + Send>)
            }) as BoxFuture<'static, Result<Box<dyn std::any::Any + Send>, WireAsyncError>>
        }))
        .await?;
    Ok(*output
        .downcast::<ColdSessionArchiveOutcome>()
        .expect("cold session archive outcome"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionArchiveBatchFailureReason {
    NotFound,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionArchiveBatchItemOutcome {
    Ok { id: String },
    Error {
        id: String,
        reason: SessionArchiveBatchFailureReason,
        message: String,
    },
}

pub async fn set_session_archived_batch(
    accessor: Arc<dyn ServicesAccessor>,
    ids: &[String],
    archived: bool,
) -> Result<Vec<SessionArchiveBatchItemOutcome>, WireAsyncError> {
    let outcomes = stream::iter(ids.iter().map(|id| {
        let accessor = accessor.clone();
        let id = id.clone();
        async move {
        match set_session_archived(accessor, id.clone(), archived).await {
            Ok(ColdSessionArchiveOutcome::Updated) => SessionArchiveBatchItemOutcome::Ok {
                id: id.clone(),
            },
            Ok(ColdSessionArchiveOutcome::NotFound) => SessionArchiveBatchItemOutcome::Error {
                id: id.clone(),
                reason: SessionArchiveBatchFailureReason::NotFound,
                message: format!("session {id} does not exist"),
            },
            Err(error) => SessionArchiveBatchItemOutcome::Error {
                id: id.clone(),
                reason: SessionArchiveBatchFailureReason::Error,
                message: error.to_string(),
            },
        }
        }
    }))
    .buffered(8)
    .collect::<Vec<_>>()
    .await;
    Ok(outcomes)
}
