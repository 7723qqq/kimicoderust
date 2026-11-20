use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};

use futures_util::future::BoxFuture;
use futures_util::{Stream, StreamExt, stream};
use regex::Regex;
use serde_json::{Value, json};

use super::config_section::READ_SECTION;
use super::read::{
    DEFAULT_MAX_CHARS, DEFAULT_MAX_CHARS_LIMIT, ReadInput, ReadRegion, TRANSCODE_MAX_BYTES,
    read_input_schema,
};
use crate::_base::text::encoding::{UtfTextEncoding, decode_utf_text, detect_text_encoding};
use crate::_base::text::line_endings::{
    LineEndingStyle, make_carriage_returns_visible, split_lines_keeping_terminator,
};
use crate::_base::errors::errors::SharedError;
use crate::_base::utils::abort::{AbortSignal, abort_error};
use crate::_base::utils::render_prompt::render_prompt;
use crate::agent::context_memory::tool_result_render::{
    RenderableToolOutput, RenderableToolResult, render_tool_result_for_model,
};
use crate::agent::environment_binding::environment_binding::IAgentEnvironmentBindingService;
use crate::agent::media::file_type::{DetectFileTypeMode, FileKind, detect_file_type};
use crate::agent::media::media_read_context::MediaReadContextService;
use crate::agent::media::media_ref::{is_daemon_file_url, parse_daemon_file_url};
use crate::agent::media::session_media_store::{
    ISessionMediaStore, SessionMediaFile, SessionMediaReadRange,
};
use crate::agent::task::persist::is_task_output_file_path;
use crate::agent::tool_executor::tool_executor::{
    ExecutableToolOutput, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::agent::tool_result_truncation::tool_result_truncation::IAgentToolResultTruncationService;
use crate::agent::tools::file_memory::FileMemoryProvider;
use crate::agent::tools::file_read_source::{FileReadSource, environment_file_source};
use crate::agent::tools::read_media_file::execute_media_read::{
    MediaReadArgs, MediaReadOutput, MediaReadRegion, execute_media_read,
};
use crate::agent::tools::read_media_file::read_media_file::MAX_MEDIA_MEGABYTES;
use crate::app::config::config::IConfigService;
use crate::app::environment::environment::EnvironmentResolver;
use crate::environment::environment::{
    EnvironmentCapability, EnvironmentLease, EnvironmentPath, EnvironmentStatus,
    EnvironmentWorkspaceRoots,
};
use crate::environment::environment_workspace_view::EnvironmentWorkspaceView;
use crate::features::skill::catalog::types::SkillCatalog as _;
use crate::features::skill::session::skill_catalog::ISessionSkillCatalog;
use crate::human::llm::message::ContentPart;
use crate::llm_adapter::contract::tokens::{
    TokenCharCounts, count_token_chars, tokens_for_char_counts,
};
use crate::native::file_type::MEDIA_SNIFF_BYTES;
use crate::os::interface::host_environment::{HostEnvironmentInfo, IHostEnvironment};
use crate::os::interface::host_file_system::{
    HostFileHead, HostFileStat, HostLineStream, IHostFileSystem,
};
use crate::os::interface::host_fs_errors::{is_host_fs_not_directory, is_host_fs_not_found};
use crate::persistence::interface::blob_store::BlobByteStream;
use crate::session::session_context::session_context::ISessionContext;
use crate::session::workspace_context::workspace_context::ISessionWorkspaceContext;
use crate::tool::path_access::{
    PathAccessOperation, PathClass, WorkspaceAccessPolicy, resolve_path_access,
};
use crate::tool::rule_match::{
    PermissionPathMatchOptions, literal_rule_pattern, matches_glob_rule_subject,
    matches_path_rule_subject,
};
use crate::tool::tool_input_display::{FileIoOperation, ToolInputDisplay};
use crate::turn_loop::types::read_file_access;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::{Error2, unwrap_error_cause};

const RANGE_STATUS_TOKENS: usize = 256;

const READ_DESCRIPTION_TEMPLATE: &str = r#"Read a text file from the active environment's filesystem. Do not fully trust comments and documentation.

The path may be a `kimi-file://` attachment reference. Its bytes come from the current session's storage, independently of the workspace environment. Next Read keeps the reference so pagination also works after a fork. For a binary attachment, the error includes a server-local path when available; a converter must be able to access that filesystem. ReadMediaFile accepts the same reference for images and videos.

If the user provides a concrete file path to a text file, call Read directly. Do not `Glob`, `ls`, or otherwise pre-check known text file paths; missing or invalid file paths return errors you can handle.

When you need several files, prefer to read them in parallel: emit multiple `Read` calls in a single response instead of reading one file per turn.

- Relative paths resolve against the working directory; a path outside the working directory must be absolute.
- Output is `<line-number>\t<content>` per line. A `<system>` status block may follow; it is not part of the file.
- Results are bounded by `max_chars`: ${DEFAULT_MAX_CHARS} by default, up to ${MAX_CHARS}, and by about ${MAX_CHARS} divided by four estimated tokens.
- If the status block reports an incomplete range, continue with its `Next Read` arguments exactly. Do not answer from a partial page when the task requires the remaining content.
- **Images and videos** (max ${MAX_MEDIA_MEGABYTES}MB) are sent to the model as multimodal media instead of text; this requires a model with the matching vision capability (`image_in` / `video_in`). `region` and `full_resolution` apply only to images; `line_offset` and `n_lines` apply only to text files.
- Large images are downsampled by default when automatic compression can safely fit them within model limits, which can blur fine detail (small text, dense UI). Compute absolute coordinates from the original dimensions reported in the `<system>` block, never by measuring the displayed copy. When the `<system>` tag reports downsampling and you need that detail, call Read again with the `region` parameter (original-image pixel coordinates) to view a crop at full fidelity, or set `full_resolution` to true when the whole file fits the per-image byte limit. Re-reading the same file without these parameters just reproduces the same downsampled image.
- If automatic compression cannot safely produce an image within model limits, the tool returns an error and does not send the original image. Follow the error: use Bash or an available image-processing tool to create a smaller copy, then read that copy. Do not retry the unchanged file.
- After a successful `Edit`/`Write`, do not re-read solely to prove the write landed.
"#;

static TEXT_DECODE_ERROR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)encoded data was not valid|invalid.*encoding|invalid.*utf-?8")
        .expect("text decode error pattern is valid")
});

#[derive(Debug, Clone, Copy, Default)]
struct LineEndingFlags {
    has_crlf: bool,
    has_lf: bool,
    has_lone_cr: bool,
}

#[derive(Debug, Clone)]
struct ReadLineEntry {
    line_no: i64,
    raw_content: String,
}

#[derive(Debug, Clone)]
struct ReadTailEntry {
    line_no: i64,
    raw_content: String,
    min_chars: usize,
}

#[derive(Clone)]
struct ReadRequest {
    args: ReadInput,
    max_chars: usize,
    token_limit: usize,
    max_chars_limit: usize,
    detected_encoding: Option<UtfTextEncoding>,
    lossy_decoding: bool,
    event_log: bool,
}

struct ReadPage {
    request: ReadRequest,
    rendered_lines: Vec<String>,
    start_line: i64,
    range_start: i64,
    range_end: i64,
    total_lines: i64,
    from_tail: bool,
    line_ending_style: LineEndingStyle,
}

#[derive(Clone)]
pub struct LocalArtifactTarget {
    pub fs: Arc<dyn IHostFileSystem>,
    pub path: String,
    pub environment_path: EnvironmentPath,
    pub path_class: PathClass,
}

pub type ReadPlanResolver = Arc<
    dyn Fn(String) -> BoxFuture<'static, Result<Option<LocalArtifactTarget>, WireAsyncError>>
        + Send
        + Sync,
