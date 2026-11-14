use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use futures_util::stream::{self, Stream};
use futures_util::{FutureExt, StreamExt};

use crate::_base::utils::abort::{AbortSignal, abort_error};
use crate::_base::utils::paths::join;
use crate::environment::environment::{Environment, EnvironmentPath};
use crate::environment::share_bin_rg::is_remote_environment;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_process::IHostProcessService;
use crate::wire::record::WireAsyncError;

use super::super::fs::{
    FsKind, FsSuggestResponse, FsSuggestStreamEvent, FsSuggestStreamPhase, FsSearchHit,
};
use super::fs_process::{RunCommandOptions, run_command};
use super::fs_search::{VCS_METADATA_DIRS, compute_match_positions, matches_any_glob};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdResolutionSource {
    SystemPath,
    ShareBinCached,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FdResolution {
    pub path: String,
    pub source: FdResolutionSource,
}

#[derive(Clone)]
pub struct FdProbe {
    pub exec: Arc<
        dyn Fn(Vec<String>) -> BoxFuture<'static, Result<FdProbeResult, WireAsyncError>>
            + Send
            + Sync,
    >,
}

#[derive(Debug, Clone, Copy)]
pub struct FdProbeResult {
    pub exit_code: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FdRoot {
    pub dir: String,
    pub real: String,
    pub primary: bool,
}

#[derive(Clone)]
struct FdTarget {
    root: FdRoot,
    base_dir: String,
    display_base: String,
    fd_query: String,
    absolute: bool,
}

#[derive(Debug, Clone)]
struct FdEntry {
    path: String,
    is_directory: bool,
}

struct FdGroup {
    target: FdTarget,
    entries: Vec<FdEntry>,
    capped: bool,
}

#[derive(Clone)]
struct ScoredEntry {
    target: FdTarget,
    entry: FdEntry,
    absolute_path: String,
    ignored: bool,
    score: i32,
}

pub struct FdSuggestOptions {
    pub fd_binary: String,
    pub query: String,
    pub cap: usize,
    pub follow_gitignore: bool,
    pub include_globs: Option<Vec<String>>,
    pub exclude_globs: Option<Vec<String>>,
    pub roots: Vec<FdRoot>,
    pub path: EnvironmentPath,
    pub host_fs: Arc<dyn IHostFileSystem>,
    pub process: Arc<dyn IHostProcessService>,
    pub work_dir: String,
    pub home_dir: Option<String>,
    pub signal: AbortSignal,
}

fn slash(value: &str) -> String {
    value.replace('\\', "/")
}

fn fd_path_query(query: &str) -> String {
    let normalized = slash(query);
    if !normalized.contains('/') {
        return regex::escape(&normalized);
    }
    let trailing = normalized.ends_with('/');
    let trimmed = normalized.trim_matches('/');
    if trimmed.is_empty() {
        return normalized;
    }
    let mut pattern = trimmed
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join("[\\\\/]");
    if trailing {
        pattern.push_str("[\\\\/]");
    }
    pattern
}

fn inside(path: &EnvironmentPath, candidate: &str, root: &str) -> bool {
    let rel = path.relative(root, candidate);
    rel.is_empty() || (!rel.starts_with("..") && !path.is_absolute(&rel))
}

fn binary_name() -> &'static str {
    if cfg!(windows) { "fd.exe" } else { "fd" }
}

fn cached_binary_path(environment: Option<&Environment>) -> String {
    if is_remote_environment(environment) {
        if let Some(home_dir) = environment.and_then(|environment| environment.host.as_ref()).and_then(|host| host.home_dir.as_deref()) {
            let binary = environment
                .and_then(|environment| environment.host.as_ref())
                .is_some_and(|host| host.path_class == "win32")
                .then_some("fd.exe")
                .unwrap_or("fd");
            return environment
                .and_then(|environment| environment.path.as_ref())
                .map(|path| path.join(home_dir, &[".kimi-code", "bin", binary]))
                .unwrap_or_else(|| format!("{home_dir}/.kimi-code/bin/{binary}"));
        }
    }
    if let Ok(override_) = std::env::var("KIMI_CODE_HOME") {
        if !override_.is_empty() {
            return join(&override_, &["bin", binary_name()]);
        }
    }
    let home = std::env::home_dir().unwrap_or_default().to_string_lossy().into_owned();
    join(&home, &[".kimi-code", "bin", binary_name()])
}

pub async fn ensure_fd_path(
    probe: FdProbe,
    signal: Option<AbortSignal>,
    environment: Option<Environment>,
) -> Result<FdResolution, WireAsyncError> {
    if signal.as_ref().is_some_and(|signal| signal.aborted()) {
        return Err(Box::new(abort_error()));
    }
    for binary in ["fd", "fdfind"] {
        let result = (probe.exec)(vec![binary.to_string(), "--version".to_string()])
            .await
            .unwrap_or(FdProbeResult { exit_code: -1 });
        if result.exit_code == 0 {
            return Ok(FdResolution {
                path: binary.to_string(),
                source: FdResolutionSource::SystemPath,
            });
        }
    }
    if signal.as_ref().is_some_and(|signal| signal.aborted()) {
        return Err(Box::new(abort_error()));
    }
    let cached = cached_binary_path(environment.as_ref());
    let result = (probe.exec)(vec![cached.clone(), "--version".to_string()])
        .await
        .unwrap_or(FdProbeResult { exit_code: -1 });
    if result.exit_code == 0 {
        return Ok(FdResolution {
            path: cached,
            source: FdResolutionSource::ShareBinCached,
        });
    }
    Err("fd is not available".into())
}

pub async fn suggest_with_fd(options: FdSuggestOptions) -> Result<FsSuggestResponse, WireAsyncError> {
    let events = suggest_with_fd_events(options);
    futures_util::pin_mut!(events);
    while let Some(event) = events.next().await {
        let event = event?;
        if event.phase == FsSuggestStreamPhase::Final {
            return Ok(event.response);
        }
    }
    Err("fd suggestion ended without a final response".into())
}

pub fn suggest_with_fd_events(
    options: FdSuggestOptions,
) -> Pin<Box<dyn Stream<Item = Result<FsSuggestStreamEvent, WireAsyncError>> + Send>> {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        if let Err(error) = run_suggest(options, sender.clone()).await {
            let _ = sender.send(Err(error)).await;
        }
    });
    Box::pin(stream::poll_fn(move |context| receiver.poll_recv(context)))
}

