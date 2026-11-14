//! Port of v2 `features/gitMemory`.
//!
//! `repo_block_from` renders the workspace repository's recent commits and
//! directory tree for the system prompt; `GitMemoryService::file_block` renders
//! the commits touching one file (plus its directory and the memory entries
//! whose `applies_to` matches it) into a Read / Write / Edit result.
//!
//! The index lives in memory and is keyed by HEAD, so a commit during a session
//! reloads it on the next file tool call. v2 caches the index on disk and
//! additionally implements `signalBlock`, `repoMap` and `fileBlock`; all three
//! are ported here. `signalBlock` is exposed for the per-turn reminder wiring.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::agent::tools::file_memory::FileMemoryProvider;
use crate::rpc::types::BoxFuture;
use crate::tools::memory_paths::parse_applies_to;
use crate::tools::memory_repo::{GitMemoryCommit, MemoryRepo, git_memory_enabled};
use crate::tools::memory_store;
use crate::turn_loop::types::ToolInfo;
use regex::Regex;

const GIT_MEMORY_WINDOW: usize = 1500;
const GIT_MEMORY_INDEX_VERSION: u32 = 2;
const GIT_MEMORY_REPO_COMMITS: usize = 25;
const GIT_MEMORY_FILE_COMMITS: usize = 8;
const GIT_MEMORY_DIR_COMMITS: usize = 5;
const GIT_MEMORY_MAX_FILES_PER_GROUP: usize = 25;
const GIT_MEMORY_TREE_DEPTH: usize = 3;
const MEMORY_BLOCK_MAX_ENTRIES: usize = 8;
const MEMORY_SHA_HISTORY: usize = 200;
const RENDER_STATE_CAP: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoHead {
    repo_root: String,
    prefix: String,
    head_sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitMemoryIndex {
    version: u32,
    repo_root: String,
    head_sha: String,
    window: usize,
    commits: Vec<GitMemoryCommit>,
}

enum IndexReadResult {
    Hit(GitMemoryIndex),
    Miss,
    Corrupt,
}

fn cache_path(store_dir: &Path, repo_root: &str) -> PathBuf {
    let normalized = repo_root.replace('\\', "/");
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let digest = hasher
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    store_dir.join("git-memory").join(format!("{digest}.json"))
}

fn legacy_cache_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".kimi-code").join("git-memory.json")
}

fn read_index(path: &Path, head_sha: &str, window: usize) -> IndexReadResult {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return IndexReadResult::Miss;
    };
    let Ok(index) = serde_json::from_str::<GitMemoryIndex>(&raw) else {
        return IndexReadResult::Corrupt;
    };
    if index.version != GIT_MEMORY_INDEX_VERSION
        || index.head_sha != head_sha
        || index.window != window
    {
        return IndexReadResult::Miss;
    }
    IndexReadResult::Hit(index)
}

fn remove_legacy_index(repo_root: &Path) -> bool {
    let path = legacy_cache_path(repo_root);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return false;
    };
    match serde_json::from_str::<GitMemoryIndex>(&raw) {
        Ok(index) if index.version == GIT_MEMORY_INDEX_VERSION => {
            std::fs::remove_file(&path).is_ok()
        }
        _ => false,
    }
}

fn write_index(path: &Path, index: &GitMemoryIndex) -> bool {
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return false;
    }
    serde_json::to_string(index)
        .map(|raw| std::fs::write(path, format!("{raw}\n")).is_ok())
        .unwrap_or(false)
}

#[derive(Default)]
struct RenderedSet {
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl RenderedSet {
    fn has(&self, key: &str) -> bool {
        self.set.contains(key)
    }

    fn insert(&mut self, key: String) {
        if !self.set.insert(key.clone()) {
            return;
        }
        self.order.push_back(key);
        while self.order.len() > RENDER_STATE_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }
    }

    fn snapshot(&self) -> HashSet<String> {
        self.set.clone()
    }
}

#[derive(Default)]
struct RenderState {
    paths: RenderedSet,
    commits: RenderedSet,
    memories: RenderedSet,
    directory_history: IndexMap<String, Vec<GitMemoryCommit>>,
    memory_scopes: IndexMap<String, Vec<String>>,
}

impl RenderState {
    fn trim(&mut self) {
        while self.directory_history.len() > RENDER_STATE_CAP {
            let Some(key) = self.directory_history.keys().next().cloned() else {
                break;
            };
            self.directory_history.shift_remove(&key);
        }
        while self.memory_scopes.len() > RENDER_STATE_CAP {
            let Some(key) = self.memory_scopes.keys().next().cloned() else {
                break;
            };
            self.memory_scopes.shift_remove(&key);
        }
    }
}

