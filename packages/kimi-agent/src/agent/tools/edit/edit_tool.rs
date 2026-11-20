use std::sync::{Arc, LazyLock};

use serde_json::{Map, Value};

use super::edit::{EditInput, IEditTool, edit_input_schema};
use crate::agent::environment_binding::agent_environment::{
    IAgentEnvironmentService, acquire_or_when_ready, pinned_generation,
};
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::tool::tool_contract::{ToolSideEffect};
use crate::agent::tools::file_memory::{FileMemoryProvider, with_file_memory};
use crate::app::edit::file_edit::{FileEditInput, FileEditResult, IFileEditService};
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

const DESCRIPTION: &str = r#"Perform exact replacements in existing files.

- Drop the line-number prefix and tab; match only file content.
- `old_string` must be unique unless `replace_all` is set.
- If `old_string` is ambiguous, add surrounding context.
"#;

static PARAMETERS: LazyLock<Value> = LazyLock::new(|| to_input_json_schema(edit_input_schema()));

#[derive(Clone)]
pub struct EditTool {
    editor: Arc<dyn IFileEditService>,
    environment: Arc<dyn IAgentEnvironmentService>,
    workspace_ctx: Arc<dyn ISessionWorkspaceContext>,
    skill_catalog: Option<Arc<dyn ISessionSkillCatalog>>,
    git_memory: Option<Arc<dyn FileMemoryProvider>>,
    plan_service: Option<Arc<dyn IAgentPlanService>>,
}

impl EditTool {
    pub fn new(
        editor: Arc<dyn IFileEditService>,
        environment: Arc<dyn IAgentEnvironmentService>,
        workspace_ctx: Arc<dyn ISessionWorkspaceContext>,
        skill_catalog: Option<Arc<dyn ISessionSkillCatalog>>,
        git_memory: Option<Arc<dyn FileMemoryProvider>>,
        plan_service: Option<Arc<dyn IAgentPlanService>>,
    ) -> Self {
        Self {
            editor,
            environment,
            workspace_ctx,
            skill_catalog,
            git_memory,
            plan_service,
        }
    }

    pub fn name(&self) -> &'static str {
        "Edit"
    }

    pub fn description(&self) -> &'static str {
        DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        PARAMETERS
            .as_object()
            .cloned()
            .expect("Edit input schema is an object")
    }

    pub async fn resolve_execution_value(
        &self,
        input: Value,
    ) -> Result<ToolExecution, WireAsyncError> {
        match serde_json::from_value::<EditInput>(input) {
            Ok(args) if args.old_string.is_empty() => Ok(ToolExecution::Error(
                ExecutableToolResult::error("Invalid arguments for Edit: old_string cannot be empty"),
            )),
            Ok(args) => self.resolve_execution(args).await,
            Err(error) => Ok(ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for Edit: {error}"
            )))),
        }
    }

    pub async fn resolve_execution(
        &self,
        args: EditInput,
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
                accesses: Some(ToolAccesses::read_write_file(&path)),
                display: Some(ToolInputDisplay::FileIo {
                    operation: FileIoOperation::Edit,
                    path: path.clone(),
                    detail: None,
                    content: None,
                    before: Some(args.old_string.clone()),
                    after: Some(args.new_string.clone()),
                }),
                description: Some(format!("Editing {}", args.path)),
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
                                &args,
                                &path,
                                lease.environment.fs(),
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

    fn plan_execution(&self, args: EditInput, target: PlanFileTarget) -> ToolExecution {
        let path = target.path.to_string_lossy().into_owned();
        let path_class = PathClass::from_str(&target.path_class).unwrap_or(PathClass::Posix);
        let match_path = path.clone();
        let match_cwd = target.environment_path.dirname(&path);
        let this = self.clone();
        let execute_target = target.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: Some(ToolAccesses::read_write_file(&path)),
            display: Some(ToolInputDisplay::FileIo {
                operation: FileIoOperation::Edit,
                path: path.clone(),
                detail: None,
                content: None,
                before: Some(args.old_string.clone()),
                after: Some(args.new_string.clone()),
            }),
            description: Some(format!("Editing {}", args.path)),
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
                        &args,
                        &path,
                        current.fs,
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
        args: &EditInput,
        safe_path: &str,
        fs: Arc<dyn IHostFileSystem>,
        workspace: &WorkspaceConfig,
        path_class: PathClass,
    ) -> Result<ExecutableToolResult, WireAsyncError> {
        if let Err(error) = assert_real_path_write_target(fs.clone(), safe_path, workspace, path_class).await {
            if let Some(security) = error.downcast_ref::<RealPathSecurityError>() {
                return Ok(ExecutableToolResult::error(security.message.clone()));
            }
            return Err(error);
        }
        if args.old_string == args.new_string {
            return Ok(ExecutableToolResult::error(
                "No changes to make: old_string and new_string are exactly the same.",
            ));
        }
        let result = self
            .editor
            .edit(
                FileEditInput {
                    path: safe_path.to_string(),
                    display_path: args.path.clone(),
                    old_string: args.old_string.clone(),
                    new_string: args.new_string.clone(),
                    replace_all: args.replace_all.unwrap_or(false),
                },
                Some(fs),
            )
            .await;
        match result {
            FileEditResult::Err { error } => Ok(ExecutableToolResult::error(error)),
            FileEditResult::Ok { count } => {
                let word = if count == 1 {
                    "occurrence"
                } else {
                    "occurrences"
                };
                let result = crate::turn_loop::types::ExecutableToolResult {
                    stop_turn: false,
                    content: format!("Replaced {count} {word} in {}", args.path),
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
        }
    }
}

impl IEditTool for EditTool {}

impl ExecutableTool for EditTool {
    fn side_effect(&self) -> Option<ToolSideEffect> {
        Some(ToolSideEffect::Write)
    }

    fn name(&self) -> &str {
        EditTool::name(self)
    }

    fn description(&self) -> &str {
        EditTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        EditTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> futures_util::future::BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let this = self.clone();
        Box::pin(async move { this.resolve_execution_value(input).await })
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
