use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures_util::StreamExt;
use kimi_core::primitives::errors::domains::fs::{FS_GIT_UNAVAILABLE, FS_PATH_NOT_FOUND};
use kimi_core::primitives::errors::{Error2, Error2Options};
use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::app::environment::environment::EnvironmentResolver;
use crate::app::scopes::LifecycleScope;
use crate::environment::environment::LOCAL_ENVIRONMENT_ID;
use crate::environment::program_path::resolve_program_path;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_process::HostOutputStream;
use crate::wire::record::WireAsyncError;

use super::git::{
    FsDiffResponse, FsGitStatusResponse, FsPullRequest, GitStatusOptions, IGitService,
};
use super::git_parsers::{parse_numstat, parse_porcelain, parse_pull_request};
use super::work_tree::{GitWorkTree, find_git_work_tree};

const DIFF_MAX_BYTES: usize = 1_048_576;
const GIT_SPAWN_TIMEOUT_MS: u64 = 30_000;
const PR_SPAWN_TIMEOUT_MS: u64 = 5_000;
const PULL_REQUEST_TTL_MS: i64 = 60_000;

#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";
#[cfg(not(windows))]
const NULL_DEVICE: &str = "/dev/null";

#[derive(Clone)]
struct PullRequestCacheEntry {
    branch: String,
    value: Option<FsPullRequest>,
    fetched_at: i64,
}

pub struct GitService {
    resolver: Arc<dyn EnvironmentResolver>,
    fs: Arc<dyn IHostFileSystem>,
    environment_id: String,
    pull_request_cache: Mutex<HashMap<String, PullRequestCacheEntry>>,
}

