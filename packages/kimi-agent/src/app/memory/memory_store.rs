use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::app::scopes::LifecycleScope;
use crate::wire::record::WireAsyncError;

use super::memory_paths::{MemoryEntry, MemoryScope, MemorySearchResult, build_snippet, detect_type, extract_title, parse_memory_path};

const TEXT_INDEX_NAME: &str = "memory_body";
const META_PREFIX: &str = "__meta__:";
const RECONCILE_INTERVAL_MS: i64 = 600_000;
const STORE_SUBDIR: &str = "memory";

#[async_trait]
pub trait IMemoryStore: Send + Sync {
    fn revision(&self) -> u64;
    fn read_only(&self) -> bool;
    fn set_read_only(&self, value: bool);
    async fn get(&self, path: &str) -> Result<Option<MemoryEntry>, WireAsyncError>;
    async fn put(&self, entry: MemoryEntry) -> Result<(), WireAsyncError>;
    async fn delete(&self, path: &str) -> Result<(), WireAsyncError>;
    async fn list(&self) -> Result<Vec<String>, WireAsyncError>;
    async fn search(&self, query: &str, limit: Option<usize>, filter: Option<(MemoryScope, String)>) -> Result<Vec<MemorySearchResult>, WireAsyncError>;
    async fn reconcile(&self) -> Result<(), WireAsyncError>;
}

#[derive(Debug, Clone)]
struct SharedMemoryError(Arc<String>);

impl fmt::Display for SharedMemoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SharedMemoryError {}

impl SharedMemoryError {
    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

struct MiniDb {
    dir: PathBuf,
    values: tokio::sync::Mutex<IndexMap<String, Value>>,
    closed: AtomicBool,
}

impl MiniDb {
    async fn open(dir: PathBuf) -> Result<Arc<Self>, SharedMemoryError> {
        let result = async {
            tokio::fs::create_dir_all(&dir).await?;
            let path = dir.join("data.json");
            let values = match tokio::fs::read_to_string(&path).await {
                Ok(content) => serde_json::from_str::<IndexMap<String, Value>>(&content).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => IndexMap::new(),
                Err(error) => return Err(error),
            };
            Ok(values)
        }.await;
        match result {
            Ok(values) => Ok(Arc::new(Self {
                dir,
                values: tokio::sync::Mutex::new(values),
                closed: AtomicBool::new(false),
            })),
            Err(error) => {
                let wrapped = SharedMemoryError(Arc::new(format!("memory store at {} failed to open ({error}); user data was not touched and the memory feature stays unavailable until the store is repaired", dir.display())));
                tracing::error!(dir = %dir.display(), error = %wrapped, "memory.store.open.failed");
                Err(wrapped)
            }
        }
    }

    fn ensure_open(&self) -> Result<(), WireAsyncError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(memory_error("memory store is closed"));
        }
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Option<Value>, WireAsyncError> {
        self.ensure_open()?;
        Ok(self.values.lock().await.get(key).cloned())
    }

    async fn query(&self) -> Result<Vec<(String, Value)>, WireAsyncError> {
        self.ensure_open()?;
        Ok(self.values.lock().await.iter().map(|(key, value)| (key.clone(), value.clone())).collect())
    }

    async fn set(&self, key: String, value: Value) -> Result<(), WireAsyncError> {
        self.ensure_open()?;
        let mut values = self.values.lock().await;
        let mut next = values.clone();
        next.insert(key, value);
        persist(&self.dir, &next).await?;
        *values = next;
        Ok(())
    }

