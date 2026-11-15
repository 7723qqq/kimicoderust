use std::path::{Path, PathBuf};
use std::sync::Arc;

use indexmap::{IndexMap, IndexSet};
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::_base::state::state_registry::StateKey;
use crate::agent::state::agent_state::IAgentStateService;
use crate::injection::find_agents_md_in_dir;
use crate::prompt::agents_md::{
    AGENTS_MD_PLAIN_NAMES, dirs_root_to_leaf, extract_agents_md_paths_from_prompt, load_agents_md,
};
use crate::tools::bash_target_dirs::bash_target_dirs;

pub const DISCOVERY_REMINDER_VARIANT: &str = "agents_md";

pub fn agents_md_reminder_known_key() -> StateKey<Value> {
    StateKey::new("agentsMdReminder.known", || json!([]))
}

pub fn agents_md_reminder_cwd_key() -> StateKey<Value> {
    StateKey::new("agentsMdReminder.cwd", || Value::Null)
}

pub fn agents_md_reminder_seeded_key() -> StateKey<Value> {
    StateKey::new("agentsMdReminder.seeded", || json!(false))
}

#[derive(Debug, Clone)]
pub struct ToolAccess {
    pub kind: String,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct ToolDidExecuteContext {
    pub outcome: String,
    pub turn_id: String,
    pub tool_name: String,
    pub args: Value,
    pub is_error: bool,
    pub accesses: Vec<ToolAccess>,
    pub trace_id: Option<String>,
}

pub struct GitWorkTree {
    pub root: PathBuf,
    pub dot_git_path: PathBuf,
    pub control_dir_path: PathBuf,
}

pub fn find_git_work_tree(cwd: &Path) -> Option<GitWorkTree> {
    if cwd.as_os_str().is_empty() || !cwd.is_absolute() {
        return None;
    }
    let mut current = cwd.to_path_buf();
    loop {
        let dot_git_path = current.join(".git");
        if let Some(control_dir_path) = probe_git_control_dir(&dot_git_path, &current) {
            return Some(GitWorkTree {
                root: current,
                dot_git_path,
                control_dir_path,
            });
        }
        let parent = current.parent().map(Path::to_path_buf);
        match parent {
            Some(parent) if parent != current => current = parent,
            _ => return None,
        }
    }
}

fn probe_git_control_dir(dot_git_path: &Path, marker_parent: &Path) -> Option<PathBuf> {
    let metadata = std::fs::metadata(dot_git_path).ok()?;
    if metadata.is_dir() {
        return Some(dot_git_path.to_path_buf());
    }
    if !metadata.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(dot_git_path).ok()?;
    parse_git_dir_pointer(&content, marker_parent)
}

fn parse_git_dir_pointer(content: &str, marker_parent: &Path) -> Option<PathBuf> {
    let stripped = content.strip_prefix('\u{feff}').unwrap_or(content);
    let line = stripped.trim_start().lines().next()?.trim();
    let raw_path = line.strip_prefix("gitdir:")?.trim();
    if raw_path.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw_path);
    Some(if path.is_absolute() {
        path
    } else {
        marker_parent.join(path)
    })
}

pub fn find_project_root(work_dir: &Path) -> PathBuf {
    find_git_work_tree(work_dir)
        .map(|tree| tree.root)
        .unwrap_or_else(|| work_dir.to_path_buf())
}

pub fn dot_kimi_agents_md_path(dir: &Path) -> PathBuf {
    dir.join(".kimi-code").join("AGENTS.md")
}

pub fn agents_md_candidate_paths(dir: &Path) -> Vec<PathBuf> {
    let mut paths = vec![dot_kimi_agents_md_path(dir)];
    paths.extend(AGENTS_MD_PLAIN_NAMES.iter().map(|name| dir.join(name)));
    paths
}

fn host_path(path: &Path) -> String {
    crate::tool_result_truncation::lexical_normalize(path)
        .to_string_lossy()
        .into_owned()
}

fn string_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

