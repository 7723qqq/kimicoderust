use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::{Value, json};

use super::glob::{DEFAULT_HEAD_LIMIT, GlobInput, WINDOWS_PATH_HINT, glob_input_schema};
use crate::_base::utils::abort::AbortSignal;
use crate::environment::environment::Environment;
use crate::tool::path_access::is_sensitive_file;
use crate::os::backends::host::tools::rg_locator::{
    EnsureRgPathOptions, ResolveRegion, RgProbe, RgProbeResult, RgResolutionSource,
    ensure_rg_path, rg_unavailable_message,
};
use crate::os::backends::host::tools::run_rg::{
    DEFAULT_TIMEOUT_MS, MAX_OUTPUT_BYTES, RunRgOutcome, RunRgResult, run_rg_once,
    should_retry_ripgrep_eagain,
};
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_fs_errors::is_host_fs_not_found;
use crate::os::interface::host_process::{HostOutputStream, IHostProcessService};
use crate::tool::path_access::{
    PathAccessOperation, PathClass, WorkspaceAccessPolicy, canonicalize_path, is_within_directory,
    normalize_path, resolve_path_access,
};
use crate::tool::tool_contract::DEFAULT_TOOL_RESULT_MAX_RETAINED_CHARS;
use crate::tool::tool_input_display::{FileIoOperation, ToolInputDisplay};
use crate::turn_loop::types::ExecutableToolResult;
use crate::wire::record::WireAsyncError;

