use std::sync::{Arc, LazyLock};

use futures_util::{StreamExt, stream};
use indexmap::IndexSet;
use regex::Regex;
use serde_json::{Map, Value, json};

use super::grep::{GrepInput, GrepOutputMode, IGrepTool, grep_input_schema};
use crate::_base::utils::abort::AbortSignal;
use crate::agent::environment_binding::agent_environment::{
    IAgentEnvironmentService, acquire_or_when_ready, pinned_generation,
};
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::app::auth::auth::IOAuthService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::environment::environment::{Environment, EnvironmentWorkspaceRoots};
use crate::environment::environment_workspace_view::EnvironmentWorkspaceView;
use crate::features::skill::catalog::types::SkillCatalog as _;
use crate::features::skill::session::skill_catalog::ISessionSkillCatalog;
use crate::os::backends::host::tools::rg_locator::{
    EnsureRgPathOptions, ResolveRegion, RgProbe, RgProbeResult, RgResolutionSource,
    ensure_rg_path, rg_unavailable_message,
};
use crate::os::backends::host::tools::run_rg::{
    DEFAULT_TIMEOUT_MS, MAX_OUTPUT_BYTES, RunRgOutcome, RunRgResult, run_rg_once,
    should_retry_ripgrep_eagain,
};
use crate::os::interface::host_environment::HostEnvironmentInfo;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_fs_errors::is_host_fs_not_found;
use crate::os::interface::host_process::{HostOutputStream, IHostProcessService};
use crate::session::workspace_context::workspace_context::ISessionWorkspaceContext;
use crate::tool::input_schema::to_input_json_schema;
use crate::tool::output_accumulator::{ToolOutputAccumulator, ToolOutputAccumulatorResult};
use crate::tool::path_access::{
    PathAccessOperation, PathClass, ResolvePathAccessPathOptions, SENSITIVE_DOT_VARIANT_SUFFIXES,
    WorkspaceAccessPolicy, WorkspaceConfig, is_sensitive_file, normalize_path,
    resolve_path_access_path,
};
use crate::tool::rule_match::{literal_rule_pattern, matches_glob_rule_subject};
use crate::tool::tool_contract::ToolAccesses;
use crate::tool::tool_input_display::{FileIoOperation, ToolInputDisplay};
use crate::wire::record::WireAsyncError;

const RG_MAX_COLUMNS: usize = 500;
const RG_MAX_THREADS: usize = 4;
const DEFAULT_HEAD_LIMIT: usize = 250;
const MTIME_STAT_CONCURRENCY: usize = 32;
const VCS_DIRECTORIES_TO_EXCLUDE: [&str; 6] = [".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];
const SENSITIVE_KEY_BASENAMES: [&str; 3] = ["id_rsa", "id_ed25519", "id_ecdsa"];
const DESCRIPTION: &str = r#"Search file contents using regular expressions (powered by ripgrep).

Use Grep to find unknown content or file locations; use Read directly when inspecting a known file's contents. Do not run `grep` or `rg` via shell: they bypass workspace policy, output limits, and sensitive-file filtering.

Write patterns in ripgrep regex syntax, which differs from POSIX `grep` syntax. For example, braces are special, so escape them as `\{` to match a literal `{`.

Hidden files are searched by default.
"#;

static CONTENT_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*?)([:-])(\d+)\2").expect("content line regex is valid"));
static CONTENT_PAYLOAD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+)([:-])").expect("content payload regex is valid"));
static PARAMETERS: LazyLock<Value> = LazyLock::new(|| to_input_json_schema(grep_input_schema()));

#[derive(Clone)]
pub struct GrepTool {
    environment: Arc<dyn IAgentEnvironmentService>,
    workspace_ctx: Arc<dyn ISessionWorkspaceContext>,
    telemetry: Arc<dyn ITelemetryService>,
    skill_catalog: Option<Arc<dyn ISessionSkillCatalog>>,
    auth: Option<Arc<dyn IOAuthService>>,
}

impl GrepTool {
    pub fn new(
        environment: Arc<dyn IAgentEnvironmentService>,
        workspace_ctx: Arc<dyn ISessionWorkspaceContext>,
        telemetry: Arc<dyn ITelemetryService>,
        skill_catalog: Option<Arc<dyn ISessionSkillCatalog>>,
        auth: Option<Arc<dyn IOAuthService>>,
    ) -> Self {
        Self {
            environment,
            workspace_ctx,
            telemetry,
            skill_catalog,
            auth,
        }
    }