struct GitMemoryInner {
    workspace_root: PathBuf,
    memory_dir: PathBuf,
    enabled: bool,
    state: Mutex<RenderState>,
    index: Mutex<Option<Arc<GitMemoryIndex>>>,
}

#[derive(Clone)]
pub struct GitMemoryService {
    inner: Arc<GitMemoryInner>,
}

impl GitMemoryService {
    pub fn new(workspace_root: PathBuf, memory_dir: PathBuf) -> Self {
        Self::with_enabled(workspace_root, memory_dir, git_memory_enabled())
    }

    pub(crate) fn with_enabled(
        workspace_root: PathBuf,
        memory_dir: PathBuf,
        enabled: bool,
    ) -> Self {
        Self {
            inner: Arc::new(GitMemoryInner {
                workspace_root,
                memory_dir,
                enabled,
                state: Mutex::new(RenderState::default()),
                index: Mutex::new(None),
            }),
        }
    }

    pub fn file_block(&self, path: &str) -> String {
        self.inner.file_block(path)
    }
}

impl FileMemoryProvider for GitMemoryService {
    fn file_block<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<String, String>> {
        let service = self.clone();
        let path = path.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || service.file_block(&path))
                .await
                .map_err(|error| error.to_string())
        })
    }
}

impl GitMemoryInner {
    fn file_block(&self, path: &str) -> String {
        if !self.enabled {
            return String::new();
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.trim();
        let Some(head) = read_repo_head(&self.workspace_root) else {
            return String::new();
        };
        let Some(index) = self.ensure_index(&head) else {
            return String::new();
        };
        let key = to_repo_path(&head, path);
        if key.is_empty() || state.paths.has(&key) {
            return String::new();
        }
        let file_commits = commits_for_path(&index.commits, &key);
        let directory = directory_of(&key);
        let dir_commits = if directory.is_empty() {
            Vec::new()
        } else {
            directory_commits(&index.commits, &directory, &mut state)
        };
        let memories = self.entries_for_path(&key, &mut state);
        if file_commits.is_empty() && dir_commits.is_empty() && memories.is_empty() {
            return String::new();
        }
        state.paths.insert(key.clone());
        let mut block = project_file_block(
            &key,
            &file_commits,
            &directory,
            &dir_commits,
            &state.commits.snapshot(),
        );
        for commit in file_commits.iter().chain(dir_commits.iter()) {
            state.commits.insert(commit.sha.clone());
        }
        if !memories.is_empty() {
            let rendered = memories.join("\n");
            block = if block.is_empty() {
                rendered
            } else {
                format!("{block}\n{rendered}")
            };
        }
        block
    }

    fn ensure_index(&self, head: &RepoHead) -> Option<Arc<GitMemoryIndex>> {
        if let Ok(cache) = self.index.lock()
            && let Some(index) = cache.as_ref()
            && index.head_sha == head.head_sha
        {
            return Some(Arc::clone(index));
        }
        remove_legacy_index(Path::new(&head.repo_root));
        let path = cache_path(&self.memory_dir, &head.repo_root);
        let index = match read_index(&path, &head.head_sha, GIT_MEMORY_WINDOW) {
            IndexReadResult::Hit(index) => index,
            IndexReadResult::Miss | IndexReadResult::Corrupt => {
                let repo = MemoryRepo::new(self.workspace_root.clone());
                let index = GitMemoryIndex {
                    version: GIT_MEMORY_INDEX_VERSION,
                    repo_root: head.repo_root.clone(),
                    head_sha: head.head_sha.clone(),
                    window: GIT_MEMORY_WINDOW,
                    commits: repo.git_memory_commits(GIT_MEMORY_WINDOW),
                };
                write_index(&path, &index);
                index
            }
        };
        let index = Arc::new(index);
        if let Ok(mut cache) = self.index.lock() {
            *cache = Some(Arc::clone(&index));
        }
        Some(index)
    }

    fn entries_for_path(&self, key: &str, state: &mut RenderState) -> Vec<String> {
        let shas = MemoryRepo::new(self.memory_dir.clone()).recent_commit_shas(MEMORY_SHA_HISTORY);
        if shas.is_empty() {
            return Vec::new();
        }
        let mut views = Vec::new();
        for entry in memory_store::list_all_entries(&self.memory_dir) {
            if views.len() >= MEMORY_BLOCK_MAX_ENTRIES {
                break;
            }
            let Ok((content, _)) = memory_store::read(&self.memory_dir, &entry.rel_path) else {
                continue;
            };
            let patterns = state
                .memory_scopes
                .entry(entry.rel_path.clone())
                .or_insert_with(|| parse_applies_to(&content))
                .clone();
            if !patterns.iter().any(|pattern| glob_matches(pattern, key)) {
                continue;
            }
            let Some(sha) = shas.get(&entry.rel_path) else {
                continue;
            };
            if state.memories.has(sha) {
                continue;
            }
            state.memories.insert(sha.clone());
            let date = memory_entry_date(&self.memory_dir, &entry.rel_path).unwrap_or_default();
            views.push(project_memory_block(
                sha,
                &date,
                &entry.title,
                &memory_store::body(&content),
            ));
        }
        views
    }
}

const GIT_MEMORY_SIGNAL_COMMITS: usize = 2;
const GIT_MEMORY_SIGNAL_PATHS: usize = 2;

fn path_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"[\w./\\-]+\.(?:ts|tsx|js|jsx|mjs|cjs|rs|go|py|java|kt|rb|vue|json|jsonc|md|yaml|yml|toml)")
            .expect("static git-memory path regex")
    })
}