const VCS_DIRECTORIES_TO_EXCLUDE: [&str; 6] = [".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];
const RG_MAX_THREADS: usize = 4;
const SENSITIVE_KEY_BASENAMES: [&str; 3] = ["id_rsa", "id_ed25519", "id_ecdsa"];
const SENSITIVE_DOT_VARIANT_SUFFIXES: [&str; 10] = [
    ".bak", ".backup", ".copy", ".disabled", ".key", ".old", ".orig", ".pem", ".save",
    ".tmp",
];

pub type GlobTelemetry = Arc<dyn Fn(&str, Value) + Send + Sync>;

pub struct GlobTool {
    fs: Arc<dyn IHostFileSystem>,
    process_service: Arc<dyn IHostProcessService>,
    environment: Option<Arc<dyn Environment>>,
    path_class: PathClass,
    workspace_dir: String,
    telemetry: Option<GlobTelemetry>,
    resolve_region: Option<ResolveRegion>,
}

impl GlobTool {
    pub fn new(
        fs: Arc<dyn IHostFileSystem>,
        process_service: Arc<dyn IHostProcessService>,
        environment: Option<Arc<dyn Environment>>,
        path_class: PathClass,
        workspace_dir: impl Into<String>,
        telemetry: Option<GlobTelemetry>,
        resolve_region: Option<ResolveRegion>,
    ) -> Self {
        Self {
            fs,
            process_service,
            environment,
            path_class,
            workspace_dir: workspace_dir.into(),
            telemetry,
            resolve_region,
        }
    }

    pub fn name(&self) -> &'static str {
        "Glob"
    }

    pub fn parameters(&self) -> Value {
        glob_input_schema()
    }

    pub fn description(&self, base: &str) -> String {
        if self.path_class == PathClass::Win32 {
            format!("{base}{WINDOWS_PATH_HINT}")
        } else {
            base.to_string()
        }
    }

    pub async fn execution(&self, args: &GlobInput, signal: AbortSignal) -> ExecutableToolResult {
        let search_root = match args.path.as_deref() {
            Some(path) => {
                let roots = [self.workspace_dir.clone()];
                let home_dir = self
                    .environment
                    .as_ref()
                    .and_then(|environment| environment.host())
                    .map(|host| host.home_dir);
                match resolve_path_access(
                    path,
                    &self.workspace_dir,
                    &roots,
                    home_dir.as_deref(),
                    self.path_class,
                    PathAccessOperation::Search,
                    &WorkspaceAccessPolicy::SEARCH,
                ) {
                    Ok(access) => access.path,
                    Err(error) => return tool_result(error.message, true),
                }
            }
            None => self.workspace_dir.clone(),
        };
        match self.fs.stat(&search_root).await {
            Ok(stat) if !stat.is_directory => {
                return tool_result(format!("{search_root} is not a directory"), true);
            }
            Ok(_) => {}
            Err(error) if is_host_fs_not_found(&error) => {
                return tool_result(format!("{search_root} does not exist"), true);
            }
            Err(error) => return tool_result(error.to_string(), true),
        }
        if signal.aborted() {
            return tool_result("Glob aborted", true);
        }
        let probe = create_rg_probe(
            self.process_service.clone(),
            glob_search_cwd(&self.workspace_dir, self.path_class),
        );
        let resolution = match ensure_rg_path(
            probe,
            EnsureRgPathOptions {
                signal: Some(signal.clone()),
                allow_cached_fallback: true,
                environment: self.environment.clone(),
                resolve_region: self.resolve_region.clone(),
                ..EnsureRgPathOptions::default()
            },
        )
        .await
        {
            Ok(resolution) => resolution,
            Err(error) => {
                if signal.aborted() {
                    return tool_result("Glob aborted", true);
                }
                self.track("glob_tool_rg_fallback", json!({ "outcome": "failed" }));
                return tool_result(
                    rg_unavailable_message(&error, self.environment.as_ref()),
                    true,
                );
            }
        };
        if resolution.source != RgResolutionSource::SystemPath {
            let source = match resolution.source {
                RgResolutionSource::SystemPath => "system-path",
                RgResolutionSource::Vendor => "vendor",
                RgResolutionSource::ShareBinCached => "share-bin-cached",
                RgResolutionSource::ShareBinDownloaded => "share-bin-downloaded",
            };
            self.track(
                "glob_tool_rg_fallback",
                json!({ "source": source, "outcome": "resolved" }),
            );
        }
        let rg_path = resolution.path;
        let search_cwd = glob_search_cwd(&search_root, self.path_class);
        let mut run = match run_rg_once(
            self.process_service.clone(),
            &build_rg_args(&rg_path, args, false, &search_root, &search_cwd),
            signal.clone(),
            Some(&search_cwd),
        )
        .await
        {
            Ok(run) => run,
            Err(error) => return tool_result(self.format_spawn_error(&error), true),
        };
        if matches!(run, RunRgOutcome::Aborted) {
            return tool_result("Glob aborted", true);
        }
        if matches!(&run, RunRgOutcome::Result(result) if should_retry_ripgrep_eagain(result)) {
            run = match run_rg_once(
                self.process_service.clone(),
                &build_rg_args(&rg_path, args, true, &search_root, &search_cwd),
                signal.clone(),
                Some(&search_cwd),
            )
            .await
            {
                Ok(run) => run,
                Err(error) => return tool_result(self.format_spawn_error(&error), true),
            };
            if matches!(run, RunRgOutcome::Aborted) {
                return tool_result("Glob aborted", true);
            }
        }
        let RunRgOutcome::Result(RunRgResult {
            exit_code,
            stdout_text,
            stderr_text,
            buffer_truncated,
            timed_out,
            ..
        }) = run
        else {
            return tool_result("Glob aborted", true);
        };
        let mut traversal_warning = None;
        if exit_code != 0 && exit_code != 1 && !timed_out && !buffer_truncated {
            if split_complete_paths(&stdout_text, true).is_empty() {
                return tool_result(format_glob_error(&search_root, &stderr_text), true);
            }
            traversal_warning = Some(format_glob_warning(&stderr_text));
        }
        if signal.aborted() {
            return tool_result("Glob aborted", true);
        }
        let raw_paths = split_complete_paths(&stdout_text, buffer_truncated || timed_out)
            .into_iter()
            .map(|path| {
                canonicalize_path(&path, &search_root, self.path_class)
                    .unwrap_or_else(|_| format!("{search_root}/{path}"))
            });
        let mut kept = Vec::new();
        let mut filtered_sensitive = 0usize;
        for path in raw_paths {
            if is_sensitive_file(&path) {
                filtered_sensitive += 1;
            } else {
                kept.push(path);
            }
        }
        let offset = args.offset.unwrap_or(0);
        let head_limit = args.head_limit.unwrap_or(DEFAULT_HEAD_LIMIT);
        let limited: Vec<String> = kept
            .iter()
            .skip(offset)
            .take(if head_limit == 0 { usize::MAX } else { head_limit })
            .cloned()
            .collect();
        let partial = buffer_truncated || timed_out || traversal_warning.is_some();
        let should_relativize =
            is_within_directory(&search_root, &self.workspace_dir, self.path_class);
        let candidates: Vec<String> = limited
            .into_iter()
            .map(|path| {
                if should_relativize {
                    relativize_if_under(&path, &search_root, self.path_class)
                } else {
                    path
                }
            })
            .collect();

        let mut detail_parts = vec![format!("pattern: {}", args.pattern)];
        if let Some(path) = args.path.as_deref() {
            detail_parts.push(format!("path: {path}"));
        }
        if args.include_ignored == Some(true) {
            detail_parts.push("include_ignored: true".to_string());
        }
        let display = ToolInputDisplay::FileIo {
            operation: FileIoOperation::Glob,
            path: search_root.clone(),
            detail: Some(detail_parts.join(", ")),
            content: None,
            before: None,
            after: None,
        };
        let mut warnings = Vec::new();
        if timed_out {
            warnings.push(format!(
                "Glob timed out after {}s; partial results returned.",
                DEFAULT_TIMEOUT_MS / 1000
            ));
        }
        if buffer_truncated {
            warnings.push(format!(
                "[stdout truncated at {MAX_OUTPUT_BYTES} bytes; results may be incomplete — use a more specific pattern]"
            ));
        }
        if let Some(warning) = traversal_warning {
            warnings.push(warning);
        }
        let notice_chars = [false, true]
            .into_iter()
            .map(|character_limited| {
                let notices = page_notices(
                    &warnings,
                    candidates.len(),
                    character_limited,
                    offset,
                    kept.len(),
                    partial,
                    filtered_sensitive,
                    head_limit,
                );
                utf16_len(&[notices.lines, notices.footer].concat().join("\n")) + 2
            })
            .max()
            .unwrap_or(0);
        let mut remaining = DEFAULT_TOOL_RESULT_MAX_RETAINED_CHARS as i64 - notice_chars as i64;
        let mut display_lines = Vec::new();
        for path in &candidates {
            let length = utf16_len(path) as i64 + 1;
            if length > remaining {
                break;
            }
            display_lines.push(path.clone());
            remaining -= length;
        }
        if !candidates.is_empty() && display_lines.is_empty() {
            return tool_result(
                "Glob cannot fit a complete path and its diagnostics within the output limit. Narrow the search path or pattern.",
                true,
            );
        }
        let notices = page_notices(
            &warnings,
            display_lines.len(),
            display_lines.len() < candidates.len(),
            offset,
            kept.len(),
            partial,
            filtered_sensitive,
            head_limit,
        );
        tool_result_with_display(
            [notices.lines, display_lines, notices.footer]
                .concat()
                .join("\n"),
            false,
            Some(display),
        )
    }

    fn track(&self, event: &str, payload: Value) {
        if let Some(telemetry) = self.telemetry.as_ref() {
            telemetry(event, payload);
        }
    }

    fn format_spawn_error(&self, error: &WireAsyncError) -> String {
        if is_host_fs_not_found(error) {
            rg_unavailable_message(error, self.environment.as_ref())
        } else {
            error.to_string()
        }
    }
}