    pub fn name(&self) -> &'static str {
        "Grep"
    }

    pub fn side_effect(&self) -> &'static str {
        "read"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        PARAMETERS
            .as_object()
            .cloned()
            .expect("Grep input schema is an object")
    }

    pub async fn resolve_execution_value(
        &self,
        input: Value,
    ) -> Result<ToolExecution, WireAsyncError> {
        match serde_json::from_value::<GrepInput>(input) {
            Ok(args) => self.resolve_execution(args).await,
            Err(error) => Ok(ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for Grep: {error}"
            )))),
        }
    }

    pub async fn resolve_execution(
        &self,
        args: GrepInput,
    ) -> Result<ToolExecution, WireAsyncError> {
        let resolution_lease = acquire_or_when_ready(self.environment.as_ref(), &["fs"]).await?;
        let resolved = async {
            let inspected = resolution_lease.environment.clone();
            let expected_generation = pinned_generation(inspected.as_ref());
            let mut additional_dirs = self.workspace_ctx.additional_dirs();
            if let Some(skill_catalog) = self.skill_catalog.as_ref() {
                additional_dirs.extend(skill_catalog.catalog().get_skill_roots());
            }
            let view = EnvironmentWorkspaceView::new(
                inspected,
                EnvironmentWorkspaceRoots {
                    work_dir: self.workspace_ctx.work_dir(),
                    additional_dirs: Some(additional_dirs),
                    ..Default::default()
                },
            );
            let env = view.host.clone();
            let workspace = WorkspaceConfig {
                workspace_dir: view.work_dir.clone(),
                additional_dirs: view.additional_dirs.clone(),
            };
            let path_class = path_class_from_name(&env.path_class);
            let resolved_path = match args.path.as_deref() {
                Some(path) => {
                    let bridge = view.shell_path_bridge.as_ref().map(|bridge| {
                        let bridge = bridge.clone();
                        move |value: &str| bridge.from_shell_path(value)
                    });
                    Some(
                        resolve_path_access_path(
                            path,
                            &ResolvePathAccessPathOptions {
                                path_class,
                                home_dir: env.home_dir.as_deref(),
                                shell_path_bridge: bridge
                                    .as_ref()
                                    .map(|bridge| bridge as &dyn Fn(&str) -> String),
                                workspace: &workspace,
                                operation: PathAccessOperation::Search,
                                policy: Some(&WorkspaceAccessPolicy::SEARCH),
                                expand_home: None,
                            },
                        )
                        .map_err(|error| -> WireAsyncError {
                            Box::new(std::io::Error::other(error.message))
                        })?,
                    )
                }
                None => None,
            };
            let search_paths = vec![
                resolved_path
                    .clone()
                    .unwrap_or_else(|| workspace.workspace_dir.clone()),
            ];
            let search_path = args
                .path
                .clone()
                .unwrap_or_else(|| workspace.workspace_dir.clone());
            let rule_pattern = args.pattern.clone();
            let this = self.clone();
            Ok(ToolExecution::Runnable(RunnableToolExecution {
                accesses: Some(ToolAccesses::search_tree(&search_paths[0])),
                display: Some(ToolInputDisplay::FileIo {
                    operation: FileIoOperation::Grep,
                    path: search_paths[0].clone(),
                    detail: None,
                    content: None,
                    before: None,
                    after: None,
                }),
                description: Some(format!(
                    "Searching for '{}' in {search_path}",
                    args.pattern
                )),
                stop_batch_after_this: None,
                approval_rule: literal_rule_pattern(self.name(), &args.pattern),
                matches_rule: Some(Arc::new(move |rule_args| {
                    matches_glob_rule_subject(rule_args, &rule_pattern)
                })),
                execute: Arc::new(move |context| {
                    let this = this.clone();
                    let args = args.clone();
                    let env = env.clone();
                    let workspace = workspace.clone();
                    let search_paths = search_paths.clone();
                    let expected_generation = expected_generation.clone();
                    Box::pin(async move {
                        let lease = acquire_or_when_ready(
                            this.environment.as_ref(),
                            &["fs", "process"],
                        )
                        .await?;
                        let result = async {
                            if expected_generation.as_ref().is_some_and(|expected| {
                                lease.environment.identity().generation != *expected
                            }) {
                                return Ok(ExecutableToolResult::error(
                                    "Environment changed before execution. Retry the tool call.",
                                ));
                            }
                            this.execution(
                                lease.environment.clone(),
                                lease.environment.process(),
                                lease.environment.fs(),
                                env,
                                workspace,
                                args,
                                context.signal,
                                search_paths,
                            )
                            .await
                        }
                        .await;
                        lease.dispose();
                        result
                    })
                }),
            }))
        }
        .await;
        resolution_lease.dispose();
        resolved
    }

    async fn execution(
        &self,
        environment: Arc<dyn Environment>,
        process_service: Arc<dyn IHostProcessService>,
        fs: Arc<dyn IHostFileSystem>,
        env: HostEnvironmentInfo,
        workspace: WorkspaceConfig,
        args: GrepInput,
        signal: AbortSignal,
        search_paths: Vec<String>,
    ) -> Result<ExecutableToolResult, WireAsyncError> {
        if signal.aborted() {
            return Ok(ExecutableToolResult::error("Aborted before search started"));
        }
        let path_class = path_class_from_name(&env.path_class);
        let resolve_region = self.resolve_region();
        let resolution = match ensure_rg_path(
            create_rg_probe(process_service.clone(), workspace.workspace_dir.clone()),
            EnsureRgPathOptions {
                signal: Some(signal.clone()),
                allow_cached_fallback: true,
                environment: Some(environment.clone()),
                resolve_region,
                ..Default::default()
            },
        )
        .await
        {
            Ok(resolution) => resolution,
            Err(error) => {
                if signal.aborted() {
                    return Ok(ExecutableToolResult::error("Grep aborted"));
                }
                self.track("grep_tool_rg_fallback", json!({ "outcome": "failed" }));
                return Ok(ExecutableToolResult::error(rg_unavailable_message(
                    &error,
                    Some(&environment),
                )));
            }
        };
        if resolution.source != RgResolutionSource::SystemPath {
            self.track(
                "grep_tool_rg_fallback",
                json!({
                    "source": rg_source_name(resolution.source),
                    "outcome": "resolved"
                }),
            );
        }
        let rg_path = resolution.path;
        let mut run_result = match run_rg_once(
            process_service.clone(),
            &build_rg_args(&rg_path, &args, &search_paths, false),
            signal.clone(),
            None,
        )
        .await
        {
            Ok(RunRgOutcome::Result(result)) => result,
            Ok(RunRgOutcome::Aborted) => {
                return Ok(ExecutableToolResult::error("Grep aborted"));
            }
            Err(error) => {
                return Ok(ExecutableToolResult::error(format_spawn_error(
                    &error,
                    &environment,
                )));
            }
        };
        if should_retry_ripgrep_eagain(&run_result) {
            run_result = match run_rg_once(
                process_service,
                &build_rg_args(&rg_path, &args, &search_paths, true),
                signal.clone(),
                None,
            )
            .await
            {
                Ok(RunRgOutcome::Result(result)) => result,
                Ok(RunRgOutcome::Aborted) => {
                    return Ok(ExecutableToolResult::error("Grep aborted"));
                }
                Err(error) => {
                    return Ok(ExecutableToolResult::error(format_spawn_error(
                        &error,
                        &environment,
                    )));
                }
            };
        }
        let RunRgResult {
            exit_code,
            mut stdout_text,
            stderr_text,
            buffer_truncated,
            stderr_truncated,
            timed_out,
        } = run_result;
        if exit_code != 0 && exit_code != 1 && !timed_out && !buffer_truncated {
            return Ok(ExecutableToolResult::error(format_ripgrep_error(
                exit_code,
                &stderr_text,
                stderr_truncated,
            )));
        }
        let mode = args.output_mode.unwrap_or(GrepOutputMode::FilesWithMatches);
        if buffer_truncated || timed_out {
            stdout_text = omit_incomplete_trailing_record(&stdout_text, mode);
        }
        if timed_out && stdout_text.trim().is_empty() {
            return Ok(ExecutableToolResult::error(format!(
                "Grep timed out after {}s. Try a more specific path or pattern.",
                DEFAULT_TIMEOUT_MS / 1000
            )));
        }
        if signal.aborted() {
            return Ok(ExecutableToolResult::error("Grep aborted"));
        }
        let raw_lines = parse_ripgrep_output(&stdout_text, mode);
        let mut filtered_sensitive = IndexSet::new();
        let kept_lines = filter_sensitive_lines(
            &raw_lines,
            mode,
            &mut filtered_sensitive,
            path_class,
        );
        let ordered_lines = if mode == GrepOutputMode::FilesWithMatches && !timed_out {
            match self
                .sort_files_with_matches_by_mtime(fs, kept_lines, signal.clone())
                .await
            {
                Ok(lines) => lines,
                Err(error) if error.is::<GrepAbortedError>() => {
                    return Ok(ExecutableToolResult::error("Grep aborted"));
                }
                Err(error) => return Err(error),
            }
        } else {
            kept_lines
        };
        let offset = args.offset.unwrap_or(0);
        let head_limit = args.head_limit.unwrap_or(DEFAULT_HEAD_LIMIT);
        let after_offset = if offset > 0 {
            ordered_lines.iter().skip(offset).cloned().collect::<Vec<_>>()
        } else {
            ordered_lines.clone()
        };
        let limit_active = head_limit > 0;
        let limited = if limit_active {
            after_offset
                .iter()
                .take(head_limit)
                .cloned()
                .collect::<Vec<_>>()
        } else {
            after_offset.clone()
        };
        let pagination_truncated = limit_active && after_offset.len() > head_limit;
        let mut header_lines = Vec::new();
        let mut messages = Vec::new();
        if !filtered_sensitive.is_empty() {
            let displayed = filtered_sensitive
                .iter()
                .map(|path| relativize_if_under(path, &workspace.workspace_dir, path_class))
                .collect::<Vec<_>>();
            messages.push(format!(
                "Filtered {} sensitive file(s): {}",
                filtered_sensitive.len(),
                displayed.join(", ")
            ));
        }
        if mode == GrepOutputMode::CountMatches && !ordered_lines.is_empty() {
            header_lines.push(format_count_summary(
                &ordered_lines,
                !filtered_sensitive.is_empty(),
            ));
        }
        if pagination_truncated {
            let total = after_offset.len() + offset;
            let next_offset = offset + head_limit;
            let notice = if buffer_truncated || timed_out {
                format!(
                    "Results truncated to {head_limit} lines (total: {total} of a partial result set). Use offset={next_offset} to see more."
                )
            } else {
                format!(
                    "Results truncated to {head_limit} lines (total: {total}). Use offset={next_offset} to see more."
                )
            };
            if mode == GrepOutputMode::CountMatches {
                header_lines.push(notice);
            } else {
                messages.push(notice);
            }
        }
        if buffer_truncated {
            messages.push(format!(
                "[Output truncated at {MAX_OUTPUT_BYTES} bytes of rg output — the result set is incomplete. Narrow the pattern, path, or glob filters and re-run to recover complete results.]"
            ));
        }
        if timed_out {
            messages.push(format!(
                "Grep timed out after {}s; partial results returned. Narrow the path, glob, or pattern and retry for complete results.",
                DEFAULT_TIMEOUT_MS / 1000
            ));
        }
        let content_includes_line_numbers =
            mode == GrepOutputMode::Content && args.line_numbers != Some(false);
        let displayed_lines = limited
            .iter()
            .map(|line| {
                format_display_line(
                    line,
                    mode,
                    &workspace.workspace_dir,
                    path_class,
                    content_includes_line_numbers,
                )
            })
            .collect::<Vec<_>>();
        let content_body = displayed_lines.join("\n");
        let visible_body = if ordered_lines.is_empty() && !filtered_sensitive.is_empty() {
            "No non-sensitive matches found".to_string()
        } else {
            content_body
        };
        let body = if visible_body.is_empty() && header_lines.is_empty() && messages.is_empty() {
            "No non-sensitive matches found".to_string()
        } else {
            visible_body
        };
        let combined = [header_lines, vec![body], messages]
            .concat()
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let mut builder = ToolOutputAccumulator::new();
        builder.write(&combined);
        Ok(from_accumulator(builder.ok("", None)))
    }

    fn resolve_region(&self) -> Option<ResolveRegion> {
        self.auth.as_ref().map(|auth| {
            let auth = auth.clone();
            Arc::new(move || Some(auth.get_region())) as ResolveRegion
        })
    }

    fn track(&self, event: &str, properties: Value) {
        self.telemetry.track2(
            event,
            properties.as_object().cloned().or_else(|| Some(Map::new())),
        );
    }

    async fn sort_files_with_matches_by_mtime(
        &self,
        fs: Arc<dyn IHostFileSystem>,
        lines: Vec<ParsedGrepLine>,
        signal: AbortSignal,
    ) -> Result<Vec<ParsedGrepLine>, WireAsyncError> {
        if signal.aborted() {
            return Err(Box::new(GrepAbortedError));
        }
        if lines.is_empty() {
            return Ok(Vec::new());
        }
        let mut entries = stream::iter(lines.into_iter().enumerate())
            .map(|(index, line)| {
                let fs = fs.clone();
                let signal = signal.clone();
                async move {
                    if signal.aborted() {
                        return Err(Box::new(GrepAbortedError) as WireAsyncError);
                    }
                    let path = match &line {
                        ParsedGrepLine::Record { file_path, .. } => Some(file_path.as_str()),
                        ParsedGrepLine::Legacy { text } => Some(text.as_str()),
                        ParsedGrepLine::Separator => None,
                    };
                    let mut mtime = 0i64;
                    if let Some(path) = path
                        && let Ok(stat) = fs.stat(path).await
                    {
                        mtime = (stat.mtime_ms.unwrap_or(0.0) / 1000.0).trunc() as i64;
                    }
                    Ok::<_, WireAsyncError>((line, mtime, index))
                }
            })
            .buffer_unordered(MTIME_STAT_CONCURRENCY)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        if signal.aborted() {
            return Err(Box::new(GrepAbortedError));
        }
        entries.sort_by(|left, right| {
            right
                .1
                .cmp(&left.1)
                .then_with(|| left.2.cmp(&right.2))
        });
        Ok(entries.into_iter().map(|entry| entry.0).collect())
    }
}

