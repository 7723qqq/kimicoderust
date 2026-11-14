use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::features::git_memory::flag::GIT_MEMORY_FLAG_ID;
use crate::features::associativeMemory::flag::ASSOCIATIVE_MEMORY_FLAG_ID;
use crate::i18n::{LocalizedText, i18n_params};
use crate::wire::record::WireAsyncError;

use super::super::memory_paths::{MemoryEntry, MemoryScope, detect_type, extract_title, legacy_project_id_from_cwd, memory_dir, parse_applies_to, project_id_from_cwd, scope_dir};
use super::super::memory_repo::IMemoryRepo;
use super::super::memory_store::IMemoryStore;

const MAX_REFERENCE_CANDIDATES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryAction {
    Search,
    Read,
    Write,
    List,
    Delete,
    Log,
    Revert,
}

impl MemoryAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Read => "read",
            Self::Write => "write",
            Self::List => "list",
            Self::Delete => "delete",
            Self::Log => "log",
            Self::Revert => "revert",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryToolInput {
    pub action: MemoryAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<MemoryScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
}

pub fn memory_tool_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["action"],
        "properties": {
            "action": {
                "type": "string",
                "enum": ["search", "read", "write", "list", "delete", "log", "revert"],
                "description": "The memory operation to perform. Required."
            },
            "query": {
                "type": "string",
                "description": "Search query (for `search` action)."
            },
            "path": {
                "type": "string",
                "description": "Memory file path (for `read`, `write`, `delete`). Can include or omit `.md` extension. For `read`/`delete`, this is the full relative path (e.g. `global/auth.md`). For `write`, this is the filename within the chosen scope."
            },
            "scope": {
                "type": "string",
                "enum": ["global", "project", "session"],
                "description": "Memory scope (for `search`, `write`, `list`). `search` defaults to every scope of every project; `write` defaults to `project`; `list` defaults to all scopes."
            },
            "content": {
                "type": "string",
                "description": "Markdown content (for `write` action)."
            },
            "sha": {
                "type": "string",
                "description": "Memory repository commit sha (for `revert` action)."
            }
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSideEffects {
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoMutations {
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryToolOutput {
    pub output: String,
    #[serde(rename = "isError")]
    pub is_error: bool,
}

impl MemoryToolOutput {
    fn ok(output: String) -> Self {
        Self {
            output,
            is_error: false,
        }
    }

    fn error(output: String) -> Self {
        Self {
            output,
            is_error: true,
        }
    }
}

pub trait IMemorySessionContext: Send + Sync {
    fn cwd(&self) -> String;
    fn session_id(&self) -> String;
}

pub trait IMemoryFlagService: Send + Sync {
    fn enabled(&self, id: &str) -> Result<bool, WireAsyncError>;
}

#[async_trait]
pub trait IMemoryTool: Send + Sync {
    fn name(&self) -> &'static str;
    fn side_effect(&self) -> ToolSideEffects;
    fn repo_mutation(&self) -> RepoMutations;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> Value;
    fn resolve_execution(self: Arc<Self>, args: MemoryToolInput) -> MemoryToolExecution;
}

pub struct MemoryTool {
    store: Arc<dyn IMemoryStore>,
    session_context: Arc<dyn IMemorySessionContext>,
    home_dir: PathBuf,
    repo: Arc<dyn IMemoryRepo>,
    flags: Arc<dyn IMemoryFlagService>,
    migration_done: AtomicBool,
}

impl MemoryTool {
    pub fn new(
        store: Arc<dyn IMemoryStore>,
        session_context: Arc<dyn IMemorySessionContext>,
        home_dir: PathBuf,
        repo: Arc<dyn IMemoryRepo>,
        flags: Arc<dyn IMemoryFlagService>,
    ) -> Self {
        Self {
            store,
            session_context,
            home_dir,
            repo,
            flags,
            migration_done: AtomicBool::new(false),
        }
    }

    async fn run_action(&self, args: MemoryToolInput, commit_allowed: bool) -> Result<MemoryToolOutput, WireAsyncError> {
        if matches!(args.action, MemoryAction::Search | MemoryAction::List | MemoryAction::Read | MemoryAction::Write | MemoryAction::Delete) {
            self.migrate_legacy_project_scope().await?;
        }
        match args.action {
            MemoryAction::Search => self.handle_search(args).await,
            MemoryAction::Read => self.handle_read(args).await,
            MemoryAction::Write => self.handle_write(args, commit_allowed).await,
            MemoryAction::List => self.handle_list(args).await,
            MemoryAction::Delete => self.handle_delete(args, commit_allowed).await,
            MemoryAction::Log => self.handle_log().await,
            MemoryAction::Revert => self.handle_revert(args).await,
        }
    }

    async fn with_memory_lock(&self, args: MemoryToolInput) -> Result<MemoryToolOutput, WireAsyncError> {
        let path = self.home_dir.join("locks").join("memory-write.lock");
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| Box::new(error) as WireAsyncError)?;
        }
        let lock_path = path.clone();
        let lock = tokio::task::spawn_blocking(move || crate::tools::memory_lock::MemoryLock::acquire(&lock_path)).await.map_err(|error| Box::new(error) as WireAsyncError)?;
        let commit_allowed = lock.is_some();
        if !commit_allowed {
            tracing::warn!(path = %path.display(), "memory write lock is held elsewhere; skipping the git commit");
        }
        self.run_action(args, commit_allowed).await
    }

    async fn migrate_legacy_project_scope(&self) -> Result<(), WireAsyncError> {
        if self.migration_done.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let cwd = self.session_context.cwd();
        let legacy = legacy_project_id_from_cwd(&cwd);
        let next = project_id_from_cwd(&cwd);
        if legacy == next {
            return Ok(());
        }
        let base = memory_dir(&self.home_dir);
        let legacy_dir = scope_dir(&base, MemoryScope::Project, &legacy);
        let next_dir = scope_dir(&base, MemoryScope::Project, &next);
        if !legacy_dir.exists() || next_dir.exists() {
            return Ok(());
        }
        match tokio::fs::rename(&legacy_dir, &next_dir).await {
            Ok(()) => self.store.reconcile().await?,
            Err(error) => tracing::warn!(%error, "legacy memory scope migration failed"),
        }
        Ok(())
    }

    async fn handle_search(&self, args: MemoryToolInput) -> Result<MemoryToolOutput, WireAsyncError> {
        let Some(query) = args.query.as_deref().filter(|query| !query.is_empty()) else {
            return Ok(MemoryToolOutput::error(text("engine.tools.memory.searchQueryRequired", &[])));
        };
        let filter = args.scope.map(|scope| (scope, self.resolve_scope_id(scope)));
        let results = self.store.search(query, Some(10), filter).await?;
        if results.is_empty() {
            return Ok(MemoryToolOutput::ok(text("engine.tools.memory.noEntriesFound", &[("query", query)])));
        }
        let plural = if results.len() == 1 { "y" } else { "ies" };
        let count = results.len().to_string();
        let mut lines = vec![format!("{}\n", text("engine.tools.memory.foundEntries", &[("count", &count), ("plural", plural)]))];
        for result in results {
            lines.push(format!("## {}", result.title));
            lines.push(format!("  path: {}", result.path));
            lines.push(format!("  scope: {}{}", result.scope.as_str(), if result.scope_id.is_empty() { String::new() } else { format!(" ({})", result.scope_id) }));
            lines.push(format!("  type: {}", result.type_.as_str()));
            lines.push(format!("  score: {:.3}", result.score));
            lines.push(format!("  snippet: {}", result.snippet));
            lines.push(String::new());
        }
        Ok(MemoryToolOutput::ok(lines.join("\n")))
    }

    async fn handle_read(&self, args: MemoryToolInput) -> Result<MemoryToolOutput, WireAsyncError> {
        let Some(path) = args.path.as_deref().filter(|path| !path.is_empty()) else {
            return Ok(MemoryToolOutput::error(text("engine.tools.memory.readPathRequired", &[])));
        };
        let path = normalize_path(path);
        let Some(entry) = self.store.get(&path).await? else {
            return Ok(MemoryToolOutput::ok(text("engine.tools.memory.notFound", &[("path", &path)])));
        };
        let updated = chrono::DateTime::from_timestamp_millis(entry.updated_at as i64).ok_or_else(|| memory_error("invalid memory timestamp"))?.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let lines = vec![
            format!("# {}", entry.title),
            format!("path: {}", entry.path),
            format!("scope: {}{}", entry.scope.as_str(), if entry.scope_id.is_empty() { String::new() } else { format!(" ({})", entry.scope_id) }),
            format!("type: {}", entry.type_.as_str()),
            format!("updated: {updated}"),
            String::new(),
            entry.body,
        ];
        Ok(MemoryToolOutput::ok(lines.join("\n")))
    }

    async fn handle_write(&self, args: MemoryToolInput, commit_allowed: bool) -> Result<MemoryToolOutput, WireAsyncError> {
        let Some(raw_path) = args.path.as_deref().filter(|path| !path.is_empty()) else {
            return Ok(MemoryToolOutput::error(text("engine.tools.memory.writePathRequired", &[])));
        };
        let Some(content) = args.content.as_deref().filter(|content| !content.is_empty()) else {
            return Ok(MemoryToolOutput::error(text("engine.tools.memory.writeContentRequired", &[])));
        };
        let scope = args.scope.unwrap_or(MemoryScope::Project);
        let scope_id = self.resolve_scope_id(scope);
        let Some(file_name) = sanitize_file_name(raw_path) else {
            return Ok(MemoryToolOutput::error(text("engine.tools.memory.invalidFilename", &[("path", raw_path)])));
        };
        let rel_path = build_rel_path(scope, &scope_id, &file_name);
        if self.git_memory_enabled() && !self.has_git_reference(content).await? {
            return Ok(MemoryToolOutput::error(text("engine.tools.memory.writeNeedsReference", &[("path", &rel_path)])));
        }
        let full_dir = scope_dir(&memory_dir(&self.home_dir), scope, &scope_id);
        let full_path = full_dir.join(&file_name);
        tokio::fs::create_dir_all(&full_dir).await.map_err(|error| Box::new(error) as WireAsyncError)?;
        tokio::fs::write(&full_path, content).await.map_err(|error| Box::new(error) as WireAsyncError)?;
        let metadata = tokio::fs::metadata(&full_path).await.map_err(|error| Box::new(error) as WireAsyncError)?;
        let updated_at = mtime_ms(&metadata);
        let entry = MemoryEntry {
            path: rel_path.clone(),
            scope,
            scope_id,
            type_: detect_type(content),
            title: extract_title(content, &file_name),
            body: content.to_string(),
            fingerprint: format!("{}-{updated_at}", metadata.len()),
            updated_at,
            sha: None,
            intent: None,
            note: None,
            applies_to: None,
        };
        let existing = self.store.get(&rel_path).await?;
        self.store.put(entry.clone()).await?;
        if !self.git_memory_enabled() {
            return Ok(MemoryToolOutput::ok(text("engine.tools.memory.written", &[("path", &rel_path), ("title", &entry.title), ("type", entry.type_.as_str())])));
        }
        if !commit_allowed {
            return Ok(MemoryToolOutput::ok(text("engine.tools.memory.writtenNotCommitted", &[("path", &rel_path), ("title", &entry.title), ("type", entry.type_.as_str())])));
        }
        let action = if existing.is_none() { "remember" } else { "update" };
        let committed = self.repo.commit(&format!("{action}: {}", entry.title)).await?;
        if !committed {
            return Ok(MemoryToolOutput::ok(text("engine.tools.memory.writtenNotCommitted", &[("path", &rel_path), ("title", &entry.title), ("type", entry.type_.as_str())])));
        }
        let commits = self.repo.log_commits(1).await?;
        let sha = commits.first().map(|commit| commit.sha.clone()).unwrap_or_default();
        let mut committed_entry = entry.clone();
        if !sha.is_empty() {
            committed_entry.sha = Some(sha.clone());
        }
        committed_entry.intent = Some(entry.title.clone());
        committed_entry.applies_to = Some(parse_applies_to(content));
        self.store.put(committed_entry).await?;
        Ok(MemoryToolOutput::ok(text("engine.tools.memory.writtenCommitted", &[("path", &rel_path), ("title", &entry.title), ("type", entry.type_.as_str()), ("sha", &sha)])))
    }

    async fn handle_log(&self) -> Result<MemoryToolOutput, WireAsyncError> {
        let commits = self.repo.log_commits(20).await?;
        let count = commits.len().to_string();
        let mut lines = vec![text("engine.tools.memory.logHeader", &[("count", &count)])];
        for commit in commits {
            let short_sha = commit.sha.chars().take(7).collect::<String>();
            lines.push(text("engine.tools.memory.logLine", &[("sha", &short_sha), ("date", &commit.date), ("message", &commit.message)]));
        }
        Ok(MemoryToolOutput::ok(lines.join("\n")))
    }

    async fn handle_revert(&self, args: MemoryToolInput) -> Result<MemoryToolOutput, WireAsyncError> {
        let Some(sha) = args.sha.as_deref().filter(|sha| !sha.is_empty()) else {
            return Ok(MemoryToolOutput::error(text("engine.tools.memory.revertFailed", &[("sha", "")])));
        };
        if !self.repo.revert(sha).await? {
            return Ok(MemoryToolOutput::error(text("engine.tools.memory.revertFailed", &[("sha", sha)])));
        }
        if let Err(error) = self.store.reconcile().await {
            tracing::warn!(%error, "memory store reconciliation after revert failed");
        }
        Ok(MemoryToolOutput::ok(text("engine.tools.memory.reverted", &[("sha", sha)])))
    }

    fn git_memory_enabled(&self) -> bool {
        self.flags.enabled(GIT_MEMORY_FLAG_ID).unwrap_or(false)
    }

    async fn has_git_reference(&self, content: &str) -> Result<bool, WireAsyncError> {
        let cwd = PathBuf::from(self.session_context.cwd());
        let memory = memory_dir(&self.home_dir);
        let content = content.to_string();
        tokio::task::spawn_blocking(move || crate::tools::memory_repo::has_git_reference(&cwd, &memory, &content)).await.map_err(|error| Box::new(error) as WireAsyncError)
    }

    async fn handle_list(&self, args: MemoryToolInput) -> Result<MemoryToolOutput, WireAsyncError> {
        let all_paths = self.store.list().await?;
        let prefix = args.scope.map(|scope| match scope {
            MemoryScope::Global => "global/".to_string(),
            MemoryScope::Project => format!("projects/{}/", self.resolve_scope_id(MemoryScope::Project)),
            MemoryScope::Session => format!("sessions/{}/", self.resolve_scope_id(MemoryScope::Session)),
        });
        let mut filtered = all_paths.into_iter().filter(|path| prefix.as_ref().is_none_or(|prefix| path.starts_with(prefix))).collect::<Vec<_>>();
        if filtered.is_empty() {
            return Ok(MemoryToolOutput::ok(text("engine.tools.memory.noFilesFound", &[])));
        }
        let count = filtered.len().to_string();
        let mut lines = vec![format!("{}\n", text("engine.tools.memory.listHeader", &[("count", &count)]))];
        filtered.sort();
        for path in filtered {
            if let Some(entry) = self.store.get(&path).await? {
                lines.push(format!("  {path} — {} [{}]", entry.title, entry.type_.as_str()));
            } else {
                lines.push(format!("  {path}"));
            }
        }
        Ok(MemoryToolOutput::ok(lines.join("\n")))
    }

    async fn handle_delete(&self, args: MemoryToolInput, commit_allowed: bool) -> Result<MemoryToolOutput, WireAsyncError> {
        let Some(raw_path) = args.path.as_deref().filter(|path| !path.is_empty()) else {
            return Ok(MemoryToolOutput::error(text("engine.tools.memory.deletePathRequired", &[])));
        };
        let path = normalize_path(raw_path);
        let Some(entry) = self.store.get(&path).await? else {
            return Ok(MemoryToolOutput::ok(text("engine.tools.memory.notFound", &[("path", &path)])));
        };
        let Some(file_name) = entry.path.rsplit('/').next().and_then(sanitize_file_name) else {
            return Ok(MemoryToolOutput::error(text("engine.tools.memory.invalidFilename", &[("path", raw_path)])));
        };
        let full_path = scope_dir(&memory_dir(&self.home_dir), entry.scope, &entry.scope_id).join(file_name);
        if let Err(error) = tokio::fs::remove_file(&full_path).await {
            tracing::warn!(%error, path = %full_path.display(), "memory file deletion failed");
        }
        self.store.delete(&path).await?;
        if !self.git_memory_enabled() || !commit_allowed {
            let key = if commit_allowed { "engine.tools.memory.deleted" } else { "engine.tools.memory.deletedNotCommitted" };
            return Ok(MemoryToolOutput::ok(text(key, &[("path", &path)])));
        }
        let committed = self.repo.commit(&format!("forget: {}", entry.title)).await?;
        let key = if committed { "engine.tools.memory.deleted" } else { "engine.tools.memory.deletedNotCommitted" };
        Ok(MemoryToolOutput::ok(text(key, &[("path", &path)])))
    }

    fn resolve_scope_id(&self, scope: MemoryScope) -> String {
        match scope {
            MemoryScope::Global => String::new(),
            MemoryScope::Project => project_id_from_cwd(&self.session_context.cwd()),
            MemoryScope::Session => self.session_context.session_id(),
        }
    }
}

#[async_trait]
impl IMemoryTool for MemoryTool {
    fn name(&self) -> &'static str {
        "Memory"
    }

    fn side_effect(&self) -> ToolSideEffects {
        ToolSideEffects::Write
    }

    fn repo_mutation(&self) -> RepoMutations {
        RepoMutations::None
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        memory_tool_input_schema()
    }

    fn resolve_execution(self: Arc<Self>, args: MemoryToolInput) -> MemoryToolExecution {
        let action = args.action.as_str();
        MemoryToolExecution {
            description: text("engine.tools.memoryAction", &[("action", action)]),
            approval_rule: "Memory".to_string(),
            tool: self,
            args,
        }
    }
}

