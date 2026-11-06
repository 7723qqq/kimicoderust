//! Advanced filesystem and git routes for native Kimi Agent server.
//!
//! Provides handlers for `fs:git_status`, `fs:diff`, `fs:stat`, `fs:stat_many`,
//! `fs:mkdir`, `fs:list`, and `fs:read`, adhering to kap-server REST conventions.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::server::router::HttpResponse;
use crate::session::sqlite_store::SqliteSessionStore;

/// Resolve effective workspace root path for a given session.
pub fn resolve_session_workdir(store: &SqliteSessionStore, session_id: &str) -> Option<PathBuf> {
    let session = store.get_session(session_id).ok().flatten()?;
    if let Some(ref ws_id) = session.workspace_id
        && let Ok(Some(ws)) = store.get_workspace(ws_id)
    {
        return Some(PathBuf::from(ws.root));
    }
    if let Ok(Some(meta)) = store.get_state("metadata", session_id)
        && let Some(cwd) = meta.get("cwd").and_then(|c| c.as_str())
    {
        return Some(PathBuf::from(cwd));
    }
    std::env::current_dir().ok()
}

/// Helper to sanitize and resolve a path relative to workspace root, preventing directory traversal escapes.
pub fn resolve_safe_path(work_dir: &Path, rel_path: &str) -> Result<PathBuf, HttpResponse> {
    resolve_safe_path_typed(work_dir, rel_path).map_err(|err| err.into_response())
}

/// The typed form of [`resolve_safe_path`]'s failure, so batch callers
/// (`fs:list_many`) can tell a path escape (fails the whole call, v2
/// `FS_PATH_ESCAPES`) apart from an ordinary per-path failure.
enum FsPathError {
    Escapes,
    NotDirectory(String),
    Internal(String),
}

impl FsPathError {
    fn into_response(self) -> HttpResponse {
        match self {
            Self::Escapes => HttpResponse::bad_request("Path escapes session workspace"),
            Self::NotDirectory(msg) => HttpResponse::bad_request(msg),
            Self::Internal(msg) => HttpResponse::internal_error(msg),
        }
    }
}

fn resolve_safe_path_typed(work_dir: &Path, rel_path: &str) -> Result<PathBuf, FsPathError> {
    let trimmed = rel_path
        .trim()
        .trim_start_matches('/')
        .trim_start_matches('\\');
    let candidate = work_dir.join(trimmed);
    // If the path exists, canonicalize both and ensure work_dir prefix
    if candidate.exists() {
        let canon_work = work_dir
            .canonicalize()
            .map_err(|e| FsPathError::Internal(format!("Cannot canonicalize work dir: {e}")))?;
        let canon_cand = candidate
            .canonicalize()
            .map_err(|e| FsPathError::Internal(format!("Cannot canonicalize path: {e}")))?;
        if !canon_cand.starts_with(&canon_work) {
            return Err(FsPathError::Escapes);
        }
        Ok(canon_cand)
    } else {
        // If not existing yet (e.g. for mkdir), check normalized components
        resolve_lexical(work_dir, rel_path)
    }
}

/// Lexical containment only (no canonicalize, no symlink resolution): v2's
/// `resolveWithin(input, allowExternalSymlinks = true)` skips the
/// realpath-roots check, so a symlink pointing outside the workspace resolves.
fn resolve_lexical(work_dir: &Path, rel_path: &str) -> Result<PathBuf, FsPathError> {
    let trimmed = rel_path
        .trim()
        .trim_start_matches('/')
        .trim_start_matches('\\');
    let mut depth: isize = 0;
    for comp in Path::new(trimmed).components() {
        match comp {
            std::path::Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(FsPathError::Escapes);
                }
            }
            std::path::Component::Normal(_) => {
                depth += 1;
            }
            _ => {}
        }
    }
    Ok(work_dir.join(trimmed))
}

/// `resolve_safe_path` with the v2 `allow_external_symlinks` switch: when set,
/// the symlink-escape (realpath) check is skipped and only the lexical `..`
/// rule applies.
pub fn resolve_safe_path_with_symlinks(
    work_dir: &Path,
    rel_path: &str,
    allow_external_symlinks: bool,
) -> Result<PathBuf, HttpResponse> {
    let resolved = if allow_external_symlinks {
        resolve_lexical(work_dir, rel_path)
    } else {
        resolve_safe_path_typed(work_dir, rel_path)
    };
    resolved.map_err(|err| err.into_response())
}

/// v2 `realpathExistingPrefix`: canonicalize the longest existing prefix and
/// reattach the non-existing tail, capping the walk at 256 levels. A path
/// whose ancestors never resolve comes back as given.
fn realpath_existing_prefix(abs: &Path) -> Result<PathBuf, std::io::Error> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut current = abs.to_path_buf();
    for _ in 0..256 {
        match current.canonicalize() {
            Ok(real) => {
                let mut out = real;
                for comp in tail.iter().rev() {
                    out.push(comp);
                }
                return Ok(out);
            }
            Err(e) => {
                // v2 rethrows anything but a missing path.
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(e);
                }
                let Some(name) = current.file_name() else {
                    return Ok(abs.to_path_buf());
                };
                tail.push(name.to_os_string());
                let Some(parent) = current.parent() else {
                    return Ok(abs.to_path_buf());
                };
                if parent == current {
                    return Ok(abs.to_path_buf());
                }
                current = parent.to_path_buf();
            }
        }
    }
    Ok(abs.to_path_buf())
}

/// v2 `pathHasSymlink`: true when the path itself or any ancestor up to the
/// filesystem root is a symlink.
fn path_has_symlink(abs: &Path) -> bool {
    let mut current = abs;
    loop {
        if let Ok(md) = std::fs::symlink_metadata(current)
            && md.is_symlink()
        {
            return true;
        }
        let Some(parent) = current.parent() else {
            return false;
        };
        if parent == current {
            return false;
        }
        current = parent;
    }
}

/// v2 `symlinkDirectoryTarget`: stat follows the link; without
/// `allow_external` the resolved real path must stay inside the workspace's
/// real roots or the link is treated as a plain (non-directory) symlink.
/// Returns `(is_directory, real_path)`.
fn symlink_directory_target(
    base: &Path,
    rel_path: &str,
    allow_external: bool,
    real_roots: &[PathBuf],
) -> (bool, Option<PathBuf>) {
    let abs = base.join(rel_path);
    let Ok(st) = std::fs::metadata(&abs) else {
        return (false, None);
    };
    if !st.is_dir() {
        return (false, None);
    }
    let real = match realpath_existing_prefix(&abs) {
        Ok(real) => real,
        Err(_) => return (false, None),
    };
    if !allow_external && !real_roots.iter().any(|root| real.starts_with(root)) {
        return (false, None);
    }
    (true, Some(real))
}

/// Format file entry object adhering to FsEntry schema.
///
/// Lstat-based (v2 `buildFsEntry` takes the lstat): a symlink reports
/// `kind: "symlink"` with the link's own size instead of being silently
/// resolved to its target's type — the previous `metadata()` (follow-links)
/// form could never produce the schema's `symlink` kind.
pub fn file_to_fs_entry(work_dir: &Path, full_path: &Path) -> Result<Value, std::io::Error> {
    let metadata = std::fs::symlink_metadata(full_path)?;
    Ok(fs_entry_from_metadata(work_dir, full_path, &metadata))
}

/// Build the FsEntry JSON from an already-taken lstat, so a directory walk
/// stats each child exactly once.
fn fs_entry_from_metadata(
    work_dir: &Path,
    full_path: &Path,
    metadata: &std::fs::Metadata,
) -> Value {
    let name = full_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let rel_path = full_path
        .strip_prefix(work_dir)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| name.clone());

    let kind = if metadata.is_symlink() {
        "symlink"
    } else if metadata.is_dir() {
        "directory"
    } else {
        "file"
    };

    let modified_millis = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());

    let modified_at = chrono::DateTime::from_timestamp_millis(modified_millis)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default();

    let is_binary = if metadata.is_file() {
        if let Ok(file) = std::fs::File::open(full_path) {
            use std::io::Read;
            let mut buffer = [0u8; 512];
            let mut handle = file.take(512);
            if let Ok(n) = handle.read(&mut buffer) {
                buffer[..n].contains(&0)
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };

    let mut entry = json!({
        "path": rel_path,
        "name": name,
        "kind": kind,
        "size": metadata.len(),
        "modified_at": modified_at,
        "is_binary": is_binary,
    });

    if metadata.is_dir()
        && let Ok(read_dir) = std::fs::read_dir(full_path)
    {
        entry["child_count"] = json!(read_dir.count());
    }

    entry
}

/// Handle `fs:git_status` (or `fs:gitStatus`).
/// Parsed fields of a `git status --porcelain=v2 -z` output blob.
struct GitStatusFields {
    branch: String,
    ahead: u64,
    behind: u64,
    entries: HashMap<String, Value>,
}

/// Parse the `-z` porcelain=v2 records. NUL-terminated output keeps paths with
/// spaces, quotes, and non-ASCII characters verbatim — line-oriented parsing
/// combined with git's core.quotePath C-quoting mangled such paths into bogus
/// quoted directory segments (upstream #3415).
///
/// `path_prefix` (from `git rev-parse --show-prefix` when scoped to a
/// sub-root) is stripped so entry keys stay relative to the requested root
/// (v2 `parsePorcelain`'s prefix step); `filter` keeps only exact path hits
/// (v2's `pathFilter`).
fn parse_git_status_z(
    text: &str,
    filter: Option<&std::collections::HashSet<String>>,
    path_prefix: &str,
) -> GitStatusFields {
    let mut branch = String::new();
    let mut ahead = 0;
    let mut behind = 0;
    let mut entries = HashMap::new();

    for record in text.split('\0') {
        let record = record.trim_start_matches('\n');
        if record.is_empty() {
            continue;
        }
        if let Some(b) = record.strip_prefix("# branch.head ") {
            if b != "(detached)" {
                branch = b.trim().to_string();
            }
        } else if let Some(ab) = record.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(a) = part.strip_prefix('+') {
                    ahead = a.parse().unwrap_or(0);
                } else if let Some(b) = part.strip_prefix('-') {
                    behind = b.parse().unwrap_or(0);
                }
            }
        } else {
            let repo_path = match git_status_record_path(record) {
                Some(p) => p,
                None => continue,
            };
            let wire_path = if !path_prefix.is_empty() && repo_path.starts_with(path_prefix) {
                repo_path[path_prefix.len()..].to_string()
            } else {
                repo_path
            };
            if let Some(filter) = filter
                && !filter.contains(&wire_path)
            {
                continue;
            }
            let status = git_status_record_status(record);
            // v2 shape: `entries` maps the path straight to the status string
            // (`fsGitStatusResponseSchema` is a record of fsGitStatusSchema).
            entries.insert(wire_path, json!(status));
        }
    }

    GitStatusFields {
        branch,
        ahead,
        behind,
        entries,
    }
}

/// The path carried by a porcelain=v2 change record (`1`/`2`/`u`/`?`).
fn git_status_record_path(record: &str) -> Option<String> {
    if record.starts_with('1') {
        // "1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>"
        let parts: Vec<&str> = record.split(' ').collect();
        return (parts.len() >= 9).then(|| parts[8..].join(" "));
    }
    if record.starts_with('2') {
        // Rename: "2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>".
        let parts: Vec<&str> = record.split(' ').collect();
        return (parts.len() >= 10).then(|| parts[9..].join(" "));
    }
    if let Some(path) = record.strip_prefix("? ") {
        return Some(path.trim().to_string());
    }
    if record.starts_with('u') {
        // "u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>"
        let parts: Vec<&str> = record.split(' ').collect();
        return (parts.len() >= 11).then(|| parts[10..].join(" "));
    }
    None
}

/// The `fsGitStatusSchema` value of a porcelain=v2 change record.
fn git_status_record_status(record: &str) -> &'static str {
    if record.starts_with('?') {
        return "untracked";
    }
    if record.starts_with('u') {
        return "conflicted";
    }
    let xy = record.split(' ').nth(1).unwrap_or("");
    if record.starts_with('2') {
        return if xy.starts_with('D') {
            "deleted"
        } else {
            "renamed"
        };
    }
    if xy.starts_with('A') {
        "added"
    } else if xy.starts_with('D') {
        "deleted"
    } else {
        "modified"
    }
}

const PR_SPAWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const PULL_REQUEST_TTL: std::time::Duration = std::time::Duration::from_secs(60);

struct CachedPullRequest {
    value: Value,
    fetched_at: std::time::Instant,
}

static PULL_REQUEST_CACHE: std::sync::OnceLock<
    std::sync::Mutex<HashMap<PathBuf, CachedPullRequest>>,
> = std::sync::OnceLock::new();

/// Read the PR associated with the checkout's current branch. `gh pr view`
/// also owns the detached-HEAD, missing-remote, non-PR-branch and auth failure
/// cases: every non-zero exit or invalid payload is the wire's nullable miss.
fn read_pull_request(cwd: &Path) -> Value {
    let cache = PULL_REQUEST_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let now = std::time::Instant::now();
    let cached = {
        let entries = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries
            .get(cwd)
            .filter(|entry| now.duration_since(entry.fetched_at) < PULL_REQUEST_TTL)
            .map(|entry| entry.value.clone())
    };
    if let Some(value) = cached {
        return value;
    }

    let value = spawn_pull_request_lookup(cwd)
        .and_then(|stdout| parse_pull_request(&stdout))
        .unwrap_or(Value::Null);
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            cwd.to_path_buf(),
            CachedPullRequest {
                value: value.clone(),
                fetched_at: now,
            },
        );
    value
}