impl IGrepTool for GrepTool {}

impl ExecutableTool for GrepTool {
    fn name(&self) -> &str {
        GrepTool::name(self)
    }

    fn description(&self) -> &str {
        GrepTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        GrepTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> futures_util::future::BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let this = self.clone();
        Box::pin(async move { this.resolve_execution_value(input).await })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedGrepLine {
    Record { file_path: String, payload: String },
    Separator,
    Legacy { text: String },
}

#[derive(Debug)]
struct GrepAbortedError;

impl std::fmt::Display for GrepAbortedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Grep aborted")
    }
}

impl std::error::Error for GrepAbortedError {}

fn build_rg_args(
    rg_path: &str,
    args: &GrepInput,
    search_paths: &[String],
    single_threaded: bool,
) -> Vec<String> {
    let mut cmd = vec![rg_path.to_string()];
    if single_threaded {
        cmd.extend(["-j".to_string(), "1".to_string()]);
    } else {
        cmd.extend(["-j".to_string(), RG_MAX_THREADS.to_string()]);
    }
    cmd.push("--hidden".to_string());
    if args.include_ignored != Some(true) {
        cmd.push("--no-require-git".to_string());
    }
    let mode = args.output_mode.unwrap_or(GrepOutputMode::FilesWithMatches);
    if mode != GrepOutputMode::Content {
        cmd.extend(["--max-columns".to_string(), RG_MAX_COLUMNS.to_string()]);
    }
    cmd.push("--null".to_string());
    for dir in VCS_DIRECTORIES_TO_EXCLUDE {
        cmd.extend(["--glob".to_string(), format!("!{dir}")]);
    }
    match mode {
        GrepOutputMode::FilesWithMatches => cmd.push("-l".to_string()),
        GrepOutputMode::CountMatches => {
            cmd.extend(["--count-matches".to_string(), "--with-filename".to_string()]);
        }
        GrepOutputMode::Content => {}
    }
    if args.case_insensitive == Some(true) {
        cmd.push("-i".to_string());
    }
    if mode == GrepOutputMode::Content {
        cmd.push("--with-filename".to_string());
        if args.line_numbers != Some(false) {
            cmd.push("-n".to_string());
        } else {
            cmd.push("--field-context-separator".to_string());
            cmd.push(":".to_string());
        }
        if let Some(context) = args.context {
            cmd.extend(["-C".to_string(), context.to_string()]);
        } else {
            if let Some(after) = args.after {
                cmd.extend(["-A".to_string(), after.to_string()]);
            }
            if let Some(before) = args.before {
                cmd.extend(["-B".to_string(), before.to_string()]);
            }
        }
    }
    if let Some(glob) = &args.glob {
        cmd.extend(["--glob".to_string(), glob.clone()]);
    }
    if let Some(type_) = &args.type_ {
        cmd.extend(["--type".to_string(), type_.clone()]);
    }
    if args.multiline == Some(true) {
        cmd.extend(["-U".to_string(), "--multiline-dotall".to_string()]);
    }
    if args.include_ignored == Some(true) {
        cmd.push("--no-ignore".to_string());
    }
    for glob in sensitive_globs_to_exclude() {
        cmd.extend(["--glob".to_string(), format!("!{glob}")]);
    }
    cmd.push("--".to_string());
    cmd.push(args.pattern.clone());
    cmd.extend(search_paths.iter().cloned());
    cmd
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

fn parse_ripgrep_output(text: &str, mode: GrepOutputMode) -> Vec<ParsedGrepLine> {
    if text.is_empty() {
        return Vec::new();
    }
    if !text.contains('\0') {
        return split_rg_lines(text)
            .into_iter()
            .map(|line| {
                if mode == GrepOutputMode::Content && line == "--" {
                    ParsedGrepLine::Separator
                } else {
                    ParsedGrepLine::Legacy { text: line }
                }
            })
            .collect();
    }
    if mode == GrepOutputMode::FilesWithMatches {
        return text
            .split('\0')
            .map(strip_trailing_carriage_return)
            .filter(|file_path| !file_path.is_empty())
            .map(|file_path| ParsedGrepLine::Record {
                file_path: file_path.to_string(),
                payload: String::new(),
            })
            .collect();
    }
    let mut records = Vec::new();
    let mut cursor = 0usize;
    while cursor < text.len() {
        if text[cursor..].starts_with('\n') {
            cursor += 1;
            continue;
        }
        if text[cursor..].starts_with("--\r\n") {
            records.push(ParsedGrepLine::Separator);
            cursor += 4;
            continue;
        }
        if text[cursor..].starts_with("--\n") {
            records.push(ParsedGrepLine::Separator);
            cursor += 3;
            continue;
        }
        let Some(nul_index) = text[cursor..].find('\0').map(|index| cursor + index) else {
            let tail = strip_trailing_carriage_return(&text[cursor..]);
            if !tail.is_empty() {
                records.push(ParsedGrepLine::Legacy {
                    text: tail.to_string(),
                });
            }
            break;
        };
        let line_end = text[nul_index + 1..]
            .find('\n')
            .map(|index| nul_index + 1 + index);
        let payload_end = line_end.unwrap_or(text.len());
        records.push(ParsedGrepLine::Record {
            file_path: text[cursor..nul_index].to_string(),
            payload: strip_trailing_carriage_return(&text[nul_index + 1..payload_end])
                .to_string(),
        });
        cursor = line_end.map_or(text.len(), |line_end| line_end + 1);
    }
    records
}

fn split_rg_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines = text.split('\n').collect::<Vec<_>>();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
        .into_iter()
        .map(|line| strip_trailing_carriage_return(line).to_string())
        .collect()
}

