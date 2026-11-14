use std::fmt::{Display, Formatter};
use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::json;

use crate::_base::log::log::ILogService;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::wire::record::WireAsyncError;

use super::knowledge::{
    IAgentKnowledgeService, KnowledgeAddInput, KnowledgeEntry, KnowledgeSearchResult, KnowledgeStats,
};
use super::sqlite_knowledge;

pub const INJECTION_CONFIDENCE_FLOOR: f64 = 0.5;

#[derive(Default)]
struct KnowledgeServiceState {
    initialized: bool,
    current_db_path: Option<String>,
}

pub struct AgentKnowledgeService {
    bootstrap: Arc<dyn IBootstrapService>,
    log: Arc<dyn ILogService>,
    state: Mutex<KnowledgeServiceState>,
}

impl AgentKnowledgeService {
    pub fn new(bootstrap: Arc<dyn IBootstrapService>, log: Arc<dyn ILogService>) -> Self {
        Self {
            bootstrap,
            log,
            state: Mutex::new(KnowledgeServiceState::default()),
        }
    }

    fn ensure_database(&self) -> bool {
        if self.state.lock().initialized {
            return true;
        }
        if let Ok(override_path) = std::env::var("KIMI_KNOWLEDGE_DB") {
            if !override_path.is_empty() {
                return self.open_at(&override_path);
            }
        }
        let project_db = format!("{}/.kimi-code/knowledge.db", self.bootstrap.cwd());
        if Path::new(&project_db).exists() && self.open_at(&project_db) {
            return true;
        }
        let user_db = format!("{}/knowledge.db", self.bootstrap.home_dir());
        self.open_at(&user_db)
    }

    fn open_existing(&self) -> bool {
        if self.state.lock().initialized {
            return true;
        }
        if let Ok(override_path) = std::env::var("KIMI_KNOWLEDGE_DB") {
            if !override_path.is_empty() {
                return Path::new(&override_path).exists() && self.open_at(&override_path);
            }
        }
        let project_db = format!("{}/.kimi-code/knowledge.db", self.bootstrap.cwd());
        if Path::new(&project_db).exists() {
            return self.open_at(&project_db);
        }
        let user_db = format!("{}/knowledge.db", self.bootstrap.home_dir());
        Path::new(&user_db).exists() && self.open_at(&user_db)
    }

    fn open_at(&self, path: &str) -> bool {
        match sqlite_knowledge::knowledge_open(path) {
            Ok(()) => {
                let mut state = self.state.lock();
                state.initialized = true;
                state.current_db_path = Some(path.to_string());
                true
            }
            Err(error) => {
                self.log.warn(
                    "failed to open knowledge database",
                    Some(json!({"error": error, "path": path})),
                );
                false
            }
        }
    }
}

impl IAgentKnowledgeService for AgentKnowledgeService {
    fn open(&self, project_db_path: &str, user_db_path: &str) {
        if self.state.lock().current_db_path.is_some() {
            sqlite_knowledge::knowledge_close();
        }
        match sqlite_knowledge::knowledge_open(project_db_path) {
            Ok(()) => {
                let mut state = self.state.lock();
                state.initialized = true;
                state.current_db_path = Some(project_db_path.to_string());
                return;
            }
            Err(error) => {
                self.log.warn(
                    "open(projectDbPath) failed, trying userDbPath",
                    Some(json!({"error": error, "projectDbPath": project_db_path})),
                );
            }
        }
        match sqlite_knowledge::knowledge_open(user_db_path) {
            Ok(()) => {
                let mut state = self.state.lock();
                state.initialized = true;
                state.current_db_path = Some(user_db_path.to_string());
            }
            Err(error) => {
                self.state.lock().initialized = false;
                self.log.error(
                    "open() failed for both project and user DB paths",
                    Some(json!({"error": error})),
                );
            }
        }
    }

    fn search(
        &self,
        query: &str,
        scope_path: Option<&str>,
        tags: Option<&[String]>,
        limit: Option<usize>,
    ) -> Result<Vec<KnowledgeSearchResult>, WireAsyncError> {
        if !self.open_existing() {
            return Ok(Vec::new());
        }
        let tags = tags.map(|tags| tags.join(","));
        let json = sqlite_knowledge::knowledge_search(
            query,
            scope_path,
            tags.as_deref(),
            limit.unwrap_or(5),
            INJECTION_CONFIDENCE_FLOOR,
        );
        match json {
            Ok(json) => match serde_json::from_str::<Vec<KnowledgeSearchResult>>(&json) {
                Ok(results) => Ok(results
                    .into_iter()
                    .filter(|result| result.entry.confidence >= INJECTION_CONFIDENCE_FLOOR)
                    .collect()),
                Err(error) => {
                    self.log.error(
                        "knowledge.search failed",
                        Some(json!({"error": error.to_string(), "query": query})),
                    );
                    Err(Box::new(error) as WireAsyncError)
                }
            },
            Err(error) => {
                self.log.error(
                    "knowledge.search failed",
                    Some(json!({"error": error, "query": query})),
                );
                Err(Box::new(KnowledgeError(error)))
            }
        }
    }