>;

type ReadLineFactory = Arc<dyn Fn() -> HostLineStream + Send + Sync>;

pub struct ReadToolDependencies {
    pub environment: Arc<dyn EnvironmentResolver>,
    pub environment_binding: Arc<dyn IAgentEnvironmentBindingService>,
    pub workspace_ctx: Arc<dyn ISessionWorkspaceContext>,
    pub skill_catalog: Arc<dyn ISessionSkillCatalog>,
    pub media_read: Arc<MediaReadContextService>,
    pub result_truncation: Arc<dyn IAgentToolResultTruncationService>,
    pub config: Arc<dyn IConfigService>,
    pub attachment_store: Option<Arc<dyn ISessionMediaStore>>,
    pub git_memory: Option<Arc<dyn FileMemoryProvider>>,
    pub plan_service: Option<ReadPlanResolver>,
    pub host_fs: Option<Arc<dyn IHostFileSystem>>,
    pub host_environment: Option<Arc<dyn IHostEnvironment>>,
    pub session: Option<Arc<dyn ISessionContext>>,
    pub agent: Option<Arc<dyn crate::agent::scope_context::scope_context::IAgentScopeContext>>,
}

#[derive(Clone)]
pub struct ReadTool {
    inner: Arc<ReadToolDependencies>,
}

struct WorkspaceConfig {
    workspace_dir: String,
    additional_dirs: Vec<String>,
}

impl ReadTool {
    pub fn new(dependencies: ReadToolDependencies) -> Self {
        Self {
            inner: Arc::new(dependencies),
        }
    }