async fn run_suggest(
    options: FdSuggestOptions,
    sender: tokio::sync::mpsc::Sender<Result<FsSuggestStreamEvent, WireAsyncError>>,
) -> Result<(), WireAsyncError> {
    let normalized_query = slash(&options.query);
    let slash_index = normalized_query.rfind('/');
    let scope_base = slash_index.map(|index| normalized_query[..=index].to_string());
    let scope_query = slash_index.map_or(normalized_query.clone(), |index| {
        normalized_query[index + 1..].to_string()
    });
    let full_target = |root: &FdRoot| FdTarget {
        root: root.clone(),
        base_dir: root.dir.clone(),
        display_base: String::new(),
        fd_query: normalized_query.clone(),
        absolute: false,
    };
    let mut targets = Vec::new();
    if let Some(scope_base) = &scope_base {
        if scope_base.starts_with('/') || scope_base.starts_with("~/") {
            let home_dir = options.home_dir.clone().unwrap_or_else(|| {
                std::env::home_dir().unwrap_or_default().to_string_lossy().into_owned()
            });
            let base_dir = if scope_base.starts_with("~/") {
                options.path.join(&home_dir, &[&scope_base[2..]])
            } else {
                scope_base.clone()
            };
            if let Some(owner) = valid_directory(&options, &base_dir).await {
                targets.push(FdTarget {
                    root: owner,
                    base_dir,
                    display_base: scope_base.clone(),
                    fd_query: scope_query,
                    absolute: true,
                });
            }
        } else {
            for root in &options.roots {
                if options.signal.aborted() {
                    break;
                }
                let base_dir = options.path.resolve(&root.dir, scope_base);
                let owner = valid_directory(&options, &base_dir).await;
                targets.push(match owner {
                    Some(_) => FdTarget {
                        root: root.clone(),
                        base_dir,
                        display_base: scope_base.clone(),
                        fd_query: scope_query.clone(),
                        absolute: false,
                    },
                    None => full_target(root),
                });
            }
        }
    } else {
        for root in &options.roots {
            if options.signal.aborted() {
                break;
            }
            targets.push(full_target(root));
        }
    }
    if targets.is_empty() || options.signal.aborted() {
        sender
            .send(Ok(FsSuggestStreamEvent {
                phase: FsSuggestStreamPhase::Final,
                response: FsSuggestResponse {
                    items: Vec::new(),
                    truncated: options.signal.aborted(),
                },
            }))
            .await
            .ok();
        return Ok(());
    }

    let ordinary = collect_groups(&options, &targets, false).await?;
    let mut best = IndexMap::new();
    collect(&options, &ordinary, false, &mut best);
    let highest = best.values().filter(|entry| entry.score >= 110).count();
    let can_skip_ignored = best.len() >= 20 && highest >= 20;
    let mut groups = ordinary;
    if !options.follow_gitignore && !can_skip_ignored && !options.signal.aborted() && !best.is_empty() {
        if sender
            .send(Ok(FsSuggestStreamEvent {
                phase: FsSuggestStreamPhase::Preview,
                response: build_response(&options, &best, &groups, true),
            }))
            .await
            .is_err()
        {
            return Ok(());
        }
    }
    if !options.follow_gitignore && !can_skip_ignored && !options.signal.aborted() {
        let ignored = collect_groups(&options, &targets, true).await?;
        collect(&options, &ignored, true, &mut best);
        groups.extend(ignored);
    }
    sender
        .send(Ok(FsSuggestStreamEvent {
            phase: FsSuggestStreamPhase::Final,
            response: build_response(&options, &best, &groups, false),
        }))
        .await
        .ok();
    Ok(())
}