fn extract_signal_paths(message: &str, head: &RepoHead) -> Vec<String> {
    let mut paths = Vec::new();
    for matched in path_pattern().find_iter(message) {
        let key = to_repo_path(head, matched.as_str());
        if !paths.contains(&key) {
            paths.push(key);
        }
        if paths.len() >= GIT_MEMORY_SIGNAL_PATHS {
            break;
        }
    }
    paths
}

fn project_signal_block(path: &str, commits: &[GitMemoryCommit], seen: &HashSet<String>) -> String {
    if commits.is_empty() {
        return String::new();
    }
    let lines = commits
        .iter()
        .flat_map(|commit| project_commit(commit, seen))
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    format!(
        "<repo-memory signal=\"{path}\">
{lines}
</repo-memory>"
    )
}

impl GitMemoryService {
    pub fn signal_block(&self, message: &str) -> String {
        self.inner.signal_block(message)
    }
}

impl GitMemoryInner {
    fn signal_block(&self, message: &str) -> String {
        if !self.enabled {
            return String::new();
        }
        let Some(head) = read_repo_head(&self.workspace_root) else {
            return String::new();
        };
        let paths = extract_signal_paths(message, &head);
        if paths.is_empty() {
            return String::new();
        }
        let Some(index) = self.ensure_index(&head) else {
            return String::new();
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.trim();
        let mut blocks = Vec::new();
        for path in paths {
            let commits = commits_for_path(&index.commits, &path);
            let commits = &commits[..commits.len().min(GIT_MEMORY_SIGNAL_COMMITS)];
            let block = project_signal_block(&path, commits, &state.commits.snapshot());
            if block.is_empty() {
                continue;
            }
            blocks.push(block);
            for commit in commits {
                state.commits.insert(commit.sha.clone());
            }
        }
        blocks.join(
            "
",
        )
    }
}

pub const REPO_MAP_TOOL_NAME: &str = "RepoMap";
pub const REPO_MAP_EMPTY_OUTPUT: &str = "No tracked paths matched. Check the path prefix against the repository layout, or raise the depth.";
const REPO_MAP_MAX_LINES: usize = 2000;

const REPO_MAP_DESCRIPTION: &str = r#"List the paths tracked by git in this repository, as recorded at HEAD.

Use it to learn the shape of a repository or a subtree in one call: which packages, directories and files exist, and where they live. The output is the raw git path list, so it reflects the committed tree rather than untracked or ignored files in the working directory.

- Omit `path` for the whole repository, or pass a directory or file prefix to narrow the listing. Give `path` relative to the repository root (for example `packages/agent-core-v2`); when nothing matches there, it is also tried relative to the working directory.
- `depth` limits how deep the listing goes below `path`: depth 1 returns the direct children of `path`, depth 2 their children, and so on. The default is 3.
- The output is capped. When it is, the last line says how many paths were left out — narrow `path` or raise `depth` to see them."#;

pub fn repo_map_tool_def() -> ToolInfo {
    ToolInfo {
        name: REPO_MAP_TOOL_NAME.into(),
        description: REPO_MAP_DESCRIPTION.into(),
        input_schema: serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Repository-relative path prefix to list, for example \"packages/agent-core-v2\". Omit to list the whole repository."
                },
                "depth": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 20,
                    "description": "How deep to list below the prefix: depth 1 returns the direct children, depth 2 their children, and so on. The default is 3."
                }
            },
            "additionalProperties": false
        }),
    }
}

