use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::_base::log::log::ILogService;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::persistence::interface::query_store::{
    Checkpoint, ColumnBounds, ColumnPageQuery, ComparisonOp, IQuery, IQueryStore, IndexDef, Page,
    QueryFilter, QueryStoreRebuiltError, SortDir, WriteOp,
};
use crate::wire::record::WireAsyncError;

const SEP: char = '\0';
const CHECKPOINT_COLLECTION: &str = "__checkpoint__";
const STORE_SUBDIR: &str = "query-store";
const SHARD_COUNT: usize = 16;
const DROP_BATCH_SIZE: usize = 500;
const TRANSIENT_ESCALATION_LIMIT: usize = 5;

static PENDING_DISPOSALS: Lazy<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
    Lazy::new(|| parking_lot::Mutex::new(Vec::new()));

pub async fn drain_query_store_disposals() {
    let pending = std::mem::take(&mut *PENDING_DISPOSALS.lock());
    for task in pending {
        let _ = task.await;
    }
}

#[derive(Clone, Copy)]
enum OperationKind {
    Read,
    Write,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredEntry {
    value: Value,
    #[serde(default)]
    columns: HashMap<String, f64>,
}

#[derive(Default, Serialize, Deserialize)]
struct MiniDbData {
    #[serde(default)]
    entries: BTreeMap<String, StoredEntry>,
    #[serde(default)]
    indexes: HashMap<String, IndexDef>,
    #[serde(skip)]
    dir: PathBuf,
}

struct MiniDbInner {
    dir: PathBuf,
    log: Arc<dyn ILogService>,
    db: Option<MiniDbData>,
    transient_read_failures: usize,
    transient_write_failures: usize,
    ensured_indexes: HashSet<String>,
}

#[derive(Clone)]
pub struct MiniDbQueryStore {
    inner: Arc<tokio::sync::Mutex<MiniDbInner>>,
    store_epoch: Arc<std::sync::atomic::AtomicU64>,
}

impl MiniDbQueryStore {
    pub fn new(bootstrap: Arc<dyn IBootstrapService>, log: Arc<dyn ILogService>) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(MiniDbInner {
                dir: Path::new(&bootstrap.cache_dir()).join(STORE_SUBDIR),
                log,
                db: None,
                transient_read_failures: 0,
                transient_write_failures: 0,
                ensured_indexes: HashSet::new(),
            })),
            store_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    async fn with_db<T, F>(
        &self,
        kind: OperationKind,
        expected_store_epoch: Option<u64>,
        op: F,
    ) -> Result<T, WireAsyncError>
    where
        F: FnOnce(&mut MiniDbData) -> Result<T, WireAsyncError> + Send,
    {
        let mut inner = self.inner.lock().await;
        if inner.db.is_none() {
            inner.log.info(
                "minidb query-store opening",
                Some(json!({ "dir": inner.dir, "shardCount": SHARD_COUNT })),
            );
            inner.db = Some(open_fresh(&inner.dir)?);
        }
        if expected_store_epoch.is_some_and(|epoch| {
            epoch != self.store_epoch.load(std::sync::atomic::Ordering::SeqCst)
        }) {
            return Err(Box::new(QueryStoreRebuiltError));
        }
        let result = op(inner.db.as_mut().unwrap());
        match result {
            Ok(value) => {
                match kind {
                    OperationKind::Read => inner.transient_read_failures = 0,
                    OperationKind::Write => inner.transient_write_failures = 0,
                }
                Ok(value)
            }
            Err(error) => {
                let rebuild = is_rebuild_error(&error);
                let failures = match kind {
                    OperationKind::Read => {
                        inner.transient_read_failures += 1;
                        inner.transient_read_failures
                    }
                    OperationKind::Write => {
                        inner.transient_write_failures += 1;
                        inner.transient_write_failures
                    }
                };
                if !rebuild && failures < TRANSIENT_ESCALATION_LIMIT {
                    return Err(error);
                }
                inner.transient_read_failures = 0;
                inner.transient_write_failures = 0;
                inner.log.warn(
                    "minidb query-store rebuilt after unrecoverable failure",
                    Some(json!({ "dir": inner.dir, "error": error.to_string() })),
                );
                inner.db = None;
                inner.ensured_indexes.clear();
                wipe_cluster(&inner.dir)?;
                self.store_epoch
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if expected_store_epoch.is_some() {
                    return Err(Box::new(QueryStoreRebuiltError));
                }
                Err(error)
            }
        }
    }