    fn add(&self, input: KnowledgeAddInput) -> Option<KnowledgeEntry> {
        if !self.ensure_database() {
            return None;
        }
        let tags = input.tags.unwrap_or_default().join(",");
        let result = sqlite_knowledge::knowledge_add(
            &input.title,
            input.category.as_str(),
            &input.content,
            &tags,
            input.scope.as_deref(),
            input.source.unwrap_or(super::knowledge::KnowledgeAddSource::AiLearned).as_str(),
            input.confidence.unwrap_or(0.7),
        );
        match result {
            Ok(json) => match serde_json::from_str(&json) {
                Ok(entry) => Some(entry),
                Err(error) => {
                    self.log.warn(
                        "knowledge.add failed (may be a duplicate)",
                        Some(json!({"error": error.to_string(), "title": input.title})),
                    );
                    None
                }
            },
            Err(error) => {
                self.log.warn(
                    "knowledge.add failed (may be a duplicate)",
                    Some(json!({"error": error, "title": input.title})),
                );
                None
            }
        }
    }

    fn confirm(&self, id: &str) -> bool {
        if !self.ensure_database() {
            return false;
        }
        match sqlite_knowledge::knowledge_confirm(id) {
            Ok(result) => result,
            Err(error) => {
                self.log.error("knowledge.confirm failed", Some(json!({"error": error, "id": id})));
                false
            }
        }
    }

    fn remove(&self, id: &str) -> bool {
        if !self.ensure_database() {
            return false;
        }
        match sqlite_knowledge::knowledge_remove(id) {
            Ok(result) => result,
            Err(error) => {
                self.log.error("knowledge.remove failed", Some(json!({"error": error, "id": id})));
                false
            }
        }
    }

    fn stats(&self) -> KnowledgeStats {
        if !self.open_existing() {
            return KnowledgeStats::default();
        }
        match sqlite_knowledge::knowledge_stats() {
            Ok(json) => match serde_json::from_str(&json) {
                Ok(stats) => stats,
                Err(error) => {
                    self.log.error(
                        "knowledge.stats failed",
                        Some(json!({"error": error.to_string()})),
                    );
                    KnowledgeStats::default()
                }
            },
            Err(error) => {
                self.log.error("knowledge.stats failed", Some(json!({"error": error})));
                KnowledgeStats::default()
            }
        }
    }

    fn import_markdown(&self, markdown: &str) -> Vec<KnowledgeEntry> {
        if !self.ensure_database() {
            return Vec::new();
        }
        match sqlite_knowledge::knowledge_import(markdown) {
            Ok(json) => match serde_json::from_str::<KnowledgeImportResult>(&json) {
                Ok(KnowledgeImportResult::Entries(entries)) => entries,
                Ok(KnowledgeImportResult::Object { entries, skipped }) => {
                    if let Some(skipped) = skipped.filter(|skipped| !skipped.is_empty()) {
                        self.log.warn(
                            "knowledge.importMarkdown skipped some entries",
                            Some(json!({"skipped": skipped})),
                        );
                    }
                    entries.unwrap_or_default()
                }
                Err(error) => {
                    self.log.error(
                        "knowledge.importMarkdown failed",
                        Some(json!({"error": error.to_string()})),
                    );
                    Vec::new()
                }
            },
            Err(error) => {
                self.log.error(
                    "knowledge.importMarkdown failed",
                    Some(json!({"error": error})),
                );
                Vec::new()
            }
        }
    }
}

impl AgentKnowledgeService {
    pub fn reject(&self, id: &str) -> bool {
        if !self.ensure_database() {
            return false;
        }
        match sqlite_knowledge::knowledge_remove(id) {
            Ok(result) => result,
            Err(error) => {
                self.log.error("knowledge.reject failed", Some(json!({"error": error, "id": id})));
                false
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum KnowledgeImportResult {
    Entries(Vec<KnowledgeEntry>),
    Object {
        #[serde(default)]
        entries: Option<Vec<KnowledgeEntry>>,
        #[serde(default)]
        skipped: Option<Vec<String>>,
    },
}

#[derive(Debug)]
struct KnowledgeError(String);

impl Display for KnowledgeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for KnowledgeError {}
