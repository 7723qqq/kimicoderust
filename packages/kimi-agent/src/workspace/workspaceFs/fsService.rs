use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use futures_util::future::BoxFuture;
use futures_util::stream::{self, Stream};
use futures_util::{FutureExt, StreamExt};
use indexmap::IndexMap;
use kimi_core::primitives::errors::domains::os_fs::OS_FS_DIRECTORY_TOO_LARGE;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use parking_lot::Mutex;

use crate::_base::text::encoding::{UtfTextEncoding, classify_text_sample, decode_utf_text};
use crate::_base::utils::abort::{AbortController, AbortSignal};
use crate::_base::utils::file_meta::{
    FS_BINARY_SAMPLE_BYTES, FileMetaStat, build_etag, count_lines, guess_language_id, guess_mime,
};
use crate::app::environment::environment::EnvironmentResolver;
use crate::app::git::git::GitStatusOptions;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::environment::environment::{
    EnvironmentBinding, EnvironmentCapability, EnvironmentError, EnvironmentPath,
};
use crate::environment::environment_defaults::POSIX_ENVIRONMENT_PATH;
use crate::os::interface::host_file_system::{HostDirEntry, HostFileStat, HostWalkEntry, HostWalkVisitor, IHostFileSystem};
use crate::os::interface::host_process::IHostProcess;
use crate::os::interface::host_fs_errors::{is_host_fs_not_directory, is_host_fs_not_found};
use crate::wire::record::WireAsyncError;
use crate::workspace::workspace_context::workspace_context::IWorkspaceContext;
use crate::workspace::workspace_dirs::workspace_dirs::IWorkspaceDirs;
use crate::workspace::workspace_git::workspace_git::IWorkspaceGitService;
use kimi_core::primitives::errors::{Error2, Error2Options};

use super::fs::*;
use super::internal::errors::*;
use super::internal::fd_suggest::{
    FdResolution, FdRoot, FdSuggestOptions, ensure_fd_path, suggest_with_fd,
    suggest_with_fd_events,
};
use super::internal::fs_process::{RunCommandOptions, read_stream, run_command};
use super::internal::fs_search::*;
use super::internal::rg_locator::{RgProbe, RgResolution, ensure_rg_path};

const SEARCH_HARD_CAP: usize = 500;
const LIST_LSTAT_CONCURRENCY: usize = 16;
const LIST_TOTAL_ENTRY_BUDGET: usize = 10_000;
const PROBE_MAX_DEPTH: u32 = 6;
const PROBE_MAX_ENTRIES: usize = 500;
const GREP_TIMEOUT_MS: u64 = 30_000;
const SUGGEST_TIMEOUT_MS: u64 = 10_000;
const FS_READ_MAX_BYTES: u64 = 10 * 1024 * 1024;

const FS_VALIDATION_FAILED: i64 = 40001;
const FS_WIRE_PATH_NOT_FOUND: i64 = 40409;
const FS_ENVIRONMENT_NOT_FOUND: i64 = 40420;
const FS_SESSION_BUSY: i64 = 40901;
const FS_WIRE_IS_DIRECTORY: i64 = 40906;
const FS_WIRE_IS_BINARY: i64 = 40907;
const FS_WIRE_GIT_UNAVAILABLE: i64 = 40908;
const FS_ENVIRONMENT_UNAVAILABLE: i64 = 40926;
const FS_WIRE_TOO_LARGE: i64 = 41302;
const FS_WIRE_TOO_MANY_RESULTS: i64 = 41303;
const FS_WIRE_PATH_ESCAPES: i64 = 41304;
const FS_INTERNAL_ERROR: i64 = 50001;

type FsResult<T> = Result<T, WireAsyncError>;
type SuggestEventStream = stream::BoxStream<'static, Result<FsSuggestStreamEvent, WireAsyncError>>;

#[derive(Clone)]
struct SuggestRoot {
    dir: String,
    real: String,
    primary: bool,
}

#[derive(Clone)]
struct SuggestDirectTarget {
    root: SuggestRoot,
    rel_path: String,
    kind: FsKind,
}

#[derive(Clone)]
struct IgnoreRules {
    matcher: Arc<Gitignore>,
    walk_rules: Vec<String>,
}

struct FsCaches {
    gitignore: HashMap<String, IgnoreRules>,
    rg_resolution: Option<Option<RgResolution>>,
    fd_resolution: Option<Option<FdResolution>>,
    real_roots: Option<(String, Vec<(String, String)>)>,
}

pub struct WorkspaceFsService {
    workspace_dirs: Arc<dyn IWorkspaceDirs>,
    host_fs: Arc<dyn IHostFileSystem>,
    resolver: Arc<dyn EnvironmentResolver>,
    telemetry: Arc<dyn ITelemetryService>,
    git: Arc<dyn IWorkspaceGitService>,
    environment_id: String,
    workspace_id: String,
    path: EnvironmentPath,
    caches: Arc<Mutex<FsCaches>>,
}

impl WorkspaceFsService {
    pub fn new(
        workspace: Arc<dyn IWorkspaceContext>,
        workspace_dirs: Arc<dyn IWorkspaceDirs>,
        host_fs: Arc<dyn IHostFileSystem>,
        resolver: Arc<dyn EnvironmentResolver>,
        telemetry: Arc<dyn ITelemetryService>,
        git: Arc<dyn IWorkspaceGitService>,
        environment_id: Option<String>,
    ) -> Self {
        let environment_id = environment_id.unwrap_or_else(|| "local".to_string());
        let binding = EnvironmentBinding {
            environment_id: environment_id.clone(),
            cwd: None,
        };
        let path = resolver
            .inspect(&binding)
            .ok()
            .and_then(|environment| environment.path())
            .unwrap_or_else(|| POSIX_ENVIRONMENT_PATH.clone());
        Self {
            workspace_dirs,
            host_fs,
            resolver,
            telemetry,
            git,
            environment_id,
            workspace_id: workspace.workspace_id(),
            path,
            caches: Arc::new(Mutex::new(FsCaches {
                gitignore: HashMap::new(),
                rg_resolution: None,
                fd_resolution: None,
                real_roots: None,
            })),
        }
    }

    fn environment_binding(&self) -> EnvironmentBinding {
        EnvironmentBinding {
            environment_id: self.environment_id.clone(),
            cwd: None,
        }
    }

    fn work_dir(&self) -> String {
        self.path.resolve_single(&self.workspace_dirs.primary())
    }

    fn resolve_path_input(&self, rel: &str) -> String {
        if self.path.is_absolute(rel) {
            self.path.resolve_single(rel)
        } else {
            self.path.resolve(&self.work_dir(), rel)
        }
    }

    fn is_within_workspace(&self, abs_path: &str) -> bool {
        let target = self.path.resolve_single(abs_path);
        let work_dir = self.work_dir();
        if target == work_dir {
            return true;
        }
        let rel = self.path.relative(&work_dir, &target);
        if !rel.is_empty() && !rel.starts_with("..") && !self.path.is_absolute(&rel) {
            return true;
        }
        self.workspace_dirs.additional_dirs().iter().any(|dir| {
            let rel = self.path.relative(&self.path.resolve_single(dir), &target);
            rel.is_empty() || (!rel.starts_with("..") && !self.path.is_absolute(&rel))
        })
    }

    fn abs_of(&self, rel: &str) -> String {
        if self.path.is_absolute(rel) {
            return rel.to_string();
        }
        if rel.is_empty() || rel == "." {
            self.work_dir()
        } else {
            self.path.join(&self.work_dir(), &[rel])
        }
    }

    pub async fn list(&self, req: FsListRequest) -> FsResult<FsListResponse> {
        let anchored = self.path.is_absolute(&req.path);
        let abs = if anchored {
            self.resolve_registered_root(&req.path).await?
        } else {
            self.resolve_within(&req.path, req.allow_external_symlinks == Some(true)).await?
        };
        let base = if anchored { self.anchor_root(&abs) } else { self.work_dir() };
        let rel = if anchored { self.to_rel_from(&base, &abs) } else { self.to_rel(&abs) };
        let base_for_closure = base.clone();
        let path_for_closure = self.path.clone();
        let abs_of: Arc<dyn Fn(&str) -> String + Send + Sync> = Arc::new(move |rel_path: &str| {
            if rel_path.is_empty() || rel_path == "." {
                base_for_closure.clone()
            } else {
                path_for_closure.join(&base_for_closure, &[rel_path])
            }
        });
        let top_stat = self.host_fs.stat(&abs).await.map_err(|error| map_fs_error(error, &req.path))?;
        if !top_stat.is_directory {
            return Err(fs_error(FS_PATH_NOT_FOUND, &format!("path not found: {}", req.path), &[("path", req.path.clone().into())]));
        }
        let gitignore = if req.follow_gitignore {
            self.matcher_for(&base).await?
        } else {
            None
        };
        let mut items = Vec::new();
        let mut children_by_path = HashMap::new();
        let probe_budget = AtomicUsize::new(0);
        let mut symlink_entries = 0;
        let mut budget_exhausted = false;
        let mut truncated = false;
        let mut nested_count = 0;
        let root_rel = if rel == "." { String::new() } else { rel.clone() };
        let root_is_symlink = self.path_has_symlink(&abs_of(&root_rel)).await;
        let mut queue = VecDeque::from([ListQueueEntry {
            rel_path: root_rel.clone(),
            depth_remaining: req.depth,
            ancestors: vec![self.realpath_existing_prefix(&abs_of(&root_rel)).await?],
            via_symlink: root_is_symlink,
        }]);
        while let Some(entry) = queue.pop_front() {
            if budget_exhausted && entry.via_symlink {
                continue;
            }
            let names = match self.host_fs.readdir(&abs_of(&entry.rel_path)).await {
                Ok(entries) => entries.into_iter().map(|entry| entry.name).collect::<Vec<_>>(),
                Err(error) => {
                    if entry.rel_path == root_rel || error_code(error.as_ref()) == Some(OS_FS_DIRECTORY_TOO_LARGE) {
                        return Err(map_fs_error(error, &req.path));
                    }
                    continue;
                }
            };
            let mut candidates = Vec::new();
            for name in names {
                if !req.show_hidden && is_hidden(&name) {
                    continue;
                }
                let child_rel = if entry.rel_path.is_empty() {
                    name.clone()
                } else {
                    format!("{}/{}", entry.rel_path, name)
                };
                let mut ancestor_of_allowed = false;
                if let Some(matcher) = &gitignore {
                    if matcher.ignores(&child_rel) || matcher.ignores(&format!("{child_rel}/")) {
                        let Some(allow_globs) = &req.allow_ignored_globs else {
                            continue;
                        };
                        if !matches_any_glob(&child_rel, allow_globs) {
                            if !glob_can_match_below(&child_rel, allow_globs) {
                                continue;
                            }
                            ancestor_of_allowed = true;
                        }
                    }
                }
                if req.exclude_globs.as_ref().is_some_and(|globs| matches_any_glob(&child_rel, globs)) {
                    continue;
                }
                candidates.push(ListCandidate { name, child_rel, ancestor_of_allowed });
            }
            let stats = map_with_concurrency(&candidates, LIST_LSTAT_CONCURRENCY, |candidate| {
                let path = abs_of(&candidate.child_rel);
                let host_fs = self.host_fs.clone();
                Box::pin(async move { host_fs.lstat(&path).await.ok() })
            }).await;
            let mut visible = Vec::new();
            for (index, candidate) in candidates.into_iter().enumerate() {
                let Some(stat) = stats[index].clone() else {
                    continue;
                };
                let mut is_directory = stat.is_directory;
                let mut real_path = None;
                if stat.is_symbolic_link {
                    let target = self.symlink_directory_target(
                        &candidate.child_rel,
                        req.allow_external_symlinks == Some(true),
                        &*abs_of,
                    ).await;
                    is_directory = target.0;
                    real_path = target.1;
                }
                if candidate.ancestor_of_allowed && !is_directory {
                    continue;
                }
                if candidate.ancestor_of_allowed
                    && !self.has_allowed_descendant(
                        &candidate.child_rel,
                        req.allow_ignored_globs.as_ref().unwrap(),
                        req.exclude_globs.as_ref(),
                        req.show_hidden,
                        req.allow_external_symlinks == Some(true),
                        &probe_budget,
                        &abs_of,
                    ).await
                {
                    continue;
                }
                visible.push(ListChild {
                    name: candidate.name,
                    rel_path: candidate.child_rel,
                    stat,
                    is_directory,
                    real_path,
                });
            }
            sort_children(&mut visible, req.sort);
            let parent_key = if entry.rel_path.is_empty() { ".".to_string() } else { entry.rel_path.clone() };
            let mut bucket = Vec::new();
            for child in visible {
                if items.len() >= req.limit && entry.depth_remaining == req.depth {
                    truncated = true;
                    break;
                }
                if entry.depth_remaining != req.depth {
                    if nested_count >= req.limit {
                        truncated = true;
                        break;
                    }
                    nested_count += 1;
                }
                if entry.via_symlink && symlink_entries >= LIST_TOTAL_ENTRY_BUDGET {
                    truncated = true;
                    budget_exhausted = true;
                    break;
                }
                let mut fs_entry = build_fs_entry(&child.rel_path, &child.name, &child.stat, false);
                if child.stat.is_symbolic_link && child.is_directory {
                    fs_entry.is_directory = Some(true);
                }
                if entry.depth_remaining == req.depth {
                    items.push(fs_entry.clone());
                }
                bucket.push(fs_entry);
                if entry.via_symlink {
                    symlink_entries += 1;
                }
                if child.is_directory && entry.depth_remaining > 1 {
                    let real_path = match child.real_path {
                        Some(path) => Some(path),
                        None => self.realpath_existing_prefix(&abs_of(&child.rel_path)).await.ok(),
                    };
                    let child_via_symlink = entry.via_symlink || child.stat.is_symbolic_link;
                    if let Some(real_path) = real_path {
                        if !entry.ancestors.contains(&real_path) && (!budget_exhausted || !child_via_symlink) {
                            let mut ancestors = entry.ancestors.clone();
                            ancestors.push(real_path);
                            queue.push_back(ListQueueEntry {
                                rel_path: child.rel_path,
                                depth_remaining: entry.depth_remaining - 1,
                                ancestors,
                                via_symlink: child_via_symlink,
                            });
                        }
                    }
                }
            }
            if entry.depth_remaining < req.depth {
                children_by_path.insert(parent_key, bucket);
            }
        }
        Ok(FsListResponse {
            items,
            children_by_path: (!children_by_path.is_empty()).then_some(children_by_path),
            truncated,
        })
    }