    pub fn dispose(self) {
        let task = tokio::spawn(async move {
            let _ = self.close().await;
        });
        PENDING_DISPOSALS.lock().push(task);
    }
}

fn open_fresh(dir: &Path) -> Result<MiniDbData, WireAsyncError> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("data.json");
    let mut data = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => MiniDbData::default(),
        Err(error) => return Err(Box::new(error)),
    };
    data.dir = dir.to_path_buf();
    Ok(data)
}

fn persist(dir: &Path, data: &MiniDbData) -> Result<(), WireAsyncError> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("data.json");
    let bytes = serde_json::to_vec(data)?;
    crate::_base::utils::fs::atomic_write(&path, &bytes, None, None, None)?;
    Ok(())
}

fn wipe_cluster(dir: &Path) -> Result<(), WireAsyncError> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Box::new(error)),
    }
}

fn is_rebuild_error(error: &WireAsyncError) -> bool {
    error.downcast_ref::<io::Error>().is_some()
}

fn physical_key(collection: &str, key: &str) -> String {
    format!("{collection}{SEP}{key}")
}

fn index_name(collection: &str, name: &str) -> String {
    format!("{collection}:{name}")
}

fn collection_from_index_name(name: &str) -> &str {
    name.split_once(':').map_or(name, |(collection, _)| collection)
}

fn check_unique_indexes(
    data: &MiniDbData,
    collection: &str,
    key: &str,
    value: &Value,
) -> Result<(), WireAsyncError> {
    for (name, definition) in &data.indexes {
        if collection_from_index_name(name) != collection {
            continue;
        }
        let IndexDef::Value {
            field,
            unique: Some(true),
            ..
        } = definition
        else {
            continue;
        };
        let Some(candidate) = field_value(value, field) else {
            continue;
        };
        for (physical, entry) in &data.entries {
            if !physical.starts_with(&format!("{collection}{SEP}"))
                || physical == &physical_key(collection, key)
            {
                continue;
            }
            if field_value(&entry.value, field) == Some(candidate) {
                return Err(Box::new(io::Error::other(format!(
                    "unique index {name} already contains this value"
                ))));
            }
        }
    }
    Ok(())
}

#[async_trait]
impl IQueryStore for MiniDbQueryStore {
    async fn put_value(
        &self,
        collection: &str,
        key: &str,
        value: Value,
        columns: Option<HashMap<String, f64>>,
    ) -> Result<(), WireAsyncError> {
        self.with_db(OperationKind::Write, None, move |db| {
            check_unique_indexes(db, collection, key, &value)?;
            let previous = db.entries.insert(
                physical_key(collection, key),
                StoredEntry {
                    value,
                    columns: columns.unwrap_or_default(),
                },
            );
            if let Err(error) = persist(&db_dir(db), db) {
                if let Some(previous) = previous {
                    db.entries.insert(physical_key(collection, key), previous);
                } else {
                    db.entries.remove(&physical_key(collection, key));
                }
                return Err(error);
            }
            Ok(())
        })
        .await
    }

    async fn batch(&self, ops: Vec<WriteOp>) -> Result<(), WireAsyncError> {
        if ops.is_empty() {
            return Ok(());
        }
        self.with_db(OperationKind::Write, None, move |db| {
            let snapshot = db.entries.clone();
            let result = (|| {
                for op in ops {
                    match op {
                        WriteOp::Put {
                            collection,
                            key,
                            value,
                            columns,
                        } => {
                            check_unique_indexes(db, &collection, &key, &value)?;
                            db.entries.insert(
                                physical_key(&collection, &key),
                                StoredEntry {
                                    value,
                                    columns: columns.unwrap_or_default(),
                                },
                            );
                        }
                        WriteOp::Delete { collection, key } => {
                            db.entries.remove(&physical_key(&collection, &key));
                        }
                    }
                }
                persist(&db_dir(db), db)
            })();
            if result.is_err() {
                db.entries = snapshot;
            }
            result
        })
        .await
    }

    async fn delete(&self, collection: &str, key: &str) -> Result<(), WireAsyncError> {
        self.with_db(OperationKind::Write, None, move |db| {
            let previous = db.entries.remove(&physical_key(collection, key));
            if let Err(error) = persist(&db_dir(db), db) {
                if let Some(previous) = previous {
                    db.entries.insert(physical_key(collection, key), previous);
                }
                return Err(error);
            }
            Ok(())
        })
        .await
    }