async fn valid_directory(options: &FdSuggestOptions, candidate: &str) -> Option<FdRoot> {
    if options.signal.aborted() {
        return None;
    }
    let (stat, real) = tokio::join!(
        options.host_fs.stat(candidate),
        options.host_fs.realpath(candidate)
    );
    let (Ok(stat), Ok(real)) = (stat, real) else {
        return None;
    };
    if options.signal.aborted() || !stat.is_directory {
        return None;
    }
    options
        .roots
        .iter()
        .find(|root| inside(&options.path, &real, &root.real))
        .cloned()
}

async fn collect_groups(
    options: &FdSuggestOptions,
    targets: &[FdTarget],
    include_ignored: bool,
) -> Result<Vec<FdGroup>, WireAsyncError> {
    let mut groups = Vec::new();
    for target in targets {
        let base = run_pass(options, target, include_ignored, Some(1)).await?;
        let recursive = run_pass(options, target, include_ignored, None).await?;
        let mut seen = base
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<HashSet<_>>();
        let mut entries = base.entries;
        entries.extend(recursive.entries.into_iter().filter(|entry| {
            seen.insert(entry.path.clone())
        }));
        groups.push(FdGroup {
            target: target.clone(),
            entries,
            capped: base.capped || recursive.capped,
        });
    }
    Ok(groups)
}

async fn run_pass(
    options: &FdSuggestOptions,
    target: &FdTarget,
    include_ignored: bool,
    max_depth: Option<usize>,
) -> Result<FdGroup, WireAsyncError> {
    let mut args = vec![
        options.fd_binary.clone(),
        "--base-directory".to_string(),
        target.base_dir.clone(),
        "--no-require-git".to_string(),
        "--max-results".to_string(),
        "100".to_string(),
        "--type".to_string(),
        "f".to_string(),
        "--type".to_string(),
        "d".to_string(),
        "--hidden".to_string(),
    ];
    for dir in VCS_METADATA_DIRS.iter() {
        args.extend([
            "--exclude".to_string(),
            dir.to_string(),
            "--exclude".to_string(),
            format!("{dir}/*"),
            "--exclude".to_string(),
            format!("{dir}/**"),
        ]);
    }
    if include_ignored {
        args.push("--no-ignore-vcs".to_string());
    }
    if let Some(max_depth) = max_depth {
        args.extend(["--max-depth".to_string(), max_depth.to_string()]);
    }
    if target.fd_query.contains('/') {
        args.push("--full-path".to_string());
    }
    if !target.fd_query.is_empty() {
        args.push(fd_path_query(&target.fd_query));
    }
    let result = run_command(
        options.process.as_ref(),
        &args,
        RunCommandOptions {
            cwd: Some(options.work_dir.clone()),
            env: None,
            signal: Some(options.signal.clone()),
        },
    )
    .await?;
    if result.exit_code != 0 || result.stdout.is_empty() || options.signal.aborted() {
        return Ok(FdGroup {
            target: target.clone(),
            entries: Vec::new(),
            capped: false,
        });
    }
    let base_dir = slash(&target.base_dir);
    let base_prefix = format!("{}/", base_dir.strip_suffix('/').unwrap_or(&base_dir));
    let mut entries = Vec::new();
    let mut raw_count = 0;
    for raw in result.stdout.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if raw.is_empty() {
            continue;
        }
        raw_count += 1;
        let mut line = slash(raw);
        if line.starts_with(&base_prefix) {
            line = line[base_prefix.len()..].to_string();
        }
        if line.starts_with("./") {
            line = line[2..].to_string();
        }
        let is_directory = line.ends_with('/');
        let rel = if is_directory { &line[..line.len() - 1] } else { &line };
        if rel.is_empty()
            || rel
                .split('/')
                .any(|segment| VCS_METADATA_DIRS.contains(segment))
        {
            continue;
        }
        entries.push(FdEntry {
            path: rel.to_string(),
            is_directory,
        });
    }
    Ok(FdGroup {
        target: target.clone(),
        entries,
        capped: raw_count >= 100,
    })
}