    async fn path_has_symlink(&self, abs_path: &str) -> bool {
        let mut current = abs_path.to_string();
        loop {
            if self.host_fs.lstat(&current).await.ok().is_some_and(|stat| stat.is_symbolic_link) {
                return true;
            }
            let parent = self.path.dirname(&current);
            if parent == current {
                return false;
            }
            current = parent;
        }
    }

    async fn symlink_directory_target(
        &self,
        rel_path: &str,
        allow_external: bool,
        abs_of: &dyn Fn(&str) -> String,
    ) -> (bool, Option<String>) {
        let result: FsResult<(bool, Option<String>)> = async {
            let target = self.host_fs.stat(&abs_of(rel_path)).await?;
            if !target.is_directory {
                return Ok((false, None));
            }
            let real_path = self.realpath_existing_prefix(&abs_of(rel_path)).await?;
            if !allow_external {
                let roots = self.real_roots().await;
                if !roots.iter().any(|root| is_inside_or_equal(&self.path, &real_path, root)) {
                    return Ok((false, None));
                }
            }
            Ok((true, Some(real_path)))
        }
        .await;
        result.unwrap_or((false, None))
    }

    async fn has_allowed_descendant(
        &self,
        rel_dir: &str,
        globs: &[String],
        exclude_globs: Option<&Vec<String>>,
        show_hidden: bool,
        allow_external_symlinks: bool,
        budget: &AtomicUsize,
        abs_of: &Arc<dyn Fn(&str) -> String + Send + Sync>,
    ) -> bool {
        fn walk<'a>(
            service: &'a WorkspaceFsService,
            rel: String,
            depth: u32,
            globs: &'a [String],
            exclude_globs: Option<&'a Vec<String>>,
            show_hidden: bool,
            allow_external_symlinks: bool,
            budget: &'a AtomicUsize,
            abs_of: &'a Arc<dyn Fn(&str) -> String + Send + Sync>,
        ) -> BoxFuture<'a, bool> {
            async move {
                if depth > PROBE_MAX_DEPTH || budget.load(Ordering::Relaxed) >= PROBE_MAX_ENTRIES {
                    return true;
                }
                let Ok(entries) = service.host_fs.readdir(&abs_of(&rel)).await else {
                    return false;
                };
                let mut candidates = Vec::new();
                for entry in entries {
                    if budget.load(Ordering::Relaxed) >= PROBE_MAX_ENTRIES {
                        return true;
                    }
                    budget.fetch_add(1, Ordering::Relaxed);
                    if !show_hidden && is_hidden(&entry.name) {
                        continue;
                    }
                    let child_rel = format!("{rel}/{}", entry.name);
                    if exclude_globs.is_some_and(|globs| matches_any_glob(&child_rel, globs)) {
                        continue;
                    }
                    if matches_any_glob(&child_rel, globs) {
                        return true;
                    }
                    candidates.push(child_rel);
                }
                let stats = map_with_concurrency(&candidates, LIST_LSTAT_CONCURRENCY, |child_rel| {
                    let path = abs_of(&child_rel);
                    let host_fs = service.host_fs.clone();
                    Box::pin(async move { host_fs.lstat(&path).await.ok() })
                }).await;
                for (index, child_rel) in candidates.into_iter().enumerate() {
                    let stat = &stats[index];
                    let is_directory = if stat.as_ref().is_some_and(|stat| stat.is_symbolic_link) {
                        service.symlink_directory_target(&child_rel, allow_external_symlinks, &**abs_of).await.0
                    } else {
                        stat.as_ref().is_some_and(|stat| stat.is_directory)
                    };
                    if is_directory && glob_can_match_below(&child_rel, globs) {
                        if walk(service, child_rel, depth + 1, globs, exclude_globs, show_hidden, allow_external_symlinks, budget, abs_of).await {
                            return true;
                        }
                    }
                }
                false
            }
            .boxed()
        }
        walk(self, rel_dir.to_string(), 1, globs, exclude_globs, show_hidden, allow_external_symlinks, budget, abs_of).await
    }

    pub async fn read(&self, req: FsReadRequest) -> FsResult<FsReadResponse> {
        let abs = self.resolve_within(&req.path, req.allow_external_symlinks == Some(true)).await?;
        let rel = self.to_rel(&abs);
        let stat = self.host_fs.stat(&abs).await.map_err(|error| map_fs_error(error, &req.path))?;
        if stat.is_directory {
            return Err(fs_error(FS_IS_DIRECTORY, &format!("path is a directory: {}", req.path), &[("path", req.path.clone().into())]));
        }
        if stat.size > FS_READ_MAX_BYTES {
            return Err(fs_error(
                FS_TOO_LARGE,
                &format!("file too large: {} ({} bytes > {FS_READ_MAX_BYTES})", req.path, stat.size),
                &[("path", req.path.clone().into()), ("size", stat.size.into())],
            ));
        }
        let sample_size = (FS_BINARY_SAMPLE_BYTES as u64).min(stat.size);
        let raw_length = stat.size.saturating_sub(req.offset).min(req.length);
        let raw_window = if raw_length == 0 {
            Vec::new()
        } else {
            self.host_fs.read_bytes(&abs, raw_length, Some(req.offset)).await?
        };
        let sample = if req.offset == 0 && raw_length >= sample_size {
            raw_window[..sample_size as usize].to_vec()
        } else if sample_size == 0 {
            Vec::new()
        } else {
            self.host_fs.read_bytes(&abs, sample_size, None).await?
        };
        let classification = classify_text_sample(&sample);
        let transcode_encoding = (!classification.is_binary
            && classification.encoding != UtfTextEncoding::Utf8
            && req.encoding != FsReadEncodingRequest::Base64)
            .then_some(classification.encoding);
        let is_binary = classification.is_binary
            || (classification.encoding != UtfTextEncoding::Utf8 && transcode_encoding.is_none());
        if is_binary && req.encoding == FsReadEncodingRequest::Utf8 {
            return Err(fs_error(FS_IS_BINARY, &format!("file is binary: {}", req.path), &[("path", req.path.clone().into())]));
        }
        let mut total_length = stat.size;
        let decoded = if let Some(encoding) = transcode_encoding {
            let decoded = decode_utf_text(&self.host_fs.read_bytes(&abs, stat.size, None).await?, encoding).into_bytes();
            total_length = decoded.len() as u64;
            Some(decoded)
        } else {
            None
        };
        let effective_length = total_length.saturating_sub(req.offset).min(req.length);
        let bytes = if effective_length == 0 {
            Vec::new()
        } else if let Some(decoded) = decoded {
            decoded[req.offset as usize..(req.offset + effective_length) as usize].to_vec()
        } else {
            raw_window[..effective_length as usize].to_vec()
        };
        let encoding = if req.encoding == FsReadEncodingRequest::Base64
            || (req.encoding == FsReadEncodingRequest::Auto && is_binary)
        {
            FsReadEncoding::Base64
        } else {
            FsReadEncoding::Utf8
        };
        let content = match encoding {
            FsReadEncoding::Utf8 => String::from_utf8_lossy(&bytes).into_owned(),
            FsReadEncoding::Base64 => base64::engine::general_purpose::STANDARD.encode(bytes),
        };
        let truncated = req.offset + effective_length < total_length;
        let language_id = (encoding == FsReadEncoding::Utf8).then(|| guess_language_id(&rel)).flatten();
        let line_count = (encoding == FsReadEncoding::Utf8).then(|| count_lines(&content) as u64);
        Ok(FsReadResponse {
            path: rel.clone(),
            content,
            encoding,
            size: stat.size,
            truncated,
            etag: build_etag(&FileMetaStat::from(&stat)),
            mime: guess_mime(&rel, is_binary),
            language_id: language_id.map(str::to_string),
            line_count,
            is_binary,
        })
    }

    pub async fn list_many(&self, req: FsListManyRequest) -> FsResult<FsListManyResponse> {
        let futures = req
            .paths
            .iter()
            .map(|path| {
                let request = FsListRequest {
                    path: path.clone(),
                    depth: req.depth,
                    limit: req.limit,
                    show_hidden: req.show_hidden,
                    follow_gitignore: req.follow_gitignore,
                    exclude_globs: req.exclude_globs.clone(),
                    allow_ignored_globs: None,
                    sort: req.sort,
                    include_git_status: req.include_git_status,
                    allow_external_symlinks: None,
                };
                async move { (path.clone(), self.list(request).await) }
            })
            .collect::<futures_util::stream::FuturesUnordered<_>>();
        futures_util::pin_mut!(futures);
        let mut results = HashMap::new();
        let mut partial_errors = HashMap::new();
        let mut truncated_paths = Vec::new();
        while let Some((path, result)) = futures.next().await {
            match result {
                Ok(response) => {
                    results.insert(path.clone(), response.items);
                    if response.truncated {
                        truncated_paths.push(path);
                    }
                }
                Err(error) if error_code(error.as_ref()) == Some(FS_PATH_ESCAPES) => return Err(error),
                Err(error) => {
                    partial_errors.insert(path, to_wire_error(error.as_ref()));
                }
            }
        }
        Ok(FsListManyResponse {
            results,
            truncated_paths: (!truncated_paths.is_empty()).then_some(truncated_paths),
            partial_errors: (!partial_errors.is_empty()).then_some(partial_errors),
        })
    }

    pub async fn stat(&self, req: FsStatRequest) -> FsResult<FsStatResponse> {
        let abs = self.resolve_within(&req.path, false).await?;
        let rel = self.to_rel(&abs);
        let stat = self.host_fs.lstat(&abs).await.map_err(|error| map_fs_error(error, &req.path))?;
        let name = if rel == "." { self.path.basename(&self.work_dir()) } else { self.path.basename(&abs) };
        Ok(build_fs_entry(&rel, &name, &stat, true))
    }

    pub async fn stat_many(&self, req: FsStatManyRequest) -> FsResult<FsStatManyResponse> {
        let mut resolved = Vec::new();
        for path in &req.paths {
            let abs = self.resolve_within(path, false).await?;
            resolved.push((path.clone(), self.to_rel(&abs), abs));
        }
        let mut entries = HashMap::new();
        for (raw, rel, abs) in resolved {
            let entry = match self.host_fs.lstat(&abs).await {
                Ok(stat) => {
                    let name = if rel == "." { self.path.basename(&self.work_dir()) } else { self.path.basename(&abs) };
                    Some(build_fs_entry(&rel, &name, &stat, false))
                }
                Err(_) => None,
            };
            entries.insert(raw, entry);
        }
        Ok(FsStatManyResponse { entries })
    }

    pub async fn mkdir(&self, req: FsMkdirRequest) -> FsResult<FsMkdirResponse> {
        let abs = self.resolve_within(&req.path, false).await?;
        let rel = self.to_rel(&abs);
        if let Err(error) = self.host_fs.mkdir(&abs, req.recursive).await {
            if errno_code(error.as_ref()) == Some("EEXIST") {
                return Err(fs_error(FS_ALREADY_EXISTS, &format!("path already exists: {}", req.path), &[("path", req.path.clone().into())]));
            }
            if is_host_fs_not_found(&error) || is_host_fs_not_directory(&error) {
                return Err(fs_error(FS_PATH_NOT_FOUND, &format!("parent not found: {}", req.path), &[("path", req.path.clone().into())]));
            }
            return Err(error);
        }
        let stat = self.host_fs.lstat(&abs).await?;
        Ok(build_fs_entry(&rel, &self.path.basename(&abs), &stat, false))
    }

    pub async fn resolve_path(&self, rel_path: &str) -> FsResult<FsPathResolved> {
        let abs = self.resolve_within(rel_path, false).await?;
        let rel = self.to_rel(&abs);
        let stat = self.host_fs.lstat(&abs).await.map_err(|error| map_fs_error(error, rel_path))?;
        Ok(FsPathResolved {
            absolute: abs,
            relative: rel,
            is_directory: stat.is_directory,
        })
    }

    pub async fn resolve_download(&self, rel_path: &str, allow_external_symlinks: bool) -> FsResult<FsDownloadResolved> {
        let abs = self.resolve_within(rel_path, allow_external_symlinks).await?;
        let rel = self.to_rel(&abs);
        let stat = self.host_fs.stat(&abs).await.map_err(|error| map_fs_error(error, rel_path))?;
        if stat.is_directory {
            return Err(fs_error(FS_IS_DIRECTORY, &format!("path is a directory: {rel_path}"), &[("path", rel_path.into())]));
        }
        let sample_size = (FS_BINARY_SAMPLE_BYTES as u64).min(stat.size);
        let sample = if sample_size == 0 { Vec::new() } else { self.host_fs.read_bytes(&abs, sample_size, None).await? };
        let classification = classify_text_sample(&sample);
        let is_binary = classification.is_binary || classification.encoding != UtfTextEncoding::Utf8;
        Ok(FsDownloadResolved {
            absolute: abs,
            relative: rel.clone(),
            size: stat.size,
            etag: build_etag(&FileMetaStat::from(&stat)),
            mime: guess_mime(&rel, is_binary),
            modified_at: chrono::DateTime::from_timestamp_millis((stat.mtime_ms.unwrap_or(0.0)) as i64).unwrap(),
        })
    }

    pub async fn search(&self, req: FsSearchRequest) -> FsResult<FsSearchResponse> {
        let query_lower = req.query.to_lowercase();
        let roots = self.suggest_roots().await?;
        if roots.is_empty() {
            return Ok(FsSearchResponse { items: Vec::new(), truncated: false });
        }
        if req.query.is_empty() {
            let suggest = FsSuggestRequest {
                query: String::new(),
                limit: req.limit,
                follow_gitignore: req.follow_gitignore,
                show_hidden: false,
                include_globs: req.include_globs.clone(),
                exclude_globs: req.exclude_globs.clone(),
            };
            return self.suggest_top_level(&suggest, &roots, true).await;
        }
        let direct = self.direct_suggest_targets(&roots, &req.query, None).await;
        let candidates = Arc::new(Mutex::new(Vec::<SearchCandidate>::new()));
        let candidate_paths = Arc::new(Mutex::new(HashSet::new()));
        let seen_paths = Arc::new(Mutex::new(HashSet::new()));
        let effective_cap = req.limit.min(SEARCH_HARD_CAP);
        let inventory_truncated = Arc::new(AtomicBool::new(false));
        for ignored in [false, true] {
            if ignored && (req.follow_gitignore || candidates.lock().iter().filter(|candidate| candidate.tier == 3).count() > effective_cap) {
                break;
            }
            for root in &roots {
                let ignore_rules = if ignored {
                    Some(VCS_METADATA_DIRS.iter().map(|dir| dir.to_string()).collect::<Vec<_>>())
                } else {
                    Some(self.walk_ignore_for(&root.dir).await?)
                };
                let candidates_sink = candidates.clone();
                let candidate_paths_sink = candidate_paths.clone();
                let seen_paths_sink = seen_paths.clone();
                let root_for_dir = root.clone();
                let root = root.clone();
                let query_lower = query_lower.clone();
                let include_globs = req.include_globs.clone();
                let exclude_globs = req.exclude_globs.clone();
                let path = self.path.clone();
                let walk_dir = root_for_dir.dir.clone();
                let truncated = self.walk(&walk_dir, ignore_rules.as_deref(), move |rel_path, name, kind| {
                    let candidates = candidates_sink.clone();
                    let candidate_paths = candidate_paths_sink.clone();
                    let seen_paths = seen_paths_sink.clone();
                    let root = root.clone();
                    let query_lower = query_lower.clone();
                    let include_globs = include_globs.clone();
                    let exclude_globs = exclude_globs.clone();
                    let path = path.clone();
                    async move {
                        let path_key = suggest_path_key(&root, &rel_path, true);
                        if !seen_paths.lock().insert(path_key) {
                            return Ok(false);
                        }
                        let score = compute_fuzzy_score(&name, &query_lower);
                        if score <= 0.0 {
                            return Ok(false);
                        }
                        if include_globs.as_ref().is_some_and(|globs| !matches_any_glob(&rel_path, globs))
                            || exclude_globs.as_ref().is_some_and(|globs| matches_any_glob(&rel_path, globs))
                        {
                            return Ok(false);
                        }
                        let name_lower = name.to_lowercase();
                        let tier = if name_lower == query_lower { 3 } else if name_lower.starts_with(&query_lower) { 2 } else { 1 };
                        let display = suggest_display_path(&path, &root, &rel_path);
                        candidate_paths.lock().insert(display.clone());
                        candidates.lock().push(SearchCandidate {
                            hit: FsSearchHit {
                                path: display.clone(),
                                name,
                                kind,
                                score,
                                match_positions: compute_match_positions(&display, &query_lower),
                            },
                            ignored,
                            tier,
                        });
                        Ok(false)
                    }
                    .boxed()
                }, None).await?;
                if truncated {
                    inventory_truncated.store(true, Ordering::Relaxed);
                }
            }
        }
        let ordinary_direct = direct.iter().filter(|target| {
            seen_paths.lock().contains(&suggest_path_key(&target.root, &target.rel_path, true))
        }).map(|target| (target.root.real.clone(), target.rel_path.clone())).collect::<HashSet<_>>();
        for target in &direct {
            if req.follow_gitignore && !ordinary_direct.contains(&(target.root.real.clone(), target.rel_path.clone())) {
                continue;
            }
            let display = suggest_display_path(&self.path, &target.root, &target.rel_path);
            if candidate_paths.lock().contains(&display) {
                continue;
            }
            if req.include_globs.as_ref().is_some_and(|globs| !matches_any_glob(&target.rel_path, globs))
                || req.exclude_globs.as_ref().is_some_and(|globs| matches_any_glob(&target.rel_path, globs))
            {
                continue;
            }
            candidate_paths.lock().insert(display.clone());
            candidates.lock().push(SearchCandidate {
                hit: FsSearchHit {
                    path: display.clone(),
                    name: target.rel_path.rsplit('/').next().unwrap_or(&target.rel_path).to_string(),
                    kind: target.kind,
                    score: 1.0,
                    match_positions: compute_match_positions(&display, &query_lower),
                },
                ignored: !ordinary_direct.contains(&(target.root.real.clone(), target.rel_path.clone())),
                tier: 3,
            });
        }
        let mut candidates = Arc::try_unwrap(candidates).unwrap().into_inner();
        candidates.sort_by(|a, b| {
            b.tier.cmp(&a.tier)
                .then_with(|| a.ignored.cmp(&b.ignored))
                .then_with(|| b.hit.score.partial_cmp(&a.hit.score).unwrap())
                .then_with(|| locale_compare(&a.hit.path, &b.hit.path))
        });
        let truncated = candidates.len() > effective_cap || inventory_truncated.load(Ordering::Relaxed);
        Ok(FsSearchResponse {
            items: candidates.into_iter().take(effective_cap).map(|candidate| candidate.hit).collect(),
            truncated,
        })
    }

    pub async fn suggest(&self, req: FsSuggestRequest, external_signal: Option<AbortSignal>) -> FsResult<FsSuggestResponse> {
        let query_lower = req.query.to_lowercase();
        let path_segments = if query_lower.contains('/') {
            query_lower.split('/').filter(|segment| !segment.is_empty()).map(str::to_string).collect()
        } else {
            Vec::new()
        };
        let query = SuggestQuery {
            name_query: query_lower.clone(),
            path_segments,
            show_hidden: req.show_hidden || query_lower.starts_with('.'),
            follow_gitignore: req.follow_gitignore,
            include_globs: req.include_globs.clone(),
            exclude_globs: req.exclude_globs.clone(),
        };
        let deadline = now_ms() + SUGGEST_TIMEOUT_MS as i64;
        let (signal, _timer) = timeout_signal(external_signal, SUGGEST_TIMEOUT_MS);
        let roots = self.suggest_roots().await?;
        if roots.is_empty() {
            return Ok(FsSuggestResponse { items: Vec::new(), truncated: false });
        }
        if let Some(fd) = self.resolve_fd(&signal).await? {
            let lease = self.resolver.acquire(&self.environment_binding(), &[EnvironmentCapability::Process])?;
            let result = suggest_with_fd(FdSuggestOptions {
                fd_binary: fd.path,
                query: req.query.clone(),
                cap: req.limit.min(20),
                follow_gitignore: req.follow_gitignore,
                include_globs: req.include_globs.clone(),
                exclude_globs: req.exclude_globs.clone(),
                roots: roots.iter().cloned().map(|root| FdRoot { dir: root.dir, real: root.real, primary: root.primary }).collect(),
                path: self.path.clone(),
                host_fs: self.host_fs.clone(),
                process: lease.environment.process().clone(),
                work_dir: self.work_dir(),
                home_dir: lease.environment.host().and_then(|host| Some(host.home_dir.clone())),
                signal: signal.clone(),
            }).await;
            drop(lease);
            match result {
                Ok(response) => return Ok(response),
                Err(error) if signal.aborted() => return Err(error),
                Err(_) => {}
            }
        }
        if req.query.is_empty() {
            return self.suggest_top_level(&req, &roots, false).await;
        }
        if query_lower.contains('/') && query.path_segments.is_empty() {
            return Ok(FsSuggestResponse { items: Vec::new(), truncated: false });
        }
        let direct = self.direct_suggest_targets(&roots, &req.query, Some((&signal, deadline))).await;
        let resolution = self.resolve_rg().await.unwrap_or(None);
        if let Some(resolution) = resolution {
            match self.suggest_with_rg(&query, req.limit, &signal, &resolution.path, &roots, &direct).await {
                Ok(response) => return Ok(response),
                Err(error) if signal.aborted() => return Err(error),
                Err(_) => {
                    self.telemetry.track2("fs_suggest_node_fallback", Some(serde_json::Map::from_iter([("reason".to_string(), "rg_error".into())])));
                    return self.suggest_with_node(&query, req.limit, &signal, &roots, &direct, deadline).await;
                }
            }
        }
        self.telemetry.track2("fs_suggest_node_fallback", Some(serde_json::Map::from_iter([("reason".to_string(), "rg_missing".into())])));
        self.suggest_with_node(&query, req.limit, &signal, &roots, &direct, deadline).await
    }

    pub fn suggest_stream(&self, req: FsSuggestRequest, external_signal: Option<AbortSignal>) -> SuggestEventStream {
        let this = self.clone_handle();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            let result = async {
                let (signal, _timer) = timeout_signal(external_signal.clone(), SUGGEST_TIMEOUT_MS);
                let roots = this.suggest_roots().await?;
                if roots.is_empty() {
                    sender.send(Ok(FsSuggestStreamEvent { phase: FsSuggestStreamPhase::Final, response: FsSuggestResponse { items: Vec::new(), truncated: false } })).await.ok();
                    return Ok(());
                }
                if let Some(fd) = this.resolve_fd(&signal).await? {
                    let lease = this.resolver.acquire(&this.environment_binding(), &[EnvironmentCapability::Process])?;
                    let events = suggest_with_fd_events(FdSuggestOptions {
                        fd_binary: fd.path,
                        query: req.query.clone(),
                        cap: req.limit.min(20),
                        follow_gitignore: req.follow_gitignore,
                        include_globs: req.include_globs.clone(),
                        exclude_globs: req.exclude_globs.clone(),
                        roots: roots.iter().cloned().map(|root| FdRoot { dir: root.dir, real: root.real, primary: root.primary }).collect(),
                        path: this.path.clone(),
                        host_fs: this.host_fs.clone(),
                        process: lease.environment.process().clone(),
                        work_dir: this.work_dir(),
                        home_dir: lease.environment.host().and_then(|host| Some(host.home_dir.clone())),
                        signal: signal.clone(),
                    });
                    futures_util::pin_mut!(events);
                    let mut failed = None;
                    while let Some(event) = events.next().await {
                        match event {
                            Ok(event) => {
                                if sender.send(Ok(event)).await.is_err() {
                                    drop(lease);
                                    return Ok(());
                                }
                            }
                            Err(error) => {
                                failed = Some(error);
                                break;
                            }
                        }
                    }
                    drop(lease);
                    if let Some(error) = failed {
                        if signal.aborted() {
                            return Err(error);
                        }
                    } else {
                        return Ok(());
                    }
                }
                let response = this.suggest(req, external_signal).await?;
                sender.send(Ok(FsSuggestStreamEvent { phase: FsSuggestStreamPhase::Final, response })).await.ok();
                Ok(())
            }
            .await;
            if let Err(error) = result {
                sender.send(Err(error)).await.ok();
            }
        });
        Box::pin(stream::poll_fn(move |context| receiver.poll_recv(context)))
    }

    fn clone_handle(&self) -> Self {
        Self {
            workspace_dirs: self.workspace_dirs.clone(),
            host_fs: self.host_fs.clone(),
            resolver: self.resolver.clone(),
            telemetry: self.telemetry.clone(),
            git: self.git.clone(),
            environment_id: self.environment_id.clone(),
            workspace_id: self.workspace_id.clone(),
            path: self.path.clone(),
            caches: self.caches.clone(),
        }
    }

    async fn suggest_roots(&self) -> FsResult<Vec<SuggestRoot>> {
        let pairs = self.real_root_pairs().await;
        let mut roots = Vec::new();
        for (index, (dir, real)) in pairs.into_iter().enumerate() {
            if roots.iter().any(|root: &SuggestRoot| is_inside_or_equal(&self.path, &real, &root.real)) {
                continue;
            }
            roots.push(SuggestRoot { dir, real, primary: index == 0 });
        }
        Ok(roots)
    }

    async fn direct_suggest_targets(
        &self,
        roots: &[SuggestRoot],
        input: &str,
        budget: Option<(&AbortSignal, i64)>,
    ) -> Vec<SuggestDirectTarget> {
        if !input.contains('/') || self.path.is_absolute(input) {
            return Vec::new();
        }
        let segments = input.split('/').filter(|segment| !segment.is_empty()).collect::<Vec<_>>();
        if segments.is_empty()
            || segments.iter().any(|segment| {
                *segment == "." || *segment == ".." || VCS_METADATA_DIRS.contains(segment.to_lowercase().as_str())
            })
        {
            return Vec::new();
        }
        let expired = || budget.is_some_and(|(signal, deadline)| signal.aborted() || now_ms() >= deadline);
        let mut targets = Vec::new();
        for root in roots {
            if expired() {
                break;
            }
            let rel_path = segments.join("/");
            let candidate = self.path.resolve(&root.dir, &rel_path);
            if !is_inside_or_equal(&self.path, &candidate, &root.dir) {
                continue;
            }
            let Ok(stat) = self.host_fs.lstat(&candidate).await else {
                continue;
            };
            if expired() {
                break;
            }
            let Ok(real_path) = self.realpath_existing_prefix(&candidate).await else {
                continue;
            };
            if expired() {
                break;
            }
            if !roots.iter().any(|configured| is_inside_or_equal(&self.path, &real_path, &configured.real)) {
                continue;
            }
            targets.push(SuggestDirectTarget {
                root: root.clone(),
                rel_path,
                kind: if stat.is_symbolic_link { FsKind::Symlink } else if stat.is_directory { FsKind::Directory } else { FsKind::File },
            });
        }
        targets
    }

    async fn suggest_top_level(&self, req: &FsSuggestRequest, roots: &[SuggestRoot], follow_symlink_directories: bool) -> FsResult<FsSuggestResponse> {
        let mut all = Vec::new();
        let mut capped = false;
        for root in roots {
            let matcher = self.matcher_for(&root.dir).await?;
            let entries = self.host_fs.readdir(&root.dir).await.map_err(|error| map_fs_error(error, &root.dir))?;
            let mut visible = Vec::new();
            for entry in entries {
                let name = entry.name;
                if !req.show_hidden && is_hidden(&name) || VCS_METADATA_DIRS.contains(name.as_str()) {
                    continue;
                }
                let ignored = matcher.as_ref().is_some_and(|matcher| matcher.ignores(&name) || matcher.ignores(&format!("{name}/")));
                if req.follow_gitignore && ignored
                    || req.exclude_globs.as_ref().is_some_and(|globs| matches_any_glob(&name, globs))
                    || req.include_globs.as_ref().is_some_and(|globs| !matches_any_glob(&name, globs))
                {
                    continue;
                }
                let kind = if entry.is_symbolic_link { FsKind::Symlink } else if entry.is_directory { FsKind::Directory } else { FsKind::File };
                let is_directory = entry.is_directory
                    || (follow_symlink_directories
                        && entry.is_symbolic_link
                        && self.host_fs.stat(&self.path.join(&root.dir, &[&name])).await.ok().is_some_and(|stat| stat.is_directory));
                visible.push(TopEntry { name, kind, is_directory, ignored });
            }
            visible.sort_by(|a, b| {
                a.ignored.cmp(&b.ignored)
                    .then_with(|| b.is_directory.cmp(&a.is_directory))
                    .then_with(|| locale_compare(&a.name, &b.name))
            });
            if visible.len() > SEARCH_HARD_CAP {
                visible.truncate(SEARCH_HARD_CAP);
                capped = true;
            }
            for entry in visible {
                all.push(TopDisplayEntry {
                    path: suggest_display_path(&self.path, root, &entry.name),
                    name: entry.name,
                    kind: entry.kind,
                    ignored: entry.ignored,
                });
            }
        }
        all.sort_by(|a, b| a.ignored.cmp(&b.ignored));
        let truncated = capped || all.len() > req.limit;
        Ok(FsSearchResponse {
            items: all.into_iter().take(req.limit).map(|entry| FsSearchHit {
                path: entry.path,
                name: entry.name,
                kind: entry.kind,
                score: 1.0,
                match_positions: Vec::new(),
            }).collect(),
            truncated,
        })
    }

    async fn suggest_with_rg(
        &self,
        query: &SuggestQuery,
        cap: usize,
        signal: &AbortSignal,
        rg_binary: &str,
        roots: &[SuggestRoot],
        direct: &[SuggestDirectTarget],
    ) -> FsResult<FsSuggestResponse> {
        let multi = roots.len() > 1;
        let top = Arc::new(Mutex::new(SuggestTopHeap::new(cap)));
        let seen_dirs = Arc::new(Mutex::new(HashSet::new()));
        let seen_paths = Arc::new(Mutex::new(HashSet::new()));
        let matched = Arc::new(AtomicUsize::new(0));
        for ignored in [false, true] {
            if ignored && (query.follow_gitignore || (matched.load(Ordering::Relaxed) > cap && top.lock().worst().is_some_and(|candidate| candidate.tier == 3)) || signal.aborted()) {
                break;
            }
            let mut args = vec!["--files".to_string(), "--no-require-git".to_string()];
            if ignored {
                args.push("--no-ignore".to_string());
            }
            if query.show_hidden {
                args.push("--hidden".to_string());
            }
            if ignored && query.path_segments.is_empty() {
                let glob = suggest_subsequence_glob(&query.name_query);
                args.extend(["--iglob".to_string(), glob.clone(), "--iglob".to_string(), format!("{glob}/**")]);
            }
            for dir in VCS_METADATA_DIRS.iter() {
                args.extend(["-g".to_string(), format!("!{dir}"), "-g".to_string(), format!("!{dir}/**")]);
            }
            if multi {
                args.extend(roots.iter().map(|root| root.dir.clone()));
            }
            let top_sink = top.clone();
            let seen_dirs_sink = seen_dirs.clone();
            let seen_paths_sink = seen_paths.clone();
            let matched_sink = matched.clone();
            let roots = roots.to_vec();
            let query = query.clone();
            let path = self.path.clone();
            let (exit_code, killed) = self.run_rg(&args, rg_binary, signal, move |line| {
                let mut line = line.trim_end_matches('\r').to_string();
                if path.separator != "/" {
                    line = line.replace(&path.separator, "/");
                }
                if line.starts_with("./") {
                    line = line[2..].to_string();
                }
                if line.is_empty() {
                    return;
                }
                let (root, rel) = if multi {
                    let Some((root, rel)) = match_suggest_root(&path, &roots, &line) else {
                        return;
                    };
                    (root, rel)
                } else {
                    (roots[0].clone(), line.clone())
                };
                let path_key = suggest_path_key(&root, &rel, multi);
                if !seen_paths_sink.lock().insert(path_key) {
                    return;
                }
                if let Some(file) = evaluate_suggest_candidate(&rel, FsKind::File, &query, ignored) {
                    matched_sink.fetch_add(1, Ordering::Relaxed);
                    top_sink.lock().push(display_candidate(&path, &root, file));
                }
                let mut slash = rel.rfind('/');
                while let Some(index) = slash.filter(|index| *index > 0) {
                    let dir = &rel[..index];
                    let dir_key = suggest_path_key(&root, dir, multi);
                    if seen_dirs_sink.lock().insert(dir_key) {
                        if let Some(candidate) = evaluate_suggest_candidate(dir, FsKind::Directory, &query, ignored) {
                            matched_sink.fetch_add(1, Ordering::Relaxed);
                            top_sink.lock().push(display_candidate(&path, &root, candidate));
                        }
                    }
                    slash = rel[..index].rfind('/');
                }
            }).await?;
            if !killed && exit_code != 0 && exit_code != 1 {
                return Err(format!("rg --files exited with code {exit_code}").into());
            }
        }
        for target in direct {
            if query.follow_gitignore {
                continue;
            }
            let path_key = suggest_path_key(&target.root, &target.rel_path, multi);
            if seen_paths.lock().contains(&path_key) || seen_dirs.lock().contains(&path_key) {
                continue;
            }
            let Some(candidate) = evaluate_suggest_candidate(&target.rel_path, target.kind, query, true) else {
                continue;
            };
            seen_paths.lock().insert(path_key);
            matched.fetch_add(1, Ordering::Relaxed);
            top.lock().push(display_candidate(&self.path, &target.root, candidate));
        }
        let items = Arc::try_unwrap(top).unwrap().into_inner().drain().into_iter().map(|candidate| FsSearchHit {
            path: candidate.path,
            name: candidate.name,
            kind: candidate.kind,
            score: candidate.score,
            match_positions: candidate.positions,
        }).collect();
        Ok(FsSuggestResponse {
            items,
            truncated: matched.load(Ordering::Relaxed) > cap || signal.aborted(),
        })
    }

    async fn suggest_with_node(
        &self,
        query: &SuggestQuery,
        cap: usize,
        signal: &AbortSignal,
        roots: &[SuggestRoot],
        direct: &[SuggestDirectTarget],
        deadline: i64,
    ) -> FsResult<FsSuggestResponse> {
        let multi = roots.len() > 1;
        let top = Arc::new(Mutex::new(SuggestTopHeap::new(cap)));
        let seen_paths = Arc::new(Mutex::new(HashSet::new()));
        let matched = Arc::new(AtomicUsize::new(0));
        let inventory_truncated = Arc::new(AtomicBool::new(false));
        for ignored in [false, true] {
            if ignored && (query.follow_gitignore || (matched.load(Ordering::Relaxed) > cap && top.lock().worst().is_some_and(|candidate| candidate.tier == 3)) || signal.aborted() || now_ms() >= deadline) {
                break;
            }
            for root in roots {
                let ignore_rules = if ignored { VCS_METADATA_DIRS.iter().map(|dir| dir.to_string()).collect() } else { self.walk_ignore_for(&root.dir).await? };
                let top_sink = top.clone();
                let seen_paths_sink = seen_paths.clone();
                let matched_sink = matched.clone();
                let root = root.clone();
                let query = query.clone();
                let path = self.path.clone();
                let signal = signal.clone();
                let walk_dir = root.dir.clone();
                let signal_sink = signal.clone();
                let truncated = self.walk(&walk_dir, Some(&ignore_rules), move |rel_path, _, kind| {
                    let top = top_sink.clone();
                    let seen_paths = seen_paths_sink.clone();
                    let matched = matched_sink.clone();
                    let root = root.clone();
                    let query = query.clone();
                    let path = path.clone();
                    let signal = signal_sink.clone();
                    async move {
                        if signal.aborted() || now_ms() >= deadline {
                            return Ok(true);
                        }
                        let path_key = suggest_path_key(&root, &rel_path, multi);
                        if !seen_paths.lock().insert(path_key) {
                            return Ok(false);
                        }
                        if let Some(candidate) = evaluate_suggest_candidate(&rel_path, kind, &query, ignored) {
                            matched.fetch_add(1, Ordering::Relaxed);
                            top.lock().push(display_candidate(&path, &root, candidate));
                        }
                        Ok(false)
                    }
                    .boxed()
                }, Some(deadline)).await?;
                if truncated {
                    inventory_truncated.store(true, Ordering::Relaxed);
                }
                if signal.aborted() || now_ms() >= deadline {
                    break;
                }
            }
        }
        for target in direct {
            if query.follow_gitignore {
                continue;
            }
            let path_key = suggest_path_key(&target.root, &target.rel_path, multi);
            if seen_paths.lock().contains(&path_key) {
                continue;
            }
            let Some(candidate) = evaluate_suggest_candidate(&target.rel_path, target.kind, query, true) else {
                continue;
            };
            seen_paths.lock().insert(path_key);
            matched.fetch_add(1, Ordering::Relaxed);
            top.lock().push(display_candidate(&self.path, &target.root, candidate));
        }
        let items = Arc::try_unwrap(top).unwrap().into_inner().drain().into_iter().map(|candidate| FsSearchHit {
            path: candidate.path,
            name: candidate.name,
            kind: candidate.kind,
            score: candidate.score,
            match_positions: candidate.positions,
        }).collect();
        Ok(FsSuggestResponse {
            items,
            truncated: matched.load(Ordering::Relaxed) > cap || signal.aborted() || inventory_truncated.load(Ordering::Relaxed),
        })
    }

    async fn run_rg(
        &self,
        args: &[String],
        rg_binary: &str,
        signal: &AbortSignal,
        on_line: impl Fn(String) + Send + Sync + 'static,
    ) -> FsResult<(i32, bool)> {
        let lease = self.resolver.acquire(&self.environment_binding(), &[EnvironmentCapability::Process])?;
        let proc = lease.environment.process().clone().spawn(rg_binary, args, Some(&self.work_dir()), None).await?;
        let killed = Arc::new(AtomicBool::new(false));
        let kill = {
            let killed = killed.clone();
            let proc = proc.clone();
            move || {
                if killed.swap(true, Ordering::Relaxed) {
                    return;
                }
                let proc = proc.clone();
                tokio::spawn(async move {
                    let _ = proc.kill("SIGKILL").await;
                });
            }
        };
        let kill_for_removal = Arc::new(kill.clone());
        if signal.aborted() {
            kill();
        } else {
            signal.add_event_listener("abort", Arc::new(kill), true);
        }
        let drain = async {
            let mut buffer = String::new();
            let mut stdout = proc.stdout();
            while let Some(chunk) = stdout.next().await {
                buffer.push_str(&chunk?);
                while let Some(index) = buffer.find('\n') {
                    let line = buffer[..index].to_string();
                    buffer = buffer[index + 1..].to_string();
                    if !line.is_empty() {
                        on_line(line);
                    }
                }
            }
            if !buffer.is_empty() {
                on_line(buffer);
            }
            Ok::<(), WireAsyncError>(())
        };
        let result = async {
            let (stdout, _, exit_code) = tokio::join!(drain, read_stream(proc.stderr()), async { proc.wait().await.unwrap_or(-1) });
            if let Err(error) = stdout {
                if !(killed.load(Ordering::Relaxed)
                    && errno_code(error.as_ref()) == Some("ERR_STREAM_PREMATURE_CLOSE"))
                {
                    return Err(error);
                }
            }
            Ok::<_, WireAsyncError>((exit_code, killed.load(Ordering::Relaxed)))
        }
        .await;
        signal.remove_event_listener("abort", kill_for_removal);
        let _ = proc.dispose().await;
        drop(lease);
        result
    }

    pub async fn grep(&self, req: FsGrepRequest) -> FsResult<FsGrepResponse> {
        let started_at = now_ms();
        let (signal, _timer) = timeout_signal(None, GREP_TIMEOUT_MS);
        if let Some(resolution) = self.resolve_rg().await.unwrap_or(None) {
            return self.grep_with_rg(&req, &signal, started_at, &resolution.path).await;
        }
        self.telemetry.track2("fs_grep_node_fallback", Some(serde_json::Map::from_iter([("reason".to_string(), "rg_missing".into())])));
        self.grep_with_node(&req, &signal, started_at).await
    }

    async fn grep_with_rg(&self, req: &FsGrepRequest, signal: &AbortSignal, started_at: i64, rg_binary: &str) -> FsResult<FsGrepResponse> {
        let mut args = vec!["--json".to_string()];
        if req.context_lines > 0 {
            args.extend(["--context".to_string(), req.context_lines.to_string()]);
        }
        if !req.case_sensitive {
            args.push("--ignore-case".to_string());
        }
        if !req.regex {
            args.push("--fixed-strings".to_string());
        }
        args.push(if req.follow_gitignore { "--no-require-git" } else { "--no-ignore" }.to_string());
        if let Some(globs) = &req.include_globs {
            for glob in globs {
                args.extend(["--glob".to_string(), glob.clone()]);
            }
        }
        if let Some(globs) = &req.exclude_globs {
            for glob in globs {
                args.extend(["--glob".to_string(), format!("!{glob}")]);
            }
        }
        args.extend(["--max-count".to_string(), req.max_matches_per_file.to_string(), req.pattern.clone()]);
        let roots = self.suggest_roots().await?;
        args.push(".".to_string());
        args.extend(roots.iter().filter(|root| !root.primary).map(|root| root.dir.clone()));
        let accumulator = Arc::new(Mutex::new(RgJsonAccumulator::new(req.clone())));
        let accumulator_sink = accumulator.clone();
        let path = self.path.clone();
        let work_dir = self.work_dir();
        self.run_rg(&args, rg_binary, signal, move |line| {
            accumulator_sink.lock().feed(&line, &path, &work_dir);
        }).await?;
        Arc::try_unwrap(accumulator).unwrap().into_inner().finish(signal.aborted(), now_ms() - started_at)
    }

    async fn grep_with_node(&self, req: &FsGrepRequest, signal: &AbortSignal, started_at: i64) -> FsResult<FsGrepResponse> {
        let mut pattern = compile_grep_pattern(req).map_err(|error| -> WireAsyncError { Box::new(error) })?;
        let mut files: Vec<FsGrepFileHit> = Vec::new();
        let mut files_scanned = 0;
        let mut total_matches = 0;
        let mut truncated = false;
        let mut file_paths = Vec::new();
        let mut seen_paths: HashSet<String> = HashSet::new();
        let roots = self.suggest_roots().await?;
        let deadline = started_at + GREP_TIMEOUT_MS as i64;
        for root in &roots {
            let ignore_rules = if req.follow_gitignore { self.walk_ignore_for(&root.dir).await? } else { VCS_METADATA_DIRS.iter().map(|dir| dir.to_string()).collect() };
            let root = root.clone();
            let include_globs = req.include_globs.clone();
            let exclude_globs = req.exclude_globs.clone();
            let path = self.path.clone();
            let seen_paths_arc = Arc::new(Mutex::new(std::mem::take(&mut seen_paths)));
            let file_paths_shared = Arc::new(Mutex::new(Vec::new()));
            let seen_sink = seen_paths_arc.clone();
            let files_sink = file_paths_shared.clone();
            let walk_dir = root.dir.clone();
            let truncated_walk = self.walk(&walk_dir, Some(&ignore_rules), move |rel, _, kind| {
                let seen = seen_sink.clone();
                let files = files_sink.clone();
                let root = root.clone();
                let include_globs = include_globs.clone();
                let exclude_globs = exclude_globs.clone();
                let path = path.clone();
                async move {
                    if kind != FsKind::File {
                        return Ok(false);
                    }
                    if include_globs.as_ref().is_some_and(|globs| !matches_any_glob(&rel, globs))
                        || exclude_globs.as_ref().is_some_and(|globs| matches_any_glob(&rel, globs))
                    {
                        return Ok(false);
                    }
                    let path_key = format!("{}/{}", root.real, rel);
                    if !seen.lock().insert(path_key) {
                        return Ok(false);
                    }
                    files.lock().push((suggest_display_path(&path, &root, &rel), path.join(&root.dir, &[&rel])));
                    Ok(false)
                }
                .boxed()
            }, Some(deadline)).await?;
            seen_paths = Arc::try_unwrap(seen_paths_arc).unwrap().into_inner();
            file_paths.extend(Arc::try_unwrap(file_paths_shared).unwrap().into_inner());
            truncated |= truncated_walk;
            if signal.aborted() || now_ms() >= deadline {
                break;
            }
        }
        if signal.aborted() || now_ms() >= deadline {
            return Err(fs_error(FS_GREP_TIMEOUT, &format!("grep timed out after {}ms", now_ms() - started_at), &[]));
        }
        for (display, absolute) in file_paths {
            if signal.aborted() || now_ms() >= deadline {
                if total_matches == 0 && files_scanned == 0 {
                    return Err(fs_error(FS_GREP_TIMEOUT, &format!("grep timed out after {}ms", now_ms() - started_at), &[]));
                }
                truncated = true;
                break;
            }
            if files_scanned >= req.max_files {
                truncated = true;
                break;
            }
            files_scanned += 1;
            let Ok(content) = self.host_fs.read_text(&absolute, None).await else {
                continue;
            };
            let lines = content.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line)).collect::<Vec<_>>();
            let mut matches = Vec::new();
            for (index, line) in lines.iter().enumerate() {
                let Some(found) = pattern.exec(line) else {
                    continue;
                };
                if matches.len() >= req.max_matches_per_file {
                    break;
                }
                let before = lines[index.saturating_sub(req.context_lines)..index].iter().map(|line| line.to_string()).collect();
                let after = lines[index + 1..(lines.len()).min(index + 1 + req.context_lines)].iter().map(|line| line.to_string()).collect();
                matches.push(FsGrepMatch {
                    line: index as u64 + 1,
                    col: found.index as u64 + 1,
                    text: line.to_string(),
                    before,
                    after,
                });
                total_matches += 1;
                if total_matches >= req.max_total_matches {
                    truncated = true;
                    break;
                }
            }
            if !matches.is_empty() {
                files.push(FsGrepFileHit { path: display, matches });
            }
            if total_matches >= req.max_total_matches {
                break;
            }
        }
        Ok(FsGrepResponse {
            files,
            files_scanned,
            truncated,
            elapsed_ms: (now_ms() - started_at) as u64,
        })
    }

    pub async fn git_status(&self, req: FsGitStatusRequest) -> FsResult<FsGitStatusResponse> {
        let cwd = self.resolve_git_root(req.root.as_deref()).await?;
        let mut filter = None;
        if req.paths.as_ref().is_some_and(|paths| !paths.is_empty()) {
            let mut paths_filter = HashSet::new();
            for path in req.paths.as_ref().unwrap() {
                let input_path = if req.root.is_some() && !self.path.is_absolute(path) {
                    self.path.join(&cwd, &[path])
                } else {
                    path.clone()
                };
                let abs = self.resolve_within(&input_path, false).await?;
                let rel = self.path.relative(&cwd, &abs).replace(&self.path.separator, "/");
                if req.root.is_some() && (rel == ".." || rel.starts_with("../") || self.path.is_absolute(&rel)) {
                    return Err(fs_error(FS_PATH_ESCAPES, &format!("path \"{path}\" escapes git root"), &[("cwd", cwd.clone().into()), ("path", path.clone().into())]));
                }
                paths_filter.insert(rel);
            }
            filter = Some(paths_filter);
        }
        self.git.status_at(&cwd, Some(GitStatusOptions {
            path_filter: filter,
            scope_to_cwd: req.root.is_some(),
            include_pull_request: req.include_pull_request != Some(false),
            ..Default::default()
        })).await
    }

    pub async fn git_status_batch(&self, req: FsGitStatusBatchRequest) -> FsResult<FsGitStatusBatchResponse> {
        let include = req.include_pull_request;
        let (results, errors) = self.batch_git_values(&req.roots, move |root| {
            async move {
                self.git_status(FsGitStatusRequest { root: Some(root), include_pull_request: include, paths: None }).await
            }
            .boxed()
        }).await?;
        Ok(FsGitStatusBatchResponse { results, errors })
    }

    pub async fn pull_request_batch(&self, req: FsPullRequestBatchRequest) -> FsResult<FsPullRequestBatchResponse> {
        let (results, errors) = self.batch_git_values(&req.roots, |root| {
            async move {
                self.pull_request(FsPullRequestRequest { root: Some(root) }).await
            }
            .boxed()
        }).await?;
        Ok(FsPullRequestBatchResponse { results, errors })
    }

    pub async fn pull_request(&self, req: FsPullRequestRequest) -> FsResult<Option<FsPullRequest>> {
        let cwd = self.resolve_git_root(req.root.as_deref()).await?;
        self.git.pull_request_at(&cwd).await
    }

    async fn batch_git_values<'a, T, F>(&'a self, input_roots: &[String], read: F) -> FsResult<(HashMap<String, T>, HashMap<String, FsBatchError>)>
    where
        T: Send + std::fmt::Debug + 'a,
        F: Fn(String) -> BoxFuture<'a, Result<T, WireAsyncError>> + Send + Sync + 'a,
    {
        let mut roots = input_roots.to_vec();
        let mut seen = HashSet::new();
        roots.retain(|root| seen.insert(root.clone()));
        let cursor = Arc::new(AtomicUsize::new(0));
        let results = Arc::new(Mutex::new(HashMap::new()));
        let errors = Arc::new(Mutex::new(HashMap::new()));
        let read = Arc::new(read);
        let mut workers = Vec::new();
        for _ in 0..2usize.min(roots.len()) {
            let cursor = cursor.clone();
            let roots = roots.clone();
            let results = results.clone();
            let errors = errors.clone();
            let read = read.clone();
            workers.push(async move {
                loop {
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    if index >= roots.len() {
                        break;
                    }
                    let root = roots[index].clone();
                    match read(root.clone()).await {
                        Ok(value) => {
                            results.lock().insert(root, value);
                        }
                        Err(error) => {
                            errors.lock().insert(root, to_git_batch_error(error.as_ref()));
                        }
                    }
                }
            });
        }
        futures_util::future::join_all(workers).await;
        Ok((Arc::try_unwrap(results).unwrap().into_inner(), Arc::try_unwrap(errors).unwrap().into_inner()))
    }

    pub async fn diff(&self, req: FsDiffRequest) -> FsResult<FsDiffResponse> {
        let cwd = self.resolve_git_root(req.root.as_deref()).await?;
        let input_path = if req.root.is_some() && !self.path.is_absolute(&req.path) {
            self.path.join(&cwd, &[&req.path])
        } else {
            req.path.clone()
        };
        let abs = self.resolve_within(&input_path, false).await?;
        let rel = self.path.relative(&cwd, &abs).replace(&self.path.separator, "/");
        if req.root.is_some() && (rel == ".." || rel.starts_with("../") || self.path.is_absolute(&rel)) {
            return Err(fs_error(FS_PATH_ESCAPES, &format!("path \"{}\" escapes git root", req.path), &[("cwd", cwd.into()), ("path", req.path.clone().into())]));
        }
        self.git.diff_at(&cwd, &rel, &abs).await
    }

    async fn resolve_git_root(&self, root: Option<&str>) -> FsResult<String> {
        let Some(root) = root else {
            return Ok(self.work_dir());
        };
        let cwd = if self.path.is_absolute(root) {
            self.resolve_registered_root(root).await?
        } else {
            self.resolve_within(root, false).await?
        };
        if !self.host_fs.stat(&cwd).await.ok().is_some_and(|stat| stat.is_directory) {
            return Err(fs_error(FS_GIT_UNAVAILABLE, &format!("git root is not a directory: {root}"), &[("cwd", cwd.into()), ("path", root.into())]));
        }
        Ok(cwd)
    }

    async fn resolve_registered_root(&self, root: &str) -> FsResult<String> {
        let abs = self.path.resolve_single(root);
        if !self.is_within_workspace(&abs) {
            return Err(fs_error(FS_PATH_ESCAPES, &format!("path \"{root}\" escapes workspace"), &[("path", root.into()), ("reason", "resolved_outside".into())]));
        }
        let resolved = self.realpath_existing_prefix(&abs).await?;
        let roots = self.real_roots().await;
        if !roots.iter().any(|entry| is_inside_or_equal(&self.path, &resolved, entry)) {
            return Err(fs_error(FS_PATH_ESCAPES, &format!("path \"{root}\" escapes workspace through a symlink"), &[("path", root.into()), ("reason", "symlink_outside".into())]));
        }
        Ok(abs)
    }

    async fn walk(
        &self,
        base_abs: &str,
        ignore_rules: Option<&[String]>,
        visit: impl Fn(String, String, FsKind) -> BoxFuture<'static, FsResult<bool>> + Send + Sync + 'static,
        deadline_ms: Option<i64>,
    ) -> FsResult<bool> {
        let visit = Arc::new(visit);
        let visit_entry: HostWalkVisitor = Arc::new(move |entry: HostWalkEntry| {
            let visit = visit.clone();
            let kind = match entry.kind {
                crate::os::interface::host_file_system::HostWalkKind::File => FsKind::File,
                crate::os::interface::host_file_system::HostWalkKind::Directory => FsKind::Directory,
                crate::os::interface::host_file_system::HostWalkKind::Symlink => FsKind::Symlink,
            };
            let name = entry.path.rsplit('/').next().unwrap_or(&entry.path).to_string();
            async move {
                visit(entry.path, name, kind).await.unwrap_or(true)
            }
            .boxed()
        });
        match self.host_fs.walk(base_abs, ignore_rules, deadline_ms, visit_entry).await {
            Ok(result) => Ok(result.truncated),
            Err(error) if error_code(error.as_ref()) == Some(OS_FS_DIRECTORY_TOO_LARGE) => Err(map_fs_error(error, base_abs)),
            Err(error) => Err(error),
        }
    }

    async fn matcher_for(&self, root_dir: &str) -> FsResult<Option<Arc<Gitignore>>> {
        Ok(self.ignore_for(root_dir).await?.map(|rules| rules.matcher))
    }

    async fn walk_ignore_for(&self, root_dir: &str) -> FsResult<Vec<String>> {
        Ok(self.ignore_for(root_dir).await?.map(|rules| rules.walk_rules).unwrap_or_default())
    }

    async fn ignore_for(&self, root_dir: &str) -> FsResult<Option<IgnoreRules>> {
        if let Some(cached) = self.caches.lock().gitignore.get(root_dir).cloned() {
            return Ok(Some(cached));
        }
        let contents = self.host_fs.read_text(&self.path.join(root_dir, &[".gitignore"]), None).await.unwrap_or_default();
        let mut builder = GitignoreBuilder::new(root_dir);
        for dir in VCS_METADATA_DIRS.iter() {
            builder.add_line(None, &format!("{dir}/")).unwrap();
        }
        if !contents.is_empty() {
            for line in contents.lines() {
                builder.add_line(None, line).unwrap();
            }
        }
        let matcher = Arc::new(builder.build().unwrap());
        let mut walk_rules = contents.lines().map(str::to_string).collect::<Vec<_>>();
        walk_rules.extend(VCS_METADATA_DIRS.iter().map(|dir| dir.to_string()));
        let result = IgnoreRules { matcher, walk_rules };
        self.caches.lock().gitignore.insert(root_dir.to_string(), result.clone());
        Ok(Some(result))
    }

    async fn resolve_fd(&self, signal: &AbortSignal) -> FsResult<Option<FdResolution>> {
        if let Some(cached) = self.caches.lock().fd_resolution.clone() {
            return Ok(cached);
        }
        let lease = self.resolver.acquire(&self.environment_binding(), &[EnvironmentCapability::Process])?;
        let process = lease.environment.process().clone();
        let work_dir = self.work_dir();
        let signal_for_exec = signal.clone();
        let probe = super::internal::fd_suggest::FdProbe {
            exec: Arc::new(move |args| {
                let process = process.clone();
                let work_dir = work_dir.clone();
                let signal = signal_for_exec.clone();
                async move {
                    let result = run_command(process.as_ref(), &args, RunCommandOptions {
                        cwd: Some(work_dir),
                        env: None,
                        signal: Some(signal),
                    }).await?;
                    Ok(super::internal::fd_suggest::FdProbeResult { exit_code: result.exit_code })
                }
                .boxed()
            }),
        };
        let result = ensure_fd_path(probe, Some(signal.clone()), lease.environment.clone().into()).await;
        drop(lease);
        let resolution = match result {
            Ok(resolution) => Some(resolution),
            Err(error) if signal.aborted() => return Err(error),
            Err(_) => None,
        };
        self.caches.lock().fd_resolution = Some(resolution.clone());
        Ok(resolution)
    }

    async fn resolve_rg(&self) -> FsResult<Option<RgResolution>> {
        if let Some(cached) = self.caches.lock().rg_resolution.clone() {
            return Ok(cached);
        }
        let lease = self.resolver.acquire(&self.environment_binding(), &[EnvironmentCapability::Process])?;
        let process = lease.environment.process().clone();
        let work_dir = self.work_dir();
        let probe = RgProbe {
            exec: Arc::new(move |args| {
                let process = process.clone();
                let work_dir = work_dir.clone();
                async move {
                    let result = run_command(process.as_ref(), &args, RunCommandOptions {
                        cwd: Some(work_dir),
                        env: None,
                        signal: None,
                    }).await?;
                    Ok(super::internal::rg_locator::RgProbeResult { exit_code: result.exit_code })
                }
                .boxed()
            }),
        };
        let resolution = ensure_rg_path(probe, super::internal::rg_locator::EnsureRgPathOptions {
            signal: None,
            allow_cached_fallback: true,
            environment: lease.environment.clone().into(),
        }).await.ok();
        drop(lease);
        self.caches.lock().rg_resolution = Some(resolution.clone());
        Ok(resolution)
    }

    async fn real_root_pairs(&self) -> Vec<(String, String)> {
        let mut dirs = vec![self.work_dir()];
        dirs.extend(self.workspace_dirs.additional_dirs().iter().map(|dir| self.path.resolve_single(dir)));
        let key = dirs.join("\n");
        if let Some((cached_key, roots)) = &self.caches.lock().real_roots {
            if *cached_key == key {
                return roots.clone();
            }
        }
        let mut roots = Vec::new();
        for dir in dirs {
            let real = self.host_fs.realpath(&dir).await.unwrap_or_else(|_| dir.clone());
            roots.push((dir, real));
        }
        self.caches.lock().real_roots = Some((key, roots.clone()));
        roots
    }

    async fn real_roots(&self) -> Vec<String> {
        self.real_root_pairs().await.into_iter().map(|(_, real)| real).collect()
    }

    async fn realpath_existing_prefix(&self, abs: &str) -> FsResult<String> {
        let mut tail = Vec::new();
        let mut current = abs.to_string();
        for _ in 0..256 {
            match self.host_fs.realpath(&current).await {
                Ok(real) => {
                    if tail.is_empty() {
                        return Ok(real);
                    }
                    tail.reverse();
                    return Ok(self.path.join(&real, &tail.iter().map(String::as_str).collect::<Vec<_>>()));
                }
                Err(error) => {
                    if !is_missing_path_error(&error) {
                        return Err(error);
                    }
                    let parent = self.path.dirname(&current);
                    if parent == current {
                        return Ok(abs.to_string());
                    }
                    tail.push(self.path.basename(&current));
                    current = parent;
                }
            }
        }
        Ok(abs.to_string())
    }

    async fn resolve_within(&self, input_path: &str, allow_external_symlinks: bool) -> FsResult<String> {
        if input_path.is_empty() || input_path == "/" {
            return Err(fs_error(FS_PATH_ESCAPES, &format!("path \"{input_path}\" rejected (empty)"), &[("path", input_path.into()), ("reason", "empty".into())]));
        }
        if self.path.is_absolute(input_path) {
            return Err(fs_error(FS_PATH_ESCAPES, &format!("path \"{input_path}\" rejected (absolute)"), &[("path", input_path.into()), ("reason", "absolute".into())]));
        }
        if input_path.split(|ch| ch == '/' || ch == '\\').any(|segment| segment == "..") {
            return Err(fs_error(FS_PATH_ESCAPES, &format!("path \"{input_path}\" rejected (dotdot segment)"), &[("path", input_path.into()), ("reason", "dotdot_segment".into())]));
        }
        let abs = self.resolve_path_input(input_path);
        if !self.is_within_workspace(&abs) {
            return Err(fs_error(FS_PATH_ESCAPES, &format!("path \"{input_path}\" escapes workspace"), &[("path", input_path.into()), ("reason", "resolved_outside".into())]));
        }
        if !allow_external_symlinks {
            let resolved = self.realpath_existing_prefix(&abs).await?;
            let roots = self.real_roots().await;
            if !roots.iter().any(|root| is_inside_or_equal(&self.path, &resolved, root)) {
                return Err(fs_error(FS_PATH_ESCAPES, &format!("path \"{input_path}\" escapes workspace through a symlink"), &[("path", input_path.into()), ("reason", "symlink_outside".into())]));
            }
        }
        Ok(abs)
    }

    fn to_rel(&self, abs: &str) -> String {
        self.to_rel_from(&self.work_dir(), abs)
    }

    fn anchor_root(&self, abs: &str) -> String {
        let mut dirs = vec![self.work_dir()];
        dirs.extend(self.workspace_dirs.additional_dirs().iter().map(|dir| self.path.resolve_single(dir)));
        let mut best = None;
        for dir in dirs {
            if !is_inside_or_equal(&self.path, abs, &dir) {
                continue;
            }
            if best.as_ref().is_none_or(|current: &String| dir.len() > current.len()) {
                best = Some(dir);
            }
        }
        best.unwrap_or_else(|| self.work_dir())
    }

    fn to_rel_from(&self, base: &str, abs: &str) -> String {
        if abs == base {
            return ".".to_string();
        }
        let rel = self.path.relative(base, abs);
        if rel.is_empty() {
            ".".to_string()
        } else {
            rel.replace(&self.path.separator, "/")
        }
    }
}