    async fn get_value(&self, collection: &str, key: &str) -> Result<Option<Value>, WireAsyncError> {
        self.with_db(OperationKind::Read, None, move |db| {
            Ok(db
                .entries
                .get(&physical_key(collection, key))
                .map(|entry| entry.value.clone()))
        })
        .await
    }

    async fn get_many_values(
        &self,
        collection: &str,
        keys: &[String],
    ) -> Result<HashMap<String, Value>, WireAsyncError> {
        if keys.is_empty() {
            return Ok(HashMap::new());
        }
        self.with_db(OperationKind::Read, None, move |db| {
            let mut out = HashMap::new();
            for key in keys {
                if let Some(entry) = db.entries.get(&physical_key(collection, key)) {
                    out.insert(key.clone(), entry.value.clone());
                }
            }
            Ok(out)
        })
        .await
    }

    fn query_values(&self, collection: &str) -> Box<dyn IQuery<Value>> {
        Box::new(MiniDbQuery {
            store: self.clone(),
            collection: collection.to_string(),
            filter: Map::new(),
            column: None,
            sort_field: None,
            sort_dir: SortDir::Asc,
            lim: None,
            skip: 0,
            marker: PhantomData,
        })
    }

    async fn page_by_column_values(
        &self,
        _collection: &str,
        query: ColumnPageQuery,
    ) -> Result<Page<Value>, WireAsyncError> {
        self.with_db(OperationKind::Read, None, move |db| {
            let mut rows = db
                .entries
                .iter()
                .filter(|(_, entry)| {
                    query.bounds.as_ref().is_none_or(|bounds| {
                        entry
                            .columns
                            .get(&query.column)
                            .is_some_and(|value| column_in_bounds(*value, bounds))
                    })
                })
                .filter(|(_, entry)| {
                    query
                        .filter
                        .as_ref()
                        .is_none_or(|filter| matches_filter(&entry.value, filter))
                })
                .collect::<Vec<_>>();
            let descending = query.dir == Some(SortDir::Desc);
            rows.sort_by(|(_, left), (_, right)| {
                let ordering = left
                    .columns
                    .get(&query.column)
                    .partial_cmp(&right.columns.get(&query.column))
                    .unwrap_or(Ordering::Equal);
                if descending { ordering.reverse() } else { ordering }
            });
            let mut items = Vec::new();
            for (_, entry) in rows.into_iter().take(query.limit) {
                items.push(entry.value.clone());
            }
            Ok(Page {
                items,
                next_cursor: None,
            })
        })
        .await
    }

    async fn ensure_index(&self, collection: &str, def: IndexDef) -> Result<(), WireAsyncError> {
        if let IndexDef::Text { name, .. } = &def {
            return Err(Box::new(io::Error::other(format!(
                "minidb query-store is a structural read model: text index \"{name}\" on collection \"{collection}\" is rejected; full-text search lives in the kap-server search-index database"
            ))));
        }
        let kind = match &def {
            IndexDef::Value { .. } => "value",
            IndexDef::Compound { .. } => "compound",
            IndexDef::Text { .. } => unreachable!(),
        };
        let name = match &def {
            IndexDef::Value { name, .. }
            | IndexDef::Compound { name, .. }
            | IndexDef::Text { name, .. } => name,
        };
        let guard = format!("{collection}:{kind}:{name}");
        if self.inner.lock().await.ensured_indexes.contains(&guard) {
            return Ok(());
        }
        let definition_name = index_name(collection, name);
        self.with_db(OperationKind::Write, None, move |db| {
            if let IndexDef::Value {
                field,
                unique: Some(true),
                ..
            } = &def
            {
                let mut seen = Vec::new();
                for (key, entry) in &db.entries {
                    if !key.starts_with(&format!("{collection}{SEP}")) {
                        continue;
                    }
                    let Some(value) = field_value(&entry.value, field) else {
                        continue;
                    };
                    if seen.contains(&value) {
                        return Err(Box::new(io::Error::other(format!(
                            "unique index {definition_name} already contains duplicate values"
                        ))));
                    }
                    seen.push(value);
                }
            }
            if db.indexes.contains_key(&definition_name) {
                return Ok(());
            }
            let previous = db.indexes.insert(definition_name.clone(), def);
            if let Err(error) = persist(&db_dir(db), db) {
                if let Some(previous) = previous {
                    db.indexes.insert(definition_name, previous);
                } else {
                    db.indexes.remove(&definition_name);
                }
                return Err(error);
            }
            Ok(())
        })
        .await?;
        self.inner.lock().await.ensured_indexes.insert(guard);
        Ok(())
    }