/// The synchronous HTTP handlers cannot await tokio's child output. Drain
/// stdout on a reader thread while polling the child so a full pipe cannot
/// deadlock the five-second deadline.
fn spawn_pull_request_lookup(cwd: &Path) -> Option<Vec<u8>> {
    use std::io::Read as _;
    use std::process::Stdio;

    let gh = which::which("gh").ok()?;
    let mut child = std::process::Command::new(gh)
        .args(["pr", "view", "--json", "number,url,state,isDraft"])
        .current_dir(cwd)
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = std::time::Instant::now() + PR_SPAWN_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    };
    let stdout = match reader.join() {
        Ok(Ok(bytes)) => bytes,
        _ => return None,
    };
    status.success().then_some(stdout)
}

fn parse_pull_request(stdout: &[u8]) -> Option<Value> {
    let raw: Value = serde_json::from_slice(stdout).ok()?;
    let record = raw.as_object()?;
    let number = record
        .get("number")?
        .as_u64()
        .filter(|number| *number > 0)?;
    let url = record.get("url")?.as_str()?;
    if url.chars().any(|ch| (ch as u32) <= 0x1f || ch == '\u{7f}') {
        return None;
    }
    let parsed_url = url::Url::parse(url).ok()?;
    if !matches!(parsed_url.scheme(), "http" | "https") {
        return None;
    }
    let state = record.get("state")?.as_str()?.to_ascii_lowercase();
    let state = match state.as_str() {
        "open" if record.get("isDraft").and_then(Value::as_bool) == Some(true) => "draft",
        "open" => "open",
        "merged" => "merged",
        "closed" => "closed",
        _ => return None,
    };
    Some(json!({ "number": number, "state": state, "url": url }))
}

/// The `fsGitStatusResponse.data` fields one `git status` run produces.
struct GitStatusCore {
    branch: String,
    ahead: u64,
    behind: u64,
    entries: HashMap<String, Value>,
    additions: usize,
    deletions: usize,
}

/// Run git status (and numstat) at `cwd`. `scope_to_cwd` mirrors v2's
/// `scopeToCwd` (`git status -- .` plus `--show-prefix` stripping);
/// `path_filter` mirrors v2's exact-match `pathFilter`. `None` when git is
/// unavailable or `cwd` is not inside a work tree.
fn git_status_at(
    cwd: &Path,
    path_filter: Option<&std::collections::HashSet<String>>,
    scope_to_cwd: bool,
) -> Option<GitStatusCore> {
    let inside = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !inside.status.success() || String::from_utf8_lossy(&inside.stdout).trim() != "true" {
        return None;
    }

    let mut path_prefix = String::new();
    if scope_to_cwd {
        let prefix = std::process::Command::new("git")
            .args(["rev-parse", "--show-prefix"])
            .current_dir(cwd)
            .output()
            .ok()?;
        if !prefix.status.success() {
            return None;
        }
        path_prefix = String::from_utf8_lossy(&prefix.stdout)
            .trim_end_matches(['\r', '\n'])
            .to_string();
    }

    let mut status_args = vec!["status", "--porcelain=v2", "--branch", "-z"];
    if scope_to_cwd {
        status_args.push("--");
        status_args.push(".");
    }
    let out = std::process::Command::new("git")
        .args(&status_args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let GitStatusFields {
        branch,
        ahead,
        behind,
        entries,
    } = parse_git_status_z(&text, path_filter, path_prefix.as_str());

    // v2 runs numstat only when the status is dirty and HEAD exists.
    let mut additions = 0;
    let mut deletions = 0;
    let dirty = text
        .split('\0')
        .any(|r| !r.trim_start_matches('\n').is_empty() && !r.starts_with('#'));
    if dirty {
        let head_ok = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", "HEAD"])
            .current_dir(cwd)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if head_ok {
            let mut diff_args = vec!["diff", "--numstat", "HEAD", "--"];
            if let Some(filter) = path_filter {
                // v2 sorts the filter paths for a deterministic argv.
                let mut sorted_filter: Vec<&str> = filter.iter().map(String::as_str).collect();
                sorted_filter.sort_unstable();
                diff_args.extend(sorted_filter);
            } else if scope_to_cwd {
                diff_args.push(".");
            }
            if let Ok(stat_out) = std::process::Command::new("git")
                .args(&diff_args)
                .current_dir(cwd)
                .output()
                && stat_out.status.success()
            {
                let stat_str = String::from_utf8_lossy(&stat_out.stdout);
                for line in stat_str.lines() {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 2 {
                        if let Ok(add) = parts[0].parse::<usize>() {
                            additions += add;
                        }
                        if let Ok(del) = parts[1].parse::<usize>() {
                            deletions += del;
                        }
                    }
                }
            }
        }
    }

    Some(GitStatusCore {
        branch,
        ahead,
        behind,
        entries,
        additions,
        deletions,
    })
}

impl GitStatusCore {
    fn into_wire(self, pull_request: Value) -> Value {
        json!({
            "branch": self.branch,
            "ahead": self.ahead,
            "behind": self.behind,
            "entries": self.entries,
            "additions": self.additions,
            "deletions": self.deletions,
            "pullRequest": pull_request,
        })
    }
}

/// The empty-but-successful payload the fork answers when git is unavailable
/// or the directory is not a repo (pre-existing wire contract, kept).
fn empty_git_status_wire() -> Value {
    json!({
        "branch": "",
        "ahead": 0,
        "behind": 0,
        "entries": {},
        "additions": 0,
        "deletions": 0,
        "pullRequest": Value::Null,
    })
}

/// v2 `fsGitStatusRequestSchema` fields.
struct GitStatusParams {
    root: Option<String>,
    paths: Option<Vec<String>>,
    include_pull_request: bool,
}

fn parse_include_pull_request(body: &Value) -> Result<bool, HttpResponse> {
    match body.get("include_pull_request") {
        None | Some(Value::Null) => Ok(true),
        Some(v) => v.as_bool().ok_or_else(|| {
            HttpResponse::bad_request("Field 'include_pull_request' must be a boolean")
        }),
    }
}

fn parse_git_status_params(body: &Value) -> Result<GitStatusParams, HttpResponse> {
    let root = match body.get("root") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_str() {
            Some(r) if !r.is_empty() => Some(r.to_string()),
            _ => {
                return Err(HttpResponse::bad_request(
                    "Field 'root' must be a non-empty string",
                ));
            }
        },
    };
    let paths = match body.get("paths") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let Some(arr) = v.as_array() else {
                return Err(HttpResponse::bad_request(
                    "Field 'paths' must be an array of strings",
                ));
            };
            let mut out = Vec::new();
            for p in arr {
                match p.as_str() {
                    Some(s) if !s.is_empty() => out.push(s.to_string()),
                    _ => {
                        return Err(HttpResponse::bad_request(
                            "Field 'paths' must be an array of strings",
                        ));
                    }
                }
            }
            Some(out)
        }
    };
    let include_pull_request = parse_include_pull_request(body)?;
    Ok(GitStatusParams {
        root,
        paths,
        include_pull_request,
    })
}

/// v2 `resolveGitRoot`: default the session workdir; a given `root` must
/// resolve inside the workspace and be a directory. The answer is always
/// canonicalized so path filters and the diff scoper share one path frame
/// (Windows `\\?\` verbatim prefixes included).
fn resolve_git_root(work_dir: &Path, root: Option<&str>) -> Result<PathBuf, HttpResponse> {
    match root {
        None => Ok(work_dir
            .canonicalize()
            .unwrap_or_else(|_| work_dir.to_path_buf())),
        Some(r) => {
            let target = resolve_safe_path(work_dir, r)?;
            if !target.is_dir() {
                return Err(HttpResponse::bad_request(format!(
                    "git root is not a directory: {r}"
                )));
            }
            Ok(target)
        }
    }
}

/// Resolve `paths` against the git root the way v2's `gitStatus` does:
/// relative inputs join the root, and the result must not escape it.
fn git_status_path_filter(
    cwd: &Path,
    paths: &[String],
    scoped: bool,
) -> Result<std::collections::HashSet<String>, HttpResponse> {
    let mut filter = std::collections::HashSet::new();
    for p in paths {
        let abs = resolve_safe_path(cwd, p)?;
        let rel = abs
            .strip_prefix(cwd)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if scoped && (rel.is_empty() || rel == ".." || rel.starts_with("../")) {
            return Err(HttpResponse::bad_request(format!(
                "path \"{p}\" escapes git root"
            )));
        }
        filter.insert(rel);
    }
    Ok(filter)
}

pub fn handle_git_status(work_dir: &Path, body: &Value) -> HttpResponse {
    let params = match parse_git_status_params(body) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let cwd = match resolve_git_root(work_dir, params.root.as_deref()) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let scoped = params.root.is_some();
    let filter = match params.paths.as_deref() {
        Some(paths) if !paths.is_empty() => match git_status_path_filter(&cwd, paths, scoped) {
            Ok(f) => Some(f),
            Err(resp) => return resp,
        },
        _ => None,
    };

    match git_status_at(&cwd, filter.as_ref(), scoped) {
        Some(core) => {
            let pull_request = if params.include_pull_request {
                read_pull_request(&cwd)
            } else {
                Value::Null
            };
            HttpResponse::ok(&core.into_wire(pull_request))
        }
        None => HttpResponse::ok(&empty_git_status_wire()),
    }
}

/// The per-root outcome of a batched git query (v2 `toGitBatchError`).
fn git_batch_entry(result: Result<Value, HttpResponse>) -> (Option<Value>, Option<Value>) {
    match result {
        Ok(v) => (Some(v), None),
        Err(resp) => {
            let msg = serde_json::from_slice::<Value>(&resp.body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str().map(str::to_string)))
                .unwrap_or_else(|| "git query failed".to_string());
            // `resolve_git_root` rejects a non-directory root; a non-repo
            // answers the fork's empty shape instead of 40908 (see
            // handle_git_status), so batch errors are validation-class.
            (None, Some(json!({ "code": 40001, "msg": msg })))
        }
    }
}

/// Run one folder-scoped git status, keeping the `Result` split batch needs.
fn git_status_for_root(
    work_dir: &Path,
    root: &str,
    include_pull_request: bool,
) -> Result<Value, HttpResponse> {
    let cwd = resolve_git_root(work_dir, Some(root))?;
    Ok(match git_status_at(&cwd, None, true) {
        Some(core) => {
            let pull_request = if include_pull_request {
                read_pull_request(&cwd)
            } else {
                Value::Null
            };
            core.into_wire(pull_request)
        }
        None => empty_git_status_wire(),
    })
}