struct ListQueueEntry {
    rel_path: String,
    depth_remaining: u32,
    ancestors: Vec<String>,
    via_symlink: bool,
}

#[derive(Clone)]
struct ListCandidate {
    name: String,
    child_rel: String,
    ancestor_of_allowed: bool,
}

struct ListChild {
    name: String,
    rel_path: String,
    stat: HostFileStat,
    is_directory: bool,
    real_path: Option<String>,
}

#[derive(Debug)]
struct SearchCandidate {
    hit: FsSearchHit,
    ignored: bool,
    tier: i32,
}

struct TopEntry {
    name: String,
    kind: FsKind,
    is_directory: bool,
    ignored: bool,
}

struct TopDisplayEntry {
    path: String,
    name: String,
    kind: FsKind,
    ignored: bool,
}

#[derive(Debug)]
struct RgJsonAccumulator {
    req: FsGrepRequest,
    file_buf: IndexMap<String, RgFileBuffer>,
    files: Vec<FsGrepFileHit>,
    seen_keys: HashSet<String>,
    alias_of: HashMap<String, String>,
    alias: HashMap<String, String>,
    total_matches: usize,
    files_scanned: usize,
    truncated: bool,
}

#[derive(Debug)]
struct RgFileBuffer {
    matches: Vec<FsGrepMatch>,
    pending: Vec<String>,
}