    async fn list_keys(&self, collection: &str) -> Result<Vec<String>, WireAsyncError> {
        self.with_db(OperationKind::Read, None, move |db| {
            let prefix = format!("{collection}{SEP}");
            Ok(db
                .entries
                .keys()
                .filter(|key| key.starts_with(&prefix))
                .map(|key| key[prefix.len()..].to_string())
                .collect())
        })
        .await
    }

    async fn drop_collection(&self, collection: &str) -> Result<(), WireAsyncError> {
        let prefix = format!("{collection}{SEP}");
        let entries = self
            .with_db(OperationKind::Read, None, move |db| {
                Ok(db
                    .entries
                    .keys()
                    .filter(|key| key.starts_with(&prefix))
                    .cloned()
                    .collect::<Vec<_>>())
            })
            .await?;
        for chunk in entries.chunks(DROP_BATCH_SIZE) {
            let chunk = chunk.to_vec();
            self.with_db(OperationKind::Write, None, move |db| {
                let snapshot = db.entries.clone();
                for key in chunk {
                    db.entries.remove(&key);
                }
                if let Err(error) = persist(&db_dir(db), db) {
                    db.entries = snapshot;
                    return Err(error);
                }
                Ok(())
            })
            .await?;
        }
        Ok(())
    }

    async fn get_checkpoint(&self, source: &str) -> Result<Option<Checkpoint>, WireAsyncError> {
        match self.get_value(CHECKPOINT_COLLECTION, source).await? {
            Some(value) => Ok(Some(serde_json::from_value(value)?)),
            None => Ok(None),
        }
    }

    async fn set_checkpoint(
        &self,
        source: &str,
        checkpoint: Checkpoint,
        expected_store_epoch: Option<u64>,
    ) -> Result<(), WireAsyncError> {
        let key = physical_key(CHECKPOINT_COLLECTION, source);
        let value = serde_json::to_value(checkpoint)?;
        self.with_db(OperationKind::Write, expected_store_epoch, move |db| {
            let previous = db.entries.insert(
                key.clone(),
                StoredEntry {
                    value,
                    columns: HashMap::new(),
                },
            );
            if let Err(error) = persist(&db_dir(db), db) {
                if let Some(previous) = previous {
                    db.entries.insert(key, previous);
                } else {
                    db.entries.remove(&key);
                }
                return Err(error);
            }
            Ok(())
        })
        .await
    }

    fn store_epoch(&self) -> u64 {
        self.store_epoch.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn close(&self) -> Result<(), WireAsyncError> {
        let mut inner = self.inner.lock().await;
        if let Some(db) = inner.db.take() {
            persist(&inner.dir, &db)?;
        }
        Ok(())
    }
}

fn db_dir(db: &MiniDbData) -> PathBuf {
    db.dir.clone()
}

struct MiniDbQuery<T> {
    store: MiniDbQueryStore,
    collection: String,
    filter: QueryFilter,
    column: Option<(String, ColumnBounds)>,
    sort_field: Option<String>,
    sort_dir: SortDir,
    lim: Option<usize>,
    skip: usize,
    marker: PhantomData<fn() -> T>,
}

#[async_trait]
impl<T> IQuery<T> for MiniDbQuery<T>
where
    T: for<'de> Deserialize<'de> + Send + Sync + 'static,
{
    fn where_(&mut self, filter: QueryFilter) {
        self.filter.extend(filter);
    }

    fn where_column(&mut self, column: String, bounds: ColumnBounds) {
        self.column = Some((column, bounds));
    }

    fn order_by(&mut self, field: String, dir: Option<SortDir>) {
        self.sort_field = Some(field);
        self.sort_dir = dir.unwrap_or_default();
    }

    fn limit(&mut self, n: usize) {
        self.lim = Some(n);
    }

    fn cursor(&mut self, cursor: Option<String>) {
        self.skip = cursor
            .filter(|cursor| !cursor.is_empty())
            .and_then(|cursor| cursor.parse().ok())
            .unwrap_or(0);
    }

    async fn execute(&self) -> Result<Page<T>, WireAsyncError> {
        let prefix = format!("{}{SEP}", self.collection);
        let filter = self.filter.clone();
        let column = self.column.clone();
        let sort_field = self.sort_field.clone();
        let descending = self.sort_dir == SortDir::Desc;
        let lim = self.lim;
        let skip = self.skip;
        self.store
            .with_db(OperationKind::Read, None, move |db| {
                let mut rows = db
                    .entries
                    .iter()
                    .filter(|(key, _)| key.starts_with(&prefix))
                    .filter(|(_, entry)| matches_filter(&entry.value, &filter))
                    .filter(|(_, entry)| {
                        column.as_ref().is_none_or(|(name, bounds)| {
                            entry
                                .columns
                                .get(name)
                                .is_some_and(|value| column_in_bounds(*value, bounds))
                        })
                    })
                    .collect::<Vec<_>>();
                if let Some(field) = sort_field {
                    rows.sort_by(|(_, left), (_, right)| {
                        let ordering = compare_values(
                            left.columns
                                .get(&field)
                                .map(|value| json!(value))
                                .or_else(|| field_value(&left.value, &field).cloned())
                                .as_ref(),
                            right
                                .columns
                                .get(&field)
                                .map(|value| json!(value))
                                .or_else(|| field_value(&right.value, &field).cloned())
                                .as_ref(),
                        );
                        if descending { ordering.reverse() } else { ordering }
                    });
                }
                let rows = rows.into_iter().skip(skip).collect::<Vec<_>>();
                let take = lim.map_or(rows.len(), |lim| lim.saturating_add(1).min(rows.len()));
                let mut items = Vec::new();
                for (_, entry) in rows.into_iter().take(take) {
                    items.push(serde_json::from_value(entry.value.clone())?);
                }
                let next_cursor = if lim.is_some_and(|lim| items.len() > lim) {
                    items.truncate(lim.unwrap());
                    Some((skip + lim.unwrap()).to_string())
                } else {
                    None
                };
                Ok(Page { items, next_cursor })
            })
            .await
    }
}

fn column_in_bounds(value: f64, bounds: &ColumnBounds) -> bool {
    bounds.gt.is_none_or(|bound| value > bound)
        && bounds.gte.is_none_or(|bound| value >= bound)
        && bounds.lt.is_none_or(|bound| value < bound)
        && bounds.lte.is_none_or(|bound| value <= bound)
}

fn field_value<'a>(value: &'a Value, field: &str) -> Option<&'a Value> {
    let mut current = value;
    for part in field.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