    pub fn name(&self) -> &'static str {
        "Read"
    }

    pub fn description(&self) -> String {
        let limits = self.limits();
        let variables = HashMap::from([
            ("DEFAULT_MAX_CHARS".to_string(), json!(limits.0)),
            ("MAX_CHARS".to_string(), json!(limits.1)),
            ("MAX_MEDIA_MEGABYTES".to_string(), json!(MAX_MEDIA_MEGABYTES)),
        ]);
        render_prompt(READ_DESCRIPTION_TEMPLATE, &variables)
    }
    pub fn side_effect(&self) -> &'static str {
        "read"
    }
    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn parameters(&self) -> Value {
        read_input_schema()
    }

    pub async fn resolve_execution_value(
        &self,
        input: Value,
    ) -> Result<ToolExecution, WireAsyncError> {
        self.resolve_execution(parse_read_input(input)?).await
    }

    pub async fn resolve_execution(
        &self,
        args: ReadInput,
    ) -> Result<ToolExecution, WireAsyncError> {
        if args.path.is_empty() {
            return Ok(ToolExecution::Error(error_result(
                "File path cannot be empty.".to_string(),
            )));
        }
        if args.column_offset.is_some() && args.line_offset.unwrap_or(1) < 0 {
            return Ok(ToolExecution::Error(error_result(
                "column_offset is only supported for forward reads. Use a positive line_offset or the forward Next Read arguments.".to_string(),
            )));
        }
        if is_daemon_file_url(&args.path) {
            return self.attachment_execution(args).await;
        }
        if let Some(plan_service) = self.inner.plan_service.as_ref()
            && let Some(target) = plan_service(args.path.clone()).await?
        {
            return Ok(self.local_artifact_execution(args, target, true));
        }
        if let Some(target) = self.local_output_target(&args.path) {
            return Ok(self.local_artifact_execution(args, target, false));
        }
        let resolution_lease = self.acquire_or_when_ready().await?;
        let resolved = async {
            let inspected = resolution_lease.environment.clone();
            let expected_generation = pinned_generation(inspected.as_ref());
            let mut additional_dirs = self.inner.workspace_ctx.additional_dirs();
            additional_dirs.extend(self.inner.skill_catalog.catalog().get_skill_roots());
            let view = EnvironmentWorkspaceView::new(
                inspected,
                EnvironmentWorkspaceRoots {
                    work_dir: self.inner.workspace_ctx.work_dir(),
                    additional_dirs: Some(additional_dirs),
                    ..Default::default()
                },
            );
            let workspace = self.workspace_config(&view);
            let path_class = path_class_from_name(&view.host.path_class);
            let home_dir = view.host.home_dir.clone();
            let path = resolve_read_path(&args.path, &view, &workspace)?;
            let description = format!("Reading {}", args.path);
            let display = file_display(&path);
            let approval_rule = literal_rule_pattern(self.name(), &path);
            let match_path = path.clone();
            let match_workspace = workspace.workspace_dir.clone();
            let match_home = home_dir.clone();
            let this = self.clone();
            let execute_args = args.clone();
            let execute_path = path.clone();
            Ok(ToolExecution::Runnable(RunnableToolExecution {
                accesses: Some(vec![read_file_access(&path)]),
                display: Some(display),
                description: Some(description),
                stop_batch_after_this: None,
                approval_rule,
                matches_rule: Some(Arc::new(move |rule_args| {
                    matches_path_rule_subject(
                        rule_args,
                        &match_path,
                        Some(&PermissionPathMatchOptions {
                            cwd: Some(&match_workspace),
                            path_class: Some(path_class),
                            home_dir: Some(&match_home),
                            case_insensitive_paths: None,
                        }),
                    )
                })),
                execute: Arc::new(move |context| {
                    let this = this.clone();
                    let args = execute_args.clone();
                    let path = execute_path.clone();
                    let expected_generation = expected_generation.clone();
                    Box::pin(async move {
                        let lease = this.acquire_or_when_ready().await?;
                        let result = async {
                            if expected_generation.as_ref().is_some_and(|expected| {
                                lease.environment.identity().generation != *expected
                            }) {
                                return Ok(error_result(
                                    "Environment changed before execution. Retry the tool call.".to_string(),
                                ));
                            }
                            let event_log = this.inner.result_truncation.is_wire_journal_path(&path);
                            let source: Arc<dyn FileReadSource> = Arc::new(
                                environment_file_source(lease.environment.fs(), path.clone()),
                            );
                            let host = lease.environment.host();
                            let mut result = this
                                .execution(
                                    source,
                                    args.clone(),
                                    host,
                                    event_log,
                                    Some(context.signal.clone()),
                                )
                                .await?;
                            result.spill_exempt = Some(true);
                            Ok(this.with_file_memory(result, &args.path).await)
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

    fn limits(&self) -> (usize, usize) {
        let section = self
            .inner
            .config
            .get(READ_SECTION)
            .and_then(|value| serde_json::from_value::<super::config_section::ReadConfig>(value).ok());
        let max_chars = section
            .as_ref()
            .and_then(|section| section.max_chars)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(DEFAULT_MAX_CHARS_LIMIT);
        let default_max_chars = section
            .and_then(|section| section.default_max_chars)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(DEFAULT_MAX_CHARS)
            .min(max_chars);
        (default_max_chars, max_chars)
    }

    fn workspace_config(&self, view: &EnvironmentWorkspaceView) -> WorkspaceConfig {
        WorkspaceConfig {
            workspace_dir: view.work_dir.clone(),
            additional_dirs: view.additional_dirs.clone(),
        }
    }

    async fn acquire_or_when_ready(&self) -> Result<EnvironmentLease, WireAsyncError> {
        let binding = self.inner.environment_binding.current();
        let ready = self
            .inner
            .environment
            .inspect(&binding)
            .ok()
            .is_some_and(|environment| {
                environment.status() == EnvironmentStatus::Ready
                    && environment
                        .capabilities()
                        .contains(&EnvironmentCapability::Fs)
            });
        if ready {
            self.inner
                .environment
                .acquire(&binding, &[EnvironmentCapability::Fs])
        } else {
            self.inner
                .environment
                .acquire_when_ready(&binding, &[EnvironmentCapability::Fs])
                .await
        }
    }

    fn local_output_target(&self, path: &str) -> Option<LocalArtifactTarget> {
        let host_fs = self.inner.host_fs.as_ref()?;
        let host_environment = self.inner.host_environment.as_ref()?;
        if path.contains('\0') {
            return None;
        }
        let path_class = path_class_from_name(&host_environment.path_class());
        let environment_path = EnvironmentPath::new(host_environment.path_class().as_str());
        if !environment_path.is_absolute(path) {
            return None;
        }
        let normalized = environment_path.resolve_single(path);
        let task_output = match (self.inner.session.as_ref(), self.inner.agent.as_ref()) {
            (Some(session), Some(agent)) => {
                let agent_id = agent.agent_id();
                let agent_dir = environment_path.join(
                    &session.session_dir(),
                    &["agents", agent_id.as_str()],
                );
                is_task_output_file_path(&normalized, &agent_dir, &environment_path)
                    || (agent_id == "main"
                        && is_task_output_file_path(
                            &normalized,
                            &session.session_dir(),
                            &environment_path,
                        ))
            }
            _ => false,
        };
        if !task_output
            && !self
                .inner
                .result_truncation
                .is_spill_file_path(&normalized)
        {
            return None;
        }
        Some(LocalArtifactTarget {
            fs: host_fs.clone(),
            path: normalized,
            environment_path,
            path_class,
        })
    }

    fn local_artifact_execution(
        &self,
        args: ReadInput,
        target: LocalArtifactTarget,
        plan_file: bool,
    ) -> ToolExecution {
        let path = target.path.clone();
        let match_path = path.clone();
        let match_cwd = target.environment_path.dirname(&path);
        let path_class = target.path_class;
        let this = self.clone();
        let execute_args = args.clone();
        let execute_target = target.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: Some(vec![read_file_access(&path)]),
            display: Some(file_display(&path)),
            description: Some(format!("Reading {}", args.path)),
            stop_batch_after_this: None,
            approval_rule: literal_rule_pattern(self.name(), &path),
            matches_rule: Some(Arc::new(move |rule_args| {
                matches_path_rule_subject(
                    rule_args,
                    &match_path,
                    Some(&PermissionPathMatchOptions {
                        cwd: Some(&match_cwd),
                        path_class: Some(path_class),
                        home_dir: None,
                        case_insensitive_paths: None,
                    }),
                )
            })),
            execute: Arc::new(move |_| {
                let this = this.clone();
                let args = execute_args.clone();
                let target = execute_target.clone();
                Box::pin(async move {
                    if plan_file {
                        let current = match this.inner.plan_service.as_ref() {
                            Some(plan_service) => plan_service(target.path.clone()).await?,
                            None => None,
                        };
                        if current.as_ref().map(|target| target.path.as_str())
                            != Some(target.path.as_str())
                        {
                            return Ok(error_result(
                                "The active plan changed before execution. Retry the tool call.".to_string(),
                            ));
                        }
                    }
                    let source: Arc<dyn FileReadSource> = Arc::new(environment_file_source(
                        target.fs.clone(),
                        target.path.clone(),
                    ));
                    let mut result = this
                        .execution(source, args.clone(), None, false, None)
                        .await?;
                    result.spill_exempt = Some(true);
                    Ok(this.with_file_memory(result, &args.path).await)
                })
            }),
        })
    }

    async fn attachment_execution(
        &self,
        args: ReadInput,
    ) -> Result<ToolExecution, WireAsyncError> {
        let source = Arc::new(
            SessionAttachmentFileSource::open_initial(
                args.path.clone(),
                self.inner.attachment_store.clone(),
            )
            .await?,
        );
        let display_path = source
            .local_path()
            .map(str::to_string)
            .unwrap_or_else(|| args.path.clone());
        let rule_path = args.path.clone();
        let this = self.clone();
        let execute_args = args.clone();
        Ok(ToolExecution::Runnable(RunnableToolExecution {
            accesses: Some(vec![read_file_access(&display_path)]),
            display: Some(file_display(&display_path)),
            description: Some(format!("Reading {}", args.path)),
            stop_batch_after_this: None,
            approval_rule: literal_rule_pattern(self.name(), &args.path),
            matches_rule: Some(Arc::new(move |rule_args| {
                matches_glob_rule_subject(rule_args, &rule_path)
            })),
            execute: Arc::new(move |context| {
                let this = this.clone();
                let args = execute_args.clone();
                let source = source.clone();
                Box::pin(async move {
                    let result = this
                        .execution(
                            source.clone(),
                            args.clone(),
                            None,
                            false,
                            Some(context.signal.clone()),
                        )
                        .await?;
                    let mut result = with_attachment_location(result, source.as_ref());
                    result.spill_exempt = Some(true);
                    Ok(this.with_file_memory(result, &args.path).await)
                })
            }),
        }))
    }

    async fn execution(
        &self,
        source: Arc<dyn FileReadSource>,
        args: ReadInput,
        env: Option<HostEnvironmentInfo>,
        event_log: bool,
        signal: Option<AbortSignal>,
    ) -> Result<ExecutableToolResult, WireAsyncError> {
        throw_if_aborted(signal.as_ref())?;
        let result = self
            .execution_inner(source, args.clone(), env, event_log, signal.clone())
            .await;
        match result {
            Ok(result) => Ok(result),
            Err(error) if signal.as_ref().is_some_and(AbortSignal::aborted) => Err(error),
            Err(error) if is_text_decode_error(&error) => Ok(error_result(
                not_utf8_decodable_file_output(&args.path),
            )),
            Err(error) => Ok(error_result(error.to_string())),
        }
    }

    async fn execution_inner(
        &self,
        source: Arc<dyn FileReadSource>,
        args: ReadInput,
        env: Option<HostEnvironmentInfo>,
        event_log: bool,
        signal: Option<AbortSignal>,
    ) -> Result<ExecutableToolResult, WireAsyncError> {
        throw_if_aborted(signal.as_ref())?;
        let HostFileHead { stat, bytes: header } = match source
            .read_head(MEDIA_SNIFF_BYTES as u64)
            .await
        {
            Ok(head) => head,
            Err(error) if is_host_fs_not_found(&error) || is_host_fs_not_directory(&error) => {
                return Ok(error_result(format!(
                    "\"{}\" does not exist.",
                    args.path
                )));
            }
            Err(error) => return Err(error),
        };
        if !stat.is_file {
            return Ok(error_result(format!("\"{}\" is not a file.", args.path)));
        }
        let file_type = detect_file_type(
            source.name(),
            Some(&header),
            DetectFileTypeMode::Text,
        );
        if matches!(file_type.kind, FileKind::Image | FileKind::Video | FileKind::Document) {
            if args.line_offset.is_some() || args.n_lines.is_some() {
                return Ok(error_result(
                    "line_offset and n_lines apply only to text files.".to_string(),
                ));
            }
            let Some(media_ctx) = self.inner.media_read.get_media_read_context() else {
                return Ok(error_result(
                    "Media reading is unavailable in the current runtime.".to_string(),
                ));
            };
            let media_args = MediaReadArgs {
                path: args.path.clone(),
                region: args.region.as_ref().map(media_region),
                full_resolution: args.full_resolution,
            };
            let result = execute_media_read(
                &media_ctx,
                &media_args,
                source.as_ref(),
                env.as_ref(),
                &header,
                signal,
            )
            .await?;
            return Ok(ExecutableToolResult {
                output: match result.output {
                    MediaReadOutput::Text(output) => ExecutableToolOutput::Text(output),
                    MediaReadOutput::Parts(parts) => ExecutableToolOutput::Parts(parts),
                },
                is_error: Some(result.is_error),
                note: result.note,
                ..Default::default()
            });
        }
        if args.region.is_some() || args.full_resolution == Some(true) {
            return Ok(error_result(
                "region and full_resolution apply only to image files.".to_string(),
            ));
        }
        let detection = detect_text_encoding(&header);
        let mut detected_encoding = None;
        let mut lossy_decoding = false;
        let read_lines: ReadLineFactory;
        if !detection.seems_binary && detection.encoding != UtfTextEncoding::Utf8 {
            if stat.size > TRANSCODE_MAX_BYTES as u64 {
                return Ok(error_result(format!(
                    "\"{}\" is {} text but too large to transcode ({} bytes > {}). Convert it to UTF-8 first (e.g. with `iconv`).",
                    args.path,
                    encoding_display_name(detection.encoding),
                    stat.size,
                    TRANSCODE_MAX_BYTES
                )));
            }
            let bytes = source.read_bytes(None).await?;
            let decoded = match decode_utf16_strict(&bytes, detection.encoding) {
                Ok(decoded) => decoded,
                Err(error) if is_text_decode_error(&error) => {
                    lossy_decoding = true;
                    decode_utf_text(&bytes, detection.encoding)
                }
                Err(error) => return Err(error),
            };
            detected_encoding = Some(detection.encoding);
            let decoded = Arc::new(split_lines_keeping_terminator(&decoded));
            read_lines = Arc::new(move || decoded_lines(decoded.clone()));
        } else if file_type.kind == FileKind::Unknown {
            return Ok(error_result(not_readable_file_output(&args.path)));
        } else {
            let source = source.clone();
            read_lines = Arc::new(move || source.read_lines());
        }
        let limits = self.limits();
        let request = ReadRequest {
            args: args.clone(),
            max_chars: args
                .max_chars
                .unwrap_or(limits.0)
                .min(limits.1),
            token_limit: limits.1.div_ceil(4),
            max_chars_limit: limits.1,
            detected_encoding,
            lossy_decoding,
            event_log,
        };
        let line_offset = args.line_offset.unwrap_or(1);
        if line_offset >= 0 {
            return self.read_forward(read_lines(), request).await;
        }
        let rereads_file = detected_encoding.is_none()
            && args
                .n_lines
                .is_some_and(|requested| requested < line_offset.unsigned_abs() as usize);
        let result = self.read_tail(read_lines, request).await?;
        if result.is_error != Some(true) && rereads_file {
            let current_stat = source.stat().await?;
            if !current_stat.is_file
                || current_stat.size != stat.size
                || current_stat.mtime_ms != stat.mtime_ms
                || current_stat.ino != stat.ino
            {
                return Ok(error_result(
                    "File changed while reading its tail. Retry Read with the updated file.".to_string(),
                ));
            }
        }
        Ok(result)
    }

    async fn read_forward(
        &self,
        mut lines: HostLineStream,
        request: ReadRequest,
    ) -> Result<ExecutableToolResult, WireAsyncError> {
        let args = request.args.clone();
        let max_chars = request.max_chars;
        let line_offset = args.line_offset.unwrap_or(1);
        let column_offset = args.column_offset.unwrap_or(0);
        let requested_lines = args
            .n_lines
            .and_then(|value| i64::try_from(value).ok())
            .unwrap_or(i64::MAX);
        let mut selected_entries = Vec::new();
        let mut flags = LineEndingFlags::default();
        let mut current_line_no = 0i64;
        let mut collection_closed = false;
        let mut minimum_chars = 0usize;
        while let Some(raw_line) = lines.next().await {
            let raw_line = raw_line?;
            if contains_nul_byte(&raw_line) {
                return Ok(error_result(not_readable_file_output(&args.path)));
            }
            current_line_no += 1;
            update_line_ending_flags(&mut flags, &raw_line);
            if collection_closed || current_line_no < line_offset {
                continue;
            }
            if selected_entries.len() as i64 >= requested_lines {
                collection_closed = true;
                continue;
            }
            let raw_content = strip_trailing_lf(&raw_line);
            let content_chars = utf16_len(&raw_content)
                .saturating_sub(usize::from(raw_content.ends_with('\r')))
                .saturating_sub(if current_line_no == line_offset {
                    column_offset
                } else {
                    0
                });
            let line_chars = current_line_no.to_string().len()
                + 1
                + content_chars
                + if selected_entries.is_empty() { 0 } else { 1 };
            if minimum_chars.saturating_add(line_chars) > max_chars
                && !selected_entries.is_empty()
            {
                collection_closed = true;
                continue;
            }
            selected_entries.push(ReadLineEntry {
                line_no: current_line_no,
                raw_content,
            });
            minimum_chars = minimum_chars.saturating_add(line_chars);
            if selected_entries.len() as i64 >= requested_lines || minimum_chars >= max_chars {
                collection_closed = true;
            }
        }
        let line_ending_style = line_ending_style_from_flags(flags);
        let mut rendered_lines = selected_entries
            .iter()
            .map(|entry| render_line(entry, line_ending_style))
            .collect::<Vec<_>>();
        if column_offset > 0 {
            let prefix = format!("{line_offset}\t");
            let text = rendered_lines
                .first()
                .map(|line| utf16_slice(line, utf16_len(&prefix), utf16_len(line)));
            let Some(text) = text else {
                return Ok(error_result(format!(
                    "column_offset={column_offset} is past the end of the starting line {line_offset}. Read the line from column 0 to inspect its current contents."
                )));
            };
            if column_offset > utf16_len(&text) {
                return Ok(error_result(format!(
                    "column_offset={column_offset} is past the end of the starting line {line_offset}. Read the line from column 0 to inspect its current contents."
                )));
            }
            if splits_surrogate_pair(&text, column_offset) {
                return Ok(error_result(format!(
                    "column_offset={column_offset} splits a Unicode character in line {line_offset}. Use a character boundary or the Next Read arguments."
                )));
            }
            rendered_lines[0] = format!(
                "{prefix}{}",
                utf16_slice(&text, column_offset, utf16_len(&text))
            );
        }
        Ok(self.finish_page(ReadPage {
            request,
            rendered_lines,
            start_line: line_offset,
            range_start: line_offset,
            range_end: current_line_no.min(
                line_offset
                    .saturating_add(requested_lines)
                    .saturating_sub(1),
            ),
            total_lines: current_line_no,
            from_tail: false,
            line_ending_style,
        }))
    }

    fn finish_page(&self, page: ReadPage) -> ExecutableToolResult {
        let args = page.request.args.clone();
        let max_chars = page.request.max_chars as i64;
        let token_limit = page.request.token_limit as i64;
        let max_chars_limit = page.request.max_chars_limit;
        let mut first = 0usize;
        let mut end = page.rendered_lines.len();
        let mut content_chars = page
            .rendered_lines
            .iter()
            .map(|line| utf16_len(line) as i64 + 1)
            .sum::<i64>()
            - 1;
        let line_counts = page
            .rendered_lines
            .iter()
            .map(|line| count_token_chars(line))
            .collect::<Vec<_>>();
        let mut content_ascii = line_counts
            .iter()
            .map(|counts| counts.ascii as i64)
            .sum::<i64>();
        let mut content_non_ascii = line_counts
            .iter()
            .map(|counts| counts.non_ascii as i64)
            .sum::<i64>();
        let first_column = if page.from_tail {
            0
        } else {
            args.column_offset.unwrap_or(0)
        };
        let first_prefix = format!("{}\t", page.start_line);
        let first_text = page
            .rendered_lines
            .first()
            .map(|line| utf16_slice(line, utf16_len(&first_prefix), utf16_len(line)))
            .unwrap_or_default();
        let mut fragment_end = None;
        loop {
            let count = end - first;
            let start_line = page.start_line + first as i64;
            let end_line = page.start_line + end as i64 - 1;
            let line_incomplete = fragment_end.is_some();
            let complete = page.range_start > page.range_end
                || (count > 0
                    && start_line == page.range_start
                    && end_line == page.range_end
                    && !line_incomplete);
            let mut summary = vec![
                if count > 0 {
                    format!(
                        "{count} {} read from file starting from line {start_line}.",
                        if count == 1 { "line" } else { "lines" }
                    )
                } else {
                    "No lines read from file.".to_string()
                },
                format!("Total lines in file: {}.", page.total_lines),
                if complete {
                    "Requested range complete.".to_string()
                } else {
                    "Character limit reached.".to_string()
                },
                format!("Effective max_chars: {max_chars}."),
            ];
            if !line_incomplete
                && ((count > 0 && end_line == page.total_lines)
                    || page.range_start > page.total_lines)
            {
                summary.push("End of file reached.".to_string());
            }
            let mut notices = Vec::new();
            if count > 0
                && !page.from_tail
                && (first_column > 0 || fragment_end.is_some())
            {
                let fragment_length = fragment_end.unwrap_or_else(|| utf16_len(&first_text));
                notices.push(format!(
                    "Line {start_line} fragment: columns [{first_column}, {}) of {}. {}",
                    first_column + fragment_length,
                    first_column + utf16_len(&first_text),
                    if line_incomplete {
                        "Line continues."
                    } else {
                        "Line complete."
                    }
                ));
            }
            if args
                .max_chars
                .is_some_and(|requested| requested > max_chars_limit)
            {
                notices.push(format!(
                    "Requested max_chars={} was capped at the configured maximum {max_chars_limit}.",
                    args.max_chars.unwrap()
                ));
            }
            if !complete && (count > 0 || page.from_tail) {
                let next_start = if page.from_tail {
                    page.range_start
                } else if line_incomplete {
                    start_line
                } else {
                    end_line + 1
                };
                let next_end = if page.from_tail && count > 0 {
                    start_line - 1
                } else {
                    page.range_end
                };
                let next_column = fragment_end.map(|fragment| first_column + fragment);
                let next_lines = (page.from_tail || args.n_lines.is_some())
                    .then_some(next_end - next_start + 1);
                notices.push(format!(
                    "Next Read: {}",
                    next_read_json(
                        &args.path,
                        next_start,
                        next_column,
                        next_lines,
                        page.request.max_chars,
                    )
                ));
                if count > 0 {
                    notices.push(
                        "If the answer may be further on, continue with Next Read or find the section with Grep instead of answering from this page alone.".to_string(),
                    );
                }
            }
            if page.request.event_log {
                notices.push(
                    "Kimi Code agent event log: read one record at a time (n_lines=1); follow Next Read for the rest of a longer record or extract fields with Bash.".to_string(),
                );
            }
            if page.line_ending_style == LineEndingStyle::Mixed {
                notices.push(
                    "Mixed or lone carriage-return line endings are shown as \\r. Use exact \\r\\n or \\r escapes in Edit.old_string for those lines.".to_string(),
                );
            }
            if let Some(encoding) = page.request.detected_encoding {
                notices.push(format!(
                    "Detected file encoding: {}; content transcoded to UTF-8 for display. Edit and Write expect UTF-8 — convert the file's encoding first (e.g. `iconv` via Bash).",
                    encoding_display_name(encoding)
                ));
            }
            if page.request.lossy_decoding {
                notices.push(
                    "Lossy UTF-16 decoding: malformed sequences were replaced with U+FFFD. The decoded text may differ from the original file.".to_string(),
                );
            }
            let _whole_file = complete
                && count > 0
                && first_column == 0
                && start_line == 1
                && end_line == page.total_lines;
            let note = format!("<system>{}</system>", [summary, notices].concat().join(" "));
            let rendered_chars = if count == 0 {
                rendered_model_chars("", Some(note.clone()), None) as i64
            } else {
                content_chars + 1 + utf16_len(&note) as i64
            };
            let note_counts = count_token_chars(&note);
            let over_tokens = if count == 0 {
                0
            } else {
                tokens_for_char_counts(TokenCharCounts {
                    ascii: (content_ascii
                        + count as i64
                        - 1
                        + 1
                        + note_counts.ascii as i64) as u64,
                    non_ascii: (content_non_ascii + note_counts.non_ascii as i64) as u64,
                }) as i64
                    - token_limit
            };
            if rendered_chars <= max_chars
                && over_tokens > 0
                && count > 1
                && first_column == 0
                && (args.n_lines.is_some() || page.from_tail)
            {
                return range_too_large(&page, &line_counts, token_limit);
            }
            if rendered_chars <= max_chars
                && over_tokens <= 0
                && (complete || count > 0)
            {
                return ExecutableToolResult {
                    output: if let Some(fragment_end) = fragment_end {
                        format!(
                            "{first_prefix}{}",
                            utf16_slice(&first_text, 0, fragment_end)
                        )
                    } else {
                        page.rendered_lines[first..end].join("\n")
                    }
                    .into(),
                    note: Some(note),
                    truncated: (!complete).then_some(true),
                    ..Default::default()
                };
            }
            if count == 1 && !page.from_tail {
                let previous_end = fragment_end.unwrap_or_else(|| utf16_len(&first_text));
                let char_end = (previous_end as i64 - (rendered_chars - max_chars))
                    .min(previous_end as i64 - 1);
                let mut next_fragment = char_end;
                if over_tokens > 0 {
                    next_fragment = next_fragment.min(
                        cut_for_tokens(&first_text, previous_end, over_tokens) as i64,
                    );
                }
                if next_fragment >= 0 && splits_surrogate_pair(&first_text, next_fragment as usize) {
                    next_fragment -= 1;
                }
                if next_fragment <= 0 {
                    return error_result(format!(
                        "max_chars={max_chars} is too small for file text and the Read status. Increase max_chars."
                    ));
                }
                fragment_end = Some(next_fragment as usize);
                content_chars = utf16_len(&first_prefix) as i64 + next_fragment;
                let counts = count_token_chars(&format!(
                    "{first_prefix}{}",
                    utf16_slice(&first_text, 0, next_fragment as usize)
                ));
                content_ascii = counts.ascii as i64;
                content_non_ascii = counts.non_ascii as i64;
                continue;
            }
            if count == 0 {
                if !complete {
                    let recovery = ExecutableToolResult {
                        output: "No complete line fits. Continue with the forward Next Read.".into(),
                        is_error: Some(true),
                        note: Some(note.clone()),
                        truncated: Some(true),
                        ..Default::default()
                    };
                    let recovery_chars = rendered_model_chars(
                        "No complete line fits. Continue with the forward Next Read.",
                        Some(note),
                        Some(true),
                    ) as i64;
                    if recovery_chars <= max_chars {
                        return recovery;
                    }
                }
                return error_result(format!(
                    "max_chars={max_chars} is too small for the Read status. Increase max_chars."
                ));
            }
            let mut remaining_tokens = if rendered_chars <= max_chars && over_tokens > 0 {
                over_tokens as f64
            } else {
                0.0
            };
            loop {
                let dropped = if page.from_tail {
                    let dropped = first;
                    first += 1;
                    dropped
                } else {
                    end -= 1;
                    end
                };
                let counts = line_counts[dropped];
                content_chars -= utf16_len(&page.rendered_lines[dropped]) as i64 + 1;
                content_ascii -= counts.ascii as i64;
                content_non_ascii -= counts.non_ascii as i64;
                remaining_tokens -= counts.non_ascii as f64 + (counts.ascii as f64 + 1.0) / 4.0;
                if remaining_tokens <= 0.0
                    || end - first <= if page.from_tail { 0 } else { 1 }
                {
                    break;
                }
            }
        }
    }

    async fn read_tail(
        &self,
        read_lines: ReadLineFactory,
        request: ReadRequest,
    ) -> Result<ExecutableToolResult, WireAsyncError> {
        let args = request.args.clone();
        let max_chars = request.max_chars;
        let line_offset = args.line_offset.unwrap_or(1);
        let requested_lines = args
            .n_lines
            .and_then(|value| i64::try_from(value).ok())
            .unwrap_or(i64::MAX);
        let tail_count = line_offset.unsigned_abs() as usize;
        let single_pass = requested_lines >= tail_count as i64;
        let mut entries: Vec<Option<ReadTailEntry>> = Vec::new();
        let mut first = 0usize;
        let mut chars = 0usize;
        let mut flags = LineEndingFlags::default();
        let mut total_lines = 0i64;
        let mut lines = read_lines();
        while let Some(raw_line) = lines.next().await {
            let raw_line = raw_line?;
            if contains_nul_byte(&raw_line) {
                return Ok(error_result(not_readable_file_output(&args.path)));
            }
            total_lines += 1;
            update_line_ending_flags(&mut flags, &raw_line);
            if single_pass {
                retain_tail_line(
                    &mut entries,
                    &mut first,
                    &mut chars,
                    &raw_line,
                    total_lines,
                    tail_count,
                    max_chars,
                );
            }
        }
        let range_start = (total_lines + line_offset + 1).max(1);
        let range_end = total_lines.min(
            range_start
                .saturating_add(requested_lines)
                .saturating_sub(1),
        );
        let line_ending_style = line_ending_style_from_flags(flags);
        if !single_pass {
            let mut current_line = 0i64;
            let mut lines = read_lines();
            while let Some(raw_line) = lines.next().await {
                let raw_line = raw_line?;
                current_line += 1;
                if current_line > range_end {
                    break;
                }
                if current_line < range_start {
                    continue;
                }
                if contains_nul_byte(&raw_line) {
                    return Ok(error_result(not_readable_file_output(&args.path)));
                }
                retain_tail_line(
                    &mut entries,
                    &mut first,
                    &mut chars,
                    &raw_line,
                    current_line,
                    tail_count,
                    max_chars,
                );
            }
            if current_line < range_end || current_line > total_lines {
                return Ok(error_result(
                    "File changed while reading its tail. Retry Read with the updated file.".to_string(),
                ));
            }
        }
        let selected = entries[first..]
            .iter()
            .map(|entry| {
                let entry = entry.as_ref().expect("retained tail entry is populated");
                render_line(
                    &ReadLineEntry {
                        line_no: entry.line_no,
                        raw_content: entry.raw_content.clone(),
                    },
                    line_ending_style,
                )
            })
            .collect::<Vec<_>>();
        Ok(self.finish_page(ReadPage {
            request,
            start_line: range_end - selected.len() as i64 + 1,
            range_start,
            range_end,
            total_lines,
            rendered_lines: selected,
            from_tail: true,
            line_ending_style,
        }))
    }

    async fn with_file_memory(
        &self,
        mut result: ExecutableToolResult,
        path: &str,
    ) -> ExecutableToolResult {
        if result.is_error == Some(true) {
            return result;
        }
        let Some(service) = self.inner.git_memory.as_ref() else {
            return result;
        };
        let Ok(block) = service.file_block(path).await else {
            return result;
        };
        if block.is_empty() {
            return result;
        }
        match &mut result.output {
            ExecutableToolOutput::Text(output) => {
                *output = format!("{output}\n{block}");
            }
            ExecutableToolOutput::Parts(parts) => parts.push(ContentPart::Text {
                text: format!("\n{block}"),
                meta: None,
            }),
        }
        result
    }
}

fn parse_read_input(input: Value) -> Result<ReadInput, WireAsyncError> {
    let object = input
        .as_object()
        .ok_or_else(|| invalid_input("Read arguments must be an object"))?;
    let path = match object.get("path") {
        None => String::new(),
        Some(Value::String(path)) => path.clone(),
        Some(_) => return Err(invalid_input("Read path must be a string")),
    };
    let line_offset = match object.get("line_offset") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_i64()
                .ok_or_else(|| invalid_input("line_offset must be an integer"))?,
        ),
    };
    if line_offset == Some(0) {
        return Err(invalid_input("line_offset must not be 0"));
    }
    let column_offset = parse_optional_usize(object.get("column_offset"), "column_offset")?;
    let n_lines = parse_optional_positive_usize(object.get("n_lines"), "n_lines")?;
    let max_chars = parse_optional_positive_usize(object.get("max_chars"), "max_chars")?;
    let full_resolution = match object.get("full_resolution") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(value)) => Some(*value),
        Some(_) => return Err(invalid_input("full_resolution must be a boolean")),
    };
    let region = match object.get("region") {
        None | Some(Value::Null) => None,
        Some(Value::Object(region)) => {
            let x = parse_required_u32(region.get("x"), "region.x")?;
            let y = parse_required_u32(region.get("y"), "region.y")?;
            let width = parse_required_u32(region.get("width"), "region.width")?;
            let height = parse_required_u32(region.get("height"), "region.height")?;
            if width == 0 {
                return Err(invalid_input("region.width must be a positive integer"));
            }
            if height == 0 {
                return Err(invalid_input("region.height must be a positive integer"));
            }
            Some(ReadRegion {
                x,
                y,
                width,
                height,
            })
        }
        Some(_) => return Err(invalid_input("region must be an object")),
    };
    Ok(ReadInput {
        path,
        line_offset,
        column_offset,
        n_lines,
        region,
        full_resolution,
        max_chars,
    })
}

fn parse_optional_usize(value: Option<&Value>, name: &str) -> Result<Option<usize>, WireAsyncError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| invalid_input(format!("{name} must be a non-negative integer"))),
    }
}