impl RgJsonAccumulator {
    fn new(req: FsGrepRequest) -> Self {
        Self {
            req,
            file_buf: IndexMap::new(),
            files: Vec::new(),
            seen_keys: HashSet::new(),
            alias_of: HashMap::new(),
            alias: HashMap::new(),
            total_matches: 0,
            files_scanned: 0,
            truncated: false,
        }
    }

    fn feed(&mut self, line: &str, path: &EnvironmentPath, work_dir: &str) {
        let Ok(record) = serde_json::from_str::<RgJsonRecord>(line) else {
            return;
        };
        let Some(data) = record.data else {
            return;
        };
        match record.type_.as_str() {
            "begin" => {
                let Some(reported) = rg_path(data.path.as_ref()) else {
                    return;
                };
                let key = if path.is_absolute(&reported) { path.resolve_single(&reported) } else { path.resolve(work_dir, &reported) };
                if let Some(seen) = self.alias_of.get(&key).cloned() {
                    self.alias.insert(reported, seen);
                    return;
                }
                if self.files_scanned >= self.req.max_files {
                    self.truncated = true;
                    return;
                }
                self.alias_of.insert(key, reported.clone());
                self.file_buf.insert(reported, RgFileBuffer { matches: Vec::new(), pending: Vec::new() });
                self.files_scanned += 1;
            }
            "context" => {
                let Some(reported) = rg_path(data.path.as_ref()) else {
                    return;
                };
                let path = self.alias.get(&reported).cloned().unwrap_or(reported);
                let Some(buffer) = self.file_buf.get_mut(&path) else {
                    return;
                };
                buffer.pending.push(strip_trailing_newline(&rg_text(data.lines.as_ref())));
                if buffer.pending.len() > self.req.context_lines * 2 {
                    buffer.pending.remove(0);
                }
            }
            "match" => {
                let Some(reported) = rg_path(data.path.as_ref()) else {
                    return;
                };
                let path = self.alias.get(&reported).cloned().unwrap_or(reported);
                let Some(buffer) = self.file_buf.get_mut(&path) else {
                    return;
                };
                if self.total_matches >= self.req.max_total_matches {
                    self.truncated = true;
                    return;
                }
                if buffer.matches.len() >= self.req.max_matches_per_file {
                    return;
                }
                let text = strip_trailing_newline(&rg_text(data.lines.as_ref()));
                let line = data.line_number.unwrap_or(0);
                let col = data.submatches.as_ref().and_then(|matches| matches.first()).map(|found| found.start + 1).unwrap_or(1);
                let before = buffer.pending.iter().rev().take(self.req.context_lines).rev().cloned().collect::<Vec<_>>();
                buffer.pending.clear();
                buffer.matches.push(FsGrepMatch { line, col, text, before, after: Vec::new() });
                self.total_matches += 1;
                if self.total_matches >= self.req.max_total_matches {
                    self.truncated = true;
                }
            }
            "end" => {
                if let Some(reported) = rg_path(data.path.as_ref()) {
                    let path = self.alias.get(&reported).cloned().unwrap_or(reported);
                    self.finalize(&path);
                }
            }
            _ => {}
        }
    }