fn matches_filter(value: &Value, filter: &QueryFilter) -> bool {
    filter.iter().all(|(field, condition)| {
        let actual = field_value(value, field);
        if let Some(object) = condition.as_object()
            && object.keys().any(|key| key.starts_with('$'))
        {
            return serde_json::from_value::<ComparisonOp>(condition.clone())
                .ok()
                .is_some_and(|comparison| matches_comparison(actual, &comparison));
        }
        actual == Some(condition)
    })
}

fn matches_comparison(actual: Option<&Value>, comparison: &ComparisonOp) -> bool {
    if comparison.eq.as_ref().is_some_and(|expected| Some(expected) != actual) {
        return false;
    }
    if comparison.ne.as_ref().is_some_and(|expected| Some(expected) == actual) {
        return false;
    }
    if comparison.gt.as_ref().is_some_and(|expected| {
        actual.is_none_or(|actual| compare_values(Some(actual), Some(expected)) != Ordering::Greater)
    }) {
        return false;
    }
    if comparison.gte.as_ref().is_some_and(|expected| {
        actual.is_none_or(|actual| compare_values(Some(actual), Some(expected)) == Ordering::Less)
    }) {
        return false;
    }
    if comparison.lt.as_ref().is_some_and(|expected| {
        actual.is_none_or(|actual| compare_values(Some(actual), Some(expected)) != Ordering::Less)
    }) {
        return false;
    }
    if comparison.lte.as_ref().is_some_and(|expected| {
        actual.is_none_or(|actual| compare_values(Some(actual), Some(expected)) == Ordering::Greater)
    }) {
        return false;
    }
    if comparison.in_.as_ref().is_some_and(|values| {
        actual.is_none_or(|actual| !values.contains(actual))
    }) {
        return false;
    }
    if comparison.nin.as_ref().is_some_and(|values| {
        actual.is_some_and(|actual| values.contains(actual))
    }) {
        return false;
    }
    if comparison
        .exists
        .is_some_and(|exists| actual.is_some() != exists)
    {
        return false;
    }
    true
}

fn compare_values(left: Option<&Value>, right: Option<&Value>) -> Ordering {
    match (left, right) {
        (Some(Value::Number(left)), Some(Value::Number(right))) => left
            .as_f64()
            .partial_cmp(&right.as_f64())
            .unwrap_or(Ordering::Equal),
        (Some(Value::String(left)), Some(Value::String(right))) => left.cmp(right),
        (Some(left), Some(right)) => left.to_string().cmp(&right.to_string()),
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
    }
}
