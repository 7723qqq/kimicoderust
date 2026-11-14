use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use serde_json::json;

use crate::_base::log::log::ILogService;
use crate::agent::injection::rendered_lines::{already_rendered, mark_rendered};
use crate::app::memory::memory_paths::parse_applies_to;
use crate::app::memory::memory_repo::IMemoryRepo;
use crate::app::memory::memory_store::IMemoryStore;
use crate::session::sessionContext::session_context::ISessionContext;
use crate::wire::record::WireAsyncError;

use super::gitMemoryRenderState::GitMemoryRenderState;
use super::projection::MemoryEntryView;

const MEMORY_BLOCK_MAX_ENTRIES: usize = 8;
const MEMORY_SHA_HISTORY: usize = 200;

#[derive(Debug, Clone)]
struct MemoryRecord {
    path: String,
    title: String,
    body: String,
    updated_at: f64,
}

#[derive(Clone)]
struct EntriesCache {
    revision: u64,
    entries: Vec<MemoryRecord>,
}

#[derive(Clone)]
struct ShaCache {
    revision: u64,
    map: HashMap<String, String>,
}

pub struct GitMemoryEntries {
    store: Arc<dyn IMemoryStore>,
    repo: Arc<dyn IMemoryRepo>,
    session: Arc<dyn ISessionContext>,
    log: Arc<dyn ILogService>,
    render_state: Arc<Mutex<GitMemoryRenderState>>,
    entries_cache: Mutex<Option<EntriesCache>>,
    sha_cache: Mutex<Option<ShaCache>>,
    warned: AtomicBool,
}

impl GitMemoryEntries {
    pub fn new(
        store: Arc<dyn IMemoryStore>,
        repo: Arc<dyn IMemoryRepo>,
        session: Arc<dyn ISessionContext>,
        log: Arc<dyn ILogService>,
        render_state: Arc<Mutex<GitMemoryRenderState>>,
    ) -> Self {
        Self {
            store,
            repo,
            session,
            log,
            render_state,
            entries_cache: Mutex::new(None),
            sha_cache: Mutex::new(None),
            warned: AtomicBool::new(false),
        }
    }

    pub async fn for_path(&self, path: &str) -> Vec<MemoryEntryView> {
        self.guard(|patterns| {
            patterns.iter().any(|pattern| glob_matches(pattern, path))
        })
        .await
    }

    pub async fn unscoped(&self) -> Vec<MemoryEntryView> {
        self.guard(|patterns| patterns.is_empty()).await
    }

    async fn guard(
        &self,
        matches: impl Fn(&[String]) -> bool,
    ) -> Vec<MemoryEntryView> {
        match self.collect(&matches).await {
            Ok(views) => views,
            Err(error) => {
                if !self.warned.swap(true, Ordering::Relaxed) {
                    self.log.warn(
                        "git memory entries unavailable; injecting git history only",
                        Some(json!({ "error": error.to_string() })),
                    );
                }
                Vec::new()
            }
        }
    }

    async fn collect(
        &self,
        matches: &dyn Fn(&[String]) -> bool,
    ) -> Result<Vec<MemoryEntryView>, WireAsyncError> {
        let mut views = Vec::new();
        let shas = self.commit_shas().await?;
        for entry in self.entries().await? {
            if views.len() >= MEMORY_BLOCK_MAX_ENTRIES {
                break;
            }
            let patterns = self.scopes(&entry.path, &entry.body);
            if !matches(&patterns) {
                continue;
            }
            let Some(sha) = shas.get(&entry.path).cloned() else {
                continue;
            };
            {
                let mut state = self.render_state.lock();
                if state.has_memory(&sha) {
                    continue;
                }
                state.mark_memory(sha.clone());
            }
            let view = self.view(&entry, sha)?;
            let session_key = self.session.session_id();
            if already_rendered(&session_key, &view.body) {
                continue;
            }
            mark_rendered(&session_key, &[view.body.clone()]);
            views.push(view);
        }
        Ok(views)
    }

    async fn entries(&self) -> Result<Vec<MemoryRecord>, WireAsyncError> {
        let revision = self.store.revision();
        if let Some(cache) = self.entries_cache.lock().as_ref() {
            if cache.revision == revision {
                return Ok(cache.entries.clone());
            }
        }
        if self.entries_cache.lock().is_some() {
            self.render_state.lock().clear_memories();
        }
        let paths = self.store.list().await?;
        let mut entries = Vec::new();
        for path in paths {
            let Some(entry) = self.store.get(&path).await? else {
                continue;
            };
            entries.push(MemoryRecord {
                path,
                title: entry.title,
                body: entry.body,
                updated_at: entry.updated_at,
            });
        }
        *self.entries_cache.lock() = Some(EntriesCache {
            revision,
            entries: entries.clone(),
        });
        Ok(entries)
    }

    async fn commit_shas(&self) -> Result<HashMap<String, String>, WireAsyncError> {
        let revision = self.store.revision();
        if let Some(cache) = self.sha_cache.lock().as_ref() {
            if cache.revision == revision {
                return Ok(cache.map.clone());
            }
        }
        let map = self.repo.recent_commit_shas(MEMORY_SHA_HISTORY).await?;
        *self.sha_cache.lock() = Some(ShaCache {
            revision,
            map: map.clone(),
        });
        Ok(map)
    }

    fn scopes(&self, path: &str, body: &str) -> Vec<String> {
        if let Some(cached) = self.render_state.lock().scopes(path) {
            return cached.to_vec();
        }
        let parsed = parse_applies_to(body);
        self.render_state
            .lock()
            .set_scopes(path.to_string(), parsed.clone());
        parsed
    }

    fn view(&self, entry: &MemoryRecord, sha: String) -> Result<MemoryEntryView, WireAsyncError> {
        let timestamp = if entry.updated_at.is_nan() {
            "NaN".to_string()
        } else if entry.updated_at == f64::INFINITY {
            "Infinity".to_string()
        } else if entry.updated_at == f64::NEG_INFINITY {
            "-Infinity".to_string()
        } else {
            entry.updated_at.to_string()
        };
        let date = rquickjs::Runtime::new()
            .and_then(|runtime| {
                rquickjs::Context::full(&runtime).and_then(|context| {
                    context.with(|ctx| {
                        ctx.eval::<String, _>(format!(
                            "new Date({timestamp}).toISOString().slice(0, 10)"
                        ))
                    })
                })
            })
            .map_err(|error| error.to_string())?;
        Ok(MemoryEntryView {
            sha,
            date,
            title: entry.title.clone(),
            body: entry.body.clone(),
        })
    }
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher().is_match(path))
        .unwrap_or(false)
}