impl GitService {
    pub fn new(
        resolver: Arc<dyn EnvironmentResolver>,
        fs: Arc<dyn IHostFileSystem>,
        environment_id: impl Into<String>,
    ) -> Self {
        Self {
            resolver,
            fs,
            environment_id: environment_id.into(),
            pull_request_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn new_local(resolver: Arc<dyn EnvironmentResolver>, fs: Arc<dyn IHostFileSystem>) -> Self {
        Self::new(resolver, fs, LOCAL_ENVIRONMENT_ID)
    }

    pub async fn status(
        &self,
        cwd: &str,
        options: Option<GitStatusOptions>,
    ) -> Result<FsGitStatusResponse, WireAsyncError> {
        let options = options.unwrap_or_default();
        let inside = self
            .run_command(
                "git",
                &string_args(&["rev-parse", "--is-inside-work-tree"]),
                cwd,
                RunOptions::default(),
            )
            .await?;
        if inside.exit_code != 0 || inside.stdout.trim() != "true" {
            return Err(self.git_unavailable(
                cwd,
                command_detail(&inside.stderr, "git rev-parse", inside.exit_code),
            ));
        }

        let mut path_prefix = String::new();
        if options.scope_to_cwd {
            let prefix = self
                .run_command(
                    "git",
                    &string_args(&["rev-parse", "--show-prefix"]),
                    cwd,
                    RunOptions::default(),
                )
                .await?;
            if prefix.exit_code != 0 {
                return Err(self.git_unavailable(
                    cwd,
                    command_detail(&prefix.stderr, "git rev-parse", prefix.exit_code),
                ));
            }
            path_prefix = strip_one_line_ending(&prefix.stdout).to_string();
        }
        let mut status_args = string_args(&["status", "--porcelain=v1", "--branch", "-z"]);
        if options.scope_to_cwd {
            status_args.extend(string_args(&["--", "."]));
        }
        let porcelain = self
            .run_command("git", &status_args, cwd, RunOptions::default())
            .await?;
        if porcelain.exit_code != 0 {
            return Err(self.git_unavailable(
                cwd,
                command_detail(&porcelain.stderr, "git status", porcelain.exit_code),
            ));
        }
        let mut result = parse_porcelain(
            &porcelain.stdout,
            options.path_filter.as_ref(),
            &path_prefix,
        );
        let dirty = porcelain
            .stdout
            .split('\0')
            .any(|record| !record.is_empty() && !record.starts_with("## "));
        if dirty {
            let head = self
                .run_command(
                    "git",
                    &string_args(&["rev-parse", "--verify", "--quiet", "HEAD"]),
                    cwd,
                    RunOptions::default(),
                )
                .await?;
            if head.exit_code == 0 {
                let mut diff_args = string_args(&["diff", "--no-color", "--numstat", "HEAD", "--"]);
                let mut paths = options
                    .path_filter
                    .as_ref()
                    .map(|paths| paths.iter().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                paths.sort();
                if !paths.is_empty() {
                    diff_args.extend(paths);
                } else if options.scope_to_cwd {
                    diff_args.push(".".to_string());
                }
                let numstat = self
                    .run_command("git", &diff_args, cwd, RunOptions::default())
                    .await?;
                if numstat.exit_code == 0 {
                    let (additions, deletions) = parse_numstat(&numstat.stdout);
                    result.additions = additions;
                    result.deletions = deletions;
                }
            }
        }
        result.pull_request = if options.include_pull_request {
            self.read_pull_request(cwd, &result.branch).await?
        } else {
            None
        };
        Ok(result)
    }

    pub async fn pull_request(
        &self,
        cwd: &str,
    ) -> Result<Option<FsPullRequest>, WireAsyncError> {
        let inside = self
            .run_command(
                "git",
                &string_args(&["rev-parse", "--is-inside-work-tree"]),
                cwd,
                RunOptions::default(),
            )
            .await?;
        if inside.exit_code != 0 || inside.stdout.trim() != "true" {
            return Err(self.git_unavailable(
                cwd,
                command_detail(&inside.stderr, "git rev-parse", inside.exit_code),
            ));
        }
        let head = self
            .run_command(
                "git",
                &string_args(&["symbolic-ref", "--short", "-q", "HEAD"]),
                cwd,
                RunOptions::default(),
            )
            .await?;
        if head.exit_code != 0 && head.exit_code != 1 {
            return Err(self.git_unavailable(
                cwd,
                command_detail(&head.stderr, "git symbolic-ref", head.exit_code),
            ));
        }
        self.read_pull_request(cwd, head.stdout.trim()).await
    }

    pub async fn diff(
        &self,
        cwd: &str,
        rel_path: &str,
        abs_path: &str,
    ) -> Result<FsDiffResponse, WireAsyncError> {
        let inside = self
            .run_command(
                "git",
                &string_args(&["rev-parse", "--is-inside-work-tree"]),
                cwd,
                RunOptions::default(),
            )
            .await?;
        if inside.exit_code != 0 || inside.stdout.trim() != "true" {
            return Err(self.git_unavailable(
                cwd,
                command_detail(&inside.stderr, "git rev-parse", inside.exit_code),
            ));
        }
        let status = self
            .run_command(
                "git",
                &[
                    string_args(&["status", "--porcelain=v1", "--"]),
                    vec![rel_path.to_string()],
                ]
                .concat(),
                cwd,
                RunOptions::default(),
            )
            .await?;
        if status.exit_code != 0 {
            return Err(self.git_unavailable(
                cwd,
                command_detail(&status.stderr, "git status", status.exit_code),
            ));
        }
        let untracked = status.stdout.starts_with("??");
        let head = self
            .run_command(
                "git",
                &string_args(&["rev-parse", "--verify", "--quiet", "HEAD"]),
                cwd,
                RunOptions::default(),
            )
            .await?;
        let has_head = head.exit_code == 0;
        let diff_stdout = if untracked || !has_head {
            let result = self
                .run_command(
                    "git",
                    &[
                        string_args(&["diff", "--no-color", "--no-index", "--", NULL_DEVICE]),
                        vec![rel_path.to_string()],
                    ]
                    .concat(),
                    cwd,
                    RunOptions::default(),
                )
                .await?;
            if result.exit_code != 0 && result.exit_code != 1 {
                return Err(self.git_unavailable(
                    cwd,
                    command_detail(&result.stderr, "git diff", result.exit_code),
                ));
            }
            result.stdout
        } else {
            let result = self
                .run_command(
                    "git",
                    &[
                        string_args(&["diff", "--no-color", "HEAD", "--"]),
                        vec![rel_path.to_string()],
                    ]
                    .concat(),
                    cwd,
                    RunOptions::default(),
                )
                .await?;
            if result.exit_code != 0 {
                return Err(self.git_unavailable(
                    cwd,
                    command_detail(&result.stderr, "git diff", result.exit_code),
                ));
            }
            if result.stdout.is_empty() && status.stdout.is_empty() && self.fs.lstat(abs_path).await.is_err() {
                let mut details = Map::new();
                details.insert("path".to_string(), Value::String(rel_path.to_string()));
                return Err(Box::new(Error2::with_options(
                    FS_PATH_NOT_FOUND,
                    format!("path not found: {rel_path}"),
                    Error2Options {
                        details: Some(details),
                        ..Default::default()
                    },
                )));
            }
            result.stdout
        };
        let truncated = diff_stdout.encode_utf16().count() > DIFF_MAX_BYTES;
        Ok(FsDiffResponse {
            path: rel_path.to_string(),
            diff: if truncated {
                truncate_utf16(&diff_stdout, DIFF_MAX_BYTES)
            } else {
                diff_stdout
            },
            truncated,
        })
    }

    pub async fn find_work_tree(&self, cwd: &str) -> Result<Option<GitWorkTree>, WireAsyncError> {
        find_git_work_tree(self.fs.as_ref(), cwd).await
    }

    async fn read_pull_request(
        &self,
        cwd: &str,
        branch: &str,
    ) -> Result<Option<FsPullRequest>, WireAsyncError> {
        if branch.is_empty() {
            return Ok(None);
        }
        let now = Utc::now().timestamp_millis();
        if let Some(cached) = self.pull_request_cache.lock().get(cwd).cloned() {
            if cached.branch == branch && now - cached.fetched_at < PULL_REQUEST_TTL_MS {
                return Ok(cached.value);
            }
        }
        let env = HashMap::from([
            ("GH_NO_UPDATE_NOTIFIER".to_string(), "1".to_string()),
            ("GH_PROMPT_DISABLED".to_string(), "1".to_string()),
        ]);
        let result = self
            .run_command(
                "gh",
                &string_args(&["pr", "view", "--json", "number,url,state,isDraft"]),
                cwd,
                RunOptions {
                    timeout_ms: Some(PR_SPAWN_TIMEOUT_MS),
                    env: Some(env),
                },
            )
            .await?;
        let value = if result.exit_code == 0 {
            parse_pull_request(&result.stdout)
        } else {
            None
        };
        self.pull_request_cache.lock().insert(
            cwd.to_string(),
            PullRequestCacheEntry {
                branch: branch.to_string(),
                value: value.clone(),
                fetched_at: now,
            },
        );
        Ok(value)
    }

    async fn run_command(
        &self,
        command: &str,
        args: &[String],
        cwd: &str,
        options: RunOptions,
    ) -> Result<RunResult, WireAsyncError> {
        let program = if self.environment_id == LOCAL_ENVIRONMENT_ID {
            local_program_path(command, cwd)
        } else {
            Some(command.to_string())
        };
        let Some(program) = program else {
            return Ok(RunResult::failed());
        };
        let lease = self.resolver.acquire(&self.environment_id, &["process"]);
        let process = lease
            .environment
            .process
            .clone()
            .expect("process capability is required");
        let proc = match process
            .spawn(&program, args, Some(cwd), options.env.as_ref())
            .await
        {
            Ok(proc) => proc,
            Err(_) => {
                drop(lease);
                return Ok(RunResult::failed());
            }
        };
        let stdout = proc.stdout();
        let stderr = proc.stderr();
        let waiting = proc.clone();
        let mut work = tokio::spawn(async move {
            let (stdout, stderr, exit_code) = tokio::join!(
                collect(stdout),
                collect(stderr),
                async move { waiting.wait().await.unwrap_or(-1) }
            );
            let stdout = stdout?;
            let stderr = stderr?;
            Ok::<_, WireAsyncError>((stdout, stderr, exit_code))
        });
        let timeout_ms = options.timeout_ms.unwrap_or(GIT_SPAWN_TIMEOUT_MS);
        let outcome = tokio::select! {
            joined = &mut work => {
                let result = match joined {
                    Ok(Ok((stdout, stderr, exit_code))) => Ok(RunResult { exit_code, stdout, stderr }),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(Box::new(error) as WireAsyncError),
                };
                CommandOutcome::Done(result)
            }
            _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => CommandOutcome::Timeout,
        };
        let result = match outcome {
            CommandOutcome::Done(result) => result,
            CommandOutcome::Timeout => {
                let _ = proc.kill("SIGKILL").await;
                let (stdout, stderr) = match work.await {
                    Ok(Ok((stdout, stderr, _))) => (stdout, stderr),
                    _ => (String::new(), String::new()),
                };
                Ok(RunResult {
                    exit_code: -1,
                    stdout,
                    stderr,
                })
            }
        };
        proc.dispose().await;
        drop(lease);
        result
    }

    fn git_unavailable(&self, cwd: &str, detail: String) -> WireAsyncError {
        let mut details = Map::new();
        details.insert("cwd".to_string(), Value::String(cwd.to_string()));
        details.insert("detail".to_string(), Value::String(detail.clone()));
        Box::new(Error2::with_options(
            FS_GIT_UNAVAILABLE,
            format!("git unavailable at {cwd}: {detail}"),
            Error2Options {
                details: Some(details),
                ..Default::default()
            },
        ))
    }
}

#[async_trait]
impl IGitService for GitService {
    async fn status(
        &self,
        cwd: &str,
        options: Option<GitStatusOptions>,
    ) -> Result<FsGitStatusResponse, WireAsyncError> {
        GitService::status(self, cwd, options).await
    }

    async fn pull_request(&self, cwd: &str) -> Result<Option<FsPullRequest>, WireAsyncError> {
        GitService::pull_request(self, cwd).await
    }

    async fn diff(
        &self,
        cwd: &str,
        rel_path: &str,
        abs_path: &str,
    ) -> Result<FsDiffResponse, WireAsyncError> {
        GitService::diff(self, cwd, rel_path, abs_path).await
    }

    async fn find_work_tree(&self, cwd: &str) -> Result<Option<GitWorkTree>, WireAsyncError> {
        GitService::find_work_tree(self, cwd).await
    }
}

struct RunResult {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

impl RunResult {
    fn failed() -> Self {
        Self {
            exit_code: -1,
            stdout: String::new(),
            stderr: String::new(),
        }
    }
}

#[derive(Default)]
struct RunOptions {
    timeout_ms: Option<u64>,
    env: Option<HashMap<String, String>>,
}

enum CommandOutcome {
    Done(Result<RunResult, WireAsyncError>),
    Timeout,
}

fn string_args(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_string()).collect()
}

fn command_detail(stderr: &str, operation: &str, exit_code: i32) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        format!("{operation} exit {exit_code}")
    } else {
        trimmed.to_string()
    }
}

fn strip_one_line_ending(value: &str) -> &str {
    value
        .strip_suffix("\r\n")
        .or_else(|| value.strip_suffix('\n'))
        .unwrap_or(value)
}

fn local_program_path(command: &str, cwd: &str) -> Option<String> {
    let resolved = std::panic::catch_unwind(|| resolve_program_path(command, None)).ok()?;
    if is_inside_cwd(Path::new(&resolved), Path::new(cwd)) {
        None
    } else {
        Some(resolved)
    }
}

fn is_inside_cwd(path: &Path, cwd: &Path) -> bool {
    let cwd = normalize_absolute(cwd);
    let path = normalize_absolute(path);
    if cfg!(windows) {
        let cwd = cwd.to_string_lossy().to_lowercase();
        let path = path.to_string_lossy().to_lowercase();
        path == cwd || path.starts_with(&format!("{cwd}\\"))
    } else {
        path == cwd || path.starts_with(&cwd)
    }
}

fn normalize_absolute(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(path)
    };
    let mut output = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => output.push(prefix.as_os_str()),
            Component::RootDir => output.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    output.push(component.as_os_str());
                }
            }
            Component::Normal(part) => output.push(part),
        }
    }
    output
}

fn truncate_utf16(value: &str, limit: usize) -> String {
    let mut units = 0;
    let mut end = value.len();
    for (index, character) in value.char_indices() {
        let width = character.len_utf16();
        if units + width > limit {
            end = index;
            break;
        }
        units += width;
    }
    value[..end].to_string()
}

async fn collect(mut stream: HostOutputStream) -> Result<String, WireAsyncError> {
    let mut output = String::new();
    while let Some(chunk) = stream.next().await {
        output.push_str(&chunk?);
    }
    Ok(output)
}

pub fn register_git_service() {
    register_scoped_service(
        LifecycleScope::App,
        ScopeActivation::OnScopeCreated,
        "git",
    );
}