fn omit_incomplete_trailing_record(text: &str, mode: GrepOutputMode) -> String {
    if !text.contains('\0') {
        return omit_incomplete_trailing_line(text);
    }
    if mode == GrepOutputMode::FilesWithMatches {
        return text
            .rfind('\0')
            .map(|index| text[..=index].to_string())
            .unwrap_or_default();
    }
    let mut cursor = 0usize;
    let mut last_complete_end = 0usize;
    while cursor < text.len() {
        if text[cursor..].starts_with('\n') {
            cursor += 1;
            last_complete_end = cursor;
            continue;
        }
        if text[cursor..].starts_with("--\r\n") {
            cursor += 4;
            last_complete_end = cursor;
            continue;
        }
        if text[cursor..].starts_with("--\n") {
            cursor += 3;
            last_complete_end = cursor;
            continue;
        }
        let Some(nul_index) = text[cursor..].find('\0').map(|index| cursor + index) else {
            break;
        };
        let Some(line_end) = text[nul_index + 1..]
            .find('\n')
            .map(|index| nul_index + 1 + index)
        else {
            break;
        };
        cursor = line_end + 1;
        last_complete_end = cursor;
    }
    text[..last_complete_end].to_string()
}

fn omit_incomplete_trailing_line(text: &str) -> String {
    text.rfind('\n')
        .map(|index| text[..index].to_string())
        .unwrap_or_default()
}