fn tool_result(content: impl Into<String>, is_error: bool) -> ExecutableToolResult {
    tool_result_with_display(content, is_error, None)
}

fn tool_result_with_display(
    content: impl Into<String>,
    is_error: bool,
    display: Option<ToolInputDisplay>,
) -> ExecutableToolResult {
    ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content: content.into(),
        is_error,
        note: None,
        display: display.map(Box::new),
    }
}



fn create_rg_probe(process_service: Arc<dyn IHostProcessService>, cwd: String) -> RgProbe {
    Arc::new(move |args| {
        let process_service = process_service.clone();
        let cwd = cwd.clone();
        Box::pin(async move {
            let Some((command, rest)) = args.split_first() else {
                return Ok(RgProbeResult { exit_code: -1 });
            };
            let proc = process_service
                .spawn(command, rest, Some(&cwd), None)
                .await?;
            let _ = proc.close_stdin().await;
            let stdout = drain(proc.stdout());
            let stderr = drain(proc.stderr());
            let wait = proc.wait();
            let (_, _, exit_code) = tokio::join!(stdout, stderr, wait);
            let exit_code = exit_code?;
            proc.dispose().await;
            Ok(RgProbeResult { exit_code })
        })
    })
}

async fn drain(mut stream: HostOutputStream) {
    while let Some(chunk) = stream.next().await {
        if chunk.is_err() {
            break;
        }
    }
}