pub fn reminder_text(paths: &[String]) -> String {
    format!(
        "The following AGENTS.md file(s) apply to paths accessed by your recent tool call, but were not included in your system prompt:\n{}\nRead them before making changes in those directories.",
        paths
            .iter()
            .map(|path| format!("- {path}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

pub struct AgentAgentsMdReminderService {
    states: Arc<dyn IAgentStateService>,
    session_cwd: PathBuf,
    brand_home: Option<PathBuf>,
    host_home: PathBuf,
    remind_queue: Mutex<IndexSet<String>>,
    read_recently: Mutex<IndexSet<String>>,
    telemetry_fired: Mutex<IndexSet<String>>,
    telemetry: Arc<dyn Fn(Value) + Send + Sync>,
}

impl AgentAgentsMdReminderService {
    pub fn new(
        states: Arc<dyn IAgentStateService>,
        session_cwd: PathBuf,
        brand_home: Option<PathBuf>,
        host_home: PathBuf,
        telemetry: Arc<dyn Fn(Value) + Send + Sync>,
    ) -> Result<Self, crate::agent::state::errors::StateError> {
        let service = Self {
            states,
            session_cwd,
            brand_home,
            host_home,
            remind_queue: Mutex::new(IndexSet::new()),
            read_recently: Mutex::new(IndexSet::new()),
            telemetry_fired: Mutex::new(IndexSet::new()),
            telemetry,
        };
        service
            .states
            .contribute_state(agents_md_reminder_known_key())?;
        service
            .states
            .contribute_state(agents_md_reminder_cwd_key())?;
        service
            .states
            .contribute_state(agents_md_reminder_seeded_key())?;
        Ok(service)
    }

    pub fn seed_injected(&self, paths: &[String], cwd: &Path) {
        let mut known = self.known();
        for path in paths {
            known.insert(host_path(Path::new(path)));
        }
        self.set_known(&known);
        let mut queue = self.remind_queue.lock();
        for path in paths {
            queue.remove(&host_path(Path::new(path)));
        }
        drop(queue);
        self.states
            .set(&agents_md_reminder_cwd_key(), json!(cwd.to_string_lossy()));
        self.states
            .set(&agents_md_reminder_seeded_key(), json!(true));
    }

    pub fn restore_seed(&self, system_prompt: &str, agents_md_paths: Option<&[String]>) {
        let paths = agents_md_paths
            .map(|paths| paths.to_vec())
            .unwrap_or_else(|| extract_agents_md_paths_from_prompt(system_prompt));
        self.seed_injected(&paths, self.session_cwd.as_path());
    }

    pub fn track_changes(&self, changes: &[crate::environment::AgentsMdChange]) {
        if !self.seeded() {
            return;
        }
        let mut entries: IndexMap<String, bool> = IndexMap::new();
        for change in changes {
            let (path, deleted) = match change {
                crate::environment::AgentsMdChange::Modified { path }
                | crate::environment::AgentsMdChange::Added { path }
                | crate::environment::AgentsMdChange::BecameReadable { path } => {
                    (host_path(path), false)
                }
                crate::environment::AgentsMdChange::Removed { path }
                | crate::environment::AgentsMdChange::BecameUnreadable { path, .. } => {
                    (host_path(path), true)
                }
            };
            entries.insert(path, deleted);
        }
        if entries.is_empty() {
            return;
        }
        let entries: Vec<(String, bool)> = entries.into_iter().collect();
        let modified: Vec<String> = entries
            .iter()
            .filter(|(_, deleted)| !deleted)
            .map(|(path, _)| path.clone())
            .collect();
        let deleted: Vec<String> = entries
            .into_iter()
            .filter(|(_, deleted)| *deleted)
            .map(|(path, _)| path)
            .collect();
        self.mark_known(&modified);
        self.mark_deleted(&deleted);
    }

    fn known(&self) -> IndexSet<String> {
        self.states
            .get(&agents_md_reminder_known_key())
            .as_array()
            .cloned()
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(|path| path.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn set_known(&self, known: &IndexSet<String>) {
        let paths: Vec<String> = known.iter().cloned().collect();
        self.states
            .set(&agents_md_reminder_known_key(), json!(paths));
    }

    fn seeded(&self) -> bool {
        self.states
            .get(&agents_md_reminder_seeded_key())
            .as_bool()
            .unwrap_or(false)
    }

    fn agent_cwd(&self) -> PathBuf {
        self.states
            .get(&agents_md_reminder_cwd_key())
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.session_cwd.clone())
    }

    pub fn inject_reminder(&self, last_disclosure: &[String]) -> Option<(String, Vec<String>)> {
        let read_recently = std::mem::take(&mut *self.read_recently.lock());
        let known = self.known();
        let queued: Vec<String> = self
            .remind_queue
            .lock()
            .iter()
            .filter(|path| !known.contains(*path) && !read_recently.contains(*path))
            .cloned()
            .collect();
        self.remind_queue.lock().clear();
        if queued.is_empty() {
            return None;
        }
        let fresh: Vec<String> = queued
            .into_iter()
            .filter(|path| !last_disclosure.contains(path))
            .collect();
        if fresh.is_empty() {
            return None;
        }
        let content = reminder_text(&fresh);
        let mut disclosure = last_disclosure.to_vec();
        disclosure.extend(fresh);
        Some((content, disclosure))
    }

    async fn ensure_seeded(&self) {
        if self.seeded() {
            return;
        }
        let loaded = load_agents_md(&self.agent_cwd(), self.brand_home.as_deref());
        self.seed_injected(&loaded.paths, &self.agent_cwd());
    }

    pub async fn probe_and_remind(&self, ctx: &ToolDidExecuteContext) {
        if ctx.outcome != "executed" {
            return;
        }
        self.ensure_seeded().await;
        let (dirs, self_known) = self.target_dirs(ctx);
        let self_known_set: IndexSet<String> = self_known.iter().cloned().collect();
        let mut discovered = Vec::new();
        for dir in &dirs {
            for path in self.probe_dir(Path::new(dir)) {
                if self.known().contains(&path)
                    || self.remind_queue.lock().contains(&path)
                    || self_known_set.contains(&path)
                {
                    continue;
                }
                discovered.push(path);
            }
        }
        for path in &self_known {
            self.remind_queue.lock().remove(path);
            self.read_recently.lock().insert(path.clone());
        }
        if discovered.is_empty() {
            return;
        }
        let untracked: Vec<String> = discovered
            .iter()
            .filter(|path| !self.telemetry_fired.lock().contains(*path))
            .cloned()
            .collect();
        if !untracked.is_empty() {
            let mut properties = json!({
                "type": "agents_md_reminder_shown",
                "turn_id": ctx.turn_id,
                "tool_name": ctx.tool_name,
                "reminded_count": untracked.len(),
            });
            if let Some(trace_id) = &ctx.trace_id {
                properties["trace_id"] = json!(trace_id);
            }
            (self.telemetry)(properties);
            for path in &untracked {
                self.telemetry_fired.lock().insert(path.clone());
            }
        }
        for path in discovered {
            self.remind_queue.lock().insert(path);
        }
    }

    fn mark_known(&self, paths: &[String]) {
        if paths.is_empty() {
            return;
        }
        let mut known = self.known();
        for path in paths {
            known.insert(path.clone());
        }
        self.set_known(&known);
    }

    fn mark_deleted(&self, paths: &[String]) {
        if paths.is_empty() {
            return;
        }
        let mut known = self.known();
        for path in paths {
            known.remove(path);
            self.remind_queue.lock().remove(path);
            self.telemetry_fired.lock().remove(path);
        }
        self.set_known(&known);
    }

    fn target_dirs(&self, ctx: &ToolDidExecuteContext) -> (Vec<String>, Vec<String>) {
        match ctx.tool_name.as_str() {
            "Read" | "Edit" | "Write" | "Glob" | "Grep" => self.target_dirs_from_accesses(ctx),
            "Bash" => {
                let Some(command) = string_arg(&ctx.args, "command") else {
                    return (Vec::new(), Vec::new());
                };
                let cwd_arg = string_arg(&ctx.args, "cwd");
                let base = host_path(&self.session_cwd);
                let effective_cwd = match cwd_arg {
                    None => base.clone(),
                    Some(arg) => {
                        let path = PathBuf::from(arg);
                        if path.is_absolute() {
                            host_path(&path)
                        } else {
                            host_path(&Path::new(&base).join(path))
                        }
                    }
                };
                match bash_target_dirs(command, &effective_cwd, &self.host_home.to_string_lossy()) {
                    None if cwd_arg.is_none() => (Vec::new(), Vec::new()),
                    None => (vec![effective_cwd], Vec::new()),
                    Some(targets) => {
                        let mut targets: Vec<String> = targets
                            .iter()
                            .map(|target| host_path(Path::new(target)))
                            .collect();
                        if cwd_arg.is_some() && !targets.contains(&effective_cwd) {
                            targets.insert(0, effective_cwd);
                        }
                        (targets, Vec::new())
                    }
                }
            }
            _ => (Vec::new(), Vec::new()),
        }
    }

    fn target_dirs_from_accesses(&self, ctx: &ToolDidExecuteContext) -> (Vec<String>, Vec<String>) {
        let targets_files = matches!(ctx.tool_name.as_str(), "Read" | "Edit" | "Write");
        let mut dirs = IndexSet::new();
        let mut self_known = IndexSet::new();
        for access in &ctx.accesses {
            if access.kind != "file" {
                continue;
            }
            let path = PathBuf::from(&access.path);
            if targets_files
                && !ctx.is_error
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| AGENTS_MD_PLAIN_NAMES.contains(&name))
            {
                self_known.insert(access.path.clone());
            }
            if targets_files {
                let parent = match path.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => parent,
                    Some(_) => Path::new("."),
                    None if path.has_root() => path.as_path(),
                    _ => Path::new("."),
                };
                dirs.insert(host_path(parent));
            } else {
                dirs.insert(host_path(&path));
            }
        }
        (dirs.into_iter().collect(), self_known.into_iter().collect())
    }

    fn probe_dir(&self, dir: &Path) -> Vec<String> {
        let Some(anchor) = nearest_existing_dir(dir) else {
            return Vec::new();
        };
        let project_root = find_project_root(&anchor);
        let chain = dirs_root_to_leaf(&project_root, &anchor);
        let mut found = Vec::new();
        for chain_dir in chain {
            let known = self.known();
            let candidates = agents_md_candidate_paths(&chain_dir);
            if candidates
                .iter()
                .all(|candidate| known.contains(&host_path(candidate)))
            {
                continue;
            }
            for path in find_agents_md_in_dir(&chain_dir) {
                found.push(host_path(&path));
            }
        }
        found
    }
}

fn nearest_existing_dir(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if std::fs::metadata(&current)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
        {
            return Some(current);
        }
        let parent = current.parent().map(Path::to_path_buf);
        match parent {
            Some(parent) if parent != current => current = parent,
            _ => {
                tracing::debug!(path = %path.display(), "agentsMdReminder probe found no existing anchor directory");
                return None;
            }
        }
    }
}