fn format_ripgrep_error(exit_code: i32, stderr_text: &str, stderr_truncated: bool) -> String {
    let stderr = stderr_text.trim();
    if stderr.is_empty() {
        return format!("Failed to grep: ripgrep exited with code {exit_code}");
    }
    let summary = summarize_ripgrep_stderr(stderr);
    let mut lines = vec![
        format!("Failed to grep: {summary}"),
        String::new(),
        "ripgrep stderr:".to_string(),
        stderr.to_string(),
    ];
    if stderr_truncated {
        lines.push(format!("[stderr truncated at {MAX_OUTPUT_BYTES} bytes]"));
    }
    lines.join("\n")
}

fn summarize_ripgrep_stderr(stderr: &str) -> String {
    let lines = split_rg_lines(stderr)
        .into_iter()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    lines
        .iter()
        .rev()
        .find(|line| line.to_lowercase().starts_with("error:"))
        .cloned()
        .or_else(|| lines.last().cloned())
        .unwrap_or_else(|| "ripgrep error".to_string())
}

fn filter_sensitive_lines(
    lines: &[ParsedGrepLine],
    mode: GrepOutputMode,
    filtered_paths: &mut IndexSet<String>,
    path_class: PathClass,
) -> Vec<ParsedGrepLine> {
    let mut kept = Vec::new();
    for line in lines {
        if matches!(line, ParsedGrepLine::Separator) {
            kept.push(line.clone());
            continue;
        }
        if let Some(file_path) = parsed_file_path(line, mode, path_class)
            && is_sensitive_file(&file_path)
        {
            filtered_paths.insert(file_path);
            continue;
        }
        kept.push(line.clone());
    }
    if mode == GrepOutputMode::Content {
        normalize_context_separators(&kept)
    } else {
        kept
    }
}