fn parse_optional_positive_usize(
    value: Option<&Value>,
    name: &str,
) -> Result<Option<usize>, WireAsyncError> {
    match parse_optional_usize(value, name)? {
        Some(0) => Err(invalid_input(format!("{name} must be a positive integer"))),
        value => Ok(value),
    }
}

fn parse_required_u32(value: Option<&Value>, name: &str) -> Result<u32, WireAsyncError> {
    value
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| invalid_input(format!("{name} must be a non-negative integer")))
}

fn invalid_input(message: impl Into<String>) -> WireAsyncError {
    Box::new(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}

fn resolve_read_path(
    path: &str,
    view: &EnvironmentWorkspaceView,
    workspace: &WorkspaceConfig,
) -> Result<String, WireAsyncError> {
    let path_class = path_class_from_name(&view.host.path_class);
    let path = view
        .shell_path_bridge
        .as_ref()
        .map_or_else(|| path.to_string(), |bridge| bridge.from_shell_path(path));
    let roots = std::iter::once(workspace.workspace_dir.clone())
        .chain(workspace.additional_dirs.iter().cloned())
        .collect::<Vec<_>>();
    let access = resolve_path_access(
        &path,
        &workspace.workspace_dir,
        &roots,
        Some(&view.host.home_dir),
        path_class,
        PathAccessOperation::Read,
        &WorkspaceAccessPolicy::default(),
    )?;
    Ok(access.path)
}

fn path_class_from_name(path_class: &str) -> PathClass {
    if path_class == "win32" {
        PathClass::Win32
    } else {
        PathClass::Posix
    }
}

fn pinned_generation(environment: &dyn crate::environment::environment::Environment) -> Option<String> {
    (environment.status() == EnvironmentStatus::Ready)
        .then(|| environment.identity().generation)
}

fn file_display(path: &str) -> ToolInputDisplay {
    ToolInputDisplay::FileIo {
        operation: FileIoOperation::Read,
        path: path.to_string(),
        detail: None,
        content: None,
        before: None,
        after: None,
    }
}

struct SharedReadError(SharedError);

impl fmt::Debug for SharedReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0.to_string())
    }
}