impl GitMemoryService {
    pub fn repo_map(&self, path: Option<&str>, depth: usize) -> String {
        self.inner.repo_map(path, depth)
    }
}

impl GitMemoryInner {
    fn repo_map(&self, path: Option<&str>, depth: usize) -> String {
        if !self.enabled {
            return String::new();
        }
        let Some(head) = read_repo_head(&self.workspace_root) else {
            return String::new();
        };
        let repo = MemoryRepo::new(self.workspace_root.clone());
        for candidate in repo_path_candidates(path, &head.prefix) {
            let Some(entries) = repo.tree_paths(candidate.as_deref()) else {
                return String::new();
            };
            let mut lines: Vec<String> = entries
                .into_iter()
                .filter(|entry| is_within_depth(entry, candidate.as_deref(), depth))
                .collect();
            if lines.is_empty() {
                continue;
            }
            if lines.len() > REPO_MAP_MAX_LINES {
                let extra = lines.len() - REPO_MAP_MAX_LINES;
                lines.truncate(REPO_MAP_MAX_LINES);
                lines.push(format!(
                    "... and {extra} more paths; narrow the path or raise the depth"
                ));
            }
            return lines.join(
                "
",
            );
        }
        String::new()
    }
}

fn repo_path_candidates(path: Option<&str>, prefix: &str) -> Vec<Option<String>> {
    let Some(path) = path.filter(|path| !path.is_empty()) else {
        return vec![None];
    };
    let normalized = path.replace('\\', "/");
    let normalized = normalized
        .strip_prefix("./")
        .unwrap_or(&normalized)
        .to_string();
    let mut candidates = vec![Some(normalized.clone())];
    let prefix = prefix.trim();
    if !prefix.is_empty() && !normalized.starts_with(prefix) {
        candidates.push(Some(format!("{prefix}{normalized}")));
    }
    candidates
}

fn is_within_depth(entry: &str, base: Option<&str>, depth: usize) -> bool {
    match base.filter(|base| !base.is_empty()) {
        None => entry.split('/').count() <= depth,
        Some(base) => {
            if entry == base {
                return false;
            }
            let prefix = format!("{base}/");
            let Some(rest) = entry.strip_prefix(&prefix) else {
                return false;
            };
            rest.split('/').count() <= depth
        }
    }
}

pub fn repo_block(workspace_root: &Path, memory_dir: &Path) -> String {
    repo_block_from(workspace_root, memory_dir, git_memory_enabled())
}

pub(crate) fn repo_block_from(workspace_root: &Path, memory_dir: &Path, enabled: bool) -> String {
    if !enabled {
        return String::new();
    }
    let workspace = MemoryRepo::new(workspace_root.to_path_buf());
    let commits = workspace.git_memory_commits(GIT_MEMORY_REPO_COMMITS);
    let mut parts = Vec::new();
    if !commits.is_empty() {
        let seen = HashSet::new();
        let lines = commits
            .iter()
            .flat_map(|commit| project_commit(commit, &seen))
            .collect::<Vec<_>>()
            .join("\n");
        parts.push(format!("<repo-memory>\n{lines}\n</repo-memory>"));
    }
    let dirs = workspace
        .git_memory_dirs()
        .into_iter()
        .filter(|dir| dir.split('/').count() <= GIT_MEMORY_TREE_DEPTH)
        .collect::<Vec<_>>();
    if !dirs.is_empty() {
        parts.push(format!(
            "<repo-tree depth=\"{GIT_MEMORY_TREE_DEPTH}\">\n{}\n</repo-tree>",
            dirs.join("\n")
        ));
    }
    let memory = MemoryRepo::new(memory_dir.to_path_buf());
    parts.extend(unscoped_memory_blocks(memory_dir, &memory));
    parts.join("\n")
}

fn unscoped_memory_blocks(memory_dir: &Path, repo: &MemoryRepo) -> Vec<String> {
    let shas = repo.recent_commit_shas(MEMORY_SHA_HISTORY);
    let mut blocks = Vec::new();
    for entry in memory_store::list_all_entries(memory_dir) {
        if blocks.len() >= MEMORY_BLOCK_MAX_ENTRIES {
            break;
        }
        let Ok((content, _)) = memory_store::read(memory_dir, &entry.rel_path) else {
            continue;
        };
        if !parse_applies_to(&content).is_empty() {
            continue;
        }
        let Some(sha) = shas.get(&entry.rel_path) else {
            continue;
        };
        let date = memory_entry_date(memory_dir, &entry.rel_path).unwrap_or_default();
        blocks.push(project_memory_block(
            sha,
            &date,
            &entry.title,
            &memory_store::body(&content),
        ));
    }
    blocks
}