pub struct MemoryToolExecution {
    pub description: String,
    pub approval_rule: String,
    tool: Arc<MemoryTool>,
    args: MemoryToolInput,
}

impl MemoryToolExecution {
    pub async fn execute(&self) -> Result<MemoryToolOutput, WireAsyncError> {
        if matches!(self.args.action, MemoryAction::Write | MemoryAction::Delete) {
            return self.tool.with_memory_lock(self.args.clone()).await;
        }
        self.tool.run_action(self.args.clone(), true).await
    }
}

fn normalize_path(path: &str) -> String {
    if path.ends_with(".md") {
        path.to_string()
    } else {
        format!("{path}.md")
    }
}

fn normalize_file_name(name: &str) -> String {
    if name.ends_with(".md") {
        name.to_string()
    } else {
        format!("{name}.md")
    }
}

fn sanitize_file_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains("..") {
        return None;
    }
    Some(normalize_file_name(trimmed))
}

fn build_rel_path(scope: MemoryScope, scope_id: &str, file_name: &str) -> String {
    match scope {
        MemoryScope::Global => format!("global/{file_name}"),
        MemoryScope::Project => format!("projects/{scope_id}/{file_name}"),
        MemoryScope::Session => format!("sessions/{scope_id}/{file_name}"),
    }
}

