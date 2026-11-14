use std::collections::HashSet;

use futures_util::future::join_all;

use crate::persistence::interface::storage::{
    IFileSystemStorageService, StorageAppendOptions,
};
use crate::wire::record::WireAsyncError;

pub const SESSION_INDEX_DIRTY_DIR: &str = ".index-dirty";

fn dirty_scope(sessions_scope: &str) -> String {
    format!("{sessions_scope}/{SESSION_INDEX_DIRTY_DIR}")
}

pub async fn mark_session_dirty(
    storage: &dyn IFileSystemStorageService,
    sessions_scope: &str,
    session_id: &str,
) -> Result<(), WireAsyncError> {
    storage
        .append(
            &dirty_scope(sessions_scope),
            &format!("{session_id}.{}", chrono::Utc::now().timestamp_millis()),
            Vec::new(),
            Some(StorageAppendOptions {
                durable: Some(false),
            }),
        )
        .await
}

pub async fn list_dirty_marks(
    storage: &dyn IFileSystemStorageService,
    sessions_scope: &str,
) -> Result<Vec<String>, WireAsyncError> {
    storage.list(&dirty_scope(sessions_scope), None, None).await
}

pub fn dirty_mark_session_ids(marks: &[String]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for name in marks {
        if let Some(dot) = name.rfind('.')
            && dot > 0
        {
            ids.insert(name[..dot].to_string());
        }
    }
    ids
}

pub async fn clear_dirty_marks(
    storage: &dyn IFileSystemStorageService,
    sessions_scope: &str,
    marks: &[String],
) {
    let scope = dirty_scope(sessions_scope);
    join_all(marks.iter().map(|name| {
        let scope = scope.clone();
        async move {
            let _ = storage.delete(&scope, name).await;
        }
    }))
    .await;
}