fn normalize_context_separators(lines: &[ParsedGrepLine]) -> Vec<ParsedGrepLine> {
    let mut normalized = Vec::new();
    for line in lines {
        if matches!(line, ParsedGrepLine::Separator)
            && (normalized.is_empty()
                || matches!(normalized.last(), Some(ParsedGrepLine::Separator)))
        {
            continue;
        }
        normalized.push(line.clone());
    }
    while matches!(normalized.last(), Some(ParsedGrepLine::Separator)) {
        normalized.pop();
    }
    normalized
}

fn parsed_file_path(
    line: &ParsedGrepLine,
    mode: GrepOutputMode,
    path_class: PathClass,
) -> Option<String> {
    match line {
        ParsedGrepLine::Record { file_path, .. } => Some(normalize_path(file_path, path_class)),
        ParsedGrepLine::Separator => None,
        ParsedGrepLine::Legacy { text } => match mode {
            GrepOutputMode::FilesWithMatches => Some(normalize_path(text, path_class)),
            GrepOutputMode::CountMatches => {
                let index = text.rfind(':');
                Some(normalize_path(
                    index.filter(|index| *index > 0).map_or(text, |index| &text[..index]),
                    path_class,
                ))
            }
            GrepOutputMode::Content => extract_content_file_path(text, path_class),
        },
    }
}