fn read_repo_head(workspace_root: &Path) -> Option<RepoHead> {
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args(["rev-parse", "--show-toplevel", "--show-prefix", "HEAD"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.split('\n');
    let repo_root = lines.next()?.trim().to_string();
    let prefix = lines.next().unwrap_or("").trim().to_string();
    let head_sha = lines.next().unwrap_or("").trim().to_string();
    if repo_root.is_empty() || head_sha.is_empty() {
        return None;
    }
    Some(RepoHead {
        repo_root,
        prefix,
        head_sha,
    })
}

fn to_repo_path(head: &RepoHead, path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let stripped = strip_root(&normalized, head.repo_root.trim_end_matches('/'));
    let relative = stripped.unwrap_or(&normalized);
    let relative = relative.strip_prefix("./").unwrap_or(relative);
    let prefix = head.prefix.trim();
    if prefix.is_empty() || relative.starts_with(prefix) {
        return relative.to_string();
    }
    format!("{prefix}{relative}")
}

fn strip_root<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    if root.is_empty() {
        return None;
    }
    let candidate = path.get(..root.len())?;
    let rest = path.get(root.len()..)?;
    if !candidate.eq_ignore_ascii_case(root) {
        return None;
    }
    rest.strip_prefix('/')
}

fn directory_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => String::new(),
        Some(index) => path[..index].to_string(),
    }
}

fn commits_for_path(commits: &[GitMemoryCommit], key: &str) -> Vec<GitMemoryCommit> {
    commits
        .iter()
        .filter(|commit| commit.files.iter().any(|file| file == key))
        .take(GIT_MEMORY_FILE_COMMITS)
        .cloned()
        .collect()
}

fn directory_commits(
    commits: &[GitMemoryCommit],
    directory: &str,
    state: &mut RenderState,
) -> Vec<GitMemoryCommit> {
    if let Some(cached) = state.directory_history.get(directory) {
        return cached.clone();
    }
    let prefix = format!("{directory}/");
    let found: Vec<GitMemoryCommit> = commits
        .iter()
        .filter(|commit| commit.files.iter().any(|file| file.starts_with(&prefix)))
        .take(GIT_MEMORY_DIR_COMMITS)
        .cloned()
        .collect();
    state
        .directory_history
        .insert(directory.to_string(), found.clone());
    found
}

fn project_file_block(
    key: &str,
    commits: &[GitMemoryCommit],
    directory: &str,
    dir_commits: &[GitMemoryCommit],
    seen: &HashSet<String>,
) -> String {
    let mut local = seen.clone();
    let mut lines = Vec::new();
    for commit in commits {
        lines.extend(project_commit(commit, &local));
        local.insert(commit.sha.clone());
    }
    let mut parts = Vec::new();
    if !lines.is_empty() {
        parts.push(lines.join("\n"));
    }
    if !directory.is_empty() && !dir_commits.is_empty() {
        parts.push(project_dir_block(directory, dir_commits, &local));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!(
        "<file-memory path=\"{key}\">\n{}\n</file-memory>",
        parts.join("\n")
    )
}

fn project_dir_block(
    directory: &str,
    commits: &[GitMemoryCommit],
    seen: &HashSet<String>,
) -> String {
    let lines = commits
        .iter()
        .flat_map(|commit| project_commit(commit, seen))
        .collect::<Vec<_>>()
        .join("\n");
    format!("<dir-memory path=\"{directory}\">\n{lines}\n</dir-memory>")
}

fn project_commit(commit: &GitMemoryCommit, seen: &HashSet<String>) -> Vec<String> {
    let short = commit.sha.chars().take(7).collect::<String>();
    if seen.contains(&commit.sha) {
        return vec![format!("see commit {short} above")];
    }
    let mut lines = vec![
        format!("Commit {short} {} {}", commit.date, commit.author),
        format!("  {}", commit.subject),
    ];
    if !commit.body.is_empty() {
        for line in commit.body.lines() {
            lines.push(format!("  {line}"));
        }
    }
    if commit.files.len() > GIT_MEMORY_MAX_FILES_PER_GROUP {
        return lines;
    }
    if !commit.body.is_empty() && !commit.files.is_empty() {
        lines.push(String::new());
    }
    for file in &commit.files {
        lines.push(format!("  {file}"));
    }
    lines
}

fn project_memory_block(sha: &str, date: &str, title: &str, body: &str) -> String {
    format!("<memory sha=\"{sha}\" date=\"{date}\">\n{title}\n{body}\n</memory>")
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher().is_match(path))
        .unwrap_or(false)
}

