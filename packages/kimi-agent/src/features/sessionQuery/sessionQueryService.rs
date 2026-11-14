use std::path::PathBuf;
use std::sync::Arc;

use kimi_core::primitives::errors::Error2;
use serde_json::Value;

use super::cursor::SessionSearchCursor;
use super::errors::{SESSION_QUERY_INDEX_UNAVAILABLE, SESSION_QUERY_SESSION_NOT_FOUND};
use super::eventText::wire_record_text;
use super::events::{
    SessionEventRecord, SessionEventSearchDocument, SessionEventSearchPage,
    SessionEventSearchRequest, SessionEventResultFilter, SessionSearchHit, SessionSearchPage,
    SessionSearchRequest,
};
use super::filters::filter_session_results;
use super::lineage::trace_lineage;
use super::search::{filter_session_events, search_event_documents};
use super::types::{
    SessionLineageTrace, SessionRecord, SessionResultFilter,
};
use crate::session::sqlite_store::{SessionSummary, SqliteSessionStore};

pub const SESSION_QUERY_SERVICE_ID: &str = "sessionQueryService";
pub const WIRE_PAGE: usize = 512;
pub const DEFAULT_SESSION_SEARCH_LIMIT: usize = 10;
pub const MAX_SESSION_SEARCH_LIMIT: usize = 100;

pub struct SessionQueryService {
    store: Option<Arc<SqliteSessionStore>>,
    session_id: Option<String>,
    _root: PathBuf,
}

impl SessionQueryService {
    pub fn new(
        store: Option<Arc<SqliteSessionStore>>,
        session_id: Option<String>,
        root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            store,
            session_id,
            _root: root.into(),
        }
    }

    pub fn list_sessions(
        &self,
        filters: &[SessionResultFilter],
    ) -> Result<Vec<SessionRecord>, Error2> {
        let records = self.list_corpus()?;
        filter_session_results(&records, filters)
    }

    pub fn get_session(&self, session_id: &str) -> Result<SessionRecord, Error2> {
        let records = self.list_corpus()?;
        records
            .into_iter()
            .find(|record| record.id == session_id)
            .ok_or_else(|| session_not_found(session_id))
    }

    pub fn trace_session_lineage(
        &self,
        session_id: &str,
    ) -> Result<SessionLineageTrace, Error2> {
        let records = self.list_corpus()?;
        if !records.iter().any(|record| record.id == session_id) {
            return Err(session_not_found(session_id));
        }
        trace_lineage(&records, session_id).map_err(|error| {
            Error2::new(SESSION_QUERY_SESSION_NOT_FOUND, error)
        })
    }

    pub fn filter_events(
        &self,
        session_id: &str,
        filters: &[SessionEventResultFilter],
    ) -> Result<Vec<SessionEventSearchDocument>, Error2> {
        let record = self.get_session(session_id)?;
        let documents = self.event_documents_of(&record)?;
        filter_session_events(&documents, filters)
    }

    pub fn search_events(
        &self,
        request: SessionEventSearchRequest,
    ) -> Result<SessionEventSearchPage, Error2> {
        let record = self.get_session(&request.session_id)?;
        let documents = self.event_documents_of(&record)?;
        let filtered = filter_session_events(&documents, &request.filters)?;
        let page = search_event_documents(
            &filtered,
            &request.query,
            request.limit,
            request.cursor,
        )?;
        Ok(SessionEventSearchPage {
            session_id: request.session_id,
            items: page.items,
            next_cursor: page.next_cursor,
        })
    }

    pub fn search_sessions(
        &self,
        request: SessionSearchRequest,
    ) -> Result<SessionSearchPage<SessionSearchHit>, Error2> {
        let records = self.list_sessions(&request.session_filters)?;
        let candidates: Vec<SessionRecord> = if request.session_filters.is_empty() {
            records.into_iter().filter(|record| record.live).collect()
        } else {
            records
        };
        let mut hits: Vec<SessionSearchHit> = Vec::new();
        for record in candidates {
            let events = self.event_documents_of(&record)?;
            let filtered = filter_session_events(&events, &request.event_filters)?;
            let page = search_event_documents(&filtered, &request.query, Some(1), None)?;
            if let Some(best_match) = page.items.into_iter().next() {
                hits.push(SessionSearchHit {
                    session: record,
                    best_match,
                });
            }
        }
        hits.sort_by(|left, right| right.best_match.record.time.cmp(&left.best_match.record.time));
        let limit = request
            .limit
            .unwrap_or(DEFAULT_SESSION_SEARCH_LIMIT)
            .clamp(1, MAX_SESSION_SEARCH_LIMIT);
        let offset = request.cursor.map(|cursor| cursor.offset).unwrap_or(0);
        let items = hits.iter().skip(offset).take(limit).cloned().collect();
        let next_cursor =
            (offset + limit < hits.len()).then(|| SessionSearchCursor::new(offset + limit));
        Ok(SessionSearchPage { items, next_cursor })
    }

    fn event_documents_of(
        &self,
        record: &SessionRecord,
    ) -> Result<Vec<SessionEventSearchDocument>, Error2> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| no_store_error())?;
        let mut documents: Vec<SessionEventSearchDocument> = Vec::new();
        let mut cursor = 0u64;
        loop {
            let page = store
                .search_wire_events(&record.id, cursor, WIRE_PAGE)
                .map_err(|error| index_error(error.to_string()))?;
            if page.is_empty() {
                break;
            }
            for wire in &page {
                cursor = wire.seq;
                if wire.event_type == "metadata" {
                    continue;
                }
                let time = wire
                    .payload
                    .get("time")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                documents.push(SessionEventSearchDocument {
                    record: SessionEventRecord {
                        session_id: record.id.clone(),
                        seq: documents.len() as u64,
                        r#type: wire.event_type.clone(),
                        time,
                    },
                    text: wire_record_text(&wire.payload),
                });
            }
            if page.len() < WIRE_PAGE {
                break;
            }
        }
        Ok(documents)
    }

    fn list_corpus(&self) -> Result<Vec<SessionRecord>, Error2> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| no_store_error())?;
        let current_session_id = self
            .session_id
            .as_deref()
            .ok_or_else(|| no_session_error())?;
        let summaries = store
            .list_sessions()
            .map_err(|error| index_error(error.to_string()))?;
        let mut records: Vec<SessionRecord> = summaries
            .into_iter()
            .map(|summary| to_session_record(&summary, summary.session_id == current_session_id))
            .collect();
        records.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(records)
    }
}

fn to_session_record(summary: &SessionSummary, live: bool) -> SessionRecord {
    SessionRecord {
        id: summary.session_id.clone(),
        workspace_id: summary.workspace_id.clone().unwrap_or_default(),
        cwd: summary.cwd.clone(),
        created_at: summary.created_at,
        parent_session_id: summary.parent_session_id.clone(),
        live,
        persisted: true,
    }
}

fn session_not_found(session_id: &str) -> Error2 {
    Error2::new(
        SESSION_QUERY_SESSION_NOT_FOUND,
        format!("session \"{session_id}\" not found"),
    )
}

fn no_store_error() -> Error2 {
    Error2::new(
        SESSION_QUERY_INDEX_UNAVAILABLE,
        "this turn has no session store to read; session history is unavailable until one is attached.",
    )
}

fn no_session_error() -> Error2 {
    Error2::new(
        SESSION_QUERY_INDEX_UNAVAILABLE,
        "this turn has no session id to read history for.",
    )
}

fn index_error(message: String) -> Error2 {
    Error2::new(SESSION_QUERY_INDEX_UNAVAILABLE, message)
}