    fn finish(mut self, aborted: bool, elapsed_ms: i64) -> FsResult<FsGrepResponse> {
        let keys = self.file_buf.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            self.finalize(&key);
        }
        let mut truncated = self.truncated;
        if aborted {
            if self.total_matches == 0 && self.files_scanned == 0 {
                return Err(fs_error(FS_GREP_TIMEOUT, &format!("grep timed out after {elapsed_ms}ms"), &[]));
            }
            truncated = true;
        }
        Ok(FsGrepResponse {
            files: self.files,
            files_scanned: self.files_scanned,
            truncated,
            elapsed_ms: elapsed_ms as u64,
        })
    }

    fn finalize(&mut self, path: &str) {
        let Some(mut buffer) = self.file_buf.shift_remove(path) else {
            return;
        };
        if let Some(last) = buffer.matches.last_mut() {
            if !buffer.pending.is_empty() {
                last.after = buffer.pending.iter().take(self.req.context_lines).cloned().collect();
            }
        }
        if !buffer.matches.is_empty() {
            self.files.push(FsGrepFileHit { path: path.to_string(), matches: buffer.matches });
        }
    }
}

#[async_trait]
impl IWorkspaceFsService for WorkspaceFsService {
    async fn list(&self, req: FsListRequest) -> FsResult<FsListResponse> {
        WorkspaceFsService::list(self, req).await
    }