fn glob_search_cwd(search_root: &str, path_class: PathClass) -> String {
    let mut cwd = search_root.to_string();
    while path_class == PathClass::Win32 && cwd.encode_utf16().count() >= 260 {
        let parent = dirname(&cwd);
        if parent == cwd {
            break;
        }
        cwd = parent;
    }
    cwd
}

fn dirname(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let Some(index) = trimmed.rfind(['/', '\\']) else {
        return path.to_string();
    };
    if index == 0 {
        return trimmed[..1].to_string();
    }
    trimmed[..index].to_string()
}

pub fn build_rg_args(
    rg_path: &str,
    args: &GlobInput,
    single_threaded: bool,
    search_root: &str,
    cwd: &str,
) -> Vec<String> {
    let relative = if cwd == search_root {
        String::new()
    } else {
        search_root[cwd.len()..]
            .trim_start_matches(['/', '\\'])
            .to_string()
    };
    let prefix = relative
        .chars()
        .flat_map(|character| {
            if matches!(character, '\\' | '*' | '?' | '[' | ']' | '{' | '}' | '!') {
                vec!['\\', character]
            } else {
                vec![character]
            }
        })
        .collect::<String>();
    let search_glob = |pattern: &str| {
        if cwd == search_root {
            return pattern.to_string();
        }
        let negative = pattern.starts_with('!');
        let body = pattern.strip_prefix('!').unwrap_or(pattern);
        if body.is_empty() {
            return pattern.to_string();
        }
        let anchored = body.starts_with('/') || body.trim_end_matches('/').contains('/');
        let body = body.strip_prefix('/').unwrap_or(body);
        format!(
            "{}{}/{}",
            if negative { "!" } else { "" },
            prefix,
            if anchored {
                body.to_string()
            } else {
                format!("**/{body}")
            }
        )
    };
    let mut command = vec![rg_path.to_string()];
    if single_threaded {
        command.extend(["-j".to_string(), "1".to_string()]);
    } else {
        command.extend(["-j".to_string(), RG_MAX_THREADS.to_string()]);
    }
    command.extend([
        "--files".to_string(),
        "--hidden".to_string(),
        "--sortr=modified".to_string(),
    ]);
    if args.include_ignored != Some(true) {
        command.push("--no-require-git".to_string());
    }
    for directory in VCS_DIRECTORIES_TO_EXCLUDE {
        command.extend(["--glob".to_string(), search_glob(&format!("!{directory}"))]);
    }
    command.extend(["--glob".to_string(), search_glob(&args.pattern)]);
    for glob in sensitive_globs_to_exclude() {
        command.extend(["--glob".to_string(), search_glob(&format!("!{glob}"))]);
    }
    if args.include_ignored == Some(true) {
        command.push("--no-ignore".to_string());
    }
    command.push(if cwd == search_root {
        ".".to_string()
    } else {
        search_root.to_string()
    });
    command
}

fn sensitive_globs_to_exclude() -> Vec<String> {
    let mut globs = vec!["**/.env".to_string()];
    for name in SENSITIVE_KEY_BASENAMES {
        globs.push(format!("**/{name}"));
        globs.push(format!("**/{name}[-_]*"));
        for suffix in SENSITIVE_DOT_VARIANT_SUFFIXES {
            globs.push(format!("**/{name}{suffix}"));
        }
    }
    globs.extend([
        "**/.aws/credentials".to_string(),
        "**/.aws/credentials/**".to_string(),
        "**/.gcp/credentials".to_string(),
        "**/.gcp/credentials/**".to_string(),
    ]);
    globs
}