fn text(key: &'static str, params: &[(&str, &str)]) -> String {
    let mut owned = std::collections::HashMap::new();
    for (key, value) in params {
        owned.insert((*key).to_string(), (*value).to_string());
    }
    LocalizedText::with_params(key, owned).render()
}

fn memory_error(message: impl Into<String>) -> WireAsyncError {
    Box::new(std::io::Error::other(message.into()))
}

fn mtime_ms(metadata: &std::fs::Metadata) -> f64 {
    metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|duration| duration.as_secs_f64() * 1000.0).unwrap_or(0.0)
}

pub fn memory_tool_available(agent_id: &str, associative_memory_enabled: bool) -> bool {
    agent_id == "main" && !associative_memory_enabled
}

pub fn memory_tool_registration_matches(agent_id: &str, flags: &dyn IMemoryFlagService) -> Result<bool, WireAsyncError> {
    Ok(memory_tool_available(agent_id, flags.enabled(ASSOCIATIVE_MEMORY_FLAG_ID)?))
}

pub const DESCRIPTION: &str = r##"Search and manage persistent memory across sessions.

Memory files are markdown documents stored under `~/.kimi-code/memory/`, organized by scope:
- **global** — cross-project knowledge (architecture decisions, coding standards, tool configs)
- **project** — project-specific knowledge tied to the current working directory
- **session** — session-specific notes and context

