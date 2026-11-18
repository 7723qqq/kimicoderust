use std::sync::Arc;

use serde_json::{Map, Value};

use crate::_base::log::log::ILogService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::persistence::interface::append_log_store::{
    AppendLogTruncation, IAppendLogStore,
};
use crate::persistence::interface::storage::{
    IFileSystemStorageService, StorageWriteOptions,
};
use crate::wire::record::WireAsyncError;

pub struct WireJournalRepairServices {
    pub append_log: Arc<dyn IAppendLogStore>,
    pub storage: Arc<dyn IFileSystemStorageService>,
    pub log: Arc<dyn ILogService>,
    pub telemetry: Arc<dyn ITelemetryService>,
}

pub fn wire_journal_backup_key(key: &str) -> String {
    format!("{key}.bak")
}

pub async fn repair_wire_journal(
    services: &WireJournalRepairServices,
    scope: &str,
    key: &str,
    records: &[Value],
    truncation: &AppendLogTruncation,
) -> &'static str {
    let mut backup_created = false;
    let mut dropped_count = 0;
    let result: Result<(), WireAsyncError> = async {
        let original = services.storage.read(scope, key).await?;
        if let Some(original) = original {
            dropped_count = count_journal_lines(&original).saturating_sub(records.len()) as u64;
            let backup_key = wire_journal_backup_key(key);
            if services.storage.size(scope, &backup_key).await?.is_none() {
                services
                    .storage
                    .write(
                        scope,
                        &backup_key,
                        original.clone(),
                        Some(StorageWriteOptions {
                            atomic: Some(true),
                            signal: None,
                        }),
                    )
                    .await?;
                backup_created = true;
            }
        }
        services
            .append_log
            .rewrite(scope, key, records.clone(), None)
            .await?;
        Ok(())
    }
    .await;
    let (outcome, repair_error) = match result {
        Ok(()) => ("repaired", None),
        Err(error) => ("failed", Some(error.to_string())),
    };
    let mut payload = Map::new();
    payload.insert("scope".to_string(), scope.into());
    payload.insert("key".to_string(), key.into());
    payload.insert("lineNumber".to_string(), truncation.line_number.into());
    payload.insert("reason".to_string(), truncation.reason.as_str().into());
    payload.insert("outcome".to_string(), outcome.into());
    payload.insert("droppedCount".to_string(), dropped_count.into());
    payload.insert("backupCreated".to_string(), backup_created.into());
    if let Some(error) = repair_error {
        payload.insert("error".to_string(), error.into());
    }
    services.log.warn(
        "corrupted wire journal truncated to its valid prefix",
        Some(Value::Object(payload)),
    );
    let mut properties = Map::new();
    properties.insert("kind".to_string(), truncation.reason.as_str().into());
    properties.insert("outcome".to_string(), outcome.into());
    properties.insert("dropped_count".to_string(), dropped_count.into());
    properties.insert("backup_created".to_string(), backup_created.into());
    services.telemetry.track2("wire_repair", Some(properties));
    outcome
}

fn count_journal_lines(data: &[u8]) -> usize {
    let mut lines = 0;
    let mut has_content = false;
    for byte in data {
        if *byte == 0x0a {
            lines += 1;
            has_content = false;
        } else {
            has_content = true;
        }
    }
    if has_content { lines + 1 } else { lines }
}
