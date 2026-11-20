use std::sync::{Arc, LazyLock};

use serde_json::{Map, Value};

use super::write::{WriteInput, WriteMode, write_input_schema};
use crate::agent::environment_binding::agent_environment::{
    IAgentEnvironmentService, acquire_or_when_ready, pinned_generation,
};
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::tool::tool_contract::{ToolSideEffect};
use crate::agent::tools::file_memory::{FileMemoryProvider, with_file_memory};
use crate::environment::environment::{EnvironmentPath, EnvironmentWorkspaceRoots};
use crate::environment::environment_workspace_view::EnvironmentWorkspaceView;
use crate::features::plan::plan::{IAgentPlanService, PlanFileTarget};
use crate::features::skill::catalog::types::SkillCatalog as _;
use crate::features::skill::session::skill_catalog::ISessionSkillCatalog;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_fs_errors::{is_host_fs_not_directory, is_host_fs_not_found};
use crate::session::workspace_context::workspace_context::ISessionWorkspaceContext;
use crate::tool::input_schema::to_input_json_schema;
use crate::tool::path_access::{
    PathAccessOperation, PathClass, ResolvePathAccessPathOptions, WorkspaceConfig,
    is_project_local_config_path, is_sensitive_file, is_within_directory, is_within_workspace,
    resolve_path_access_path,
};
use crate::tool::rule_match::{
    PermissionPathMatchOptions, literal_rule_pattern, matches_path_rule_subject,
};
use crate::tool::tool_contract::ToolAccesses;
use crate::tool::tool_input_display::{FileIoOperation, ToolInputDisplay};
use crate::wire::record::WireAsyncError;

const DESCRIPTION: &str = r#"Create, append to, or replace a file entirely.

- Use Write only for new files, complete replacements, or content with little continuity.
- Do not create unsolicited documentation files (`*.md` write-ups, `README`s, summaries) just because a task finished — write one only when the user asks for it, or when a task or project instruction requires it (e.g. the plan-mode plan file, created with Write when plan mode directs you to, or a changeset the repo mandates).
- Read before overwriting an existing file.
- Write ignores the Read/Edit line-number view. NEVER include line prefixes.
- Write outputs content literally, including supplied line endings: \n stays LF, \r\n stays CRLF.
- For new content too large for one call, overwrite the first chunk, then append subsequent chunks. Never chunk Write to modify an existing file.
"#;

static PARAMETERS: LazyLock<Value> = LazyLock::new(|| to_input_json_schema(write_input_schema()));

#[derive(Clone)]
pub struct WriteTool {
    environment: Arc<dyn IAgentEnvironmentService>,
    workspace_ctx: Arc<dyn ISessionWorkspaceContext>,
    skill_catalog: Option<Arc<dyn ISessionSkillCatalog>>,
    git_memory: Option<Arc<dyn FileMemoryProvider>>,
    plan_service: Option<Arc<dyn IAgentPlanService>>,
}

impl WriteTool {
    pub fn new(
        environment: Arc<dyn IAgentEnvironmentService>,
        workspace_ctx: Arc<dyn ISessionWorkspaceContext>,
        skill_catalog: Option<Arc<dyn ISessionSkillCatalog>>,
        git_memory: Option<Arc<dyn FileMemoryProvider>>,
        plan_service: Option<Arc<dyn IAgentPlanService>>,
    ) -> Self {
        Self {
            environment,
            workspace_ctx,
            skill_catalog,
            git_memory,
            plan_service,
        }
    }