/// Handle `fs:git_status:batch` (v2 `fsGitStatusBatchRequestSchema`):
/// `{ roots: 1..=32, include_pull_request? }` → per-root results/errors.
/// Upstream caps the fan-out at two concurrent workers; the fork's spawn-per-
/// query is synchronous, so the results/errors maps are identical either way.
pub fn handle_git_status_batch(work_dir: &Path, body: &Value) -> HttpResponse {
    let roots = match parse_batch_roots(body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let include_pull_request = match parse_include_pull_request(body) {
        Ok(include) => include,
        Err(resp) => return resp,
    };
    let mut results = serde_json::Map::new();
    let mut errors = serde_json::Map::new();
    for root in roots {
        let (ok, err) = git_batch_entry(git_status_for_root(work_dir, &root, include_pull_request));
        if let Some(v) = ok {
            results.insert(root.clone(), v);
        }
        if let Some(e) = err {
            errors.insert(root, e);
        }
    }
    HttpResponse::ok(&json!({ "results": results, "errors": errors }))
}

/// v2 dedupes `roots` and caps the batch at 32.
fn parse_batch_roots(body: &Value) -> Result<Vec<String>, HttpResponse> {
    let Some(arr) = body.get("roots").and_then(|v| v.as_array()) else {
        return Err(HttpResponse::bad_request(
            "Field 'roots' must be an array of strings",
        ));
    };
    if arr.is_empty() || arr.len() > 32 {
        return Err(HttpResponse::bad_request(
            "Field 'roots' must contain between 1 and 32 entries",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    let mut roots = Vec::new();
    for r in arr {
        match r.as_str() {
            Some(s) if !s.is_empty() => {
                if seen.insert(s.to_string()) {
                    roots.push(s.to_string());
                }
            }
            _ => {
                return Err(HttpResponse::bad_request(
                    "Field 'roots' must be an array of strings",
                ));
            }
        }
    }
    Ok(roots)
}

/// Handle `fs:pull_request` (v2 `fsPullRequestRequestSchema`).
pub fn handle_pull_request(work_dir: &Path, body: &Value) -> HttpResponse {
    let root = body.get("root").and_then(|v| v.as_str());
    match resolve_git_root(work_dir, root) {
        Ok(cwd) => HttpResponse::ok(&read_pull_request(&cwd)),
        Err(resp) => resp,
    }
}

/// Handle `fs:pull_request:batch` (v2 `fsPullRequestBatchRequestSchema`).
pub fn handle_pull_request_batch(work_dir: &Path, body: &Value) -> HttpResponse {
    let roots = match parse_batch_roots(body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let mut results = serde_json::Map::new();
    let mut errors = serde_json::Map::new();
    for root in roots {
        match resolve_git_root(work_dir, Some(&root)) {
            Ok(cwd) => {
                results.insert(root, read_pull_request(&cwd));
            }
            Err(resp) => {
                let (_, err) = git_batch_entry(Err(resp));
                if let Some(e) = err {
                    errors.insert(root, e);
                }
            }
        }
    }
    HttpResponse::ok(&json!({ "results": results, "errors": errors }))
}

/// Handle `fs:diff` (v2 `fsDiffRequestSchema`: `path` + optional `root`).
pub fn handle_diff(work_dir: &Path, body: &Value) -> HttpResponse {
    let rel_path = match body.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => return HttpResponse::bad_request("Field 'path' is required"),
    };
    let root = body.get("root").and_then(|v| v.as_str());

    // v2 resolves `path` against the scoped git root and rejects escapes.
    let cwd = match resolve_git_root(work_dir, root) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let target_path = match resolve_safe_path(&cwd, rel_path) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let scoped_rel = target_path
        .strip_prefix(&cwd)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| rel_path.to_string());
    if root.is_some()
        && (scoped_rel.is_empty() || scoped_rel == ".." || scoped_rel.starts_with("../"))
    {
        return HttpResponse::bad_request(format!("path \"{rel_path}\" escapes git root"));
    }

    // 1. Try git diff HEAD -- <path>
    let output = std::process::Command::new("git")
        .args(["diff", "HEAD", "--", scoped_rel.as_str()])
        .current_dir(&cwd)
        .output();

    if let Ok(out) = output
        && out.status.success()
        && !out.stdout.is_empty()
    {
        let diff_str = String::from_utf8_lossy(&out.stdout).to_string();
        return HttpResponse::ok(&json!({
            "path": rel_path,
            "diff": diff_str,
            "truncated": false,
        }));
    }

    // 2. If git diff was empty or file is untracked, synthesize diff if file exists
    if target_path.is_file()
        && let Ok(content) = std::fs::read_to_string(&target_path)
    {
        let line_count = content.lines().count();
        let mut diff = format!("--- /dev/null\n+++ b/{rel_path}\n@@ -0,0 +1,{line_count} @@\n");
        for line in content.lines() {
            diff.push('+');
            diff.push_str(line);
            diff.push('\n');
        }
        return HttpResponse::ok(&json!({
            "path": rel_path,
            "diff": diff,
            "truncated": false,
        }));
    }

    HttpResponse::ok(&json!({
        "path": rel_path,
        "diff": "",
        "truncated": false,
    }))
}

/// Handle `fs:stat`.
pub fn handle_stat(work_dir: &Path, body: &Value) -> HttpResponse {
    let rel_path = match body.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => return HttpResponse::bad_request("Field 'path' is required"),
    };

    let target = match resolve_safe_path(work_dir, rel_path) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    if !target.exists() {
        return HttpResponse::not_found();
    }

    match file_to_fs_entry(work_dir, &target) {
        Ok(entry) => HttpResponse::ok(&entry),
        Err(e) => HttpResponse::internal_error(format!("Failed to stat file: {e}")),
    }
}

/// Handle `fs:stat_many` (or `fs:statMany`).
pub fn handle_stat_many(work_dir: &Path, body: &Value) -> HttpResponse {
    let paths = match body.get("paths").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return HttpResponse::bad_request("Field 'paths' must be an array of strings"),
    };

    let mut entries = HashMap::new();
    for p_val in paths {
        if let Some(p_str) = p_val.as_str() {
            if let Ok(target) = resolve_safe_path(work_dir, p_str)
                && target.exists()
                && let Ok(entry) = file_to_fs_entry(work_dir, &target)
            {
                entries.insert(p_str.to_string(), entry);
            } else {
                entries.insert(p_str.to_string(), Value::Null);
            }
        }
    }

    HttpResponse::ok(&json!({ "entries": entries }))
}

/// Handle `fs:mkdir`.
pub fn handle_mkdir(work_dir: &Path, body: &Value) -> HttpResponse {
    let rel_path = match body.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => return HttpResponse::bad_request("Field 'path' is required"),
    };
    let recursive = body
        .get("recursive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let target = match resolve_safe_path(work_dir, rel_path) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let create_res = if recursive {
        std::fs::create_dir_all(&target)
    } else {
        std::fs::create_dir(&target)
    };

    if let Err(e) = create_res {
        return HttpResponse::internal_error(format!("Failed to create directory: {e}"));
    }

    match file_to_fs_entry(work_dir, &target) {
        Ok(entry) => HttpResponse::json(201, &entry),
        Err(e) => HttpResponse::internal_error(format!("Failed to stat created directory: {e}")),
    }
}

/// v2 `fsListSortSchema` (`packages/protocol/src/rest/fs.ts`). Upstream's
/// `mtime_desc` / `size_desc` are placeholders that compare by name
/// (`sortChildren` in v2 `workspaceFs/fsService.ts`), so they deliberately
/// fall back to the name order here as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListSort {
    TypeFirst,
    NameAsc,
    NameDesc,
    MtimeDesc,
    SizeDesc,
}

/// A glob list that keeps its raw patterns next to the compiled matchers:
/// `glob_can_match_below` needs the raw segments (v2 `picomatch.scan`), while
/// whole-path matching uses the compiled form.
#[derive(Debug, Default, Clone)]
struct GlobList {
    raw: Vec<String>,
    matchers: Vec<globset::GlobMatcher>,
}

impl GlobList {
    fn is_empty(&self) -> bool {
        self.matchers.is_empty()
    }

    /// v2 `matchesAnyGlob`: whole-path match, empty patterns never match.
    fn matches(&self, rel: &str) -> bool {
        self.matchers.iter().any(|m| m.is_match(rel))
    }
}

/// The filter/sort fields shared by `fs:list` and `fs:list_many` (v2
/// `fsListRequestSchema` / `fsListManyRequestSchema`).
#[derive(Debug, Clone)]
struct ListFilters {
    depth: usize,
    limit: usize,
    show_hidden: bool,
    follow_gitignore: bool,
    exclude_globs: Vec<globset::GlobMatcher>,
    allow_ignored_globs: GlobList,
    sort: ListSort,
    /// Accepted and deliberately inert: upstream declares the field but no
    /// code path attaches per-entry `git_status` (`include_git_status` grep
    /// on upstream/main lands only in schemas and forwarding).
    #[allow(dead_code)]
    include_git_status: bool,
}

impl Default for ListFilters {
    fn default() -> Self {
        Self {
            depth: 1,
            limit: 200,
            show_hidden: false,
            follow_gitignore: true,
            exclude_globs: Vec::new(),
            allow_ignored_globs: GlobList::default(),
            sort: ListSort::TypeFirst,
            include_git_status: false,
        }
    }
}

/// Parse the shared list fields, mirroring the zod ranges (`depth` 1..=10,
/// `limit` 1..=1000); violations are 400 like any other bad field here.
fn parse_list_filters(body: &Value) -> Result<ListFilters, HttpResponse> {
    let mut filters = ListFilters::default();
    if let Some(v) = body.get("depth") {
        let Some(d) = v.as_u64() else {
            return Err(HttpResponse::bad_request(
                "Field 'depth' must be an integer",
            ));
        };
        if !(1..=10).contains(&d) {
            return Err(HttpResponse::bad_request(
                "Field 'depth' must be between 1 and 10",
            ));
        }
        filters.depth = d as usize;
    }
    if let Some(v) = body.get("limit") {
        let Some(l) = v.as_u64() else {
            return Err(HttpResponse::bad_request(
                "Field 'limit' must be an integer",
            ));
        };
        if !(1..=1000).contains(&l) {
            return Err(HttpResponse::bad_request(
                "Field 'limit' must be between 1 and 1000",
            ));
        }
        filters.limit = l as usize;
    }
    if let Some(v) = body.get("show_hidden") {
        let Some(b) = v.as_bool() else {
            return Err(HttpResponse::bad_request(
                "Field 'show_hidden' must be a boolean",
            ));
        };
        filters.show_hidden = b;
    }
    if let Some(v) = body.get("follow_gitignore") {
        let Some(b) = v.as_bool() else {
            return Err(HttpResponse::bad_request(
                "Field 'follow_gitignore' must be a boolean",
            ));
        };
        filters.follow_gitignore = b;
    }
    if let Some(v) = body.get("exclude_globs") {
        let Some(arr) = v.as_array() else {
            return Err(HttpResponse::bad_request(
                "Field 'exclude_globs' must be an array of strings",
            ));
        };
        for g in arr {
            let Some(pattern) = g.as_str() else {
                return Err(HttpResponse::bad_request(
                    "Field 'exclude_globs' must be an array of strings",
                ));
            };
            // Empty patterns never match (v2 `matchesAnyGlob` filters them out
            // because picomatch throws on empty input).
            if pattern.is_empty() {
                continue;
            }
            // `literal_separator(true)` pins picomatch's `*`/`?` never crossing
            // `/`; globset has no dotfile or negation concept, matching
            // picomatch's `{ dot: true, nonegate: true }` options.
            let glob = globset::GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map_err(|e| {
                    HttpResponse::bad_request(format!("Invalid exclude_globs pattern: {e}"))
                })?;
            filters.exclude_globs.push(glob.compile_matcher());
        }
    }
    if let Some(v) = body.get("allow_ignored_globs") {
        let Some(arr) = v.as_array() else {
            return Err(HttpResponse::bad_request(
                "Field 'allow_ignored_globs' must be an array of strings",
            ));
        };
        for g in arr {
            let Some(pattern) = g.as_str() else {
                return Err(HttpResponse::bad_request(
                    "Field 'allow_ignored_globs' must be an array of strings",
                ));
            };
            if pattern.is_empty() {
                continue;
            }
            let glob = globset::GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map_err(|e| {
                    HttpResponse::bad_request(format!("Invalid allow_ignored_globs pattern: {e}"))
                })?;
            filters.allow_ignored_globs.raw.push(pattern.to_string());
            filters
                .allow_ignored_globs
                .matchers
                .push(glob.compile_matcher());
        }
    }
    if let Some(v) = body.get("sort") {
        let Some(s) = v.as_str() else {
            return Err(HttpResponse::bad_request("Field 'sort' must be a string"));
        };
        filters.sort = match s {
            "type_first" => ListSort::TypeFirst,
            "name_asc" => ListSort::NameAsc,
            "name_desc" => ListSort::NameDesc,
            "mtime_desc" => ListSort::MtimeDesc,
            "size_desc" => ListSort::SizeDesc,
            _ => {
                return Err(HttpResponse::bad_request(
                    "Field 'sort' must be one of type_first/name_asc/name_desc/mtime_desc/size_desc",
                ));
            }
        };
    }
    if let Some(v) = body.get("include_git_status") {
        let Some(b) = v.as_bool() else {
            return Err(HttpResponse::bad_request(
                "Field 'include_git_status' must be a boolean",
            ));
        };
        filters.include_git_status = b;
    }
    Ok(filters)
}

/// The v2 gitignore matcher (`matcherFor`): the workspace root's `.gitignore`
/// plus a built-in `.git/` rule. The handlers are stateless per request, so
/// the matcher is built per call rather than cached per root — one extra file
/// read per listing, and `.gitignore` edits are always picked up.
fn gitignore_matcher(work_dir: &Path) -> ignore::gitignore::Gitignore {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(work_dir);
    let _ = builder.add_line(None, ".git/");
    let _ = builder.add(work_dir.join(".gitignore"));
    builder
        .build()
        .unwrap_or_else(|_| ignore::gitignore::Gitignore::empty())
}

/// Case-insensitive primary order with a case-sensitive tiebreak — the
/// closest stable approximation of v2's `localeCompare` for ASCII names.
fn cmp_entry_name(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.cmp(b))
}

/// Split a glob into path segments without cutting through brace groups
/// (v2 uses `picomatch.scan`; a `{a/b,c}` group keeps its slash inside one
/// segment here, and the caller answers conservatively for such segments).
fn scan_glob_segments(glob: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    for ch in glob.chars() {
        match ch {
            '/' if depth == 0 => parts.push(std::mem::take(&mut cur)),
            '{' => {
                depth += 1;
                cur.push(ch);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                cur.push(ch);
            }
            _ => cur.push(ch),
        }
    }
    parts.push(cur);
    parts
}

/// v2 `globSegmentsMatchPrefix`: dynamic-programming check whether the glob
/// can match a path *below* `rel` — i.e. `rel_segments` is a viable prefix of
/// a glob match. `**` consumes zero or more segments; an empty segment (from
/// consecutive separators) matches nothing, as upstream pins.
fn glob_segments_match_prefix(glob_segments: &[String], rel_segments: &[&str]) -> bool {
    let g = glob_segments.len();
    let r = rel_segments.len();
    let width = r + 1;
    let mut dp = vec![0u8; (g + 1) * width];
    for gi in (0..=g).rev() {
        let head = glob_segments.get(gi).map(String::as_str);
        let head_matcher = match head {
            Some(h) if h != "**" && !h.is_empty() => globset::GlobBuilder::new(h)
                .literal_separator(true)
                .build()
                .ok()
                .map(|glob| glob.compile_matcher()),
            _ => None,
        };
        for ri in (0..=r).rev() {
            let at = gi * width + ri;
            dp[at] = if ri == r {
                1
            } else if head == Some("**") {
                dp[(gi + 1) * width + ri] | dp[gi * width + ri + 1]
            } else {
                match head_matcher.as_ref() {
                    Some(matcher) if matcher.is_match(rel_segments[ri]) => {
                        dp[(gi + 1) * width + ri + 1]
                    }
                    _ => 0,
                }
            };
        }
    }
    dp[0] == 1
}