fn extract_content_file_path(line: &str, path_class: PathClass) -> Option<String> {
    if let Some(captures) = CONTENT_LINE_RE.captures(line)
        && let Some(path) = captures.get(1)
    {
        return Some(normalize_path(path.as_str(), path_class));
    }
    no_line_number_content_separator_index(line, path_class)
        .filter(|index| *index > 0)
        .map(|index| normalize_path(&line[..index], path_class))
}

fn no_line_number_content_separator_index(line: &str, path_class: PathClass) -> Option<usize> {
    let bytes = line.as_bytes();
    let search_from = if path_class == PathClass::Win32
        && bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
    {
        2
    } else {
        0
    };
    line[search_from..]
        .find(':')
        .map(|index| search_from + index)
}

fn format_display_line(
    line: &ParsedGrepLine,
    mode: GrepOutputMode,
    workspace_dir: &str,
    path_class: PathClass,
    content_includes_line_numbers: bool,
) -> String {
    match line {
        ParsedGrepLine::Separator => "--".to_string(),
        ParsedGrepLine::Record { file_path, payload } => {
            let display_path = relativize_if_under(file_path, workspace_dir, path_class);
            match mode {
                GrepOutputMode::FilesWithMatches => display_path,
                GrepOutputMode::CountMatches => format!("{display_path}:{payload}"),
                GrepOutputMode::Content => {
                    let separator = if content_includes_line_numbers {
                        content_payload_path_separator(payload)
                    } else {
                        ':'
                    };
                    format!("{display_path}{separator}{payload}")
                }
            }
        }
        ParsedGrepLine::Legacy { text } => match mode {
            GrepOutputMode::FilesWithMatches => {
                relativize_if_under(text, workspace_dir, path_class)
            }
            GrepOutputMode::CountMatches => {
                let Some(index) = text.rfind(':').filter(|index| *index > 0) else {
                    return text.clone();
                };
                format!(
                    "{}{}",
                    relativize_if_under(&text[..index], workspace_dir, path_class),
                    &text[index..]
                )
            }
            GrepOutputMode::Content => {
                if let Some(file_path) = extract_content_file_path(text, path_class) {
                    format!(
                        "{}{}",
                        relativize_if_under(&file_path, workspace_dir, path_class),
                        text.get(file_path.len()..).unwrap_or("")
                    )
                } else {
                    text.clone()
                }
            }
        },
    }
}