fn collect(
    options: &FdSuggestOptions,
    groups: &[FdGroup],
    ignored: bool,
    best: &mut IndexMap<String, ScoredEntry>,
) {
    for group in groups {
        for entry in &group.entries {
            let lower_name = entry
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&entry.path)
                .to_lowercase();
            let lower_query = group.target.fd_query.to_lowercase();
            let lower_path = entry.path.to_lowercase();
            let mut score = if group.target.fd_query.is_empty() {
                1
            } else if lower_name == lower_query {
                100
            } else if lower_name.starts_with(&lower_query) {
                80
            } else if lower_name.contains(&lower_query) {
                50
            } else if lower_path.contains(&lower_query) {
                30
            } else {
                0
            };
            if entry.is_directory && !group.target.fd_query.is_empty() && score > 0 {
                score += 10;
            }
            if score <= 0 {
                continue;
            }
            let absolute_path = options.path.join(&group.target.base_dir, &[&entry.path]);
            let rel_for_globs = if group.target.absolute {
                slash(&options.path.relative(&group.target.root.dir, &absolute_path))
            } else {
                format!("{}{}", group.target.display_base, entry.path)
            };
            if options
                .include_globs
                .as_ref()
                .is_some_and(|globs| !matches_any_glob(&rel_for_globs, globs))
                || options
                    .exclude_globs
                    .as_ref()
                    .is_some_and(|globs| matches_any_glob(&rel_for_globs, globs))
            {
                continue;
            }
            let candidate = ScoredEntry {
                target: group.target.clone(),
                entry: entry.clone(),
                absolute_path: slash(&absolute_path),
                ignored,
                score,
            };
            let replace = best.get(&candidate.absolute_path).is_none_or(|existing| {
                (existing.ignored == ignored && score > existing.score)
                    || (existing.ignored && !ignored)
            });
            if replace {
                best.insert(candidate.absolute_path.clone(), candidate);
            }
        }
    }
}

fn build_response(
    options: &FdSuggestOptions,
    best: &IndexMap<String, ScoredEntry>,
    groups: &[FdGroup],
    preview: bool,
) -> FsSuggestResponse {
    let mut ranked = best.values().cloned().collect::<Vec<_>>();
    ranked.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.ignored.cmp(&b.ignored))
            .then_with(|| {
                a.entry
                    .path
                    .split('/')
                    .filter(|segment| !segment.is_empty())
                    .count()
                    .cmp(
                        &b.entry
                            .path
                            .split('/')
                            .filter(|segment| !segment.is_empty())
                            .count(),
                    )
            })
            .then_with(|| {
                a.entry
                    .path
                    .encode_utf16()
                    .count()
                    .cmp(&b.entry.path.encode_utf16().count())
            })
            .then_with(|| locale_compare(&a.entry.path, &b.entry.path))
    });
    let items = ranked
        .iter()
        .take(options.cap)
        .map(|candidate| {
            let display_path = if candidate.target.absolute || !candidate.target.root.primary {
                if candidate.target.absolute {
                    format!(
                        "{}{}",
                        candidate.target.display_base, candidate.entry.path
                    )
                } else {
                    candidate.absolute_path.clone()
                }
            } else {
                format!(
                    "{}{}",
                    candidate.target.display_base, candidate.entry.path
                )
            };
            FsSearchHit {
                path: display_path.clone(),
                name: candidate
                    .entry
                    .path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&candidate.entry.path)
                    .to_string(),
                kind: if candidate.entry.is_directory {
                    FsKind::Directory
                } else {
                    FsKind::File
                },
                score: (candidate.score as f64 / 110.0).min(1.0),
                match_positions: compute_match_positions(
                    &display_path,
                    &options.query.to_lowercase(),
                ),
            }
        })
        .collect::<Vec<_>>();
    FsSuggestResponse {
        items,
        truncated: preview
            || options.signal.aborted()
            || ranked.len() > options.cap
            || groups.iter().any(|group| group.capped),
    }
}

fn locale_compare(left: &str, right: &str) -> std::cmp::Ordering {
    let script = format!(
        "String((function (left, right) {{ return left.localeCompare(right); }})({}, {}))",
        serde_json::to_string(left).unwrap(),
        serde_json::to_string(right).unwrap()
    );
    rquickjs::Runtime::new()
        .and_then(|runtime| {
            rquickjs::Context::full(&runtime).and_then(|context| {
                context.with(|ctx| ctx.eval::<String, _>(script))
            })
        })
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .map(|value| value.cmp(&0))
        .unwrap_or_else(|| left.encode_utf16().cmp(right.encode_utf16()))
}