/// v2 `globCanMatchBelow`: can any of the globs match a path underneath
/// `rel`? Used to keep an ignored directory alive when it might be the
/// ancestor of an `allow_ignored_globs` match.
fn glob_can_match_below(rel: &str, globs: &[String]) -> bool {
    let rel_segments: Vec<&str> = rel.split('/').collect();
    for g in globs {
        if g.is_empty() {
            continue;
        }
        let parts = scan_glob_segments(g);
        // A segment that still embeds a separator comes from a brace/extglob
        // construct the splitter cannot see through — v2 answers
        // conservatively (visible) for those too.
        if parts.iter().any(|p| p.contains('/')) {
            return true;
        }
        if glob_segments_match_prefix(&parts, &rel_segments) {
            return true;
        }
    }
    false
}

/// v2 `hasAllowedDescendant`: probe an ignored directory for an actual
/// `allow_ignored_globs` match underneath. Budgets are shared across one
/// `fs:list` request (`visited`), and exhaustion fails *open* (keeps the
/// directory), matching upstream's cap semantics.
#[allow(clippy::too_many_arguments)]
fn probe_allowed_descendant(
    base: &Path,
    rel: &str,
    depth: usize,
    globs: &GlobList,
    exclude_globs: &[globset::GlobMatcher],
    show_hidden: bool,
    allow_external_symlinks: bool,
    real_roots: &[PathBuf],
    visited: &mut usize,
) -> bool {
    const PROBE_MAX_DEPTH: usize = 6;
    const PROBE_MAX_ENTRIES: usize = 500;
    if depth > PROBE_MAX_DEPTH || *visited >= PROBE_MAX_ENTRIES {
        return true;
    }
    let Ok(read_dir) = std::fs::read_dir(base.join(rel)) else {
        return false;
    };
    for dir_entry in read_dir.flatten() {
        if *visited >= PROBE_MAX_ENTRIES {
            return true;
        }
        *visited += 1;
        let name = dir_entry.file_name().to_string_lossy().to_string();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let child_rel = format!("{rel}/{name}");
        if exclude_globs.iter().any(|g| g.is_match(&child_rel)) {
            continue;
        }
        if globs.matches(&child_rel) {
            return true;
        }
        // v2 (from #4457 on): a symlink's directory-ness inside the probe is
        // resolved through the same policy-checked target lookup.
        let is_dir = match std::fs::symlink_metadata(dir_entry.path()) {
            Ok(md) if md.is_symlink() => {
                symlink_directory_target(base, &child_rel, allow_external_symlinks, real_roots).0
            }
            Ok(md) => md.is_dir(),
            Err(_) => false,
        };
        if is_dir
            && glob_can_match_below(&child_rel, &globs.raw)
            && probe_allowed_descendant(
                base,
                &child_rel,
                depth + 1,
                globs,
                exclude_globs,
                show_hidden,
                allow_external_symlinks,
                real_roots,
                visited,
            )
        {
            return true;
        }
    }
    false
}

/// One directory level of a listing walk, after filtering but before sorting.
struct ListChild {
    name: String,
    rel_path: String,
    /// Directory-ness after symlink resolution (v2 `Child.isDirectory`).
    is_dir: bool,
    is_symlink: bool,
    /// The resolved real path for a symlinked directory (loop detection).
    real_path: Option<PathBuf>,
    entry: Value,
}

/// The result of one [`list_dir_core`] walk: v2's `{ items, children_by_path?,
/// truncated }` before the HTTP shape is assembled around it.
struct ListOutcome {
    items: Vec<Value>,
    children_by_path: serde_json::Map<String, Value>,
    truncated: bool,
}

/// The shared `fs:list` walk (v2 `WorkspaceFsService.list`): BFS from the
/// resolved root, filtered by `show_hidden` / gitignore / `exclude_globs`,
/// sorted per level, top-level `items` capped at `limit` (deeper buckets are
/// not capped), `children_by_path` filled for expanded non-root levels.
///
/// Symlinked directories expand in place: without `allow_external_symlinks`
/// only links whose real target stays inside the workspace resolve (v2
/// `symlinkDirectoryTarget`), with it any directory target does. Entries
/// reached through a symlink share a separate 10,000-entry budget (v2
/// `LIST_TOTAL_ENTRY_BUDGET`) whose exhaustion sets `truncated` and stops
/// further symlinked subtrees; ordinary recursion is not budgeted. Ancestor
/// real-paths pin the walk against symlink loops.
#[allow(clippy::too_many_lines)]
fn list_dir_core(
    work_dir: &Path,
    rel_path: &str,
    filters: &ListFilters,
    allow_external_symlinks: bool,
) -> Result<ListOutcome, FsPathError> {
    // With external symlinks allowed, v2 resolves lexically (the realpath
    // check is skipped), so the whole walk stays in the raw work_dir frame;
    // otherwise the canonicalized resolver and frame keep symlink escapes
    // rejected (and the Windows `\\?\` prefix consistent).
    let (target, frame_root) = if allow_external_symlinks {
        (resolve_lexical(work_dir, rel_path)?, work_dir.to_path_buf())
    } else {
        let target = resolve_safe_path_typed(work_dir, rel_path)?;
        let canon_work = work_dir
            .canonicalize()
            .map_err(|e| FsPathError::Internal(format!("Cannot canonicalize work dir: {e}")))?;
        (target, canon_work)
    };
    // The listed root itself follows symlinks (v2 stats the top with
    // follow-semantics), so a symlinked root still lists.
    if !target.is_dir() {
        return Err(FsPathError::NotDirectory(format!(
            "Target path is not a directory: {rel_path}"
        )));
    }

    let gitignore = filters
        .follow_gitignore
        .then(|| gitignore_matcher(&frame_root));
    // v2 `realRoots()`: the real path of the workspace root (the fork has no
    // additional workspace dirs).
    let real_roots =
        vec![realpath_existing_prefix(&frame_root).unwrap_or_else(|_| frame_root.clone())];

    let root_rel = {
        let rel = target
            .strip_prefix(&frame_root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if rel == "." { String::new() } else { rel }
    };

    struct QueueEntry {
        rel_path: String,
        depth_remaining: usize,
        /// Real paths of every ancestor level (v2 loop guard).
        ancestors: Vec<PathBuf>,
        /// Whether this subtree was reached through a symlink (budget scope).
        via_symlink: bool,
    }

    let root_real = realpath_existing_prefix(&target).unwrap_or_else(|_| target.clone());
    let mut queue = std::collections::VecDeque::from([QueueEntry {
        rel_path: root_rel.clone(),
        depth_remaining: filters.depth,
        ancestors: vec![root_real],
        via_symlink: path_has_symlink(&target),
    }]);
    let mut items: Vec<Value> = Vec::new();
    let mut children_by_path: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut truncated = false;
    // Shared across the whole request (v2 keeps one probe budget per list).
    let mut probe_visited = 0usize;
    // v2 `LIST_TOTAL_ENTRY_BUDGET`: caps only symlink-reached entries.
    const LIST_TOTAL_ENTRY_BUDGET: usize = 10_000;
    let mut symlink_entries = 0usize;
    let mut budget_exhausted = false;

    while let Some(entry) = queue.pop_front() {
        if budget_exhausted && entry.via_symlink {
            continue;
        }
        let dir_abs = if entry.rel_path.is_empty() {
            target.clone()
        } else {
            frame_root.join(&entry.rel_path)
        };
        let read_dir = match std::fs::read_dir(&dir_abs) {
            Ok(rd) => rd,
            // v2 rethrows the root's own readdir failure and skips deeper
            // unreadable levels.
            Err(e) if entry.rel_path == root_rel => {
                return Err(FsPathError::Internal(format!(
                    "Failed to read directory: {e}"
                )));
            }
            Err(_) => continue,
        };

        let mut children: Vec<ListChild> = Vec::new();
        for dir_entry in read_dir.flatten() {
            let name = dir_entry.file_name().to_string_lossy().to_string();
            if !filters.show_hidden && name.starts_with('.') {
                continue;
            }
            let child_rel = if entry.rel_path.is_empty() {
                name.clone()
            } else {
                format!("{}/{name}", entry.rel_path)
            };
            let mut ancestor_of_allowed = false;
            if let Some(ig) = &gitignore {
                // v2 `ignores(childRel) || ignores(childRel + '/')`: the npm
                // `ignore` package is parent-aware (a child of an ignored
                // directory is ignored), and checks both kind forms because
                // the kind is not known yet — mirrored with
                // `matched_path_or_any_parents` in both is_dir forms.
                if ig
                    .matched_path_or_any_parents(&child_rel, false)
                    .is_ignore()
                    || ig.matched_path_or_any_parents(&child_rel, true).is_ignore()
                {
                    // v2 `allow_ignored_globs`: an ignored entry stays visible
                    // when it matches outright, and an ignored *directory*
                    // stays when it might be the ancestor of a match (probed
                    // below after the lstat confirms the kind).
                    if filters.allow_ignored_globs.is_empty() {
                        continue;
                    }
                    if !filters.allow_ignored_globs.matches(&child_rel) {
                        if !glob_can_match_below(&child_rel, &filters.allow_ignored_globs.raw) {
                            continue;
                        }
                        ancestor_of_allowed = true;
                    }
                }
            }
            if filters.exclude_globs.iter().any(|g| g.is_match(&child_rel)) {
                continue;
            }
            let child_abs = dir_entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&child_abs) else {
                continue;
            };
            // v2: a symlink's directory-ness comes from its (policy-checked)
            // target, not the lstat.
            let mut is_dir = metadata.is_dir();
            let mut real_path: Option<PathBuf> = None;
            if metadata.is_symlink() {
                let (target_is_dir, target_real) = symlink_directory_target(
                    &frame_root,
                    &child_rel,
                    allow_external_symlinks,
                    &real_roots,
                );
                is_dir = target_is_dir;
                real_path = target_real;
            }
            if ancestor_of_allowed && !is_dir {
                continue;
            }
            if ancestor_of_allowed
                && !probe_allowed_descendant(
                    &frame_root,
                    &child_rel,
                    1,
                    &filters.allow_ignored_globs,
                    &filters.exclude_globs,
                    filters.show_hidden,
                    allow_external_symlinks,
                    &real_roots,
                    &mut probe_visited,
                )
            {
                continue;
            }
            let mut fs_entry = fs_entry_from_metadata(&frame_root, &child_abs, &metadata);
            if metadata.is_symlink() && is_dir {
                // v2 marks a symlink whose target is a directory so the file
                // tree can render it as an expandable directory.
                fs_entry["is_directory"] = json!(true);
            }
            children.push(ListChild {
                name,
                rel_path: child_rel,
                is_dir,
                is_symlink: metadata.is_symlink(),
                real_path,
                entry: fs_entry,
            });
        }

        let is_top_level = entry.depth_remaining == filters.depth;
        match filters.sort {
            ListSort::TypeFirst => children.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| cmp_entry_name(&a.name, &b.name))
            }),
            ListSort::NameAsc | ListSort::MtimeDesc | ListSort::SizeDesc => {
                children.sort_by(|a, b| cmp_entry_name(&a.name, &b.name));
            }
            ListSort::NameDesc => children.sort_by(|a, b| cmp_entry_name(&b.name, &a.name)),
        }

        let parent_key = if entry.rel_path.is_empty() {
            "."
        } else {
            entry.rel_path.as_str()
        };
        let mut bucket: Vec<Value> = Vec::new();
        for child in children {
            if items.len() >= filters.limit && is_top_level {
                truncated = true;
                break;
            }
            // v2's symlink budget is checked per child before it is emitted;
            // ordinary (non-symlink-reached) listings are never capped.
            if entry.via_symlink && symlink_entries >= LIST_TOTAL_ENTRY_BUDGET {
                truncated = true;
                budget_exhausted = true;
                break;
            }
            if is_top_level {
                items.push(child.entry.clone());
            }
            bucket.push(child.entry);
            if entry.via_symlink {
                symlink_entries += 1;
            }
            if child.is_dir && entry.depth_remaining > 1 {
                let real_path = match child.real_path {
                    Some(rp) => Some(rp),
                    None => realpath_existing_prefix(&frame_root.join(&child.rel_path)).ok(),
                };
                let child_via_symlink = entry.via_symlink || child.is_symlink;
                if let Some(real_path) = real_path
                    && !entry.ancestors.contains(&real_path)
                    && (!budget_exhausted || !child_via_symlink)
                {
                    let mut ancestors = entry.ancestors.clone();
                    ancestors.push(real_path);
                    queue.push_back(QueueEntry {
                        rel_path: child.rel_path,
                        depth_remaining: entry.depth_remaining - 1,
                        ancestors,
                        via_symlink: child_via_symlink,
                    });
                }
            }
        }
        if !is_top_level {
            children_by_path.insert(parent_key.to_string(), json!(bucket));
        }
    }

    Ok(ListOutcome {
        items,
        children_by_path,
        truncated,
    })
}