fn content_payload_path_separator(payload: &str) -> char {
    CONTENT_PAYLOAD_RE
        .captures(payload)
        .and_then(|captures| captures.get(2))
        .filter(|separator| separator.as_str() == "-")
        .map(|_| '-')
        .unwrap_or(':')
}

fn relativize_if_under(candidate: &str, base: &str, path_class: PathClass) -> String {
    let norm_candidate = normalize_path(candidate, path_class);
    let norm_base = normalize_path(base, path_class);
    let comparable_candidate = if path_class == PathClass::Win32 {
        norm_candidate.to_lowercase()
    } else {
        norm_candidate.clone()
    };
    let comparable_base = if path_class == PathClass::Win32 {
        norm_base.to_lowercase()
    } else {
        norm_base.clone()
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
        return norm_candidate[prefix.len()..].to_string();
    }
    norm_candidate
}

fn format_count_summary(lines: &[ParsedGrepLine], redacted_sensitive: bool) -> String {
    let mut total_matches = 0f64;
    let mut total_files = 0usize;
    for line in lines {
        let raw_count = match line {
            ParsedGrepLine::Record { payload, .. } => Some(payload.as_str()),
            ParsedGrepLine::Legacy { text } => count_payload_from_legacy_line(text),
            ParsedGrepLine::Separator => None,
        };
        let Some(count) = raw_count.and_then(|raw| {
            let raw = raw.trim();
            if raw.is_empty() {
                Some(0.0)
            } else {
                raw.parse::<f64>().ok()
            }
        }) else {
            continue;
        };
        if !count.is_finite()
            || count.fract() != 0.0
            || count < 0.0
            || count > 9_007_199_254_740_991.0
        {
            continue;
        }
        total_matches += count;
        total_files += 1;
    }
    let occurrence_word = if total_matches == 1.0 {
        "occurrence"
    } else {
        "occurrences"
    };
    let file_word = if total_files == 1 { "file" } else { "files" };
    let scope = if redacted_sensitive {
        "total non-sensitive"
    } else {
        "total"
    };
    format!(
        "Found {} {scope} {occurrence_word} across {total_files} {file_word}.",
        format_js_number(total_matches)
    )
}

fn count_payload_from_legacy_line(line: &str) -> Option<&str> {
    line.rfind(':')
        .filter(|index| *index > 0)
        .map(|index| &line[index + 1..])
}

fn format_js_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

fn strip_trailing_carriage_return(value: &str) -> &str {
    value.strip_suffix('\r').unwrap_or(value)
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

fn format_spawn_error(error: &WireAsyncError, environment: &Arc<dyn Environment>) -> String {
    if is_host_fs_not_found(error) {
        rg_unavailable_message(error, Some(environment))
    } else {
        error.to_string()
    }
}

fn rg_source_name(source: RgResolutionSource) -> &'static str {
    match source {
        RgResolutionSource::SystemPath => "system-path",
        RgResolutionSource::Vendor => "vendor",
        RgResolutionSource::ShareBinCached => "share-bin-cached",
        RgResolutionSource::ShareBinDownloaded => "share-bin-downloaded",
    }
}

fn path_class_from_name(path_class: &str) -> PathClass {
    if path_class == "win32" {
        PathClass::Win32
    } else {
        PathClass::Posix
    }
}

fn from_accumulator(result: ToolOutputAccumulatorResult) -> ExecutableToolResult {
    ExecutableToolResult {
        output: result.content.into(),
        is_error: Some(result.is_error),
        note: result.brief,
        spill: result.spill,
        ..Default::default()
    }
}