impl fmt::Display for SharedReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

impl Error for SharedReadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.0.as_ref())
    }
}

fn throw_if_aborted(signal: Option<&AbortSignal>) -> Result<(), WireAsyncError> {
    if !signal.is_some_and(AbortSignal::aborted) {
        return Ok(());
    }
    if let Some(reason) = signal.and_then(AbortSignal::reason) {
        return Err(Box::new(SharedReadError(reason)));
    }
    Err(Box::new(abort_error()))
}

fn media_region(region: &ReadRegion) -> MediaReadRegion {
    MediaReadRegion {
        x: region.x as f64,
        y: region.y as f64,
        width: region.width as f64,
        height: region.height as f64,
    }
}

fn error_result(output: String) -> ExecutableToolResult {
    ExecutableToolResult {
        output: output.into(),
        is_error: Some(true),
        ..Default::default()
    }
}

fn strip_trailing_lf(line: &str) -> String {
    line.strip_suffix('\n').unwrap_or(line).to_string()
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn utf16_byte_index(text: &str, offset: usize) -> Option<usize> {
    if offset == 0 {
        return Some(0);
    }
    let mut units = 0usize;
    for (index, character) in text.char_indices() {
        if units == offset {
            return Some(index);
        }
        units += character.len_utf16();
        if units > offset {
            return None;
        }
    }
    (units == offset).then_some(text.len())
}

fn utf16_slice(text: &str, start: usize, end: usize) -> String {
    let length = utf16_len(text);
    let start = start.min(length);
    let end = end.min(length).max(start);
    let Some(start_byte) = utf16_byte_index(text, start) else {
        return String::new();
    };
    let Some(end_byte) = utf16_byte_index(text, end) else {
        return String::new();
    };
    text[start_byte..end_byte].to_string()
}

fn splits_surrogate_pair(text: &str, offset: usize) -> bool {
    let units = text.encode_utf16().collect::<Vec<_>>();
    offset > 0
        && offset < units.len()
        && (0xd800..=0xdbff).contains(&units[offset - 1])
        && (0xdc00..=0xdfff).contains(&units[offset])
}

fn cut_for_tokens(text: &str, end: usize, overflow: i64) -> usize {
    let units = text.encode_utf16().collect::<Vec<_>>();
    let mut index = end.min(units.len());
    let mut removed = 0.0;
    while index > 0 && removed < overflow as f64 {
        let code = units[index - 1];
        let step = if (0xdc00..=0xdfff).contains(&code) && index > 1 {
            2
        } else {
            1
        };
        index = index.saturating_sub(step);
        removed += if code <= 127 { 0.25 } else { 1.0 };
    }
    index
}

fn range_too_large(
    page: &ReadPage,
    line_counts: &[TokenCharCounts],
    token_limit: i64,
) -> ExecutableToolResult {
    let indexes = (0..page.rendered_lines.len()).collect::<Vec<_>>();
    let indexes = if page.from_tail {
        indexes.into_iter().rev().collect::<Vec<_>>()
    } else {
        indexes
    };
    let mut ascii = 0i64;
    let mut non_ascii = 0i64;
    let mut fit = 0i64;
    for index in indexes {
        let counts = line_counts[index];
        ascii += counts.ascii as i64 + i64::from(fit > 0);
        non_ascii += counts.non_ascii as i64;
        if tokens_for_char_counts(TokenCharCounts {
            ascii: ascii as u64,
            non_ascii: non_ascii as u64,
        }) as i64
            > token_limit - RANGE_STATUS_TOKENS as i64
        {
            break;
        }
        fit += 1;
    }
    let lines = fit.max(1);
    let line_offset = if page.from_tail {
        page.range_end - lines + 1
    } else {
        page.range_start
    };
    error_result(format!(
        "Lines {}-{} are more than one Read can return (about {token_limit} estimated tokens). Read at most {lines} lines at a time, for example {}, or find the section with Grep.",
        page.range_start,
        page.range_end,
        range_suggestion_json(&page.request.args.path, line_offset, lines)
    ))
}

fn range_suggestion_json(path: &str, line_offset: i64, lines: i64) -> String {
    format!(
        "{{\"path\":{},\"line_offset\":{line_offset},\"n_lines\":{lines}}}",
        serde_json::to_string(path).expect("path is JSON serializable")
    )
}

fn next_read_json(
    path: &str,
    line_offset: i64,
    column_offset: Option<usize>,
    n_lines: Option<i64>,
    max_chars: usize,
) -> String {
    let mut output = format!(
        "{{\"path\":{},\"line_offset\":{line_offset}",
        serde_json::to_string(path).expect("path is JSON serializable")
    );
    if let Some(column_offset) = column_offset {
        output.push_str(&format!(",\"column_offset\":{column_offset}"));
    }
    if let Some(n_lines) = n_lines {
        output.push_str(&format!(",\"n_lines\":{n_lines}"));
    }
    output.push_str(&format!(",\"max_chars\":{max_chars}}}"));
    output
}

fn update_line_ending_flags(flags: &mut LineEndingFlags, text: &str) {
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == 13 {
            if bytes.get(index + 1) == Some(&10) {
                flags.has_crlf = true;
                index += 2;
                continue;
            }
            flags.has_lone_cr = true;
        } else if bytes[index] == 10 {
            flags.has_lf = true;
        }
        index += 1;
    }
}