/// Handle `fs:list`.
pub fn handle_list(work_dir: &Path, body: &Value) -> HttpResponse {
    let rel_path = body
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let filters = match parse_list_filters(body) {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    // v2 `fsListRequestSchema.allow_external_symlinks` (not present on
    // `fs:list_many`'s schema upstream, so it is parsed here only).
    let allow_external_symlinks = body
        .get("allow_external_symlinks")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    match list_dir_core(work_dir, rel_path, &filters, allow_external_symlinks) {
        Ok(outcome) => {
            let mut out = json!({
                "items": outcome.items.clone(),
                "entries": outcome.items,
                "truncated": outcome.truncated,
            });
            if !outcome.children_by_path.is_empty() {
                out["children_by_path"] = Value::Object(outcome.children_by_path);
            }
            HttpResponse::ok(&out)
        }
        Err(err) => err.into_response(),
    }
}

/// Handle `fs:list_many` (or `fs:listMany`).
///
/// v2 `WorkspaceFsService.listMany`: each path lists independently; a path
/// that fails lands in `partial_errors` (no `results` key for it), a truncated
/// one is echoed in `truncated_paths`, and a path escape fails the whole call.
pub fn handle_list_many(work_dir: &Path, body: &Value) -> HttpResponse {
    let paths = match body.get("paths").and_then(|v| v.as_array()) {
        Some(p) => p,
        None => return HttpResponse::bad_request("Field 'paths' must be an array of strings"),
    };
    let filters = match parse_list_filters(body) {
        Ok(f) => f,
        Err(resp) => return resp,
    };

    let mut results = serde_json::Map::new();
    let mut truncated_paths: Vec<String> = Vec::new();
    let mut partial_errors = serde_json::Map::new();
    for p_val in paths {
        let Some(rel_path) = p_val.as_str() else {
            continue;
        };
        match list_dir_core(work_dir, rel_path, &filters, false) {
            Ok(outcome) => {
                if outcome.truncated {
                    truncated_paths.push(rel_path.to_string());
                }
                results.insert(rel_path.to_string(), json!(outcome.items));
            }
            Err(FsPathError::Escapes) => return FsPathError::Escapes.into_response(),
            Err(FsPathError::NotDirectory(msg)) | Err(FsPathError::Internal(msg)) => {
                // 40409 covers both "missing" and "not a directory" here (v2
                // `FS_PATH_NOT_FOUND` answers both for a list target).
                partial_errors.insert(rel_path.to_string(), json!({ "code": 40409, "msg": msg }));
            }
        }
    }

    let mut out = json!({ "results": results });
    if !truncated_paths.is_empty() {
        out["truncated_paths"] = json!(truncated_paths);
    }
    if !partial_errors.is_empty() {
        out["partial_errors"] = Value::Object(partial_errors);
    }
    HttpResponse::ok(&out)
}

/// Handle `fs:read`.
pub fn handle_read(work_dir: &Path, body: &Value) -> HttpResponse {
    let rel_path = match body.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => return HttpResponse::bad_request("Field 'path' is required"),
    };

    // v2 `fsReadRequestSchema.allow_external_symlinks`: allow reading through
    // symlinks whose targets live outside the workspace.
    let allow_external_symlinks = body
        .get("allow_external_symlinks")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let target = match resolve_safe_path_with_symlinks(work_dir, rel_path, allow_external_symlinks)
    {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    if !target.exists() {
        return HttpResponse::not_found();
    }

    if !target.is_file() {
        return HttpResponse::bad_request("Target is not a regular file");
    }

    let offset = body.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let length = body
        .get("length")
        .and_then(|v| v.as_u64())
        .unwrap_or(1_048_576) as usize;

    let content_bytes = match std::fs::read(&target) {
        Ok(b) => b,
        Err(e) => return HttpResponse::internal_error(format!("Failed to read file: {e}")),
    };

    let total_size = content_bytes.len();
    let is_binary = content_bytes.iter().take(512).any(|&b| b == 0);

    let (slice, truncated) = if offset >= total_size {
        (&[][..], false)
    } else {
        let end = (offset + length).min(total_size);
        (&content_bytes[offset..end], end < total_size)
    };

    let content_str = String::from_utf8_lossy(slice).to_string();

    let mime = if is_binary {
        "application/octet-stream"
    } else if rel_path.ends_with(".json") {
        "application/json"
    } else {
        "text/plain"
    };

    let etag = format!("\"{:x}-{:x}\"", total_size, total_size ^ offset);

    HttpResponse::ok(&json!({
        "path": rel_path,
        "content": content_str,
        "encoding": "utf-8",
        "size": total_size,
        "truncated": truncated,
        "etag": etag,
        "mime": mime,
        "is_binary": is_binary,
    }))
}

/// Character offsets in `path` where `query` matched, as UTF-16 code-unit
/// indices — the frame the Web UI and the VS Code mention picker highlight in
/// (`match_positions` on the wire). Empty when the query is empty or absent.
fn match_positions(path: &str, query: &str) -> Vec<usize> {
    if query.is_empty() {
        return Vec::new();
    }
    let path_chars: Vec<char> = path.chars().collect();
    let query_chars: Vec<char> = query.chars().collect();
    if query_chars.len() > path_chars.len() {
        return Vec::new();
    }
    let fold = |c: char| c.to_lowercase().next().unwrap_or(c);
    let needle: Vec<char> = query_chars.iter().map(|c| fold(*c)).collect();
    for start in 0..=(path_chars.len() - query_chars.len()) {
        if (0..needle.len()).all(|i| fold(path_chars[start + i]) == needle[i]) {
            let unit_start: usize = path_chars[..start].iter().map(|c| c.len_utf16()).sum();
            let unit_len: usize = path_chars[start..start + needle.len()]
                .iter()
                .map(|c| c.len_utf16())
                .sum();
            return (unit_start..unit_start + unit_len).collect();
        }
    }
    Vec::new()
}

/// Search `work_dir` for paths matching `query`, returning `(items, truncated)`.
///
/// Shared by the `fs:search` / `fs::suggest` HTTP routes and the `fs_suggest`
/// napi export, so the host and the Web UI see one ranking and one match frame.
pub fn search_files(work_dir: &Path, query: &str, limit: usize) -> (Vec<Value>, bool) {
    let mut items = Vec::new();
    if query.is_empty() {
        if let Ok(read_dir) = std::fs::read_dir(work_dir) {
            for entry in read_dir.flatten() {
                if let Ok(fs_entry) = file_to_fs_entry(work_dir, &entry.path()) {
                    let path = fs_entry["path"].as_str().unwrap_or_default().to_string();
                    let name = fs_entry["name"].as_str().unwrap_or_default().to_string();
                    let kind = fs_entry["kind"].as_str().unwrap_or("file").to_string();
                    items.push(json!({
                        "path": path,
                        "name": name,
                        "kind": kind,
                        "match_positions": [],
                    }));
                }
                if items.len() >= limit {
                    break;
                }
            }
        }
    } else {
        let git_ls = std::process::Command::new("git")
            .args(["ls-files", "--cached", "--others", "--exclude-standard"])
            .current_dir(work_dir)
            .output();

        let q_lower = query.to_lowercase();
        if let Ok(out) = git_ls
            && out.status.success()
        {
            let files_str = String::from_utf8_lossy(&out.stdout);
            for line in files_str.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed.to_lowercase().contains(&q_lower) {
                    let path = trimmed.replace('\\', "/");
                    let name = Path::new(&path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.clone());
                    let size = std::fs::metadata(work_dir.join(&path))
                        .map(|m| m.len())
                        .unwrap_or(0);
                    let positions = match_positions(&path, query);
                    items.push(json!({
                        "path": path,
                        "name": name,
                        "kind": "file",
                        "size": size,
                        "match_positions": positions,
                    }));
                    if items.len() >= limit {
                        break;
                    }
                }
            }
        } else {
            let mut stack = vec![work_dir.to_path_buf()];
            while let Some(dir) = stack.pop() {
                if let Ok(read_dir) = std::fs::read_dir(&dir) {
                    for entry in read_dir.flatten() {
                        let path = entry.path();
                        if path.is_dir() {
                            let name = entry.file_name().to_string_lossy().to_string();
                            if !name.starts_with('.') && name != "node_modules" && name != "target"
                            {
                                stack.push(path);
                            }
                        } else if path.is_file() {
                            let rel = path
                                .strip_prefix(work_dir)
                                .unwrap_or(&path)
                                .to_string_lossy()
                                .replace('\\', "/");
                            if rel.to_lowercase().contains(&q_lower) {
                                let name = entry.file_name().to_string_lossy().to_string();
                                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                                let positions = match_positions(&rel, query);
                                items.push(json!({
                                    "path": rel,
                                    "name": name,
                                    "kind": "file",
                                    "size": size,
                                    "match_positions": positions,
                                }));
                                if items.len() >= limit {
                                    break;
                                }
                            }
                        }
                    }
                }
                if items.len() >= limit {
                    break;
                }
            }
        }
    }

    let truncated = items.len() >= limit;
    (items, truncated)
}

/// Handle `fs:search`.
pub fn handle_search(work_dir: &Path, body: &Value) -> HttpResponse {
    let query = body
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let limit = body.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
    let (items, truncated) = search_files(work_dir, query, limit);
    HttpResponse::ok(&json!({
        "items": items,
        "truncated": truncated,
    }))
}

/// Handle `fs:grep`.
pub fn handle_grep(work_dir: &Path, body: &Value) -> HttpResponse {
    let pattern = match body.get("pattern").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => return HttpResponse::bad_request("Field 'pattern' is required"),
    };
    let case_sensitive = body
        .get("case_sensitive")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let is_regex = body.get("regex").and_then(|v| v.as_bool()).unwrap_or(false);
    let max_files = body
        .get("max_files")
        .and_then(|v| v.as_u64())
        .unwrap_or(200) as usize;

    let start_time = std::time::Instant::now();

    let mut args = vec!["grep", "-n", "-I"];
    if !case_sensitive {
        args.push("-i");
    }
    if is_regex {
        args.push("-E");
    } else {
        args.push("-F");
    }
    args.push("-e");
    args.push(pattern);
    args.push("--");
    args.push(".");

    let output = std::process::Command::new("git")
        .args(&args)
        .current_dir(work_dir)
        .output();

    let mut file_hits: HashMap<String, Vec<Value>> = HashMap::new();
    let mut files_scanned = 0;

    if let Ok(out) = output
        && (out.status.success() || out.status.code() == Some(1))
    {
        let stdout_str = String::from_utf8_lossy(&out.stdout);
        for line in stdout_str.lines() {
            let parts: Vec<&str> = line.splitn(3, ':').collect();
            if parts.len() == 3 {
                let file = parts[0].replace('\\', "/");
                let line_num = parts[1].parse::<usize>().unwrap_or(1);
                let content = parts[2];

                let hits = file_hits.entry(file).or_default();
                hits.push(json!({
                    "line_number": line_num,
                    "line_content": content,
                }));
            }
        }
        files_scanned = file_hits.len();
    }

    let truncated = file_hits.len() > max_files;
    let mut files: Vec<Value> = file_hits
        .into_iter()
        .take(max_files)
        .map(|(path, hits)| {
            json!({
                "path": path,
                "hits": hits,
            })
        })
        .collect();
    files.sort_by(|a, b| {
        a["path"]
            .as_str()
            .unwrap_or("")
            .cmp(b["path"].as_str().unwrap_or(""))
    });

    let elapsed_ms = start_time.elapsed().as_millis() as usize;

    HttpResponse::ok(&json!({
        "files": files,
        "files_scanned": files_scanned,
        "truncated": truncated,
        "elapsed_ms": elapsed_ms,
    }))
}

/// Handle `fs:open`: launch the resolved file in the user's editor (or the
/// platform opener) and report the real outcome. Previously this discarded the
/// resolved path and answered `{"opened": true}` without launching anything.
pub fn handle_open(work_dir: &Path, body: &Value) -> HttpResponse {
    let rel_path = match body.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => return HttpResponse::bad_request("Field 'path' is required"),
    };
    let target = match resolve_safe_path(work_dir, rel_path) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let line = body.get("line").and_then(|v| v.as_u64());
    match crate::server::file_launch::open_file(&target, line) {
        Ok(()) => HttpResponse::ok(&json!({ "opened": true })),
        Err(error) => HttpResponse::internal_error(error),
    }
}

/// Handle `fs:reveal`: select the resolved file in the platform file manager.
pub fn handle_reveal(work_dir: &Path, body: &Value) -> HttpResponse {
    let rel_path = match body.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => return HttpResponse::bad_request("Field 'path' is required"),
    };
    let target = match resolve_safe_path(work_dir, rel_path) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    match crate::server::file_launch::reveal_file(&target) {
        Ok(()) => HttpResponse::ok(&json!({ "revealed": true })),
        Err(error) => HttpResponse::internal_error(error),
    }
}

/// Handle `fs:open-in`: open the resolved path in the requested app
/// (v2 `OpenInAppId`: finder / cursor / vscode / iterm / terminal).
pub fn handle_open_in(work_dir: &Path, body: &Value) -> HttpResponse {
    let rel_path = match body.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => return HttpResponse::bad_request("Field 'path' is required"),
    };
    let app = match body.get("app").and_then(|v| v.as_str()) {
        Some(a) if !a.trim().is_empty() => a.trim(),
        _ => return HttpResponse::bad_request("Field 'app' is required"),
    };
    let target = match resolve_safe_path(work_dir, rel_path) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let line = body.get("line").and_then(|v| v.as_u64());
    match crate::server::file_launch::open_in_app(app, &target, line) {
        Ok(()) => HttpResponse::ok(&json!({ "opened": true })),
        Err(error) => HttpResponse::internal_error(error),
    }
}

