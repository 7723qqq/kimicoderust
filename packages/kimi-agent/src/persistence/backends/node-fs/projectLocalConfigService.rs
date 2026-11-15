use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use kimi_core::primitives::errors::domains::config::CONFIG_INVALID;
use kimi_core::primitives::errors::{Error2, Error2Options, unwrap_error_cause};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;
use toml::map::Map as TomlMap;
use toml::Value as TomlValue;
use uuid::Uuid;

use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::project_local_config::project_local_config::{
    IProjectLocalConfigService, ProjectAdditionalDirsLoadResult, ProjectPermissionRulesLoadResult,
};
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_fs_errors::{
    HostFsError, OS_FS_NOT_DIRECTORY, OS_FS_NOT_FOUND,
};
use crate::persistence::interface::storage::{
    STORAGE_DECODE_FAILED, StorageError, to_storage_io_error,
};
use crate::wire::record::WireAsyncError;

const POSIX_SYSTEM_DIRS: &[&str] = &[
    "/etc", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/boot", "/dev", "/proc",
    "/sys", "/var",
];

struct ProjectLocalTomlFile {
    raw: TomlMap<String, TomlValue>,
    additional_dirs: Option<Vec<String>>,
}

static PERMISSION_WRITE_LOCKS: LazyLock<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn permission_write_lock(config_path: &str) -> Arc<AsyncMutex<()>> {
    let mut locks = PERMISSION_WRITE_LOCKS.lock();
    locks
        .entry(config_path.to_string())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

pub struct FileProjectLocalConfigService {
    bootstrap: Arc<dyn IBootstrapService>,
    fs: Arc<dyn IHostFileSystem>,
    home_dir: String,
}

impl FileProjectLocalConfigService {
    pub fn new(
        bootstrap: Arc<dyn IBootstrapService>,
        fs: Arc<dyn IHostFileSystem>,
        home_dir: String,
    ) -> Self {
        Self {
            bootstrap,
            fs,
            home_dir,
        }
    }

    fn get_project_local_config_path(&self, project_root: &str) -> String {
        join_path(project_root, &[".kimi-code", "local.toml"])
    }

    async fn find_project_root(&self, work_dir: &str) -> String {
        let initial = normalize_path(work_dir);
        let mut current = initial.clone();
        loop {
            if self.path_exists(&join_path(&current, &[".git"])).await {
                return current;
            }
            let parent = dirname(&current);
            if parent == current {
                return initial;
            }
            current = parent;
        }
    }

    async fn read_project_local_toml(
        &self,
        config_path: &str,
    ) -> Result<Option<ProjectLocalTomlFile>, WireAsyncError> {
        let text = match self.fs.read_text(config_path, None).await {
            Ok(text) => text,
            Err(error) if is_path_missing(&error) => return Ok(None),
            Err(error) => return Err(to_storage_io_error(error, config_path, "read")),
        };
        if text.trim().is_empty() {
            return Ok(Some(ProjectLocalTomlFile {
                raw: TomlMap::new(),
                additional_dirs: None,
            }));
        }
        let raw = match toml::from_str::<TomlValue>(&text) {
            Ok(raw) => raw,
            Err(error) => {
                let mut details = serde_json::Map::new();
                details.insert(
                    "path".to_string(),
                    serde_json::Value::String(config_path.to_string()),
                );
                details.insert(
                    "format".to_string(),
                    serde_json::Value::String("toml".to_string()),
                );
                return Err(Box::new(StorageError::with_options(
                    STORAGE_DECODE_FAILED,
                    format!("Invalid TOML in {config_path}"),
                    Error2Options {
                        details: Some(details),
                        cause: Some(Box::new(error)),
                        name: None,
                    },
                )));
            }
        };
        let Some(raw) = raw.as_table().cloned() else {
            return Err(Box::new(Error2::new(
                CONFIG_INVALID,
                format!("Invalid project local config in {config_path}"),
            )));
        };
        let additional_dirs = parse_project_local_toml(&raw)?;
        Ok(Some(ProjectLocalTomlFile {
            raw,
            additional_dirs,
        }))
    }

    async fn resolve_additional_dirs_internal(
        &self,
        base_dir: &str,
        additional_dirs: &[String],
        source: &str,
    ) -> Result<Vec<String>, WireAsyncError> {
        let mut resolved_dirs = Vec::new();
        for additional_dir in normalize_additional_dirs(additional_dirs) {
            let resolved_dir = self.resolve_additional_dir(base_dir, &additional_dir).await?;
            if source == "session" {
                assert_session_dir_within_sandbox(
                    &resolved_dir,
                    base_dir,
                    self.bootstrap.as_ref(),
                )?;
            }
            if self.has_same_additional_dir(&resolved_dirs, &resolved_dir) {
                continue;
            }
            resolved_dirs.push(resolved_dir);
        }
        Ok(resolved_dirs)
    }

    fn resolve_existing_additional_dirs(
        &self,
        project_root: &str,
        additional_dirs: &[String],
    ) -> Vec<String> {
        let mut resolved_dirs = Vec::new();
        for additional_dir in normalize_additional_dirs(additional_dirs) {
            let resolved_dir = self.resolve_path(project_root, &additional_dir);
            if self.has_same_additional_dir(&resolved_dirs, &resolved_dir) {
                continue;
            }
            resolved_dirs.push(resolved_dir);
        }
        resolved_dirs
    }

    async fn resolve_additional_dir(
        &self,
        base_dir: &str,
        additional_dir: &str,
    ) -> Result<String, WireAsyncError> {
        let normalized_input = normalize_additional_dir_input(additional_dir)?;
        let resolved_dir = self.resolve_path(base_dir, &normalized_input);
        self.assert_directory(&resolved_dir).await?;
        Ok(resolved_dir)
    }

    fn resolve_path(&self, base_dir: &str, additional_dir: &str) -> String {
        let expanded = self.expand_home(additional_dir);
        if Path::new(&expanded).is_absolute() {
            normalize_path(&expanded)
        } else {
            normalize_path(&join_path(base_dir, &[&expanded]))
        }
    }

    fn expand_home(&self, value: &str) -> String {
        if self.home_dir.is_empty() || (value != "~" && !value.starts_with("~/")) {
            return value.to_string();
        }
        if value == "~" {
            return self.home_dir.clone();
        }
        join_path(&self.home_dir, &[&value[2..]])
    }

    fn has_same_additional_dir(&self, dirs: &[String], target: &str) -> bool {
        let normalized_target = normalize_path(target);
        dirs.iter()
            .any(|dir| normalize_path(dir) == normalized_target)
    }

    async fn assert_directory(&self, file_path: &str) -> Result<(), WireAsyncError> {
        let stat = match self.fs.stat(file_path).await {
            Ok(stat) => stat,
            Err(error) if is_path_missing(&error) => {
                return Err(Box::new(Error2::new(
                    CONFIG_INVALID,
                    format!(
                        "workspace.additional_dir must exist and be a directory: {file_path}"
                    ),
                )));
            }
            Err(error) => return Err(to_storage_io_error(error, file_path, "stat")),
        };
        if !stat.is_directory {
            return Err(Box::new(Error2::new(
                CONFIG_INVALID,
                format!(
                    "workspace.additional_dir must exist and be a directory: {file_path}"
                ),
            )));
        }
        Ok(())
    }

    async fn path_exists(&self, file_path: &str) -> bool {
        self.fs.lstat(file_path).await.is_ok()
    }

    async fn assert_no_symlinks(
        &self,
        project_root: &str,
        config_path: &str,
    ) -> Result<(), WireAsyncError> {
        let parent = dirname(config_path);
        for path in [project_root.to_string(), parent, config_path.to_string()] {
            let stat = match self.fs.lstat(&path).await {
                Ok(stat) => stat,
                Err(error) if is_path_missing(&error) => continue,
                Err(error) => return Err(to_storage_io_error(error, &path, "stat")),
            };
            if stat.is_symbolic_link {
                return Err(Box::new(Error2::new(
                    CONFIG_INVALID,
                    format!("Refusing to write project permission rules through symlink {path}"),
                )));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl IProjectLocalConfigService for FileProjectLocalConfigService {
    async fn read_additional_dirs(
        &self,
        work_dir: &str,
    ) -> Result<ProjectAdditionalDirsLoadResult, WireAsyncError> {
        let project_root = self.find_project_root(work_dir).await;
        let config_path = self.get_project_local_config_path(&project_root);
        let file = self.read_project_local_toml(&config_path).await?;
        let additional_dirs = match file.as_ref().and_then(|file| file.additional_dirs.as_ref()) {
            None => Vec::new(),
            Some(dirs) => {
                self.resolve_additional_dirs_internal(&project_root, dirs, "config")
                    .await?
            }
        };
        Ok(ProjectAdditionalDirsLoadResult {
            project_root,
            config_path,
            additional_dirs,
        })
    }

    async fn resolve_additional_dirs(
        &self,
        base_dir: &str,
        additional_dirs: &[String],
        source: &str,
    ) -> Result<Vec<String>, WireAsyncError> {
        self.resolve_additional_dirs_internal(base_dir, additional_dirs, source)
            .await
    }

    async fn append_additional_dir(
        &self,
        work_dir: &str,
        input_path: &str,
    ) -> Result<ProjectAdditionalDirsLoadResult, WireAsyncError> {
        let project_root = self.find_project_root(work_dir).await;
        let config_path = self.get_project_local_config_path(&project_root);
        let additional_dir = self.resolve_additional_dir(work_dir, input_path).await?;
        let mut file = self
            .read_project_local_toml(&config_path)
            .await?
            .unwrap_or_else(|| ProjectLocalTomlFile {
                raw: TomlMap::new(),
                additional_dirs: None,
            });
        let file_additional_dirs = file.additional_dirs.clone().unwrap_or_default();
        let file_existing_dirs =
            self.resolve_existing_additional_dirs(&project_root, &file_additional_dirs);
        if self
            .has_same_additional_dir(&file_existing_dirs, &additional_dir)
        {
            return Ok(ProjectAdditionalDirsLoadResult {
                project_root,
                config_path,
                additional_dirs: file_existing_dirs,
            });
        }
        let mut workspace = file
            .raw
            .get("workspace")
            .and_then(TomlValue::as_table)
            .cloned()
            .unwrap_or_default();
        let mut serialized_dirs = file_existing_dirs.clone();
        serialized_dirs.push(additional_dir.clone());
        workspace.insert(
            "additional_dir".to_string(),
            TomlValue::Array(
                serialized_dirs
                    .iter()
                    .map(|dir| TomlValue::String(dir.clone()))
                    .collect(),
            ),
        );
        file.raw
            .insert("workspace".to_string(), TomlValue::Table(workspace));
        if let Err(error) = async {
            self.fs.mkdir(&dirname(&config_path), true).await?;
            let encoded = toml::to_string(&TomlValue::Table(file.raw))?;
            self.fs
                .write_text(&config_path, &format!("{}\n", encoded.trim_end()))
                .await
        }
        .await
        {
            return Err(to_storage_io_error(error, &config_path, "write"));
        }
        Ok(ProjectAdditionalDirsLoadResult {
            project_root,
            config_path,
            additional_dirs: serialized_dirs,
        })
    }

    async fn read_permission_rules(
        &self,
        work_dir: &str,
    ) -> Result<ProjectPermissionRulesLoadResult, WireAsyncError> {
        let project_root = self.find_project_root(work_dir).await;
        let config_path = self.get_project_local_config_path(&project_root);
        let file = self.read_project_local_toml(&config_path).await?;
        let rules = read_raw_permission_rules(file.as_ref().map(|file| &file.raw))?;
        Ok(ProjectPermissionRulesLoadResult {
            project_root,
            config_path,
            rules,
        })
    }

    async fn append_permission_rules(
        &self,
        work_dir: &str,
        rules: &[Value],
    ) -> Result<ProjectPermissionRulesLoadResult, WireAsyncError> {
        let project_root = self.find_project_root(work_dir).await;
        let config_path = self.get_project_local_config_path(&project_root);
        let lock = permission_write_lock(&config_path);
        let _guard = lock.lock().await;
        self.assert_no_symlinks(&project_root, &config_path).await?;
        let mut file = self
            .read_project_local_toml(&config_path)
            .await?
            .unwrap_or_else(|| ProjectLocalTomlFile {
                raw: TomlMap::new(),
                additional_dirs: None,
            });
        let mut appended = read_raw_permission_rules(Some(&file.raw))?;
        appended.extend_from_slice(rules);
        let mut permission = file
            .raw
            .get("permission")
            .and_then(TomlValue::as_table)
            .cloned()
            .unwrap_or_default();
        permission.insert(
            "rules".to_string(),
            permission_rules_to_toml(&appended)?,
        );
        file.raw
            .insert("permission".to_string(), TomlValue::Table(permission));
        let encoded = toml::to_string(&TomlValue::Table(file.raw))?;
        let temp_path = format!(
            "{config_path}.tmp.{}.{}",
            std::process::id(),
            Uuid::new_v4()
        );
        let written = async {
            self.fs.mkdir(&dirname(&config_path), true).await?;
            self.fs
                .write_text(&temp_path, &format!("{}\n", encoded.trim_end()))
                .await?;
            self.fs.rename(&temp_path, &config_path).await
        }
        .await;
        if let Err(error) = written {
            let _ = self.fs.remove(&temp_path).await;
            return Err(to_storage_io_error(error, &config_path, "write"));
        }
        Ok(ProjectPermissionRulesLoadResult {
            project_root,
            config_path,
            rules: appended,
        })
    }
}

fn normalize_additional_dirs(additional_dirs: &[String]) -> Vec<String> {
    let mut normalized_dirs = Vec::new();
    for additional_dir in additional_dirs {
        let normalized = normalize_path(additional_dir);
        if normalized_dirs.contains(&normalized) {
            continue;
        }
        normalized_dirs.push(normalized);
    }
    normalized_dirs
}

fn normalize_additional_dir_input(additional_dir: &str) -> Result<String, WireAsyncError> {
    let trimmed = additional_dir.trim();
    if trimmed.is_empty() {
        return Err(Box::new(Error2::new(
            CONFIG_INVALID,
            "workspace.additional_dir must not be empty",
        )));
    }
    Ok(normalize_path(trimmed))
}

fn parse_project_local_toml(
    raw: &TomlMap<String, TomlValue>,
) -> Result<Option<Vec<String>>, WireAsyncError> {
    let Some(workspace) = raw.get("workspace") else {
        return Ok(None);
    };
    let Some(workspace) = workspace.as_table() else {
        return Err(Box::new(Error2::new(
            CONFIG_INVALID,
            "workspace must be a table",
        )));
    };
    let Some(additional_dir) = workspace.get("additional_dir") else {
        return Ok(None);
    };
    let Some(entries) = additional_dir.as_array() else {
        return Err(Box::new(Error2::new(
            CONFIG_INVALID,
            "workspace.additional_dir must be an array of strings",
        )));
    };
    let mut dirs = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(entry) = entry.as_str() else {
            return Err(Box::new(Error2::new(
                CONFIG_INVALID,
                "workspace.additional_dir must be an array of strings",
            )));
        };
        dirs.push(entry.to_string());
    }
    Ok(Some(dirs))
}

fn read_raw_permission_rules(
    raw: Option<&TomlMap<String, TomlValue>>,
) -> Result<Vec<Value>, WireAsyncError> {
    let Some(permission) = raw.and_then(|raw| raw.get("permission")) else {
        return Ok(Vec::new());
    };
    let Some(permission) = permission.as_table() else {
        return Err(Box::new(Error2::new(
            CONFIG_INVALID,
            "permission must be a table",
        )));
    };
    let Some(rules) = permission.get("rules") else {
        return Ok(Vec::new());
    };
    let Some(rules) = rules.as_array() else {
        return Err(Box::new(Error2::new(
            CONFIG_INVALID,
            "permission.rules must be an array",
        )));
    };
    let mut out = Vec::with_capacity(rules.len());
    for rule in rules {
        out.push(serde_json::to_value(rule).map_err(|error| Box::new(error) as WireAsyncError)?);
    }
    Ok(out)
}

fn permission_rules_to_toml(rules: &[Value]) -> Result<TomlValue, WireAsyncError> {
    let mut out = Vec::with_capacity(rules.len());
    for rule in rules {
        out.push(
            serde_json::from_value::<TomlValue>(rule.clone())
                .map_err(|error| Box::new(error) as WireAsyncError)?,
        );
    }
    Ok(TomlValue::Array(out))
}

fn is_path_missing(error: &WireAsyncError) -> bool {
    if error.downcast_ref::<HostFsError>().is_some_and(|error| {
        error.code().as_str() == OS_FS_NOT_FOUND || error.code().as_str() == OS_FS_NOT_DIRECTORY
    }) {
        return true;
    }
    let cause = unwrap_error_cause(error.as_ref());
    if cause.downcast_ref::<HostFsError>().is_some_and(|error| {
        error.code().as_str() == OS_FS_NOT_FOUND || error.code().as_str() == OS_FS_NOT_DIRECTORY
    }) {
        return true;
    }
    cause.downcast_ref::<io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
        )
    })
}

fn assert_session_dir_within_sandbox(
    resolved_dir: &str,
    base_dir: &str,
    bootstrap: &dyn IBootstrapService,
) -> Result<(), WireAsyncError> {
    let target = normalize_path(resolved_dir);
    let home = normalize_path(&bootstrap.os_home_dir());
    let workspace_root = normalize_path(base_dir);
    let is_filesystem_root = dirname(&target) == target;
    let is_home = target == home;
    let is_system_dir = if bootstrap.platform() == "win32" {
        is_windows_system_dir(&target, bootstrap)
            || target
                == normalize_path(
                    &bootstrap
                        .get_env("SystemRoot")
                        .unwrap_or_else(|| "C:\\Windows".to_string()),
                )
    } else {
        POSIX_SYSTEM_DIRS
            .iter()
            .any(|dir| target == normalize_path(dir))
    };
    let is_workspace_ancestor = target != workspace_root
        && workspace_root.starts_with(&format!("{target}/"));
    if is_filesystem_root || is_home || is_system_dir || is_workspace_ancestor {
        return Err(Box::new(Error2::new(
            CONFIG_INVALID,
            format!(
                "session additional_dir \"{resolved_dir}\" is not allowed: it is the filesystem root, the home directory, a system directory, or an ancestor of the workspace"
            ),
        )));
    }
    Ok(())
}

fn is_windows_system_dir(target: &str, bootstrap: &dyn IBootstrapService) -> bool {
    for name in ["ProgramFiles", "ProgramData"] {
        let Some(candidate) = bootstrap.get_env(name) else {
            continue;
        };
        if candidate.is_empty() {
            continue;
        }
        let normalized = normalize_path(&candidate);
        if target == normalized || target.starts_with(&format!("{normalized}/")) {
            return true;
        }
    }
    false
}

fn normalize_path(path: &str) -> String {
    let mut prefix = String::new();
    let mut rooted = false;
    let mut parts: Vec<String> = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Prefix(value) => {
                prefix = value.as_os_str().to_string_lossy().replace('\\', "/")
            }
            Component::RootDir => rooted = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.last().is_some_and(|part| part != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..".to_string());
                }
            }
            Component::Normal(value) => parts.push(value.to_string_lossy().into_owned()),
        }
    }
    let mut result = prefix;
    if rooted && !result.ends_with('/') {
        result.push('/');
    }
    result.push_str(&parts.join("/"));
    if result.is_empty() {
        if rooted { "/".to_string() } else { ".".to_string() }
    } else if rooted && result.len() > 1 && result.ends_with('/') && parts.is_empty() {
        result
    } else {
        result.trim_end_matches('/').to_string()
    }
}

fn join_path(base: &str, parts: &[&str]) -> String {
    let mut path = PathBuf::from(base);
    for part in parts {
        path.push(part);
    }
    normalize_path(&path.to_string_lossy())
}

fn dirname(path: &str) -> String {
    let normalized = normalize_path(path);
    let path = Path::new(&normalized);
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => normalize_path(&parent.to_string_lossy()),
        _ => normalized,
    }
}
