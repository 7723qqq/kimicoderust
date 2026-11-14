use std::sync::Arc;

use rusqlite::{OptionalExtension, params};
use serde_json::json;

use crate::agent::prompt::prompt_metadata_text::title_from_prompt_metadata_text;
use crate::app::event::event::IEventService;
use crate::session::sqlite_store::{SESSION_META_VERSION, SqliteSessionStore};
use crate::wire::record::WireAsyncError;

use super::session_meta_events::{SessionMetaUpdated, SessionMetaUpdatedPayload};
use super::session_metadata::ISessionMetadata;

pub const DEFAULT_SESSION_TITLE: &str = "New Session";

pub fn is_untitled(title: Option<&str>) -> bool {
    title.is_none_or(|title| title.trim().is_empty() || title == DEFAULT_SESSION_TITLE)
}

pub struct PromptMetadataUpdateTarget {
    pub metadata: Arc<dyn ISessionMetadata>,
    pub event_service: Arc<dyn IEventService>,
    pub session_id: String,
}

pub async fn apply_prompt_metadata_update(
    target: &PromptMetadataUpdateTarget,
    text: Option<&str>,
) -> Result<(), WireAsyncError> {
    let Some(text) = text else {
        return Ok(());
    };
    let title = title_from_prompt_metadata_text(text);
    let applied = target.metadata.apply_prompt_metadata(text, &title).await?;
    let patch = if applied {
        json!({"title": title, "isCustomTitle": false, "lastPrompt": text})
    } else {
        json!({"lastPrompt": text})
    };
    target.event_service.publish(SessionMetaUpdated::new(
        SessionMetaUpdatedPayload {
            agent_id: "main".to_string(),
            session_id: target.session_id.clone(),
            title: applied.then_some(title),
            patch,
        },
    ));
    Ok(())
}

impl SqliteSessionStore {
    pub fn apply_prompt_metadata(
        &self,
        session_id: &str,
        last_prompt: &str,
        title: &str,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let now = chrono::Utc::now().timestamp_millis();
        let current: Option<(Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT title, title_kind FROM sessions WHERE session_id = ?1",
                params![session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((current_title, current_kind)) = current else {
            return Ok(false);
        };
        let apply_title =
            current_kind.as_deref() != Some("custom") && is_untitled(current_title.as_deref());
        if apply_title {
            conn.execute(
                "UPDATE sessions
                    SET last_prompt = ?2, title = ?3, title_kind = 'replaceable',
                        version = ?4, updated_at = ?5
                  WHERE session_id = ?1",
                params![session_id, last_prompt, title, SESSION_META_VERSION, now],
            )?;
        } else {
            conn.execute(
                "UPDATE sessions SET last_prompt = ?2, updated_at = ?3 WHERE session_id = ?1",
                params![session_id, last_prompt, now],
            )?;
        }
        Ok(apply_title)
    }
}
