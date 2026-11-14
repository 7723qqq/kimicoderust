use std::sync::{Arc, LazyLock};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::_base::utils::paths::canonical_workspace_root;
use crate::_base::workdir_slug::encode_workdir_key;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::persistence::interface::atomic_document_store::IAtomicDocumentStore;
use crate::persistence::interface::storage::{
    STORAGE_DECODE_FAILED, StorageError, is_storage_error,
};
use crate::wire::record::WireAsyncError;

const TRUST_SCOPE: &str = "workspace-trust";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrustRecord {
    root: String,
    trusted_at: i64,
}

struct TrustRecordEntry {
    key: String,
    record: Value,
}

pub async fn read_workspace_trust(
    docs: &dyn IAtomicDocumentStore,
    root: &str,
    telemetry: Option<&Arc<dyn ITelemetryService>>,
) -> Result<bool, WireAsyncError> {
    let result = read_workspace_trust_inner(docs, root).await;
    if let (Some(telemetry), Err(error)) = (telemetry, &result) {
        let error_type = error
            .downcast_ref::<StorageError>()
            .map(|error| error.name())
            .or_else(|| {
                error
                    .downcast_ref::<kimi_core::primitives::errors::Error2>()
                    .map(|error| error.name())
            })
            .unwrap_or("Error");
        let mut properties = serde_json::Map::new();
        properties.insert("error_type".to_string(), error_type.into());
        telemetry.track2("workspace_trust_read_failed", Some(properties));
    }
    result
}

async fn read_workspace_trust_inner(
    docs: &dyn IAtomicDocumentStore,
    root: &str,
) -> Result<bool, WireAsyncError> {
    let canonical_key = trust_key(root);
    if docs.get(TRUST_SCOPE, &canonical_key).await?.is_some() {
        return Ok(true);
    }
    let legacy_key = encode_workdir_key(root);
    if legacy_key != canonical_key {
        let legacy = docs.get(TRUST_SCOPE, &legacy_key).await?;
        if let Some(legacy) = legacy {
            migrate_workspace_trust(docs, &canonical_key, &legacy_key, legacy).await?;
            return Ok(true);
        }
    }
    let variant = find_legacy_trust_records(docs, root).await?.into_iter().next();
    let Some(variant) = variant else {
        return Ok(false);
    };
    migrate_workspace_trust(docs, &canonical_key, &variant.key, variant.record).await?;
    Ok(true)
}

pub async fn write_workspace_trust(
    docs: &dyn IAtomicDocumentStore,
    root: &str,
    trusted_at: i64,
) -> Result<(), WireAsyncError> {
    let record = TrustRecord {
        root: root.to_string(),
        trusted_at,
    };
    docs.set(
        TRUST_SCOPE,
        &trust_key(root),
        serde_json::to_value(record).unwrap(),
    )
    .await
}

pub async fn delete_workspace_trust(
    docs: &dyn IAtomicDocumentStore,
    root: &str,
) -> Result<(), WireAsyncError> {
    let canonical_key = trust_key(root);
    let legacy_key = encode_workdir_key(root);
    docs.delete(TRUST_SCOPE, &canonical_key).await?;
    if legacy_key != canonical_key {
        docs.delete(TRUST_SCOPE, &legacy_key).await?;
    }
    for variant in find_legacy_trust_records(docs, root).await? {
        docs.delete(TRUST_SCOPE, &variant.key).await?;
    }
    Ok(())
}

async fn migrate_workspace_trust(
    docs: &dyn IAtomicDocumentStore,
    canonical_key: &str,
    legacy_key: &str,
    record: Value,
) -> Result<(), WireAsyncError> {
    docs.set(TRUST_SCOPE, canonical_key, record).await?;
    docs.delete(TRUST_SCOPE, legacy_key).await
}

async fn find_legacy_trust_records(
    docs: &dyn IAtomicDocumentStore,
    root: &str,
) -> Result<Vec<TrustRecordEntry>, WireAsyncError> {
    static TEMP_KEY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\.tmp\.\d+\.[0-9a-f]+$").unwrap());
    let canonical_root = canonical_workspace_root(root);
    let canonical_key = trust_key(root);
    let legacy_key = encode_workdir_key(root);
    let mut result = Vec::new();
    for key in docs.list(TRUST_SCOPE).await? {
        if key == canonical_key || key == legacy_key || TEMP_KEY.is_match(&key) {
            continue;
        }
        let record = match docs.get(TRUST_SCOPE, &key).await {
            Ok(record) => record,
            Err(error) if is_storage_error(error.as_ref(), STORAGE_DECODE_FAILED) => continue,
            Err(error) => return Err(error),
        };
        let Some(record) = record else {
            continue;
        };
        let Some(record_root) = record.get("root").and_then(Value::as_str) else {
            continue;
        };
        if canonical_workspace_root(record_root) == canonical_root {
            result.push(TrustRecordEntry { key, record });
        }
    }
    Ok(result)
}

fn trust_key(root: &str) -> String {
    encode_workdir_key(&canonical_workspace_root(root))
}
