use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use kimi_core::primitives::errors::{Error2, Error2Options, domains::config::CONFIG_INVALID};
use serde_json::Value;

use crate::app::git::work_tree::find_git_work_tree;
use crate::mcp_core::config_schema::{McpServerConfig, McpTransport, validate_mcp_server_config};
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_fs_errors::is_host_fs_not_found;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpJsonPaths {
    pub user: String,
    pub project_root: String,
    pub project: String,
}

pub struct ResolveMcpJsonPathsInput {
    pub fs: Arc<dyn IHostFileSystem>,
    pub cwd: String,
    pub home_dir: Option<String>,
}

impl ResolveMcpJsonPathsInput {
    pub async fn resolve(&self) -> Result<McpJsonPaths, WireAsyncError> {
        resolve_mcp_json_paths(
            self.fs.as_ref(),
            &self.cwd,
            self.home_dir.as_deref().unwrap_or(""),
        )
        .await
    }
}

pub struct LoadMcpServersInput {
    pub fs: Arc<dyn IHostFileSystem>,
    pub cwd: String,
    pub home_dir: Option<String>,
    pub include_project: Option<bool>,
}

impl LoadMcpServersInput {
    pub async fn load(&self) -> Result<HashMap<String, McpServerConfig>, WireAsyncError> {
        load_mcp_servers(
            self.fs.as_ref(),
            &self.cwd,
            self.home_dir.as_deref().unwrap_or(""),
            self.include_project.unwrap_or(true),
        )
        .await
    }

    pub async fn load_detailed(&self) -> Result<LoadMcpServersDetailedResult, WireAsyncError> {
        load_mcp_servers_detailed(
            self.fs.as_ref(),
            &self.cwd,
            self.home_dir.as_deref().unwrap_or(""),
            self.include_project.unwrap_or(true),
        )
        .await
    }
}

#[derive(Debug, Clone, Default)]
pub struct LoadMcpServersDetailedResult {
    pub servers: HashMap<String, McpServerConfig>,
    pub origins: HashMap<String, String>,
}

pub async fn resolve_mcp_json_paths(
    fs: &dyn IHostFileSystem,
    cwd: &str,
    home_dir: &str,
) -> Result<McpJsonPaths, WireAsyncError> {
    let start = normalize(cwd);
    let project_root = find_git_work_tree(fs, &start)
        .await?
        .map(|work_tree| work_tree.root)
        .unwrap_or_else(|| start.clone());
    let kimi_home = resolve_kimi_home(home_dir);
    Ok(McpJsonPaths {
        user: join(&kimi_home, "mcp.json"),
        project_root: join(&project_root, ".mcp.json"),
        project: join(&join(cwd, ".kimi-code"), "mcp.json"),
    })
}

pub async fn load_mcp_servers(
    fs: &dyn IHostFileSystem,
    cwd: &str,
    home_dir: &str,
    include_project: bool,
) -> Result<HashMap<String, McpServerConfig>, WireAsyncError> {
    Ok(
        load_mcp_servers_detailed(fs, cwd, home_dir, include_project)
            .await?
            .servers,
    )
}

pub async fn load_mcp_servers_detailed(
    fs: &dyn IHostFileSystem,
    cwd: &str,
    home_dir: &str,
    include_project: bool,
) -> Result<LoadMcpServersDetailedResult, WireAsyncError> {
    let paths = resolve_mcp_json_paths(fs, cwd, home_dir).await?;
    if !include_project {
        let servers = read_mcp_json(fs, &paths.user, None).await?;
        let origins = map_values_to_path(&servers, &paths.user);
        return Ok(LoadMcpServersDetailedResult { servers, origins });
    }
    let project_root_base = dirname(&paths.project_root);
    let (user, project_root, project) = tokio::try_join!(
        read_mcp_json(fs, &paths.user, None),
        read_mcp_json(fs, &paths.project_root, Some(&project_root_base)),
        read_mcp_json(fs, &paths.project, None),
    )?;
    let mut servers = HashMap::new();
    let mut origins = HashMap::new();
    for (path, layer) in [
        (paths.user, user),
        (paths.project_root, project_root),
        (paths.project, project),
    ] {
        for (name, config) in layer {
            servers.insert(name.clone(), config);
            origins.insert(name, path.clone());
        }
    }
    Ok(LoadMcpServersDetailedResult { servers, origins })
}

