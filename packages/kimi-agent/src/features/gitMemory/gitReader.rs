use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::Notify;

use crate::_base::errors::unexpected_error::on_unexpected_error;
use crate::os::interface::host_process::{IHostProcessService, IHostProcess};
use crate::os::interface::host_timers::IHostTimers;
use crate::wire::record::WireAsyncError;

use super::types::CommitRecord;

const GIT_TIMEOUT_MS: u64 = 5_000;
const LOG_FORMAT: &str = "%H%x09%ad%x09%an%x09%s%x00%b%x00";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoHead {
    pub repo_root: String,
    pub prefix: String,
    pub head_sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitReadResult {
    Ok {
        head_sha: String,
        commits: Vec<CommitRecord>,
    },
    Unavailable,
    Failed {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitFailureKind {
    SpawnFailed,
    Timeout,
    Exit,
}

#[derive(Debug)]
struct CommandOutcome {
    ok: bool,
    stdout: String,
    stderr: String,
    failure: Option<GitFailureKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitUnavailableReason {
    NotARepo,
    SpawnFailed,
    Timeout,
    Permission,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoHeadResult {
    Ok(RepoHead),
    Unavailable {
        reason: GitUnavailableReason,
        detail: String,
    },
}

async fn collect(mut stream: crate::os::interface::host_process::HostOutputStream) -> Result<String, WireAsyncError> {
    let mut output = String::new();
    while let Some(chunk) = stream.next().await {
        output.push_str(&chunk?);
    }
    Ok(output)
}

fn command_error(error: impl ToString) -> String {
    error.to_string()
}

async fn run_git(
    process: &Arc<dyn IHostProcessService>,
    cwd: &str,
    args: &[String],
    timers: Option<Arc<dyn IHostTimers>>,
) -> CommandOutcome {
    let child = match process.spawn("git", args, Some(cwd), None).await {
        Ok(child) => child,
        Err(error) => {
            return CommandOutcome {
                ok: false,
                stdout: String::new(),
                stderr: command_error(error),
                failure: Some(GitFailureKind::SpawnFailed),
            };
        }
    };
    if let Err(error) = child.close_stdin().await {
        on_unexpected_error(error);
    }

    let outcome = run_with_timeout(child.clone(), timers).await;
    child.dispose().await;
    outcome
}

async fn run_with_timeout(
    child: Arc<dyn IHostProcess>,
    timers: Option<Arc<dyn IHostTimers>>,
) -> CommandOutcome {
    let work_child = child.clone();
    let mut work = tokio::spawn(async move {
        let (stdout, stderr, exit_code) = tokio::join!(
            collect(work_child.stdout()),
            collect(work_child.stderr()),
            work_child.wait(),
        );
        Ok::<_, WireAsyncError>((stdout?, stderr?, exit_code?))
    });

    let timer_notify = Arc::new(Notify::new());
    let timer_handle = timers.as_ref().map(|timers| {
        let notify = timer_notify.clone();
        timers.set_timeout(
            Arc::new(move || notify.notify_one()),
            GIT_TIMEOUT_MS,
        )
    });
    let notified = timer_notify.notified();
    let mut timed_out = false;
    let selected = if timer_handle.is_some() {
        tokio::select! {
            result = &mut work => Some(result),
            _ = notified => {
                timed_out = true;
                None
            }
        }
    } else {
        tokio::select! {
            result = &mut work => Some(result),
            _ = tokio::time::sleep(Duration::from_millis(GIT_TIMEOUT_MS)) => {
                timed_out = true;
                None
            }
        }
    };
    if let (Some(timers), Some(handle)) = (timers, timer_handle) {
        timers.clear_timeout(handle);
    }

    match selected {
        Some(Ok(Ok((stdout, stderr, exit_code)))) => {
            if exit_code == 0 {
                CommandOutcome {
                    ok: true,
                    stdout,
                    stderr,
                    failure: None,
                }
            } else {
                CommandOutcome {
                    ok: false,
                    stdout,
                    stderr,
                    failure: Some(GitFailureKind::Exit),
                }
            }
        }
        Some(Ok(Err(error))) => {
            let detail = command_error(error);
            if let Err(error) = child.kill("SIGKILL").await {
                on_unexpected_error(error);
            }
            CommandOutcome {
                ok: false,
                stdout: String::new(),
                stderr: detail,
                failure: Some(GitFailureKind::Exit),
            }
        }
        Some(Err(error)) => {
            let detail = command_error(error);
            if let Err(error) = child.kill("SIGKILL").await {
                on_unexpected_error(error);
            }
            CommandOutcome {
                ok: false,
                stdout: String::new(),
                stderr: detail,
                failure: Some(GitFailureKind::Exit),
            }
        }
        None => {
            if let Err(error) = child.kill("SIGKILL").await {
                on_unexpected_error(error);
            }
            let _ = work.await;
            CommandOutcome {
                ok: false,
                stdout: String::new(),
                stderr: "git command timed out".to_string(),
                failure: Some(if timed_out {
                    GitFailureKind::Timeout
                } else {
                    GitFailureKind::Exit
                }),
            }
        }
    }
}

#[derive(Debug, Clone)]
struct CommitMeta {
    date: String,
    author: String,
    subject: String,
    body: String,
}

fn parse_meta(stdout: &str) -> HashMap<String, CommitMeta> {
    let mut meta = HashMap::new();
    let chunks = stdout.split('\0').collect::<Vec<_>>();
    for index in (0..chunks.len()).step_by(2) {
        let header = chunks[index];
        let parts = trim_js(header).split('\t').collect::<Vec<_>>();
        let sha = parts.first().map(|part| trim_js(part)).unwrap_or_default();
        if sha.is_empty() {
            continue;
        }
        meta.insert(
            sha.to_string(),
            CommitMeta {
                date: parts.get(1).map(|part| trim_js(part).to_string()).unwrap_or_default(),
                author: parts.get(2).map(|part| trim_js(part).to_string()).unwrap_or_default(),
                subject: trim_js(&parts.get(3..).unwrap_or_default().join("\t")).to_string(),
                body: chunks
                    .get(index + 1)
                    .map(|body| trim_js(body).to_string())
                    .unwrap_or_default(),
            },
        );
    }
    meta
}

fn is_sha(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn parse_commits(stdout: &str, meta: &HashMap<String, CommitMeta>) -> Vec<CommitRecord> {
    let mut commits = Vec::new();
    let mut sha: Option<String> = None;
    let mut files = Vec::new();
    let mut flush = |sha: &mut Option<String>, files: &mut Vec<String>| {
        let Some(current) = sha.take() else {
            return;
        };
        if !files.is_empty() {
            let info = meta.get(&current);
            commits.push(CommitRecord {
                sha: current,
                date: info.map(|info| info.date.clone()).unwrap_or_default(),
                author: info.map(|info| info.author.clone()).unwrap_or_default(),
                subject: info.map(|info| info.subject.clone()).unwrap_or_default(),
                body: info.map(|info| info.body.clone()).unwrap_or_default(),
                files: std::mem::take(files),
            });
        }
        files.clear();
    };
    for line in stdout.split('\n') {
        let text = trim_js(line);
        if text.is_empty() {
            continue;
        }
        if is_sha(text) {
            flush(&mut sha, &mut files);
            sha = Some(text.to_string());
            continue;
        }
        if sha.is_some() {
            files.push(text.to_string());
        }
    }
    flush(&mut sha, &mut files);
    commits
}

pub async fn read_repo_head(
    process: &Arc<dyn IHostProcessService>,
    cwd: &str,
    timers: Option<Arc<dyn IHostTimers>>,
) -> RepoHeadResult {
    let args = vec![
        "-C".to_string(),
        cwd.to_string(),
        "rev-parse".to_string(),
        "--show-toplevel".to_string(),
        "--show-prefix".to_string(),
        "HEAD".to_string(),
    ];
    let head = run_git(process, cwd, &args, timers).await;
    if !head.ok {
        let reason = if head.failure == Some(GitFailureKind::SpawnFailed) {
            GitUnavailableReason::SpawnFailed
        } else if head.failure != Some(GitFailureKind::Timeout)
            && head.stderr.contains("not a git repository")
        {
            GitUnavailableReason::NotARepo
        } else if head.failure != Some(GitFailureKind::Timeout)
            && (head.stderr.contains("detected dubious ownership")
                || head.stderr.contains("Permission denied"))
        {
            GitUnavailableReason::Permission
        } else {
            GitUnavailableReason::Timeout
        };
        return RepoHeadResult::Unavailable {
            reason,
            detail: head.stderr,
        };
    }
    let lines = head.stdout.split('\n').collect::<Vec<_>>();
    let repo_root = lines.first().map(|line| trim_js(line).to_string()).unwrap_or_default();
    let prefix = lines.get(1).map(|line| trim_js(line).to_string()).unwrap_or_default();
    let head_sha = lines.get(2).map(|line| trim_js(line).to_string()).unwrap_or_default();
    if repo_root.is_empty() || head_sha.is_empty() {
        return RepoHeadResult::Unavailable {
            reason: GitUnavailableReason::Timeout,
            detail: "git rev-parse returned incomplete repository data".to_string(),
        };
    }
    RepoHeadResult::Ok(RepoHead {
        repo_root,
        prefix,
        head_sha,
    })
}

pub async fn read_git_history(
    process: &Arc<dyn IHostProcessService>,
    cwd: &str,
    window: usize,
    head_sha: String,
    timers: Option<Arc<dyn IHostTimers>>,
) -> GitReadResult {
    let meta_args = vec![
        "-c".to_string(),
        "core.quotePath=false".to_string(),
        "-C".to_string(),
        cwd.to_string(),
        "log".to_string(),
        format!("-{window}"),
        "--date=short".to_string(),
        format!("--format={LOG_FORMAT}"),
    ];
    let meta = run_git(process, cwd, &meta_args, timers.clone()).await;
    if !meta.ok {
        if meta.stderr.contains("not a git repository") {
            return GitReadResult::Unavailable;
        }
        return GitReadResult::Failed {
            reason: meta.stderr,
        };
    }
    let file_args = vec![
        "-c".to_string(),
        "core.quotePath=false".to_string(),
        "-C".to_string(),
        cwd.to_string(),
        "log".to_string(),
        format!("-{window}"),
        "--name-only".to_string(),
        "--no-renames".to_string(),
        "--format=%H".to_string(),
    ];
    let files = run_git(process, cwd, &file_args, timers).await;
    if !files.ok {
        if files.stderr.contains("not a git repository") {
            return GitReadResult::Unavailable;
        }
        return GitReadResult::Failed {
            reason: files.stderr,
        };
    }
    GitReadResult::Ok {
        head_sha,
        commits: parse_commits(&files.stdout, &parse_meta(&meta.stdout)),
    }
}

pub async fn read_repo_tree(
    process: &Arc<dyn IHostProcessService>,
    cwd: &str,
    timers: Option<Arc<dyn IHostTimers>>,
) -> Option<Vec<String>> {
    let args = vec![
        "-c".to_string(),
        "core.quotePath=false".to_string(),
        "-C".to_string(),
        cwd.to_string(),
        "ls-tree".to_string(),
        "-d".to_string(),
        "-r".to_string(),
        "--name-only".to_string(),
        "HEAD".to_string(),
    ];
    let tree = run_git(process, cwd, &args, timers).await;
    tree.ok.then(|| split_lines(&tree.stdout))
}

pub async fn read_repo_map(
    process: &Arc<dyn IHostProcessService>,
    cwd: &str,
    path: Option<&str>,
    timers: Option<Arc<dyn IHostTimers>>,
) -> Option<Vec<String>> {
    let mut args = vec![
        "-c".to_string(),
        "core.quotePath=false".to_string(),
        "-C".to_string(),
        cwd.to_string(),
        "ls-tree".to_string(),
        "-r".to_string(),
        "-t".to_string(),
        "--name-only".to_string(),
        "HEAD".to_string(),
    ];
    if path.is_some_and(|path| !path.is_empty()) {
        args.push("--".to_string());
        args.push(path.unwrap().to_string());
    }
    let map = run_git(process, cwd, &args, timers).await;
    map.ok.then(|| split_lines(&map.stdout))
}

fn split_lines(stdout: &str) -> Vec<String> {
    stdout
        .split('\n')
        .map(trim_js)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}