    async fn del(&self, key: &str) -> Result<(), WireAsyncError> {
        self.ensure_open()?;
        let mut values = self.values.lock().await;
        let mut next = values.clone();
        next.shift_remove(key);
        persist(&self.dir, &next).await?;
        *values = next;
        Ok(())
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

async fn persist(dir: &Path, values: &IndexMap<String, Value>) -> Result<(), WireAsyncError> {
    let content = serde_json::to_vec(values).map_err(|error| Box::new(error) as WireAsyncError)?;
    let temporary = dir.join("data.json.tmp");
    let target = dir.join("data.json");
    tokio::fs::write(&temporary, content).await.map_err(|error| Box::new(error) as WireAsyncError)?;
    tokio::fs::rename(&temporary, &target).await.map_err(|error| Box::new(error) as WireAsyncError)?;
    Ok(())
}

pub struct MemoryStore {
    db_promise: tokio::sync::OnceCell<Shared<BoxFuture<'static, Result<Arc<MiniDb>, SharedMemoryError>>>>,
    content_revision: AtomicU64,
    read_only_flag: AtomicBool,
    seeded: AtomicBool,
    dir: PathBuf,
    memory_base_dir: PathBuf,
}

impl MemoryStore {
    pub fn new(store_dir: PathBuf, home_dir: PathBuf) -> Self {
        Self {
            db_promise: tokio::sync::OnceCell::new(),
            content_revision: AtomicU64::new(0),
            read_only_flag: AtomicBool::new(false),
            seeded: AtomicBool::new(false),
            dir: store_dir.join(STORE_SUBDIR),
            memory_base_dir: home_dir.join("memory"),
        }
    }

    async fn open_db(&self) -> Result<Arc<MiniDb>, WireAsyncError> {
        let dir = self.dir.clone();
        let future = self.db_promise.get_or_init(|| {
            async move { MiniDb::open(dir).await }.boxed().shared()
        }).await;
        future.clone().await.map_err(SharedMemoryError::boxed)
    }

    async fn ensure_fresh(&self) -> Result<(), WireAsyncError> {
        if self.seeded.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let db = self.open_db().await?;
        let marker = db.get(&format!("{META_PREFIX}reconciled")).await?;
        let at = marker.as_ref().and_then(|marker| marker.get("at")).and_then(Value::as_i64).unwrap_or(0);
        if chrono::Utc::now().timestamp_millis() - at < RECONCILE_INTERVAL_MS {
            return Ok(());
        }
        self.reconcile().await
    }

    pub async fn close(&self) {
        let Some(future) = self.db_promise.get() else {
            return;
        };
        if let Ok(db) = future.clone().await {
            db.close();
        }
    }
}

#[async_trait]
impl IMemoryStore for MemoryStore {
    fn revision(&self) -> u64 {
        self.content_revision.load(Ordering::Acquire)
    }

    fn read_only(&self) -> bool {
        self.read_only_flag.load(Ordering::Acquire)
    }

    fn set_read_only(&self, value: bool) {
        self.read_only_flag.store(value, Ordering::Release);
    }

    async fn get(&self, path: &str) -> Result<Option<MemoryEntry>, WireAsyncError> {
        self.ensure_fresh().await?;
        let db = self.open_db().await?;
        Ok(db.get(path).await?.and_then(|value| serde_json::from_value(value).ok()))
    }

    async fn put(&self, entry: MemoryEntry) -> Result<(), WireAsyncError> {
        if self.read_only() {
            return Err(memory_error("memory store is read-only: associative memory has absorbed it"));
        }
        self.content_revision.fetch_add(1, Ordering::AcqRel);
        let db = self.open_db().await?;
        let value = serde_json::to_value(entry).map_err(|error| Box::new(error) as WireAsyncError)?;
        db.set(value.get("path").and_then(Value::as_str).unwrap_or_default().to_string(), value).await
    }

    async fn delete(&self, path: &str) -> Result<(), WireAsyncError> {
        if self.read_only() {
            return Err(memory_error("memory store is read-only: associative memory has absorbed it"));
        }
        self.content_revision.fetch_add(1, Ordering::AcqRel);
        self.open_db().await?.del(path).await
    }

    async fn list(&self) -> Result<Vec<String>, WireAsyncError> {
        self.ensure_fresh().await?;
        Ok(self.open_db().await?.query().await?.into_iter().map(|(key, _)| key).filter(|key| !key.starts_with(META_PREFIX)).collect())
    }

    async fn search(&self, query: &str, limit: Option<usize>, filter: Option<(MemoryScope, String)>) -> Result<Vec<MemorySearchResult>, WireAsyncError> {
        self.ensure_fresh().await?;
        let limit = limit.unwrap_or(10);
        let entries = self.open_db().await?.query().await?.into_iter().filter(|(key, _)| !key.starts_with(META_PREFIX)).filter_map(|(_, value)| serde_json::from_value::<MemoryEntry>(value).ok()).collect::<Vec<_>>();
        let query_terms = unique_terms(&crate::tools::memory_search::tokenize(query));
        if entries.is_empty() || query_terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let documents = entries.iter().map(|entry| {
            let tokens = crate::tools::memory_search::tokenize(&entry.body);
            let mut term_freqs = HashMap::new();
            for token in &tokens {
                *term_freqs.entry(token.clone()).or_insert(0_u32) += 1;
            }
            (entry, term_freqs, tokens.len())
        }).collect::<Vec<_>>();
        let n = documents.len() as f64;
        let mut document_freq = HashMap::<&str, usize>::new();
        for (_, term_freqs, _) in &documents {
            for term in &query_terms {
                if term_freqs.contains_key(term) {
                    *document_freq.entry(term.as_str()).or_default() += 1;
                }
            }
        }
        let mut hits = Vec::<(usize, f64)>::new();
        for (index, (_, term_freqs, len)) in documents.iter().enumerate() {
            if *len == 0 {
                continue;
            }
            let mut score = 0.0;
            for term in &query_terms {
                let Some(tf) = term_freqs.get(term) else {
                    continue;
                };
                let df = document_freq.get(term.as_str()).copied().unwrap_or(0) as f64;
                score += (*tf as f64 / *len as f64) * (1.0 + n / df.max(1.0)).ln();
            }
            if score > 0.0 {
                hits.push((index, score));
            }
        }
        hits.sort_by(|left, right| right.1.partial_cmp(&left.1).unwrap_or(std::cmp::Ordering::Equal).then_with(|| documents[left.0].0.path.cmp(&documents[right.0].0.path)));
        hits.truncate((limit * 3).min(50));
        let mut results = Vec::new();
        for (index, score) in hits {
            if score < 0.15 {
                continue;
            }
            let entry = documents[index].0;
            let Some(parsed) = parse_memory_path(&entry.path) else {
                continue;
            };
            if let Some((scope, scope_id)) = &filter
                && (&parsed.scope != scope || &parsed.scope_id != scope_id)
            {
                continue;
            }
            results.push(MemorySearchResult {
                path: entry.path.clone(),
                scope: entry.scope,
                scope_id: entry.scope_id.clone(),
                type_: entry.type_,
                title: entry.title.clone(),
                snippet: build_snippet(&entry.body, query),
                score,
            });
        }
        results.truncate(limit);
        Ok(results)
    }

    async fn reconcile(&self) -> Result<(), WireAsyncError> {
        self.content_revision.fetch_add(1, Ordering::AcqRel);
        let db = self.open_db().await?;
        let base = self.memory_base_dir.clone();
        let disk_files = tokio::task::spawn_blocking(move || walk_dir(&base)).await.map_err(|error| Box::new(error) as WireAsyncError)??;
        let mut indexed_keys = HashSet::new();
        for (key, _) in db.query().await? {
            if !key.starts_with(META_PREFIX) {
                indexed_keys.insert(key);
            }
        }
        for (rel_path, fingerprint) in &disk_files {
            let existing = db.get(rel_path).await?.and_then(|value| serde_json::from_value::<MemoryEntry>(value).ok());
            if existing.as_ref().is_some_and(|existing| &existing.fingerprint == fingerprint) {
                indexed_keys.remove(rel_path);
                continue;
            }
            let Some(parsed) = parse_memory_path(rel_path) else {
                continue;
            };
            let full_path = self.memory_base_dir.join(rel_path);
            let body = tokio::fs::read_to_string(&full_path).await.map_err(|error| Box::new(error) as WireAsyncError)?;
            let metadata = tokio::fs::metadata(&full_path).await.map_err(|error| Box::new(error) as WireAsyncError)?;
            let entry = MemoryEntry {
                path: rel_path.clone(),
                scope: parsed.scope,
                scope_id: parsed.scope_id,
                type_: detect_type(&body),
                title: extract_title(&body, &parsed.file_name),
                body,
                fingerprint: fingerprint.clone(),
                updated_at: mtime_ms(&metadata),
                sha: None,
                intent: None,
                note: None,
                applies_to: None,
            };
            db.set(rel_path.clone(), serde_json::to_value(entry).map_err(|error| Box::new(error) as WireAsyncError)?).await?;
            indexed_keys.remove(rel_path);
        }
        let removed = indexed_keys.len();
        for stale_key in indexed_keys {
            db.del(&stale_key).await?;
        }
        db.set(format!("{META_PREFIX}reconciled"), json!({ "at": chrono::Utc::now().timestamp_millis() })).await?;
        tracing::debug!(total = disk_files.len(), removed, "memory.store.reconciled");
        Ok(())
    }
}

fn unique_terms(terms: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for term in terms {
        if seen.insert(term.as_str()) {
            unique.push(term.clone());
        }
    }
    unique
}

fn walk_dir(base: &Path) -> std::io::Result<IndexMap<String, String>> {
    let mut result = IndexMap::new();
    walk_dir_prefix(base, "", &mut result)?;
    Ok(result)
}

fn walk_dir_prefix(base: &Path, rel_prefix: &str, result: &mut IndexMap<String, String>) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(base.join(rel_prefix)) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if rel_prefix.is_empty() { name.clone() } else { format!("{rel_prefix}/{name}") };
        let full = base.join(&rel);
        let metadata = std::fs::metadata(&full)?;
        if metadata.is_dir() {
            walk_dir_prefix(base, &rel, result)?;
        } else if metadata.is_file() && name.ends_with(".md") {
            result.insert(rel, format!("{}-{}", metadata.len(), mtime_ms(&metadata)));
        }
    }
    Ok(())
}

fn mtime_ms(metadata: &std::fs::Metadata) -> f64 {
    metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|duration| duration.as_secs_f64() * 1000.0).unwrap_or(0.0)
}

fn memory_error(message: impl Into<String>) -> WireAsyncError {
    Box::new(std::io::Error::other(message.into()))
}

pub fn register_memory_store_service() {
    register_scoped_service(LifecycleScope::App, ScopeActivation::OnScopeCreated, "memory");
}