fn line_ending_style_from_flags(flags: LineEndingFlags) -> LineEndingStyle {
    if flags.has_lone_cr || (flags.has_crlf && flags.has_lf) {
        return LineEndingStyle::Mixed;
    }
    if flags.has_crlf {
        return LineEndingStyle::CrLf;
    }
    LineEndingStyle::Lf
}

fn render_line(entry: &ReadLineEntry, line_ending_style: LineEndingStyle) -> String {
    let model_content = if line_ending_style == LineEndingStyle::CrLf
        && entry.raw_content.ends_with('\r')
    {
        entry.raw_content[..entry.raw_content.len() - 1].to_string()
    } else {
        entry.raw_content.clone()
    };
    let rendered_content = if line_ending_style == LineEndingStyle::Mixed {
        make_carriage_returns_visible(&model_content)
    } else {
        model_content
    };
    format!("{}\t{rendered_content}", entry.line_no)
}

fn is_text_decode_error(error: &WireAsyncError) -> bool {
    let unwrapped = unwrap_error_cause(error.as_ref());
    if unwrapped
        .downcast_ref::<Error2>()
        .is_some_and(|error| error.code().as_str() == "ERR_ENCODING_INVALID_ENCODED_DATA")
    {
        return true;
    }
    TEXT_DECODE_ERROR.is_match(&unwrapped.to_string())
}