    pub fn name(&self) -> &'static str {
        "Write"
    }

    pub fn description(&self) -> &'static str {
        DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        PARAMETERS
            .as_object()
            .cloned()
            .expect("Write input schema is an object")
    }

    pub async fn resolve_execution_value(
        &self,
        input: Value,
    ) -> Result<ToolExecution, WireAsyncError> {
        match parse_write_input(input) {
            Ok(args) => self.resolve_execution(args).await,
            Err(error) => Ok(ToolExecution::Error(ExecutableToolResult::error(error))),
        }
    }

    pub async fn resolve_execution(
        &self,
        args: WriteInput,
    ) -> Result<ToolExecution, WireAsyncError> {
        if let Some(target) = self.resolve_plan_file(&args.path).await {
            return Ok(self.plan_execution(args, target));
        }
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
            let bridge = view.shell_path_bridge.as_ref().map(|bridge| {
                let bridge = bridge.clone();
                move |value: &str| bridge.from_shell_path(value)
            });
            let path = resolve_path_access_path(
                &args.path,
                &ResolvePathAccessPathOptions {
                    path_class,
                    home_dir: Some(&env.home_dir),
                    shell_path_bridge: bridge
                        .as_ref()
                        .map(|bridge| bridge as &dyn Fn(&str) -> String),
                    workspace: &workspace,
                    operation: PathAccessOperation::Write,
                    policy: None,
                    expand_home: None,
                },
            )
            .map_err(|error| -> WireAsyncError {
                Box::new(std::io::Error::other(error.message))
            })?;
            let match_path = path.clone();
            let match_workspace = workspace.workspace_dir.clone();
            let match_home = env.home_dir.clone();
            let this = self.clone();
            let execute_args = args.clone();
            let execute_path = path.clone();
            Ok(ToolExecution::Runnable(RunnableToolExecution {
                accesses: Some(ToolAccesses::write_file(&path)),
                display: Some(ToolInputDisplay::FileIo {
                    operation: FileIoOperation::Write,
                    path: path.clone(),
                    detail: None,
                    content: Some(args.content.clone()),
                    before: None,
                    after: None,
                }),
                description: Some(format!("Writing {}", args.path)),
                stop_batch_after_this: None,
                approval_rule: literal_rule_pattern(self.name(), &path),
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
                execute: Arc::new(move |_| {
                    let this = this.clone();
                    let args = execute_args.clone();
                    let path = execute_path.clone();
                    let workspace = workspace.clone();
                    let expected_generation = expected_generation.clone();
                    Box::pin(async move {
                        let lease = acquire_or_when_ready(this.environment.as_ref(), &["fs"]).await?;
                        let result = async {
                            if expected_generation.as_ref().is_some_and(|expected| {
                                lease.environment.identity().generation != *expected
                            }) {
                                return Ok(ExecutableToolResult::error(
                                    "Environment changed before execution. Retry the tool call.",
                                ));
                            }
                            this.execution(
                                lease.environment.fs(),
                                &args,
                                &path,
                                &workspace,
                                path_class,
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

    async fn resolve_plan_file(&self, path: &str) -> Option<PlanFileTarget> {
        match self.plan_service.as_ref() {
            Some(service) => service.resolve_file(path, None).await,
            None => None,
        }
    }

    fn plan_execution(&self, args: WriteInput, target: PlanFileTarget) -> ToolExecution {
        let path = target.path.to_string_lossy().into_owned();
        let path_class = PathClass::from_str(&target.path_class).unwrap_or(PathClass::Posix);
        let match_path = path.clone();
        let match_cwd = target.environment_path.dirname(&path);
        let this = self.clone();
        let execute_target = target.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: Some(ToolAccesses::write_file(&path)),
            display: Some(ToolInputDisplay::FileIo {
                operation: FileIoOperation::Write,
                path: path.clone(),
                detail: None,
                content: Some(args.content.clone()),
                before: None,
                after: None,
            }),
            description: Some(format!("Writing {}", args.path)),
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
                let args = args.clone();
                let target = execute_target.clone();
                Box::pin(async move {
                    let current = this.resolve_plan_file(&target.path.to_string_lossy()).await;
                    let Some(current) = current else {
                        return Ok(ExecutableToolResult::error(
                            "The active plan changed before execution. Retry the tool call.",
                        ));
                    };
                    if current.path != target.path {
                        return Ok(ExecutableToolResult::error(
                            "The active plan changed before execution. Retry the tool call.",
                        ));
                    }
                    let path = current.path.to_string_lossy().into_owned();
                    let workspace = WorkspaceConfig {
                        workspace_dir: current.environment_path.dirname(&path),
                        additional_dirs: Vec::new(),
                    };
                    let path_class = PathClass::from_str(&current.path_class)
                        .unwrap_or(PathClass::Posix);
                    this.execution(
                        current.fs,
                        &args,
                        &path,
                        &workspace,
                        path_class,
                    )
                    .await
                })
            }),
        })
    }

    async fn execution(
        &self,
        fs: Arc<dyn IHostFileSystem>,
        args: &WriteInput,
        safe_path: &str,
        workspace: &WorkspaceConfig,
        path_class: PathClass,
    ) -> Result<ExecutableToolResult, WireAsyncError> {
        if let Err(error) = assert_real_path_write_target(fs.clone(), safe_path, workspace, path_class).await {
            if let Some(security) = error.downcast_ref::<RealPathSecurityError>() {
                return Ok(ExecutableToolResult::error(security.message.clone()));
            }
            return Err(error);
        }
        let mode = args.mode.unwrap_or(WriteMode::Overwrite);
        match write_content(fs.clone(), safe_path, &args.content, mode).await {
            Ok(()) => {}
            Err(error) if is_host_fs_not_found(&error) || is_host_fs_not_directory(&error) => {
                if let Some(parent_error) = self
                    .ensure_parent_directory(fs.clone(), safe_path, path_class)
                    .await
                {
                    return Ok(ExecutableToolResult::error(parent_error));
                }
                if let Err(error) = write_content(fs.clone(), safe_path, &args.content, mode).await {
                    if is_host_fs_not_found(&error) {
                        return Ok(ExecutableToolResult::error(format!(
                            "Failed to write {}: parent directory does not exist.",
                            args.path
                        )));
                    }
                    return Ok(ExecutableToolResult::error(error.to_string()));
                }
            }
            Err(error) => return Ok(ExecutableToolResult::error(error.to_string())),
        }
        let bytes_written = args.content.len();
        let output = format!(
            "{} {bytes_written} bytes to {}",
            if mode == WriteMode::Append {
                "Appended"
            } else {
                "Wrote"
            },
            args.path
        );
        let result = crate::turn_loop::types::ExecutableToolResult {
            stop_turn: false,
            content: output,
            is_error: false,
            note: None,
            delivery: None,
            parts: Vec::new(),
            display: None,
            spill: None,
        };
        Ok(from_legacy_result(
            with_file_memory(result, &args.path, self.git_memory.as_deref()).await,
        ))
    }

    async fn ensure_parent_directory(
        &self,
        fs: Arc<dyn IHostFileSystem>,
        safe_path: &str,
        path_class: PathClass,
    ) -> Option<String> {
        let path = EnvironmentPath::new(path_class_name(path_class));
        let parent = path.dirname(safe_path);
        match fs.stat(&parent).await {
            Ok(stat) => {
                if !stat.is_directory {
                    return Some(format!("Parent path is not a directory: {parent}."));
                }
            }
            Err(error) => {
                if is_host_fs_not_found(&error) {
                    return match fs.mkdir(&parent, true).await {
                        Ok(()) => None,
                        Err(error) => Some(error.to_string()),
                    };
                }
            }
        }
        None
    }
}

impl ExecutableTool for WriteTool {
    fn side_effect(&self) -> Option<ToolSideEffect> {
        Some(ToolSideEffect::Write)
    }

    fn name(&self) -> &str {
        WriteTool::name(self)
    }

    fn description(&self) -> &str {
        WriteTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        WriteTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> futures_util::future::BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let this = self.clone();
        Box::pin(async move { this.resolve_execution_value(input).await })
    }
}

fn parse_write_input(input: Value) -> Result<WriteInput, String> {
    let object = input
        .as_object()
        .ok_or_else(|| "Invalid arguments for Write: expected an object".to_string())?;
    let path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "Invalid arguments for Write: path must be a string".to_string())?;
    let content = object
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| "Invalid arguments for Write: content must be a string".to_string())?;
    let mode = match object.get("mode") {
        None => None,
        Some(Value::String(mode)) if mode == "overwrite" => Some(WriteMode::Overwrite),
        Some(Value::String(mode)) if mode == "append" => Some(WriteMode::Append),
        Some(_) => {
            return Err(
                "Invalid arguments for Write: mode must be overwrite or append".to_string(),
            );
        }
    };
    Ok(WriteInput {
        path: path.to_string(),
        content: content.to_string(),
        mode,
    })
}

async fn write_content(
    fs: Arc<dyn IHostFileSystem>,
    path: &str,
    content: &str,
    mode: WriteMode,
) -> Result<(), WireAsyncError> {
    if mode == WriteMode::Append {
        fs.append_text(path, content).await
    } else {
        fs.write_text(path, content).await
    }
}

#[derive(Debug)]
struct RealPathSecurityError {
    message: String,
}

impl std::fmt::Display for RealPathSecurityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RealPathSecurityError {}

fn security_error(message: String) -> WireAsyncError {
    Box::new(RealPathSecurityError { message })
}

async fn realpath_existing_prefix(
    fs: Arc<dyn IHostFileSystem>,
    abs_path: &str,
    path: &EnvironmentPath,
) -> Result<String, WireAsyncError> {
    let mut tail = Vec::new();
    let mut current = abs_path.to_string();
    for _ in 0..256 {
        match fs.realpath(&current).await {
            Ok(real) => {
                let parts = tail.iter().rev().map(String::as_str).collect::<Vec<_>>();
                return Ok(if parts.is_empty() {
                    real
                } else {
                    path.join(&real, &parts)
                });
            }
            Err(error) => {
                if !is_missing_path_error(&error) {
                    return Err(error);
                }
                let exists = match fs.lstat(&current).await {
                    Ok(_) => true,
                    Err(error) if is_missing_path_error(&error) => false,
                    Err(error) => return Err(error),
                };
                if exists {
                    return Err(security_error(format!(
                        "\"{current}\" is a symbolic link whose target does not exist. Access is blocked."
                    )));
                }
                let parent = path.dirname(&current);
                if parent == current {
                    return Ok(abs_path.to_string());
                }
                tail.push(path.basename(&current));
                current = parent;
            }
        }
    }
    Err(security_error(format!(
        "\"{abs_path}\" is too deep to resolve to a real path. Access is blocked."
    )))
}

async fn assert_real_path_within_workspace(
    fs: Arc<dyn IHostFileSystem>,
    abs_path: &str,
    workspace: &WorkspaceConfig,
    path_class: PathClass,
) -> Result<String, WireAsyncError> {
    let path = EnvironmentPath::new(path_class_name(path_class));
    let resolved = realpath_existing_prefix(fs.clone(), abs_path, &path).await?;
    if is_sensitive_file(&resolved) {
        return Err(security_error(format!(
            "\"{abs_path}\" resolves to \"{resolved}\" through a symbolic link, which matches a sensitive-file pattern (env / credential / SSH key). Access is blocked to protect secrets."
        )));
    }
    let roots = std::iter::once(workspace.workspace_dir.clone())
        .chain(workspace.additional_dirs.iter().cloned())
        .collect::<Vec<_>>();
    if !is_within_workspace(abs_path, &roots, path_class) {
        return Ok(abs_path.to_string());
    }
    let mut real_roots = Vec::new();
    for root in roots {
        real_roots.push(fs.realpath(&root).await.unwrap_or(root));
    }
    if real_roots
        .iter()
        .any(|root| is_within_directory(&resolved, root, path_class))
    {
        return Ok(resolved);
    }
    Err(security_error(format!(
        "\"{abs_path}\" resolves to \"{resolved}\" through a symbolic link that points outside the working directory. Access is blocked; use the real path directly or add the target directory to the workspace."
    )))
}

async fn assert_real_path_write_target(
    fs: Arc<dyn IHostFileSystem>,
    abs_path: &str,
    workspace: &WorkspaceConfig,
    path_class: PathClass,
) -> Result<String, WireAsyncError> {
    let resolved = assert_real_path_within_workspace(fs, abs_path, workspace, path_class).await?;
    if !is_project_local_config_path(abs_path) && is_project_local_config_path(&resolved) {
        return Err(security_error(format!(
            "\"{abs_path}\" resolves to the project-local config \"{resolved}\" through a symbolic link. Access is blocked; use the real path so the write goes through approval."
        )));
    }
    Ok(resolved)
}

fn is_missing_path_error(error: &WireAsyncError) -> bool {
    is_host_fs_not_found(error) || is_host_fs_not_directory(error)
}

fn path_class_from_name(value: &str) -> PathClass {
    PathClass::from_str(value).unwrap_or(PathClass::Posix)
}

fn path_class_name(value: PathClass) -> &'static str {
    match value {
        PathClass::Win32 => "win32",
        PathClass::Posix => "posix",
    }
}

fn from_legacy_result(result: crate::turn_loop::types::ExecutableToolResult) -> ExecutableToolResult {
    ExecutableToolResult {
        output: result.content.into(),
        is_error: Some(result.is_error),
        stop_turn: Some(result.stop_turn),
        note: result.note,
        delivery: result.delivery,
        spill: result.spill,
        ..Default::default()
    }
}