    async fn read(&self, req: FsReadRequest) -> FsResult<FsReadResponse> {
        WorkspaceFsService::read(self, req).await
    }

    async fn list_many(&self, req: FsListManyRequest) -> FsResult<FsListManyResponse> {
        WorkspaceFsService::list_many(self, req).await
    }

    async fn stat(&self, req: FsStatRequest) -> FsResult<FsStatResponse> {
        WorkspaceFsService::stat(self, req).await
    }

    async fn stat_many(&self, req: FsStatManyRequest) -> FsResult<FsStatManyResponse> {
        WorkspaceFsService::stat_many(self, req).await
    }

    async fn mkdir(&self, req: FsMkdirRequest) -> FsResult<FsMkdirResponse> {
        WorkspaceFsService::mkdir(self, req).await
    }

    async fn search(&self, req: FsSearchRequest) -> FsResult<FsSearchResponse> {
        WorkspaceFsService::search(self, req).await
    }

    async fn suggest(&self, req: FsSuggestRequest, signal: Option<AbortSignal>) -> FsResult<FsSuggestResponse> {
        WorkspaceFsService::suggest(self, req, signal).await
    }

    fn suggest_stream(&self, req: FsSuggestRequest, signal: Option<AbortSignal>) -> Option<SuggestEventStream> {
        Some(WorkspaceFsService::suggest_stream(self, req, signal))
    }

