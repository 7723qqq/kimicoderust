use indexmap::{IndexMap, IndexSet};

use super::types::CommitRecord;

const RENDER_STATE_CAP: usize = 512;

#[derive(Debug, Clone, Default)]
pub struct GitMemoryRenderState {
    paths: IndexSet<String>,
    commits: IndexSet<String>,
    memories: IndexSet<String>,
    directory_history: IndexMap<String, Vec<CommitRecord>>,
    memory_scopes: IndexMap<String, Vec<String>>,
}

impl GitMemoryRenderState {
    pub fn has_path(&self, path: &str) -> bool {
        self.paths.contains(path)
    }

    pub fn mark_path(&mut self, path: String) {
        self.paths.insert(path);
    }

    pub fn rendered_commits(&self) -> &IndexSet<String> {
        &self.commits
    }

    pub fn mark_commit(&mut self, sha: String) {
        self.commits.insert(sha);
    }

    pub fn has_memory(&self, sha: &str) -> bool {
        self.memories.contains(sha)
    }

    pub fn mark_memory(&mut self, sha: String) {
        self.memories.insert(sha);
    }

    pub fn clear_memories(&mut self) {
        self.memories.clear();
    }

    pub fn directory_commits(&self, directory: &str) -> Option<&[CommitRecord]> {
        self.directory_history
            .get(directory)
            .map(Vec::as_slice)
    }

    pub fn set_directory_commits(&mut self, directory: String, commits: Vec<CommitRecord>) {
        self.directory_history.insert(directory, commits);
    }

    pub fn scopes(&self, path: &str) -> Option<&[String]> {
        self.memory_scopes.get(path).map(Vec::as_slice)
    }

    pub fn set_scopes(&mut self, path: String, patterns: Vec<String>) {
        self.memory_scopes.insert(path, patterns);
    }

    pub fn clear(&mut self) {
        self.paths.clear();
        self.commits.clear();
        self.memories.clear();
        self.directory_history.clear();
        self.memory_scopes.clear();
    }

    pub fn trim(&mut self) {
        trim_set(&mut self.paths);
        trim_set(&mut self.commits);
        trim_set(&mut self.memories);
        trim_map(&mut self.directory_history);
        trim_map(&mut self.memory_scopes);
    }
}

fn trim_set(set: &mut IndexSet<String>) {
    while set.len() > RENDER_STATE_CAP {
        if set.shift_remove_index(0).is_none() {
            return;
        }
    }
}

fn trim_map<V>(map: &mut IndexMap<String, V>) {
    while map.len() > RENDER_STATE_CAP {
        if map.shift_remove_index(0).is_none() {
            return;
        }
    }
}