fn contains_nul_byte(text: &str) -> bool {
    text.contains('\0')
}

fn encoding_display_name(encoding: UtfTextEncoding) -> &'static str {
    match encoding {
        UtfTextEncoding::Utf16Le => "UTF-16 LE",
        UtfTextEncoding::Utf16Be => "UTF-16 BE",
        UtfTextEncoding::Utf8 => "UTF-8",
    }
}

fn decoded_lines(lines: Arc<Vec<String>>) -> HostLineStream {
    Box::pin(stream::iter(
        lines
            .iter()
            .cloned()
            .map(Ok::<String, WireAsyncError>)
            .collect::<Vec<_>>(),
    ))
}

fn not_readable_file_output(path: &str) -> String {
    format!("\"{path}\" is not readable as UTF-8 text. Only text files can be read.")
}

fn not_utf8_decodable_file_output(path: &str) -> String {
    format!(
        "\"{path}\" is not valid UTF-8 or UTF-16 text. Only UTF-8 and UTF-16 text files can be read; for other encodings (e.g. GBK), convert the file to UTF-8 first (e.g. with `iconv`)."
    )
}

fn decode_utf16_strict(
    bytes: &[u8],
    encoding: UtfTextEncoding,
) -> Result<String, WireAsyncError> {
    let bytes = match encoding {
        UtfTextEncoding::Utf16Le => bytes.strip_prefix(&[0xff, 0xfe]).unwrap_or(bytes),
        UtfTextEncoding::Utf16Be => bytes.strip_prefix(&[0xfe, 0xff]).unwrap_or(bytes),
        UtfTextEncoding::Utf8 => bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes),
    };
    if bytes.len() % 2 != 0 {
        return Err(text_decode_error());
    }
    let units = bytes
        .chunks_exact(2)
        .map(|chunk| match encoding {
            UtfTextEncoding::Utf16Be => u16::from_be_bytes([chunk[0], chunk[1]]),
            _ => u16::from_le_bytes([chunk[0], chunk[1]]),
        })
        .collect::<Vec<_>>();
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .map_err(|_| text_decode_error())
}