## Actions

### `search`
Full-text search over all memory entries. Returns matching snippets with scope and relevance score. Requires `query`.
```
Memory({ action: "search", query: "authentication pattern" })
```

### `read`
Read a specific memory file by its relative path.
```
Memory({ action: "read", path: "global/auth-pattern.md" })
```

### `write`
Create or update a memory file. The `scope` determines where it lives:
- `global` — stored at `~/.kimi-code/memory/global/`
- `project` — stored at `~/.kimi-code/memory/projects/<projectId>/`
- `session` — stored at `~/.kimi-code/memory/sessions/<sessionId>/`

```
Memory({
  action: "write",
  scope: "project",
  path: "auth-pattern",
  content: "# Authentication Pattern\n\nUse JWT with refresh tokens..."
})
```

### `list`
List all memory files, optionally filtered by scope.
```
Memory({ action: "list", scope: "global" })
```

### `delete`
Delete a memory file by its relative path.
```
Memory({ action: "delete", path: "global/old-note.md" })
```

### `log`
List the memory repository's recent commits, newest first, each with its short
sha, date and message. Use it to pick a revision before `revert`.
```
Memory({ action: "log" })
```

### `revert`
Roll one memory commit back. Takes the full `sha` reported by `log`; a short sha
is rejected. The search index is resynchronised from disk before the call
returns, so a reverted file stops matching immediately.
```
Memory({ action: "revert", sha: "<full sha>" })
```