#[derive(Default)]
struct ReadMcpJsonOptions<'a> {
    stdio_cwd_base: Option<&'a str>,
}

async fn read_mcp_json(
    fs: &dyn IHostFileSystem,
    file_path: &str,
    stdio_cwd_base: Option<&str>,
) -> Result<HashMap<String, McpServerConfig>, WireAsyncError> {
    let text = match fs.read_text(file_path, None).await {
        Ok(text) => text,
        Err(error) if is_host_fs_not_found(&error) => return Ok(HashMap::new()),
        Err(error) => {
            let message = format!("Failed to read {file_path}: {error}");
            return Err(config_error(message, Some(error)));
        }
    };
    if text.trim().is_empty() {
        return Ok(HashMap::new());
    }
    let data: Value = match serde_json::from_str(&text) {
        Ok(data) => data,
        Err(error) => {
            let message = format!("Invalid JSON in {file_path}: {error}");
            return Err(config_error(message, Some(Box::new(error))));
        }
    };
    normalize_mcp_servers(
        parse_mcp_json_servers(data).map_err(|error| {
            let message = format!("Invalid MCP server config in {file_path}: {error}");
            config_error(message, None)
        })?,
        ReadMcpJsonOptions { stdio_cwd_base },
    )
    .map_err(|error| {
        let message = format!("Invalid MCP server config in {file_path}: {error}");
        config_error(message, None)
    })
}

fn parse_mcp_json_servers(data: Value) -> Result<HashMap<String, McpServerConfig>, String> {
    let Some(data) = data.as_object() else {
        return Err("expected a JSON object".to_string());
    };
    if !data.contains_key("mcpServers") {
        return Ok(HashMap::new());
    }
    let Some(raw) = data.get("mcpServers").and_then(Value::as_object) else {
        return Err("\"mcpServers\" must be an object".to_string());
    };
    raw.iter()
        .map(|(name, value)| {
            validate_mcp_server_config(value.clone())
                .map(|config| (name.clone(), config))
                .map_err(|error| format!("invalid server \"{name}\": {error}"))
        })
        .collect()
}

fn normalize_mcp_servers(
    servers: HashMap<String, McpServerConfig>,
    options: ReadMcpJsonOptions<'_>,
) -> Result<HashMap<String, McpServerConfig>, WireAsyncError> {
    let Some(stdio_cwd_base) = options.stdio_cwd_base else {
        return Ok(servers);
    };
    Ok(servers
        .into_iter()
        .map(|(name, config)| (name, normalize_stdio_cwd(config, stdio_cwd_base)))
        .collect())
}

fn normalize_stdio_cwd(mut config: McpServerConfig, cwd_base: &str) -> McpServerConfig {
    if config.transport != McpTransport::Stdio {
        return config;
    }
    config.cwd = Some(match config.cwd {
        Some(cwd) => resolve_path(cwd_base, &cwd),
        None => cwd_base.to_string(),
    });
    config
}

fn map_values_to_path(servers: &HashMap<String, McpServerConfig>, path: &str) -> HashMap<String, String> {
    servers
        .keys()
        .map(|name| (name.clone(), path.to_string()))
        .collect()
}

fn resolve_kimi_home(home_dir: &str) -> String {
    if !home_dir.is_empty() {
        return home_dir.to_string();
    }
    if let Ok(home) = std::env::var("KIMI_CODE_HOME") {
        return home;
    }
    let os_home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    join(&os_home, ".kimi-code")
}

fn normalize(path: &str) -> String {
    let mut output = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Prefix(prefix) => output.push(prefix.as_os_str()),
            Component::RootDir => output.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    output.push(component.as_os_str());
                }
            }
            Component::Normal(part) => output.push(part),
        }
    }
    path_string(&output)
}

fn join(base: &str, segment: &str) -> String {
    normalize(&Path::new(base).join(segment).to_string_lossy())
}

fn dirname(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(path_string)
        .unwrap_or_default()
}

fn resolve_path(base: &str, path: &str) -> String {
    if Path::new(path).is_absolute() {
        normalize(path)
    } else {
        join(base, path)
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn config_error(message: String, cause: Option<WireAsyncError>) -> WireAsyncError {
    Box::new(Error2::with_options(
        CONFIG_INVALID,
        message,
        Error2Options {
            cause,
            ..Default::default()
        },
    ))
}