fn memory_entry_date(memory_dir: &Path, rel_path: &str) -> Option<String> {
    let modified = std::fs::metadata(memory_dir.join(rel_path))
        .ok()?
        .modified()
        .ok()?;
    let date = chrono::DateTime::<chrono::Utc>::from(modified);
    Some(date.format("%Y-%m-%d").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn memory_repo_for(dir: &Path) -> MemoryRepo {
        MemoryRepo::new(dir.to_path_buf())
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn service_for(workspace: &Path, memory: &Path) -> GitMemoryService {
        GitMemoryService::with_enabled(workspace.to_path_buf(), memory.to_path_buf(), true)
    }

    #[test]
    fn render_state_evicts_the_oldest_insertions_first() {
        let mut state = RenderState::default();
        for index in 0..RENDER_STATE_CAP + 8 {
            state
                .directory_history
                .insert(format!("dir-{index}"), Vec::new());
        }
        state.trim();
        assert_eq!(state.directory_history.len(), RENDER_STATE_CAP);
        for index in 0..8 {
            assert!(
                !state.directory_history.contains_key(&format!("dir-{index}")),
                "dir-{index} is the oldest and must be evicted first"
            );
        }
        assert!(state.directory_history.contains_key("dir-8"));
        assert!(
            state
                .directory_history
                .contains_key(&format!("dir-{}", RENDER_STATE_CAP + 7))
        );

        for index in 0..RENDER_STATE_CAP + 4 {
            state
                .memory_scopes
                .insert(format!("scope-{index}"), Vec::new());
        }
        state.trim();
        assert_eq!(state.memory_scopes.len(), RENDER_STATE_CAP);
        assert!(!state.memory_scopes.contains_key("scope-0"));
        assert!(state.memory_scopes.contains_key("scope-4"));
    }

    fn cached_index(head_sha: &str) -> GitMemoryIndex {
        GitMemoryIndex {
            version: GIT_MEMORY_INDEX_VERSION,
            repo_root: "/repo".into(),
            head_sha: head_sha.into(),
            window: GIT_MEMORY_WINDOW,
            commits: vec![GitMemoryCommit {
                sha: "a".repeat(40),
                date: "2026-01-01".into(),
                author: "anne".into(),
                subject: "s".into(),
                body: "b".into(),
                files: vec!["src/lib.rs".into()],
            }],
        }
    }

    #[test]
    fn cache_path_hashes_the_normalized_repo_root() {
        let first = cache_path(Path::new("/store"), "C:\\repo\\sub");
        let second = cache_path(Path::new("/store"), "C:/repo/sub");
        assert_eq!(first, second);
        let name = first.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(name.len(), 21, "{name}");
        assert!(name.ends_with(".json"), "{name}");
    }

    #[test]
    fn index_round_trips_through_the_cache_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(dir.path(), "/repo");
        let index = cached_index(&"b".repeat(40));
        assert!(write_index(&path, &index));
        assert!(matches!(
            read_index(&path, &"b".repeat(40), GIT_MEMORY_WINDOW),
            IndexReadResult::Hit(read) if read == index
        ));
    }

    #[test]
    fn read_index_classifies_missing_corrupt_and_stale_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(dir.path(), "/repo");
        assert!(matches!(
            read_index(&path, &"b".repeat(40), GIT_MEMORY_WINDOW),
            IndexReadResult::Miss
        ));

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        assert!(matches!(
            read_index(&path, &"b".repeat(40), GIT_MEMORY_WINDOW),
            IndexReadResult::Corrupt
        ));

        let mut stale = cached_index(&"b".repeat(40));
        stale.head_sha = "c".repeat(40);
        write_index(&path, &stale);
        assert!(matches!(
            read_index(&path, &"b".repeat(40), GIT_MEMORY_WINDOW),
            IndexReadResult::Miss
        ));

        let mut wrong_window = cached_index(&"b".repeat(40));
        wrong_window.window = GIT_MEMORY_WINDOW + 1;
        write_index(&path, &wrong_window);
        assert!(matches!(
            read_index(&path, &"b".repeat(40), GIT_MEMORY_WINDOW),
            IndexReadResult::Miss
        ));
    }

    #[test]
    fn legacy_cache_removal_keeps_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = legacy_cache_path(dir.path());
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();

        std::fs::write(&legacy, "{ not json").unwrap();
        assert!(!remove_legacy_index(dir.path()));
        assert!(legacy.exists());

        let mut foreign = cached_index(&"b".repeat(40));
        foreign.version = GIT_MEMORY_INDEX_VERSION + 1;
        write_index(&legacy, &foreign);
        assert!(!remove_legacy_index(dir.path()));
        assert!(legacy.exists());

        write_index(&legacy, &cached_index(&"b".repeat(40)));
        assert!(remove_legacy_index(dir.path()));
        assert!(!legacy.exists());
    }

    #[test]
    fn file_block_renders_file_and_directory_sections_then_dedupes() {
        if !git_available() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let memory = tempfile::tempdir().unwrap();
        let repo = memory_repo_for(workspace.path());
        write(&workspace.path().join("src/a.ts"), "one\n");
        assert!(repo.commit("add a"));
        write(&workspace.path().join("src/b.ts"), "b\n");
        assert!(repo.commit("add b"));
        write(&workspace.path().join("src/a.ts"), "two\n");
        assert!(repo.commit("change a"));

        let service = service_for(workspace.path(), memory.path());
        let block = service.file_block("src/a.ts");
        assert!(block.starts_with("<file-memory path=\"src/a.ts\">"));
        assert!(block.contains("change a"));
        assert!(block.contains("add a"));
        assert!(block.contains("<dir-memory path=\"src\">"));
        assert!(block.contains("add b"));
        assert!(block.contains("see commit "));
        assert_eq!(service.file_block("src/a.ts"), "");
    }

    #[test]
    fn file_block_accepts_an_absolute_path_and_returns_empty_without_history() {
        if !git_available() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let memory = tempfile::tempdir().unwrap();
        let repo = memory_repo_for(workspace.path());
        write(&workspace.path().join("src/a.ts"), "one\n");
        assert!(repo.commit("add a"));

        let service = service_for(workspace.path(), memory.path());
        let head = read_repo_head(workspace.path()).expect("temporary workspace is a repository");
        let absolute = Path::new(&head.repo_root).join("src/a.ts");
        let block = service.file_block(absolute.to_str().unwrap());
        assert!(block.starts_with("<file-memory path=\"src/a.ts\">"));

        assert_eq!(service.file_block("docs/missing.md"), "");
    }

    #[test]
    fn file_block_includes_memory_entries_whose_applies_to_matches() {
        if !git_available() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let memory = tempfile::tempdir().unwrap();
        write(&workspace.path().join("src/a.ts"), "one\n");
        assert!(memory_repo_for(workspace.path()).commit("add a"));
        write(
            &memory.path().join("global/scoped.md"),
            "---\napplies_to: [\"src/**\"]\n---\n# Scoped\n\nRemember the file.\n",
        );
        write(
            &memory.path().join("global/other.md"),
            "---\napplies_to: [\"docs/**\"]\n---\n# Other\n\nNot this file.\n",
        );
        assert!(memory_repo_for(memory.path()).commit("remember: Scoped"));

        let block = service_for(workspace.path(), memory.path()).file_block("src/a.ts");
        assert!(block.contains("<memory sha=\""));
        assert!(block.contains("Remember the file."));
        assert!(!block.contains("Not this file."));
    }

    #[test]
    fn repo_block_reads_the_workspace_repository_and_unscoped_memory() {
        if !git_available() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let memory = tempfile::tempdir().unwrap();
        write(&workspace.path().join("README.md"), "hello\n");
        assert!(memory_repo_for(workspace.path()).commit("project change"));
        write(&workspace.path().join("src/lib.rs"), "fn main() {}\n");
        assert!(memory_repo_for(workspace.path()).commit("add src"));
        write(
            &memory.path().join("global/note.md"),
            "# Note\n\nRemember this.\n",
        );
        write(
            &memory.path().join("global/scoped.md"),
            "---\napplies_to: [\"src/**\"]\n---\n# Scoped\n\nOnly for source files.\n",
        );
        assert!(memory_repo_for(memory.path()).commit("remember: Note"));

        let block = repo_block_from(workspace.path(), memory.path(), true);
        assert!(block.contains("<repo-memory>"));
        assert!(block.contains("project change"));
        assert!(block.contains("add src"));
        assert!(block.contains("<repo-tree depth=\"3\">"));
        assert!(block.contains("src"));
        assert!(block.contains("<memory sha=\""));
        assert!(block.contains("Remember this."));
        assert!(!block.contains("Only for source files."));
        assert!(!block.contains("remember: Note"));
        assert_eq!(repo_block_from(workspace.path(), memory.path(), false), "");
    }

    #[test]
    fn repo_map_filters_by_depth_and_prefix() {
        if !git_available() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let memory = tempfile::tempdir().unwrap();
        write(
            &workspace.path().join("README.md"),
            "hello
",
        );
        write(
            &workspace.path().join("src/a.ts"),
            "a
",
        );
        write(
            &workspace.path().join("src/nested/b.ts"),
            "b
",
        );
        assert!(memory_repo_for(workspace.path()).commit("seed"));

        let service = service_for(workspace.path(), memory.path());
        let root = service.repo_map(None, 1);
        assert!(root.contains("README.md"), "{root}");
        assert!(root.contains("src"), "{root}");
        assert!(!root.contains("src/a.ts"), "{root}");

        let src = service.repo_map(Some("src"), 2);
        assert!(src.contains("src/a.ts"), "{src}");
        assert!(src.contains("src/nested/b.ts"), "{src}");

        let shallow = service.repo_map(Some("src"), 1);
        assert!(shallow.contains("src/nested"), "{shallow}");
        assert!(!shallow.contains("src/nested/b.ts"), "{shallow}");

        assert_eq!(service.repo_map(Some("missing"), 3), "");
    }

    #[test]
    fn is_within_depth_handles_the_root_and_prefix_cases() {
        assert!(is_within_depth("src", None, 1));
        assert!(!is_within_depth("src/a.ts", None, 1));
        assert!(is_within_depth("src/a.ts", Some("src"), 1));
        assert!(!is_within_depth("src/a.ts", Some("src"), 0));
        assert!(!is_within_depth("src", Some("src"), 3));
        assert!(!is_within_depth("docs/a.md", Some("src"), 3));
    }

    #[test]
    fn the_repo_map_tool_def_is_the_v2_schema() {
        let def = repo_map_tool_def();
        assert_eq!(def.name, "RepoMap");
        assert!(def.description.starts_with("List the paths tracked by git"));
        assert_eq!(def.input_schema["properties"]["depth"]["maximum"], 20);
        assert_eq!(def.input_schema["additionalProperties"], false);
        assert!(def.input_schema.get("required").is_none());
    }

    #[test]
    fn signal_block_renders_commits_for_a_mentioned_path_and_dedupes() {
        if !git_available() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let memory = tempfile::tempdir().unwrap();
        write(
            &workspace.path().join("src/a.ts"),
            "a
",
        );
        assert!(memory_repo_for(workspace.path()).commit("add a"));

        let service = service_for(workspace.path(), memory.path());
        let block = service.signal_block("please fix src/a.ts");
        assert!(
            block.contains("<repo-memory signal=\"src/a.ts\">"),
            "{block}"
        );
        assert!(block.contains("add a"), "{block}");
        let again = service.signal_block("please fix src/a.ts");
        assert!(again.contains("see commit"), "{again}");
        assert!(!again.contains("add a"), "{again}");
        assert_eq!(service.signal_block("no path here"), "");
    }

    #[test]
    fn signal_block_takes_at_most_two_paths() {
        if !git_available() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let memory = tempfile::tempdir().unwrap();
        for name in ["a", "b", "c"] {
            write(
                &workspace.path().join(format!("src/{name}.ts")),
                "x
",
            );
            assert!(memory_repo_for(workspace.path()).commit(&format!("add {name}")));
        }

        let service = service_for(workspace.path(), memory.path());
        let block = service.signal_block("touch src/a.ts, src/b.ts and src/c.ts");
        assert!(block.contains("signal=\"src/a.ts\""), "{block}");
        assert!(block.contains("signal=\"src/b.ts\""), "{block}");
        assert!(!block.contains("signal=\"src/c.ts\""), "{block}");
    }

    #[test]
    fn to_repo_path_strips_the_root_and_applies_the_workspace_prefix() {
        let head = RepoHead {
            repo_root: "C:/work/project".to_string(),
            prefix: String::new(),
            head_sha: "a".repeat(40),
        };
        assert_eq!(
            to_repo_path(&head, "C:\\work\\project\\src\\a.ts"),
            "src/a.ts"
        );
        assert_eq!(to_repo_path(&head, "./src/a.ts"), "src/a.ts");
        assert_eq!(to_repo_path(&head, "src/a.ts"), "src/a.ts");

        let nested = RepoHead {
            repo_root: "C:/work/project".to_string(),
            prefix: "packages/app/".to_string(),
            head_sha: "a".repeat(40),
        };
        assert_eq!(to_repo_path(&nested, "src/a.ts"), "packages/app/src/a.ts");
        assert_eq!(
            to_repo_path(&nested, "packages/app/src/a.ts"),
            "packages/app/src/a.ts"
        );
    }
}