## Scope semantics

- `search` covers **every** project and session unless `scope` is given; pass
  `scope` to restrict it to the current project, the global scope, or this
  session.
- `write` defaults to `project` and never writes outside the scope you name.
- `list` defaults to everything; with `scope` it returns only that scope of the
  current project or session.

`project` scope is keyed by the repository when there is one: the identity comes
from the `origin` remote first, then from the repository root, and only falls
back to the working directory path outside git. A repository with an `origin`
remote keeps the same project bucket after re-cloning, renaming or moving; a
repository without one keeps it when you work from any directory inside it.

## Git memory interaction

When the `git_memory` experiment is enabled, `write` additionally requires the
content to cite a real commit: a 7–40 character hex string that `git cat-file`
resolves in the session working directory or in the memory repository. A write
without one is rejected. With the experiment on, every write and delete is
committed to that repository (`remember:`, `update:`, `forget:`), which is what
`log` lists and `revert` rolls back. With the experiment off, no such
requirement applies and nothing is committed.

## When to use

- **Before starting a task** — search memory for relevant context from past sessions
- **After learning something important** — write it to memory for future reference
- **When you encounter a pattern or decision** — persist it so future you doesn't rediscover it
- **When debugging** — search for similar past issues and their resolutions

Memory persists across sessions and projects. Use it to build up institutional knowledge.

The search index is rebuilt from the files on disk at most once every ten minutes, so
a file edited outside this tool shows up in `search` and `list` within that window
rather than immediately.
"##;