/// The `fs:open-in` app ids available on this machine (v2
/// `getAvailableOpenInApps`).
pub fn handle_open_in_apps(_body: &Value) -> HttpResponse {
    let apps: Vec<&str> = crate::server::file_launch::OPEN_IN_APP_IDS
        .iter()
        .copied()
        .filter(|id| crate::server::file_launch::is_open_in_app_available(id))
        .collect();
    HttpResponse::ok(&json!({ "apps": apps }))
}

/// Content-Type for a served file, from its extension. Deliberately small:
/// the caller (web UI) renders text itself, and `application/octet-stream` is
/// the honest fallback for everything the map does not know.
fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some(
            "txt" | "md" | "log" | "json" | "toml" | "yaml" | "yml" | "csv" | "rs" | "ts" | "tsx"
            | "js" | "jsx" | "mjs" | "cjs" | "py" | "go" | "java" | "c" | "h" | "cpp" | "html"
            | "css" | "sh",
        ) => "text/plain; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Handle `fs::content` — serve the raw bytes of a host file by **absolute**
/// path (kap-server `fsContentQuerySchema`): `path` is required and must be
/// absolute, the response is raw bytes with an ETag, `If-None-Match` yields a
/// 304, and a single `Range: bytes=a-b` request yields a 206 slice.
///
/// Like `fs:home` / `fs:browse` this is a whole-host surface by design; the
/// bearer auth at the HTTP layer is what gates it.
pub fn handle_fs_content(
    query_path: Option<&str>,
    if_none_match: Option<&str>,
    range: Option<&str>,
) -> HttpResponse {
    use std::time::UNIX_EPOCH;

    let Some(raw_path) = query_path.map(str::trim).filter(|p| !p.is_empty()) else {
        return HttpResponse::bad_request("Query parameter 'path' is required");
    };
    let path = Path::new(raw_path);
    if !path.is_absolute() {
        return HttpResponse::bad_request("Query parameter 'path' must be an absolute path");
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return HttpResponse::json(
            404,
            &json!({ "error": "FS_PATH_NOT_FOUND", "path": raw_path }),
        );
    };
    if meta.is_dir() {
        return HttpResponse::bad_request("FS_IS_DIRECTORY");
    }
    let len = meta.len() as usize;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let etag = format!("\"{mtime_ms:x}-{len:x}\"");

    if let Some(inm) = if_none_match
        && inm.split(',').any(|tag| tag.trim() == etag)
    {
        return HttpResponse::json(304, &Value::Null).with_header("ETag", etag.clone());
    }

    let Ok(bytes) = std::fs::read(path) else {
        return HttpResponse::internal_error(format!("failed to read {raw_path}"));
    };

    // Single-range request (`bytes=a-b`, `bytes=a-`, `bytes=-suffix`). Anything
    // else (multiple ranges, units other than bytes) is served in full, which
    // is what a client that ignores 206 semantics expects anyway.
    let range = range
        .filter(|r| r.starts_with("bytes="))
        .and_then(|r| r["bytes=".len()..].split(',').next().map(str::to_string));
    if let Some(spec) = range.as_deref() {
        let (start_str, end_str) = match spec.split_once('-') {
            Some(pair) => pair,
            None => return HttpResponse::bad_request("Malformed Range header"),
        };
        let parsed = match (start_str.parse::<usize>(), end_str.parse::<usize>()) {
            (Ok(start), Ok(end)) if start <= end && end < len => Some((start, end)),
            (Ok(start), Err(_)) if start < len => Some((start, len - 1)),
            (Err(_), Ok(suffix)) if suffix > 0 => {
                Some((len.saturating_sub(suffix), len.saturating_sub(1)))
            }
            _ => None,
        };
        if let Some((start, end)) = parsed {
            let slice = bytes[start..=end].to_vec();
            let mut resp = HttpResponse::json(206, &Value::Null);
            resp.body = slice;
            resp.headers
                .insert("Content-Type".into(), content_type_for(path).into());
            resp.headers
                .insert("Content-Range".into(), format!("bytes {start}-{end}/{len}"));
            resp.headers.insert("Accept-Ranges".into(), "bytes".into());
            resp.headers.insert("ETag".into(), etag);
            return resp;
        }
    }

    let mut resp = HttpResponse::json(200, &Value::Null);
    resp.body = bytes;
    resp.headers
        .insert("Content-Type".into(), content_type_for(path).into());
    resp.headers.insert("Accept-Ranges".into(), "bytes".into());
    resp.headers.insert("ETag".into(), etag);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_git_status_z_verbatim_paths() {
        // Real git -z output: every record, header records included, is
        // NUL-terminated with no LF separators.
        let raw = concat!(
            "# branch.oid 1234\0",
            "# branch.head main\0",
            "# branch.ab +1 -2\0",
            "1 .M N... 100644 100644 100644 <hH> <hI> src/main.rs\0",
            "2 RM N... 100644 100644 100644 <hH> <hI> R100 新文档笔记测试.txt\0old name.txt\0",
            "? 未跟踪 目录/笔记.md\0",
            "u UU N... 100644 100644 100644 100644 <h1> <h2> <h3> 冲突.txt\0",
        );
        let parsed = parse_git_status_z(raw, None, "");
        assert_eq!(parsed.branch, "main");
        assert_eq!(parsed.ahead, 1);
        assert_eq!(parsed.behind, 2);
        // v2 wire shape: path → status string (`record<path, fsGitStatusSchema>`).
        assert_eq!(
            parsed.entries["src/main.rs"], "modified",
            "ordinary modification is classified"
        );
        assert!(
            parsed.entries.contains_key("新文档笔记测试.txt"),
            "C-quoted rename path is decoded to its real name, not bogus segments: {:?}",
            parsed.entries.keys().collect::<Vec<_>>()
        );
        assert_eq!(parsed.entries["新文档笔记测试.txt"], "renamed");
        assert!(
            parsed.entries.contains_key("未跟踪 目录/笔记.md"),
            "untracked path with space and non-ASCII stays verbatim"
        );
        assert!(
            parsed.entries.contains_key("冲突.txt"),
            "unmerged path stays verbatim"
        );
        // `conflicted`, not the fork's earlier off-enum `conflict`.
        assert_eq!(parsed.entries["冲突.txt"], "conflicted");
    }

    #[test]
    fn parse_git_status_z_strips_scope_prefix_and_filters() {
        let raw = concat!(
            "# branch.head main\0",
            "1 .M N... 100644 100644 100644 <hH> <hI> pkg/a.ts\0",
            "1 .M N... 100644 100644 100644 <hH> <hI> other/b.ts\0",
        );
        // Folder-scoped to `pkg/`: keys come back relative to the root.
        let parsed = parse_git_status_z(raw, None, "pkg/");
        assert!(parsed.entries.contains_key("a.ts"));
        assert!(!parsed.entries.contains_key("pkg/a.ts"));

        // Exact-match path filter (v2 `pathFilter`), applied after stripping.
        let filter = std::collections::HashSet::from(["a.ts".to_string()]);
        let parsed = parse_git_status_z(raw, Some(&filter), "pkg/");
        assert_eq!(parsed.entries.len(), 1);
        assert!(parsed.entries.contains_key("a.ts"));
    }

    #[test]
    fn test_parse_git_status_z_empty_and_failure_shapes() {
        let parsed = parse_git_status_z("", None, "");
        assert_eq!(parsed.branch, "");
        assert_eq!(parsed.ahead, 0);
        assert_eq!(parsed.behind, 0);
        assert!(parsed.entries.is_empty());

        let parsed = parse_git_status_z("# branch.head (detached)\0", None, "");
        assert_eq!(parsed.branch, "", "detached HEAD yields no branch name");
    }

    #[test]
    fn test_resolve_safe_path_boundaries() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path();

        let sub_file = root.join("hello.txt");
        std::fs::write(&sub_file, "world").unwrap();

        // Normal path resolves
        let safe = resolve_safe_path(root, "hello.txt").unwrap();
        assert_eq!(safe, sub_file.canonicalize().unwrap());

        // Escape path rejected
        assert!(resolve_safe_path(root, "../outside.txt").is_err());
    }

    #[test]
    fn test_file_to_fs_entry_stat() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path();

        let sub_file = root.join("demo.txt");
        std::fs::write(&sub_file, "abc").unwrap();

        let entry = file_to_fs_entry(root, &sub_file).unwrap();
        assert_eq!(entry["name"], "demo.txt");
        assert_eq!(entry["kind"], "file");
        assert_eq!(entry["size"], 3);
        assert_eq!(entry["is_binary"], false);
    }

    // --- F0: fs:list filtering baseline (v2 fsListRequestSchema semantics) ---

    /// Collect the `name`s of a list response's top-level items.
    fn list_names(res: &HttpResponse) -> Vec<String> {
        assert_eq!(res.status, 200);
        let json: Value = serde_json::from_slice(&res.body).unwrap();
        json["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn list_hides_dotfiles_unless_show_hidden() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("visible.txt"), "v").unwrap();
        std::fs::write(root.path().join(".hidden"), "h").unwrap();

        assert_eq!(
            list_names(&handle_list(root.path(), &json!({}))),
            vec!["visible.txt"]
        );
        let mut shown = list_names(&handle_list(root.path(), &json!({ "show_hidden": true })));
        shown.sort();
        assert_eq!(shown, vec![".hidden", "visible.txt"]);
    }

    #[test]
    fn list_follows_root_gitignore_by_default() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".gitignore"), "ignored.txt\nignoredir/\n").unwrap();
        std::fs::write(root.path().join("ignored.txt"), "i").unwrap();
        std::fs::write(root.path().join("kept.txt"), "k").unwrap();
        std::fs::create_dir(root.path().join("ignoredir")).unwrap();
        std::fs::write(root.path().join("ignoredir/x.txt"), "x").unwrap();

        assert_eq!(
            list_names(&handle_list(root.path(), &json!({}))),
            vec!["kept.txt"]
        );
        let mut off = list_names(&handle_list(
            root.path(),
            &json!({ "follow_gitignore": false, "show_hidden": true }),
        ));
        off.sort();
        assert_eq!(
            off,
            vec![".gitignore", "ignored.txt", "ignoredir", "kept.txt"]
        );
    }

    #[test]
    fn list_keeps_git_dir_hidden_even_with_show_hidden() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".git").join("HEAD"), "ref").unwrap();
        std::fs::write(root.path().join("visible.txt"), "v").unwrap();

        // v2 pins `.git/` into the matcher, so the repository internals stay
        // hidden even when dotfiles are explicitly requested.
        assert_eq!(
            list_names(&handle_list(root.path(), &json!({ "show_hidden": true }))),
            vec!["visible.txt"]
        );
        let mut off = list_names(&handle_list(
            root.path(),
            &json!({ "show_hidden": true, "follow_gitignore": false }),
        ));
        off.sort();
        assert_eq!(off, vec![".git", "visible.txt"]);
    }

    #[test]
    fn list_exclude_globs_skip_matches() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.log"), "a").unwrap();
        std::fs::write(root.path().join("b.txt"), "b").unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        std::fs::write(root.path().join("sub/c.log"), "c").unwrap();

        // `*.log` does not cross `/` (picomatch semantics via literal_separator).
        assert_eq!(
            list_names(&handle_list(
                root.path(),
                &json!({ "exclude_globs": ["*.log"], "sort": "name_asc" })
            )),
            vec!["b.txt", "sub"]
        );
        assert_eq!(
            list_names(&handle_list(
                root.path(),
                &json!({ "exclude_globs": ["**/*.log"], "sort": "name_asc" })
            )),
            vec!["b.txt", "sub"]
        );
    }

    #[test]
    fn list_depth_two_emits_children_by_path() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        std::fs::write(root.path().join("sub/inner.txt"), "i").unwrap();
        std::fs::write(root.path().join("top.txt"), "t").unwrap();

        let shallow: Value =
            serde_json::from_slice(&handle_list(root.path(), &json!({})).body).unwrap();
        assert!(shallow.get("children_by_path").is_none());

        let deep: Value =
            serde_json::from_slice(&handle_list(root.path(), &json!({ "depth": 2 })).body).unwrap();
        assert_eq!(deep["items"].as_array().unwrap().len(), 2);
        let bucket = deep["children_by_path"]["sub"].as_array().unwrap();
        assert_eq!(bucket.len(), 1);
        assert_eq!(bucket[0]["path"], "sub/inner.txt");
        // The grandchild level is not expanded further at depth 2.
        assert!(deep["children_by_path"].get("sub/inner.txt").is_none());
    }

    #[test]
    fn list_limit_truncates_top_level_only() {
        let root = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(root.path().join(name), "x").unwrap();
        }
        std::fs::create_dir(root.path().join("sub")).unwrap();
        for name in ["d.txt", "e.txt", "f.txt"] {
            std::fs::write(root.path().join("sub").join(name), "x").unwrap();
        }

        let res: Value = serde_json::from_slice(
            &handle_list(root.path(), &json!({ "limit": 2, "depth": 2 })).body,
        )
        .unwrap();
        assert_eq!(res["items"].as_array().unwrap().len(), 2);
        assert_eq!(res["truncated"], true);
        // Deeper buckets are not capped by `limit` (v2 checks the cap only on
        // the top level).
        assert_eq!(res["children_by_path"]["sub"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn list_sort_variants() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("B.txt"), "b").unwrap();
        std::fs::write(root.path().join("a.txt"), "a").unwrap();
        std::fs::create_dir(root.path().join("zeta")).unwrap();

        let names = |body: Value| list_names(&handle_list(root.path(), &body));
        assert_eq!(names(json!({})), vec!["zeta", "a.txt", "B.txt"]);
        assert_eq!(
            names(json!({ "sort": "name_asc" })),
            vec!["a.txt", "B.txt", "zeta"]
        );
        assert_eq!(
            names(json!({ "sort": "name_desc" })),
            vec!["zeta", "B.txt", "a.txt"]
        );
        // v2's mtime_desc/size_desc are name-ordered placeholders.
        assert_eq!(
            names(json!({ "sort": "mtime_desc" })),
            vec!["a.txt", "B.txt", "zeta"]
        );
    }

    #[test]
    fn list_validates_field_ranges() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(handle_list(root.path(), &json!({ "depth": 0 })).status, 400);
        assert_eq!(
            handle_list(root.path(), &json!({ "depth": 11 })).status,
            400
        );
        assert_eq!(handle_list(root.path(), &json!({ "limit": 0 })).status, 400);
        assert_eq!(
            handle_list(root.path(), &json!({ "limit": 1001 })).status,
            400
        );
        assert_eq!(
            handle_list(root.path(), &json!({ "sort": "bogus" })).status,
            400
        );
        assert_eq!(
            handle_list(root.path(), &json!({ "show_hidden": "yes" })).status,
            400
        );
    }

    #[cfg(unix)]
    #[test]
    fn list_reports_symlink_kind_without_following() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("target.txt"), "t").unwrap();
        std::fs::create_dir(root.path().join("realdir")).unwrap();
        std::os::unix::fs::symlink(root.path().join("target.txt"), root.path().join("link.txt"))
            .unwrap();
        std::os::unix::fs::symlink(root.path().join("realdir"), root.path().join("linkdir"))
            .unwrap();

        let res: Value =
            serde_json::from_slice(&handle_list(root.path(), &json!({ "sort": "name_asc" })).body)
                .unwrap();
        let kinds: std::collections::HashMap<&str, &str> = res["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item["name"].as_str().unwrap(),
                    item["kind"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(kinds["link.txt"], "symlink");
        assert_eq!(kinds["linkdir"], "symlink");
        assert_eq!(kinds["realdir"], "directory");

        // A symlinked directory is not descended into even at depth > 1.
        let deep: Value =
            serde_json::from_slice(&handle_list(root.path(), &json!({ "depth": 3 })).body).unwrap();
        assert!(deep.get("children_by_path").is_none());
    }

    #[test]
    fn list_many_partial_errors_and_escape_semantics() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("ok")).unwrap();
        std::fs::write(root.path().join("ok/f.txt"), "f").unwrap();

        // Missing path lands in partial_errors and gets no `results` key (v2).
        let res: Value = serde_json::from_slice(
            &handle_list_many(root.path(), &json!({ "paths": ["ok", "missing"] })).body,
        )
        .unwrap();
        assert_eq!(res["results"]["ok"].as_array().unwrap().len(), 1);
        assert!(res["results"].get("missing").is_none());
        assert_eq!(res["partial_errors"]["missing"]["code"], 40409);

        // A path escape fails the whole call (v2 rethrows FS_PATH_ESCAPES).
        assert_eq!(
            handle_list_many(root.path(), &json!({ "paths": ["ok", "../escape"] })).status,
            400
        );
    }

    // --- F1: allow_ignored_globs (v2 #4058) ---

    /// A temp workspace whose `.gitignore` ignores `ignored.txt`, the whole
    /// `ignored-dir/` directory, and the dotfile `.secret`.
    fn ignored_fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".gitignore"),
            "ignored.txt\nignored-dir/\n.secret\n",
        )
        .unwrap();
        std::fs::write(root.path().join("ignored.txt"), "i").unwrap();
        std::fs::write(root.path().join("kept.txt"), "k").unwrap();
        std::fs::create_dir(root.path().join("ignored-dir")).unwrap();
        std::fs::write(root.path().join("ignored-dir/keep.txt"), "keep").unwrap();
        std::fs::write(root.path().join("ignored-dir/other.txt"), "other").unwrap();
        std::fs::write(root.path().join(".secret"), "s").unwrap();
        root
    }

    #[test]
    fn allow_ignored_globs_surfaces_named_ignored_file() {
        let root = ignored_fixture();
        let names = list_names(&handle_list(
            root.path(),
            &json!({ "allow_ignored_globs": ["ignored.txt"] }),
        ));
        assert_eq!(names, vec!["ignored.txt", "kept.txt"]);
    }

    #[test]
    fn allow_ignored_globs_keeps_ancestor_dirs_reachable() {
        let root = ignored_fixture();
        // The ignored directory itself matches no glob, but stays as the
        // ancestor of `ignored-dir/keep.txt`; the probe hides its sibling.
        let res: Value = serde_json::from_slice(
            &handle_list(
                root.path(),
                &json!({ "depth": 2, "allow_ignored_globs": ["ignored-dir/keep.txt"] }),
            )
            .body,
        )
        .unwrap();
        let names: Vec<&str> = res["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["ignored-dir", "kept.txt"]);
        let bucket: Vec<&str> = res["children_by_path"]["ignored-dir"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap())
            .collect();
        assert_eq!(bucket, vec!["keep.txt"]);
    }

    #[test]
    fn allow_ignored_globs_matches_across_wildcard_segments() {
        let root = ignored_fixture();
        // `ignored-*/keep.txt`: the wildcard segment must not sever the
        // ancestor check (v2 #4058 follow-up fix).
        let res: Value = serde_json::from_slice(
            &handle_list(
                root.path(),
                &json!({ "depth": 2, "allow_ignored_globs": ["ignored-*/keep.txt"] }),
            )
            .body,
        )
        .unwrap();
        let names: Vec<&str> = res["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["ignored-dir", "kept.txt"]);
    }

    #[test]
    fn allow_ignored_globs_drops_ancestors_without_matching_descendant() {
        let root = ignored_fixture();
        // `ignored-dir/` can never be an ancestor of `nope/keep.txt`, and the
        // probe finds no match inside it either.
        let names = list_names(&handle_list(
            root.path(),
            &json!({ "depth": 2, "allow_ignored_globs": ["nope/keep.txt"] }),
        ));
        assert_eq!(names, vec!["kept.txt"]);
    }

    #[test]
    fn allow_ignored_globs_drops_file_prefix_ancestors() {
        let root = ignored_fixture();
        // `ignored.txt` is a *file* prefix of the glob — v2 keeps ancestors
        // only when they are directories.
        let names = list_names(&handle_list(
            root.path(),
            &json!({ "allow_ignored_globs": ["ignored.txt/keep.txt"] }),
        ));
        assert_eq!(names, vec!["kept.txt"]);
    }

    #[test]
    fn allow_ignored_globs_dot_paths_still_require_show_hidden() {
        let root = ignored_fixture();
        // Documented upstream: allowing a dot-path needs show_hidden too.
        let names = list_names(&handle_list(
            root.path(),
            &json!({ "allow_ignored_globs": [".secret"] }),
        ));
        assert_eq!(names, vec!["kept.txt"]);
        let mut names = list_names(&handle_list(
            root.path(),
            &json!({ "allow_ignored_globs": [".secret"], "show_hidden": true }),
        ));
        names.sort();
        assert_eq!(names, vec![".gitignore", ".secret", "kept.txt"]);
    }

    #[test]
    fn allow_ignored_globs_rejects_non_array() {
        let root = ignored_fixture();
        assert_eq!(
            handle_list(root.path(), &json!({ "allow_ignored_globs": "x" })).status,
            400
        );
    }

    #[test]
    fn glob_can_match_below_prefix_semantics() {
        let globs = |g: &str| vec![g.to_string()];
        // Direct prefix chains.
        assert!(glob_can_match_below(
            "ignored-dir",
            &globs("ignored-dir/keep.txt")
        ));
        assert!(glob_can_match_below(
            "ignored-dir",
            &globs("ignored-dir/**")
        ));
        assert!(glob_can_match_below(
            "ignored-dir",
            &globs("ignored-*/keep.txt")
        ));
        assert!(glob_can_match_below("a", &globs("a/**/b")));
        // `**` may consume zero segments, so `a` is a valid ancestor of
        // `a/**/xxb` (the match could be `a/xxb`). The segment-boundary fix
        // upstream landed on *whole-path* matching, pinned below.
        assert!(glob_can_match_below("a", &globs("a/**/xxb")));
        assert!(!glob_can_match_below("axxb", &globs("a/**/b")));
        // A file-prefix is "below-able" here; the directory-only drop happens
        // at the lstat check in the walk.
        assert!(glob_can_match_below(
            "ignored.txt",
            &globs("ignored.txt/keep.txt")
        ));
        // No relation at all.
        assert!(!glob_can_match_below(
            "other",
            &globs("ignored-dir/keep.txt")
        ));
        // Empty segments (consecutive separators) match no path segment — but
        // the ancestor query still answers conservatively true when the
        // prefix itself matched (upstream's DP behaves the same: `dp[gi][r]`
        // is 1 regardless of what the remaining segments look like).
        assert!(glob_can_match_below("a", &globs("a//b")));
        assert!(!glob_can_match_below("a", &globs("b//a")));
        // A brace group embedding a slash answers conservatively.
        assert!(glob_can_match_below("x", &globs("{a/b,c}/keep.txt")));
        // Mid-segment `**` is an ordinary within-segment wildcard, not a
        // globstar: `foo**/keep.txt` opens only `foo*` directories.
        assert!(glob_can_match_below("foox", &globs("foo**/keep.txt")));
        assert!(!glob_can_match_below("foo", &globs("bar**/keep.txt")));
    }

    #[test]
    fn glob_list_whole_path_segment_boundaries() {
        // v2 #4058's glob fix on the whole-path matcher: `a/**/b` must not
        // match `a/xxb`, but matches across real segment boundaries.
        let mut list = GlobList::default();
        let glob = globset::GlobBuilder::new("a/**/b")
            .literal_separator(true)
            .build()
            .unwrap();
        list.raw.push("a/**/b".to_string());
        list.matchers.push(glob.compile_matcher());
        assert!(list.matches("a/b"));
        assert!(list.matches("a/x/b"));
        assert!(!list.matches("a/xxb"));
        assert!(!list.matches("ab"));
        // A leading `!` is a literal character (picomatch `nonegate: true`).
        let mut bang = GlobList::default();
        let glob = globset::GlobBuilder::new("!foo")
            .literal_separator(true)
            .build()
            .unwrap();
        bang.raw.push("!foo".to_string());
        bang.matchers.push(glob.compile_matcher());
        assert!(bang.matches("!foo"));
        assert!(!bang.matches("bar"));
    }

    // --- F2: allow_external_symlinks (v2 #4457) ---

    /// Workspace with `realdir/inner.txt` plus symlinks: `linkdir` → realdir,
    /// `extlink` → an external directory with `secret.txt`, `loop` → the
    /// workspace root. Returns (workspace, external) tempdirs.
    #[cfg(unix)]
    fn symlink_fixture() -> (tempfile::TempDir, tempfile::TempDir) {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("realdir")).unwrap();
        std::fs::write(root.path().join("realdir/inner.txt"), "i").unwrap();
        let external = tempfile::tempdir().unwrap();
        std::fs::write(external.path().join("secret.txt"), "s").unwrap();
        std::os::unix::fs::symlink(root.path().join("realdir"), root.path().join("linkdir"))
            .unwrap();
        std::os::unix::fs::symlink(external.path(), root.path().join("extlink")).unwrap();
        std::os::unix::fs::symlink(root.path(), root.path().join("loop")).unwrap();
        (root, external)
    }

    #[cfg(unix)]
    #[test]
    fn list_expands_internal_symlink_dirs_by_default() {
        let (root, _ext) = symlink_fixture();
        let res: Value = serde_json::from_slice(
            &handle_list(root.path(), &json!({ "depth": 2, "sort": "name_asc" })).body,
        )
        .unwrap();
        let linkdir = res["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["name"] == "linkdir")
            .unwrap();
        // kind stays `symlink`; `is_directory` carries the resolved shape.
        assert_eq!(linkdir["kind"], "symlink");
        assert_eq!(linkdir["is_directory"], true);
        let bucket = res["children_by_path"]["linkdir"].as_array().unwrap();
        assert_eq!(bucket[0]["path"], "linkdir/inner.txt");
    }

    #[cfg(unix)]
    #[test]
    fn list_external_symlink_policy() {
        let (root, _ext) = symlink_fixture();
        // Default: the external link is demoted to a plain symlink entry and
        // never expanded.
        let res: Value =
            serde_json::from_slice(&handle_list(root.path(), &json!({ "depth": 2 })).body).unwrap();
        let extlink = res["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["name"] == "extlink")
            .unwrap();
        assert_eq!(extlink["kind"], "symlink");
        assert!(extlink.get("is_directory").is_none());
        assert!(res["children_by_path"].get("extlink").is_none());

        // With the flag the same link expands and is marked.
        let res: Value = serde_json::from_slice(
            &handle_list(
                root.path(),
                &json!({ "depth": 2, "allow_external_symlinks": true }),
            )
            .body,
        )
        .unwrap();
        let extlink = res["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["name"] == "extlink")
            .unwrap();
        assert_eq!(extlink["is_directory"], true);
        let bucket = res["children_by_path"]["extlink"].as_array().unwrap();
        assert_eq!(bucket[0]["path"], "extlink/secret.txt");
    }

    #[cfg(unix)]
    #[test]
    fn list_external_symlink_root_path_requires_the_flag() {
        let (root, _ext) = symlink_fixture();
        // Listing through an external link escapes the workspace: rejected
        // without the flag (the canonicalize prefix check), allowed with it.
        assert_eq!(
            handle_list(root.path(), &json!({ "path": "extlink" })).status,
            400
        );
        let res = handle_list(
            root.path(),
            &json!({ "path": "extlink", "allow_external_symlinks": true }),
        );
        assert_eq!(res.status, 200);
        let json: Value = serde_json::from_slice(&res.body).unwrap();
        assert_eq!(json["items"].as_array().unwrap()[0]["name"], "secret.txt");
    }

    #[cfg(unix)]
    #[test]
    fn list_symlink_loop_terminates_via_ancestor_guard() {
        let (root, _ext) = symlink_fixture();
        // `loop` points back at the root; without the ancestor real-path
        // guard this walk would never finish.
        let res = handle_list(
            root.path(),
            &json!({ "depth": 10, "allow_external_symlinks": true }),
        );
        assert_eq!(res.status, 200);
        let json: Value = serde_json::from_slice(&res.body).unwrap();
        // `loop` points back at the root; its real path is already an
        // ancestor, so it is listed as an entry but never queued — no
        // `children_by_path["loop"]` bucket exists and the walk terminates.
        assert!(json["children_by_path"].get("loop").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn read_through_external_symlink_requires_the_flag() {
        let (root, _ext) = symlink_fixture();
        assert_eq!(
            handle_read(root.path(), &json!({ "path": "extlink/secret.txt" })).status,
            400
        );
        let res = handle_read(
            root.path(),
            &json!({ "path": "extlink/secret.txt", "allow_external_symlinks": true }),
        );
        assert_eq!(res.status, 200);
        let json: Value = serde_json::from_slice(&res.body).unwrap();
        assert_eq!(json["content"], "s");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_with_symlinks_lexical_escape_still_rejected() {
        let root = tempfile::tempdir().unwrap();
        // `..` segments are rejected regardless of the flag.
        assert!(
            crate::server::fs_routes::resolve_safe_path_with_symlinks(
                root.path(),
                "../outside.txt",
                true,
            )
            .is_err()
        );
    }

    // --- F3: folder-scoped git status (v2 #4527) ---

    /// A real git repo: committed `pkg/committed.txt`, then dirty changes in
    /// `pkg/` (modified + untracked) and at the root (untracked).
    fn git_repo_fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(root.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {:?} failed", args);
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "test"]);
        std::fs::create_dir(root.path().join("pkg")).unwrap();
        std::fs::write(root.path().join("pkg/committed.txt"), "v1\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "init"]);
        // Dirty the tree: one tracked modification and untracked files.
        std::fs::write(root.path().join("pkg/committed.txt"), "v1\nv2\n").unwrap();
        std::fs::write(root.path().join("pkg/new.txt"), "new\n").unwrap();
        std::fs::write(root.path().join("root-new.txt"), "root\n").unwrap();
        root
    }

    #[test]
    fn git_status_scopes_entries_and_numstat_to_root() {
        let root = git_repo_fixture();
        let res = handle_git_status(
            root.path(),
            &json!({ "root": "pkg", "include_pull_request": false }),
        );
        assert_eq!(res.status, 200);
        let json: Value = serde_json::from_slice(&res.body).unwrap();
        // Entry keys are relative to the scoped root (v2 prefix stripping).
        assert_eq!(json["entries"]["committed.txt"], "modified");
        assert_eq!(json["entries"]["new.txt"], "untracked");
        assert!(
            json["entries"].get("root-new.txt").is_none(),
            "the root-level change stays out of the scoped query: {:?}",
            json["entries"]
        );
        // numstat scoped to `.`: only the pkg modification counts.
        assert_eq!(json["additions"], 1);
        assert_eq!(json["deletions"], 0);
        assert_eq!(json["branch"], "main");
    }

    #[test]
    fn git_status_paths_filter_exact_matches() {
        let root = git_repo_fixture();
        let res = handle_git_status(
            root.path(),
            &json!({ "paths": ["pkg/committed.txt"], "include_pull_request": false }),
        );
        let json: Value = serde_json::from_slice(&res.body).unwrap();
        let entries = json["entries"].as_object().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries["pkg/committed.txt"], "modified");

        // Paths resolve against the scoped root when `root` is given.
        let res = handle_git_status(
            root.path(),
            &json!({ "root": "pkg", "paths": ["new.txt"], "include_pull_request": false }),
        );
        let json: Value = serde_json::from_slice(&res.body).unwrap();
        let entries = json["entries"].as_object().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries["new.txt"], "untracked");
    }

    #[test]
    fn git_status_rejects_escaping_paths_and_bad_roots() {
        let root = git_repo_fixture();
        assert_eq!(
            handle_git_status(
                root.path(),
                &json!({ "root": "pkg", "paths": ["../root-new.txt"] })
            )
            .status,
            400
        );
        assert_eq!(
            handle_git_status(root.path(), &json!({ "root": "pkg/committed.txt" })).status,
            400,
            "a file as root is rejected (v2 FS_GIT_UNAVAILABLE-class)"
        );
        assert_eq!(
            handle_git_status(root.path(), &json!({ "root": "../" })).status,
            400
        );
    }

    #[test]
    fn git_status_batch_collects_per_root_results_and_errors() {
        let root = git_repo_fixture();
        let res = handle_git_status_batch(
            root.path(),
            &json!({ "roots": ["pkg", "missing-dir", "pkg"], "include_pull_request": false }),
        );
        assert_eq!(res.status, 200);
        let json: Value = serde_json::from_slice(&res.body).unwrap();
        // v2 dedupes roots; `pkg` appears once.
        assert_eq!(json["results"].as_object().unwrap().len(), 1);
        assert_eq!(
            json["results"]["pkg"]["entries"]["committed.txt"],
            "modified"
        );
        assert_eq!(json["errors"]["missing-dir"]["code"], 40001);

        assert_eq!(
            handle_git_status_batch(root.path(), &json!({ "roots": [] })).status,
            400
        );
    }

    #[test]
    fn diff_scopes_to_root() {
        let root = git_repo_fixture();
        let res = handle_diff(
            root.path(),
            &json!({ "root": "pkg", "path": "committed.txt" }),
        );
        assert_eq!(res.status, 200);
        let json: Value = serde_json::from_slice(&res.body).unwrap();
        assert!(json["diff"].as_str().unwrap().contains("+v2"));

        // An escaping path is rejected under a scoped root.
        assert_eq!(
            handle_diff(
                root.path(),
                &json!({ "root": "pkg", "path": "../root-new.txt" })
            )
            .status,
            400
        );
    }

    #[test]
    fn match_positions_frames_the_hit_in_the_path() {
        // Offsets index the whole path, in UTF-16 code units — the frame the
        // VS Code mention picker highlights in.
        assert_eq!(match_positions("src/app.ts", "app"), vec![4, 5, 6]);
        assert_eq!(match_positions("src/app.ts", "APP"), vec![4, 5, 6]);
        assert_eq!(match_positions("src/app.ts", "zzz"), Vec::<usize>::new());
        assert_eq!(match_positions("src/app.ts", ""), Vec::<usize>::new());
        // A non-BMP char before the hit shifts the offsets by two units.
        assert_eq!(match_positions("a😀b.ts", "b"), vec![3]);
    }

    #[test]
    fn search_files_reports_items_and_truncation() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path();
        std::fs::write(root.join("alpha.txt"), "a").unwrap();
        std::fs::write(root.join("beta.txt"), "b").unwrap();

        // No git repo here, so the walk fallback answers.
        let (items, truncated) = search_files(root, "alpha", 50);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["path"], "alpha.txt");
        assert_eq!(items[0]["match_positions"], json!([0, 1, 2, 3, 4]));
        assert!(!truncated);

        // The empty query lists the directory and reports the cap as truncation.
        let (items, truncated) = search_files(root, "", 1);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["match_positions"], json!([]));
        assert!(truncated);
    }

    #[test]
    fn test_fs_handlers_crud() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path();

        // 1. mkdir
        let mkdir_res = handle_mkdir(root, &json!({ "path": "src/subdir", "recursive": true }));
        assert_eq!(mkdir_res.status, 201);

        // 2. write a file
        let file_path = root.join("src/subdir/test.txt");
        std::fs::write(&file_path, "Hello Kimi!").unwrap();

        // 3. stat
        let stat_res = handle_stat(root, &json!({ "path": "src/subdir/test.txt" }));
        assert_eq!(stat_res.status, 200);
        let stat_json: Value = serde_json::from_slice(&stat_res.body).unwrap();
        assert_eq!(stat_json["name"], "test.txt");
        assert_eq!(stat_json["size"], 11);

        // 4. stat_many
        let stat_many_res = handle_stat_many(
            root,
            &json!({ "paths": ["src/subdir/test.txt", "nonexistent.txt"] }),
        );
        assert_eq!(stat_many_res.status, 200);
        let stat_many_json: Value = serde_json::from_slice(&stat_many_res.body).unwrap();
        assert!(stat_many_json["entries"]["src/subdir/test.txt"].is_object());
        assert!(stat_many_json["entries"]["nonexistent.txt"].is_null());

        // 5. read
        let read_res = handle_read(root, &json!({ "path": "src/subdir/test.txt" }));
        assert_eq!(read_res.status, 200);
        let read_json: Value = serde_json::from_slice(&read_res.body).unwrap();
        assert_eq!(read_json["content"], "Hello Kimi!");
        assert_eq!(read_json["size"], 11);
        assert_eq!(read_json["truncated"], false);

        // 6. list & list_many
        let list_res = handle_list(root, &json!({ "path": "src/subdir" }));
        assert_eq!(list_res.status, 200);
        let list_json: Value = serde_json::from_slice(&list_res.body).unwrap();
        assert_eq!(list_json["items"].as_array().unwrap().len(), 1);

        let list_many_res = handle_list_many(root, &json!({ "paths": ["src/subdir"] }));
        assert_eq!(list_many_res.status, 200);
        let list_many_json: Value = serde_json::from_slice(&list_many_res.body).unwrap();
        assert_eq!(
            list_many_json["results"]["src/subdir"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        // 7. search
        let search_res = handle_search(root, &json!({ "query": "test" }));
        assert_eq!(search_res.status, 200);
        let search_json: Value = serde_json::from_slice(&search_res.body).unwrap();
        let items = search_json["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["name"], "test.txt");
        assert_eq!(items[0]["path"], "src/subdir/test.txt");
        assert_eq!(items[0]["kind"], "file");
        assert_eq!(items[0]["size"], 11);
        assert_eq!(search_json["truncated"], false);

        // 8. diff
        let diff_res = handle_diff(root, &json!({ "path": "src/subdir/test.txt" }));
        assert_eq!(diff_res.status, 200);
        let diff_json: Value = serde_json::from_slice(&diff_res.body).unwrap();
        assert!(diff_json["diff"].as_str().unwrap().contains("+Hello Kimi!"));

        // 9. git_status returns standard structure
        let git_res = handle_git_status(root, &json!({ "include_pull_request": false }));
        assert_eq!(git_res.status, 200);
        let git_json: Value = serde_json::from_slice(&git_res.body).unwrap();
        assert!(git_json.get("branch").is_some());
        assert_eq!(git_json["ahead"], 0);
        assert_eq!(git_json["behind"], 0);
        assert!(git_json["entries"].is_object());

        // 10. open & reveal
        let open_res = handle_open(root, &json!({ "path": "src/subdir/test.txt" }));
        assert_eq!(open_res.status, 200);
        let open_val: Value = serde_json::from_slice(&open_res.body).unwrap();
        assert_eq!(open_val["opened"], true);

        let reveal_res = handle_reveal(root, &json!({ "path": "src/subdir/test.txt" }));
        assert_eq!(reveal_res.status, 200);
        let reveal_val: Value = serde_json::from_slice(&reveal_res.body).unwrap();
        assert_eq!(reveal_val["revealed"], true);

        // Required path validation on open and reveal
        assert_eq!(handle_open(root, &json!({})).status, 400);
        assert_eq!(handle_reveal(root, &json!({})).status, 400);

        // 11. grep handler
        assert_eq!(handle_grep(root, &json!({})).status, 400);
        assert_eq!(handle_grep(root, &json!({ "pattern": "" })).status, 400);
        let grep_res = handle_grep(root, &json!({ "pattern": "Hello" }));
        assert_eq!(grep_res.status, 200);
        let grep_json: Value = serde_json::from_slice(&grep_res.body).unwrap();
        assert!(grep_json["files"].is_array());
        assert_eq!(grep_json["truncated"], false);

        // 12. read error cases
        assert_eq!(handle_read(root, &json!({})).status, 400);
        assert_eq!(
            handle_read(root, &json!({ "path": "../outside.txt" })).status,
            400
        );
        assert_eq!(
            handle_read(root, &json!({ "path": "non-existent.txt" })).status,
            404
        );
    }
}