fn format_glob_error(search_root: &str, stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.to_ascii_lowercase().contains("no such file or directory") {
        return format!("{search_root} does not exist");
    }
    if trimmed.is_empty() {
        "Glob failed".to_string()
    } else {
        format!("Glob failed: {trimmed}")
    }
}

fn format_glob_warning(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        "Glob completed with warnings; some directories could not be read.".to_string()
    } else {
        format!(
            "Glob completed with warnings; some directories could not be read: {trimmed}"
        )
    }
}

pub fn split_complete_paths(stdout_text: &str, truncated_output: bool) -> Vec<String> {
    let text = if truncated_output && !stdout_text.ends_with('\n') {
        match stdout_text.rfind('\n') {
            Some(index) => &stdout_text[..=index],
            None => "",
        }
    } else {
        stdout_text
    };
    text.split('\n')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect()
}

fn relativize_if_under(candidate: &str, base: &str, path_class: PathClass) -> String {
    let normalized_candidate = normalize_path(candidate, path_class);
    let normalized_base = normalize_path(base, path_class);
    let comparable_candidate = if path_class == PathClass::Win32 {
        normalized_candidate.to_lowercase()
    } else {
        normalized_candidate.clone()
    };
    let comparable_base = if path_class == PathClass::Win32 {
        normalized_base.to_lowercase()
    } else {
        normalized_base.clone()
    };
    if comparable_candidate == comparable_base {
        return ".".to_string();
    }
    let prefix = if comparable_base.ends_with('/') {
        comparable_base
    } else {
        format!("{comparable_base}/")
    };
    if comparable_candidate.starts_with(&prefix) {
        normalized_candidate[prefix.len()..].to_string()
    } else {
        normalized_candidate
    }
}

struct PageNotices {
    lines: Vec<String>,
    footer: Vec<String>,
}

fn page_notices(
    warnings: &[String],
    count: usize,
    character_limited: bool,
    offset: usize,
    total_kept: usize,
    partial: bool,
    filtered_sensitive: usize,
    head_limit: usize,
) -> PageNotices {
    let mut lines = warnings.to_vec();
    let mut footer = Vec::new();
    let truncated = character_limited || offset + count < total_kept;
    if count == 0 {
        if total_kept > 0 {
            let result_set = if partial {
                "collected partial result set"
            } else {
                "current result set"
            };
            lines.push(format!(
                "No more matches at offset={offset} in the {result_set} ({total_kept} matches)."
            ));
        } else if partial {
            lines.push("No matches collected; search incomplete.".to_string());
        } else if filtered_sensitive > 0 {
            lines.push(format!(
                "No non-sensitive matches found ({filtered_sensitive} sensitive file(s) filtered)."
            ));
        } else {
            lines.push("No matches found".to_string());
        }
    } else if truncated || offset > 0 || partial {
        let total = if partial {
            format!("{total_kept} collected matches (partial result set)")
        } else {
            total_kept.to_string()
        };
        lines.push(format!(
            "Showing matches {}–{} of {total}.",
            offset + 1,
            offset + count
        ));
    }
    if character_limited {
        lines.push("Character limit reached; only complete paths are returned.".to_string());
    }
    if truncated {
        lines.push(format!(
            "Continue with the same search arguments and offset={}.",
            offset + count
        ));
        if !character_limited {
            lines.push("To remove the match-count limit, omit offset and use head_limit=0.".to_string());
        }
    }
    if filtered_sensitive > 0 && (total_kept > 0 || partial) {
        footer.push(format!("Filtered {filtered_sensitive} sensitive file(s)."));
    }
    if !truncated && !partial && offset == 0 && head_limit > 0 && count == head_limit {
        footer.push(format!("Found {count} matches"));
    }
    PageNotices { lines, footer }
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}