    async fn grep(&self, req: FsGrepRequest) -> FsResult<FsGrepResponse> {
        WorkspaceFsService::grep(self, req).await
    }

    async fn git_status(&self, req: FsGitStatusRequest) -> FsResult<FsGitStatusResponse> {
        WorkspaceFsService::git_status(self, req).await
    }

    async fn git_status_batch(&self, req: FsGitStatusBatchRequest) -> FsResult<FsGitStatusBatchResponse> {
        WorkspaceFsService::git_status_batch(self, req).await
    }

    async fn pull_request(&self, req: FsPullRequestRequest) -> FsResult<Option<FsPullRequest>> {
        WorkspaceFsService::pull_request(self, req).await
    }

    async fn pull_request_batch(&self, req: FsPullRequestBatchRequest) -> FsResult<FsPullRequestBatchResponse> {
        WorkspaceFsService::pull_request_batch(self, req).await
    }

    async fn diff(&self, req: FsDiffRequest) -> FsResult<FsDiffResponse> {
        WorkspaceFsService::diff(self, req).await
    }

    async fn resolve_path(&self, rel_path: &str) -> FsResult<FsPathResolved> {
        WorkspaceFsService::resolve_path(self, rel_path).await
    }

    async fn resolve_download(&self, rel_path: &str, allow_external_symlinks: bool) -> FsResult<FsDownloadResolved> {
        WorkspaceFsService::resolve_download(self, rel_path, allow_external_symlinks).await
    }
}