fn text_decode_error() -> WireAsyncError {
    Box::new(io::Error::new(
        io::ErrorKind::InvalidData,
        "encoded data was not valid",
    ))
}

fn rendered_model_chars(output: &str, note: Option<String>, is_error: Option<bool>) -> usize {
    render_tool_result_for_model(&RenderableToolResult {
        output: RenderableToolOutput::Text(output.to_string()),
        note,
        is_error,
        duration_ms: None,
    })
    .iter()
    .map(|part| match part {
        ContentPart::Text { text, .. } => utf16_len(text),
        _ => 0,
    })
    .sum()
}

fn retain_tail_line(
    entries: &mut Vec<Option<ReadTailEntry>>,
    first: &mut usize,
    chars: &mut usize,
    raw_line: &str,
    line_no: i64,
    tail_count: usize,
    max_chars: usize,
) {
    let raw_content = strip_trailing_lf(raw_line);
    let min_chars = line_no.to_string().len()
        + 2
        + utf16_len(&raw_content)
        - usize::from(raw_content.ends_with('\r'));
    entries.push(Some(ReadTailEntry {
        line_no,
        raw_content,
        min_chars,
    }));
    *chars = chars.saturating_add(min_chars);
    while *first < entries.len()
        && (chars.saturating_sub(1) > max_chars || entries.len() - *first > tail_count)
    {
        *chars = chars.saturating_sub(entries[*first].as_ref().expect("tail entry").min_chars);
        entries[*first] = None;
        *first += 1;
    }
    if *first > 1024 && *first >= entries.len() / 2 {
        *entries = entries[*first..].to_vec();
        *first = 0;
    }
}

fn with_attachment_location(
    mut result: ExecutableToolResult,
    source: &dyn FileReadSource,
) -> ExecutableToolResult {
    if result.is_error != Some(true) || !matches!(&result.output, ExecutableToolOutput::Text(_)) {
        return result;
    }
    let Some(local_path) = source.local_path() else {
        return result;
    };
    let path = serde_json::Value::String(local_path.to_string()).to_string();
    if let ExecutableToolOutput::Text(output) = &mut result.output {
        *output = format!("{output}\nServer-local attachment path: {path}");
    }
    result
}

struct SessionAttachmentFileSource {
    reference: String,
    store: Option<Arc<dyn ISessionMediaStore>>,
    name: String,
    local_path: Option<String>,
}

impl SessionAttachmentFileSource {
    async fn open_initial(
        reference: String,
        store: Option<Arc<dyn ISessionMediaStore>>,
    ) -> Result<Self, WireAsyncError> {
        let mut source = Self {
            reference,
            store,
            name: String::new(),
            local_path: None,
        };
        let initial = source.open().await?;
        source.name = initial.name;
        source.local_path = initial.path;
        Ok(source)
    }

    fn unavailable(&self) -> WireAsyncError {
        io::Error::other(format!(
            "Attachment {} is not available in the current session.",
            serde_json::Value::String(self.reference.clone()).to_string()
        ))
        .into()
    }

    async fn open(&self) -> Result<SessionMediaFile, WireAsyncError> {
        let Some(reference) = parse_daemon_file_url(&self.reference) else {
            return Err(self.unavailable());
        };
        let Some(store) = self.store.as_ref() else {
            return Err(self.unavailable());
        };
        store.open(&reference.file_id).await?.ok_or_else(|| self.unavailable())
    }

    fn byte_stream(&self) -> ByteStream {
        Box::pin(stream::try_unfold(
            AttachmentStreamState {
                source: self.clone_for_stream(),
                stream: None,
                expected: None,
                read: 0,
            },
            |mut state| async move {
                if state.stream.is_none() {
                    let file = state
                        .source
                        .open()
                        .await
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    state.expected = Some(file.size);
                    state.stream = Some((file.stream)(None));
                }
                let next = state
                    .stream
                    .as_mut()
                    .expect("attachment stream is initialized")
                    .next()
                    .await;
                match next {
                    Some(Ok(bytes)) => {
                        state.read += bytes.len() as u64;
                        Ok(Some((bytes, state)))
                    }
                    Some(Err(error)) => Err(io::Error::other(error.to_string())),
                    None if state.read != state.expected.unwrap_or(0) => Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Attachment changed or became unavailable while reading.",
                    )),
                    None => Ok(None),
                }
            },
        ))
    }

    fn clone_for_stream(&self) -> Self {
        Self {
            reference: self.reference.clone(),
            store: self.store.clone(),
            name: self.name.clone(),
            local_path: self.local_path.clone(),
        }
    }
}

struct AttachmentStreamState {
    source: SessionAttachmentFileSource,
    stream: Option<BlobByteStream>,
    expected: Option<u64>,
    read: u64,
}

type ByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, io::Error>> + Send>>;

impl FileReadSource for SessionAttachmentFileSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn local_path(&self) -> Option<&str> {
        self.local_path.as_deref()
    }

    fn stat(&self) -> BoxFuture<'static, Result<HostFileStat, WireAsyncError>> {
        let source = self.clone_for_stream();
        Box::pin(async move {
            let file = source.open().await?;
            Ok(HostFileStat {
                is_file: true,
                is_directory: false,
                size: file.size,
                ..Default::default()
            })
        })
    }

    fn read_head(&self, max_bytes: u64) -> BoxFuture<'static, Result<HostFileHead, WireAsyncError>> {
        let source = self.clone_for_stream();
        Box::pin(async move {
            let stat = source.stat().await?;
            let bytes = source.read_bytes(Some(max_bytes)).await?;
            Ok(HostFileHead { stat, bytes })
        })
    }

    fn read_bytes(&self, n: Option<u64>) -> BoxFuture<'static, Result<Vec<u8>, WireAsyncError>> {
        let source = self.clone_for_stream();
        Box::pin(async move {
            let file = source.open().await?;
            let size = n.unwrap_or(file.size).min(file.size);
            if size == 0 {
                return Ok(Vec::new());
            }
            let mut stream = (file.stream)(Some(SessionMediaReadRange {
                start: 0,
                end: size - 1,
            }));
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                bytes.extend(chunk?);
            }
            if bytes.len() as u64 != size {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Attachment changed or became unavailable while reading.",
                )
                .into());
            }
            Ok(bytes)
        })
    }

    fn read_lines(&self) -> HostLineStream {
        let lines = crate::_base::exec_env::decode_text::read_utf8_lines(
            self.byte_stream(),
            crate::_base::exec_env::decode_text::TextDecodeErrors::Strict,
        );
        Box::pin(lines.map(|line| {
            line.map_err(|error| Box::new(error) as WireAsyncError)
        }))
    }
}
