use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KnowledgeCategory {
    CodingStyle,
    Pitfall,
    Architecture,
    Workflow,
}

impl KnowledgeCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CodingStyle => "coding-style",
            Self::Pitfall => "pitfall",
            Self::Architecture => "architecture",
            Self::Workflow => "workflow",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KnowledgeSource {
    Human,
    AiLearned,
    AiConfirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KnowledgeAddSource {
    Human,
    AiLearned,
}

impl KnowledgeAddSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::AiLearned => "ai-learned",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeEntry {
    pub id: String,
    pub category: KnowledgeCategory,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub scope: Option<String>,
    pub confidence: f64,
    pub source: KnowledgeSource,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeSearchResult {
    pub entry: KnowledgeEntry,
    pub relevance: f64,
    pub match_source: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeAddInput {
    pub title: String,
    pub category: KnowledgeCategory,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<KnowledgeAddSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeStats {
    pub total: usize,
    pub by_category: HashMap<String, usize>,
    pub by_source: HashMap<String, usize>,
    pub avg_confidence: f64,
}

pub trait IAgentKnowledgeService: Send + Sync {
    fn open(&self, project_db_path: &str, user_db_path: &str);
    fn search(
        &self,
        query: &str,
        scope_path: Option<&str>,
        tags: Option<&[String]>,
        limit: Option<usize>,
    ) -> Result<Vec<KnowledgeSearchResult>, WireAsyncError>;
    fn add(&self, input: KnowledgeAddInput) -> Option<KnowledgeEntry>;
    fn confirm(&self, id: &str) -> bool;
    fn remove(&self, id: &str) -> bool;
    fn stats(&self) -> KnowledgeStats;
    fn import_markdown(&self, markdown: &str) -> Vec<KnowledgeEntry>;
}