async fn map_with_concurrency<'a, T, R, F>(items: &[T], concurrency: usize, mut fn_: F) -> Vec<R>
where
    T: Clone + Send + Sync + 'a,
    R: Send + 'a,
    F: FnMut(T) -> BoxFuture<'a, R> + Send + 'a,
{
    stream::iter(items.iter().cloned().map(&mut fn_))
        .buffered(concurrency)
        .collect()
        .await
}

fn sort_children(children: &mut [ListChild], sort: FsListSort) {
    children.sort_by(|a, b| {
        match sort {
            FsListSort::TypeFirst => a.is_directory.cmp(&b.is_directory).reverse().then_with(|| locale_compare(&a.name, &b.name)),
            FsListSort::NameAsc => locale_compare(&a.name, &b.name),
            FsListSort::NameDesc => locale_compare(&b.name, &a.name),
            FsListSort::MtimeDesc => (b.stat.mtime_ms.unwrap_or(0.0) as i64).cmp(&(a.stat.mtime_ms.unwrap_or(0.0) as i64)),
            FsListSort::SizeDesc => b.stat.size.cmp(&a.stat.size),
        }
    });
}

fn build_fs_entry(rel_path: &str, name: &str, stat: &HostFileStat, with_mime: bool) -> FsEntry {
    let kind = if stat.is_symbolic_link { FsKind::Symlink } else if stat.is_directory { FsKind::Directory } else { FsKind::File };
    let modified_at = chrono::DateTime::from_timestamp_millis(stat.mtime_ms.unwrap_or(0.0) as i64)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    FsEntry {
        path: rel_path.to_string(),
        name: name.to_string(),
        kind,
        size: (kind == FsKind::File).then_some(stat.size),
        modified_at,
        etag: Some(build_etag(&FileMetaStat::from(stat))),
        mime: (with_mime && kind == FsKind::File).then(|| guess_mime(rel_path, false).to_string()),
        language_id: (with_mime && kind == FsKind::File).then(|| guess_language_id(rel_path).map(str::to_string)).flatten(),
        is_binary: None,
        is_symlink_to: None,
        is_directory: None,
        git_status: None,
        child_count: None,
    }
}

trait GitignoreExt {
    fn ignores(&self, path: &str) -> bool;
}

impl GitignoreExt for Gitignore {
    fn ignores(&self, path: &str) -> bool {
        self.matched(path, false).is_ignore()
    }
}

fn is_hidden(name: &str) -> bool {
    name.starts_with('.') || matches!(name, ".DS_Store" | ".AppleDouble" | ".LSOverride")
}

fn is_inside_or_equal(path: &EnvironmentPath, child: &str, parent: &str) -> bool {
    let rel = path.relative(parent, child);
    rel.is_empty() || (!rel.starts_with("..") && !path.is_absolute(&rel))
}

fn suggest_path_key(root: &SuggestRoot, rel: &str, multi: bool) -> String {
    if multi { format!("{}/{}", root.real, rel) } else { rel.to_string() }
}

fn suggest_subsequence_glob(query: &str) -> String {
    let parts = query.chars().map(|ch| match ch {
        '*' | '?' | '[' | ']' | '{' | '}' | '!' | '\\' => format!("\\{ch}"),
        _ => ch.to_string(),
    }).collect::<Vec<_>>();
    format!("**/*{}*", parts.join("*"))
}

fn suggest_display_path(path: &EnvironmentPath, root: &SuggestRoot, rel: &str) -> String {
    if root.primary {
        return rel.to_string();
    }
    let dir = if path.separator == "/" { root.dir.clone() } else { root.dir.replace(&path.separator, "/") };
    if dir.ends_with('/') { format!("{dir}{rel}") } else { format!("{dir}/{rel}") }
}

fn display_candidate(path: &EnvironmentPath, root: &SuggestRoot, candidate: SuggestCandidate) -> SuggestCandidate {
    if root.primary {
        return candidate;
    }
    let display = suggest_display_path(path, root, &candidate.path);
    let offset = display.encode_utf16().count() - candidate.path.encode_utf16().count();
    SuggestCandidate {
        path: display,
        positions: candidate.positions.into_iter().map(|position| position + offset as u64).collect(),
        ..candidate
    }
}

fn match_suggest_root(path: &EnvironmentPath, roots: &[SuggestRoot], line: &str) -> Option<(SuggestRoot, String)> {
    let mut best = None;
    for root in roots {
        let dir = if path.separator == "/" { root.dir.clone() } else { root.dir.replace(&path.separator, "/") };
        let prefix = if dir.ends_with('/') { dir } else { format!("{dir}/") };
        if line.starts_with(&prefix) && best.as_ref().is_none_or(|(_, current): &(String, String)| prefix.len() > current.len()) {
            best = Some((root.dir.clone(), prefix));
        }
    }
    let (dir, prefix) = best?;
    let root = roots.iter().find(|root| root.dir == dir).unwrap().clone();
    Some((root, line[prefix.len()..].to_string()))
}

fn fs_error(code: &str, message: &str, details: &[(&str, serde_json::Value)]) -> WireAsyncError {
    let mut details_map = serde_json::Map::new();
    for (key, value) in details {
        details_map.insert((*key).to_string(), value.clone());
    }
    Box::new(Error2::with_options(code, message, Error2Options {
        details: (!details_map.is_empty()).then_some(details_map),
        ..Default::default()
    }))
}

fn error_code<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a str> {
    error.downcast_ref::<Error2>().map(|error| error.code().as_str())
}

fn errno_code<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a str> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(code) = error
            .downcast_ref::<crate::os::interface::host_fs_errors::HostFsError>()
            .and_then(|error| Some(error.code().as_str()))
        {
            return Some(code);
        }
        current = error.source();
    }
    None
}

fn is_missing_path_error(error: &WireAsyncError) -> bool {
    is_host_fs_not_found(error) || is_host_fs_not_directory(error)
}

fn map_fs_error(error: WireAsyncError, input_path: &str) -> WireAsyncError {
    if error_code(error.as_ref()) == Some(OS_FS_DIRECTORY_TOO_LARGE) {
        let message = error.to_string();
        let details = error.downcast_ref::<Error2>().and_then(|error| error.details().cloned());
        return Box::new(Error2::with_options(FS_TOO_MANY_RESULTS, message, Error2Options {
            details,
            cause: Some(error),
            ..Default::default()
        }));
    }
    if is_missing_path_error(&error) {
        return fs_error(FS_PATH_NOT_FOUND, &format!("path not found: {input_path}"), &[("path", input_path.into())]);
    }
    error
}

fn to_wire_error(error: &(dyn std::error::Error + 'static)) -> FsListManyPartialError {
    let code = match error_code(error) {
        Some(FS_PATH_NOT_FOUND) => FS_WIRE_PATH_NOT_FOUND,
        Some(FS_IS_DIRECTORY) => FS_WIRE_IS_DIRECTORY,
        Some(FS_IS_BINARY) => FS_WIRE_IS_BINARY,
        Some(FS_TOO_LARGE) => FS_WIRE_TOO_LARGE,
        Some(FS_TOO_MANY_RESULTS) => FS_WIRE_TOO_MANY_RESULTS,
        _ => FS_INTERNAL_ERROR,
    };
    FsListManyPartialError { code, msg: error.to_string() }
}

fn to_git_batch_error(error: &(dyn std::error::Error + 'static)) -> FsBatchError {
    if let Some(environment) = error.downcast_ref::<EnvironmentError>() {
        let code = match environment.code {
            "environment.not_found" => FS_ENVIRONMENT_NOT_FOUND,
            "environment.invalid_cwd" => FS_VALIDATION_FAILED,
            "environment.conflict" => FS_SESSION_BUSY,
            "environment.unavailable" | "environment.capability_unavailable" => FS_ENVIRONMENT_UNAVAILABLE,
            _ => FS_INTERNAL_ERROR,
        };
        return FsBatchError { code, msg: environment.to_string() };
    }
    let code = match error_code(error) {
        Some(FS_GIT_UNAVAILABLE) => FS_WIRE_GIT_UNAVAILABLE,
        Some(FS_PATH_ESCAPES) => FS_WIRE_PATH_ESCAPES,
        _ => {
            let error = to_wire_error(error);
            return FsBatchError { code: error.code, msg: error.msg };
        }
    };
    FsBatchError { code, msg: error.to_string() }
}

struct TimeoutTimerGuard(tokio::task::JoinHandle<()>);

impl Drop for TimeoutTimerGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn timeout_signal(external: Option<AbortSignal>, timeout_ms: u64) -> (AbortSignal, TimeoutTimerGuard) {
    let controller = Arc::new(AbortController::new());
    let signal = match external {
        Some(external) => AbortSignal::any(&[controller.signal(), external]),
        None => controller.signal(),
    };
    let controller_for_timer = controller.clone();
    let timer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
        controller_for_timer.abort(None);
    });
    (signal, TimeoutTimerGuard(timer))
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

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
