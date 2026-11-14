use std::sync::Arc;
use std::time::Duration;

use futures_util::future::{BoxFuture, join_all};
use serde_json::json;
use url::Url;

use crate::_base::log::log::ILogger;
use crate::os::interface::host_process::{IHostProcess, IHostProcessService};
use crate::os::interface::host_timers::IHostTimers;
use crate::wire::record::WireAsyncError;
use crate::workspace::workspace_fs::internal::fs_process::read_stream;

pub const GIT_TIMEOUT_MS: u64 = 5_000;
pub const MAX_DIRTY_FILES: usize = 20;
pub const MAX_COMMIT_LINE_LENGTH: usize = 200;

pub const ALLOWED_HOSTS: &[&str] = &[
    "github.com",
    "gitlab.com",
    "gitee.com",
    "bitbucket.org",
    "codeberg.org",
    "git.sr.ht",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitFailure {
    Timeout,
    SpawnError,
    CommandFailed {
        exit_code: Option<i32>,
        stderr: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitResult {
    Ok { stdout: String },
    Err { failure: GitFailure },
}

impl GitResult {
    pub fn ok(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }

    pub fn failure(&self) -> Option<&GitFailure> {
        match self {
            Self::Ok { .. } => None,
            Self::Err { failure } => Some(failure),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaggedGitResult {
    pub args: Vec<String>,
    pub result: GitResult,
}

pub async fn collect_git_context(
    process: Arc<dyn IHostProcessService>,
    cwd: &str,
    log: Option<Arc<dyn ILogger>>,
    timers: Option<Arc<dyn IHostTimers>>,
) -> String {
    let rev_parse_args = vec!["rev-parse".to_string(), "--is-inside-work-tree".to_string()];
    let rev_parse = run_git(&process, cwd, &rev_parse_args, timers.clone()).await;
    if !rev_parse.ok() {
        if let Some(GitFailure::CommandFailed { stderr, .. }) = rev_parse.failure()
            && is_not_a_repo(stderr.as_deref())
        {
            return "<git-context status=\"unavailable\" reason=\"not-a-repo\"/>".to_string();
        }
        log_git_failure(cwd, &rev_parse_args, rev_parse.failure().unwrap(), log.as_deref());
        return String::new();
    }

    let command_args = vec![
        vec!["remote".to_string(), "get-url".to_string(), "origin".to_string()],
        vec!["symbolic-ref".to_string(), "--short".to_string(), "HEAD".to_string()],
        vec!["status".to_string(), "--porcelain".to_string()],
        vec!["log".to_string(), "-3".to_string(), "--format=%h %s".to_string()],
    ];
    let tagged = join_all(command_args.into_iter().map(|args| {
        let process = process.clone();
        let cwd = cwd.to_string();
        let timers = timers.clone();
        async move {
            let result = run_git(&process, &cwd, &args, timers).await;
            TaggedGitResult { args, result }
        }
    }))
    .await;

    for entry in &tagged {
        if let Some(failure) = entry.result.failure() {
            log_git_failure(cwd, &entry.args, failure, log.as_deref());
        }
    }

    let remote_url = stdout_of(&tagged[0].result);
    let branch_name = stdout_of(&tagged[1].result);
    let dirty_raw = stdout_of(&tagged[2].result);
    let log_raw = stdout_of(&tagged[3].result);
    let mut sections = vec![format!("Working directory: {cwd}")];

    if !remote_url.is_empty()
        && let Some(safe_url) = sanitize_remote_url(&remote_url)
    {
        sections.push(format!("Remote: {safe_url}"));
        if let Some(project) = parse_project_name(&safe_url) {
            sections.push(format!("Project: {project}"));
        }
    }
    if !branch_name.is_empty() {
        sections.push(format!("Branch: {branch_name}"));
    }

    let dirty_lines = dirty_raw
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if !dirty_lines.is_empty() {
        let total = dirty_lines.len();
        let mut body = dirty_lines
            .iter()
            .take(MAX_DIRTY_FILES)
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        if total > MAX_DIRTY_FILES {
            body.push_str(&format!("\n  ... and {} more", total - MAX_DIRTY_FILES));
        }
        sections.push(format!("Dirty files ({total}):\n{body}"));
    }

    if !log_raw.is_empty() {
        let log_lines = log_raw
            .split('\n')
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>();
        if !log_lines.is_empty() {
            let body = log_lines
                .iter()
                .map(|line| format!("  {}", truncate_utf16(line, MAX_COMMIT_LINE_LENGTH)))
                .collect::<Vec<_>>()
                .join("\n");
            sections.push(format!("Recent commits:\n{body}"));
        }
    }

    if sections.len() <= 1 {
        return String::new();
    }
    format!("<git-context>\n{}\n</git-context>", sections.join("\n"))
}

pub fn sanitize_remote_url(remote_url: &str) -> Option<String> {
    for host in ALLOWED_HOSTS {
        if remote_url.starts_with(&format!("git@{host}:")) {
            return Some(remote_url.to_string());
        }
    }
    let parsed = Url::parse(remote_url).ok()?;
    let hostname = parsed.host_str()?;
    if !ALLOWED_HOSTS.contains(&hostname) {
        return None;
    }
    let port = parsed.port().map(|port| format!(":{port}")).unwrap_or_default();
    Some(format!("https://{hostname}{port}{}", parsed.path()))
}

pub fn parse_project_name(remote_url: &str) -> Option<String> {
    let scp = regex::Regex::new(r"^[^/]+@[^/:]+:(.+)$").ok()?;
    let raw_path = scp
        .captures(remote_url)
        .and_then(|captures| captures.get(1).map(|value| value.as_str().to_string()))
        .or_else(|| try_url_path(remote_url))?;
    let trimmed = raw_path.trim_start_matches('/').trim_end_matches('/');
    let project = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    (!project.is_empty()).then_some(project.to_string())
}

fn try_url_path(remote_url: &str) -> Option<String> {
    Url::parse(remote_url).ok().map(|url| url.path().to_string())
}

fn stdout_of(result: &GitResult) -> String {
    match result {
        GitResult::Ok { stdout } => stdout.clone(),
        GitResult::Err { .. } => String::new(),
    }
}

fn is_not_a_repo(stderr: Option<&str>) -> bool {
    stderr.is_some_and(|stderr| stderr.contains("not a git repository"))
}

fn log_git_failure(cwd: &str, args: &[String], failure: &GitFailure, log: Option<&dyn ILogger>) {
    let Some(log) = log else {
        return;
    };
    let command = format!("git {}", args.join(" "));
    match failure {
        GitFailure::Timeout => log.debug(
            "git context command timed out",
            Some(json!({ "cwd": cwd, "command": command })),
        ),
        GitFailure::SpawnError => log.warn(
            "git context command failed to spawn",
            Some(json!({ "cwd": cwd, "command": command })),
        ),
        GitFailure::CommandFailed { exit_code, stderr } => log.debug(
            "git context command failed",
            Some(json!({
                "cwd": cwd,
                "command": command,
                "exitCode": exit_code,
                "stderr": stderr,
            })),
        ),
    }
}

async fn run_git(
    process: &Arc<dyn IHostProcessService>,
    cwd: &str,
    args: &[String],
    timers: Option<Arc<dyn IHostTimers>>,
) -> GitResult {
    let mut spawn_args = vec!["-C".to_string(), cwd.to_string()];
    spawn_args.extend_from_slice(args);
    let proc = match process.spawn("git", &spawn_args, Some(cwd), None).await {
        Ok(proc) => proc,
        Err(_) => {
            return GitResult::Err {
                failure: GitFailure::SpawnError,
            };
        }
    };
    let _ = proc.stdin_end().await;
    let work_proc = proc.clone();
    let mut work = tokio::spawn(async move { collect_process_output(work_proc).await });
    let clear_timers = timers.clone();
    let (timeout, timer) = timeout_future(timers, args);
    let result = tokio::select! {
        result = &mut work => {
            match result {
                Ok(Ok((stdout, _, 0))) => GitResult::Ok { stdout: stdout.trim().to_string() },
                Ok(Ok((_, stderr, exit_code))) => GitResult::Err {
                    failure: GitFailure::CommandFailed {
                        exit_code: Some(exit_code),
                        stderr: Some(stderr.trim().to_string()),
                    },
                },
                _ => GitResult::Err {
                    failure: GitFailure::CommandFailed { exit_code: None, stderr: None },
                },
            }
        }
        _ = timeout => {
            let _ = proc.kill("SIGKILL").await;
            let _ = work.await;
            GitResult::Err { failure: GitFailure::Timeout }
        }
    };
    if let (Some(timers), Some(timer)) = (clear_timers, timer) {
        timers.clear_timeout(timer);
    }
    dispose_process(&proc).await;
    result
}

fn timeout_future(
    timers: Option<Arc<dyn IHostTimers>>,
    args: &[String],
) -> (
    BoxFuture<'static, ()>,
    Option<crate::os::interface::host_timers::IHostTimerHandle>,
) {
    if let Some(timers) = timers {
        let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
        let message = format!("git {} timed out", args.join(" "));
        let sender = std::sync::Mutex::new(Some(sender));
        let handle = timers.set_timeout(
            Arc::new(move || {
                let _ = &message;
                if let Some(sender) = sender.lock().unwrap_or_else(|error| error.into_inner()).take() {
                    let _ = sender.send(());
                }
            }),
            GIT_TIMEOUT_MS,
        );
        let timeout = Box::pin(async move {
            let _ = receiver.await;
        }) as BoxFuture<'static, ()>;
        (timeout, Some(handle))
    } else {
        (
            Box::pin(tokio::time::sleep(Duration::from_millis(GIT_TIMEOUT_MS))),
            None,
        )
    }
}

async fn collect_process_output(
    proc: Arc<dyn IHostProcess>,
) -> Result<(String, String, i32), WireAsyncError> {
    let (stdout, stderr, exit_code) = tokio::join!(
        read_stream(proc.stdout()),
        read_stream(proc.stderr()),
        async { proc.wait().await.unwrap_or(-1) }
    );
    Ok((stdout?, stderr?, exit_code))
}

async fn dispose_process(proc: &Arc<dyn IHostProcess>) {
    let _ = proc.dispose().await;
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
