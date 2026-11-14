use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

pub const GIT_MEMORY_WINDOW: usize = 1500;
pub const GIT_MEMORY_REPO_COMMITS: usize = 25;
pub const GIT_MEMORY_FILE_COMMITS: usize = 8;
pub const GIT_MEMORY_DIR_COMMITS: usize = 5;
pub const GIT_MEMORY_SIGNAL_COMMITS: usize = 2;
pub const GIT_MEMORY_SIGNAL_PATHS: usize = 2;
pub const GIT_MEMORY_MAX_FILES_PER_GROUP: usize = 25;
pub const GIT_MEMORY_TREE_DEPTH: usize = 3;
pub const GIT_MEMORY_INDEX_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRecord {
    pub sha: String,
    pub date: String,
    pub author: String,
    pub subject: String,
    pub body: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitMemoryIndex {
    pub version: u32,
    pub repo_root: String,
    pub head_sha: String,
    pub window: usize,
    pub computed_at: String,
    pub commits: Vec<CommitRecord>,
    pub by_path: IndexMap<String, Vec<usize>>,
    pub dirs: Vec<String>,
}
