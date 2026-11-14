use crate::_base::utils::error_message::error_message;
use crate::_base::utils::paths::{dirname, join, normalize_slashes, resolve};
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_fs_errors::{HostFsError, OS_FS_UNAVAILABLE};
use crate::wire::record::WireAsyncError;

use super::paths::{is_directory_path, path_exists, resolve_agent_path};
use super::types::{AgentFileRoot, AgentFileSource};

pub type AgentRootWarn<'a> = &'a dyn Fn(&str, Option<&WireAsyncError>);

const USER_BRAND_DIRS: &[&str] = &["agents"];
const USER_GENERIC_DIRS: &[&str] = &[".agents/agents"];
const PROJECT_BRAND_DIRS: &[&str] = &[".kimi-code/agents"];
const PROJECT_GENERIC_DIRS: &[&str] = &[".agents/agents"];

pub async fn user_agent_roots(
    fs: &dyn IHostFileSystem,
    home_dir: &str,
    os_home_dir: &str,
    warn: Option<AgentRootWarn<'_>>,
) -> Result<Vec<AgentFileRoot>, WireAsyncError> {
    let mut roots = Vec::new();
    push_first_existing(fs, &mut roots, USER_BRAND_DIRS, home_dir, AgentFileSource::User, warn).await?;
    push_first_existing(fs, &mut roots, USER_GENERIC_DIRS, os_home_dir, AgentFileSource::User, warn).await?;
    Ok(roots)
}

pub async fn project_agent_roots(
    fs: &dyn IHostFileSystem,
    work_dir: &str,
    warn: Option<AgentRootWarn<'_>>,
) -> Result<Vec<AgentFileRoot>, WireAsyncError> {
    let project_root = find_project_root(fs, work_dir, warn).await?;
    let mut roots = Vec::new();
    push_first_existing(fs, &mut roots, PROJECT_BRAND_DIRS, &project_root, AgentFileSource::Project, warn).await?;
    push_first_existing(fs, &mut roots, PROJECT_GENERIC_DIRS, &project_root, AgentFileSource::Project, warn).await?;
    Ok(roots)
}

pub struct ProjectAgentRootCandidates {
    pub project_root: String,
    pub candidates: Vec<String>,
}

pub async fn project_agent_root_candidates(
    fs: &dyn IHostFileSystem,
    work_dir: &str,
    warn: Option<AgentRootWarn<'_>>,
) -> Result<ProjectAgentRootCandidates, WireAsyncError> {
    let project_root = find_project_root(fs, work_dir, warn).await?;
    let candidates = PROJECT_BRAND_DIRS
        .iter()
        .chain(PROJECT_GENERIC_DIRS)
        .map(|dir| join(&project_root, &[dir]))
        .collect();
    Ok(ProjectAgentRootCandidates {
        project_root,
        candidates,
    })
}

pub async fn configured_agent_roots(
    fs: &dyn IHostFileSystem,
    dirs: &[String],
    work_dir: &str,
    os_home_dir: &str,
    source: AgentFileSource,
    warn: Option<AgentRootWarn<'_>>,
) -> Result<Vec<AgentFileRoot>, WireAsyncError> {
    let project_root = find_project_root(fs, work_dir, warn).await?;
    let mut roots = Vec::new();
    for dir in dirs {
        push_existing_root(
            fs,
            &mut roots,
            &resolve_agent_path(dir, &project_root, os_home_dir),
            source,
            warn,
        )
        .await?;
    }
    Ok(roots)
}

async fn find_project_root(
    fs: &dyn IHostFileSystem,
    work_dir: &str,
    warn: Option<AgentRootWarn<'_>>,
) -> Result<String, WireAsyncError> {
    let start = resolve(work_dir, ".");
    let mut current = start.clone();
    loop {
        let marker = join(&current, ".git");
        match path_exists(fs, &marker).await {
            Ok(true) => return Ok(normalize_slashes(&current)),
            Ok(false) => {}
            Err(error) => {
                if is_unavailable(&error) {
                    return Err(error);
                }
                if let Some(warn) = warn {
                    warn(
                        &format!(
                            "Skipping unreadable project marker {marker}: {}",
                            error_message(&error)
                        ),
                        Some(&error),
                    );
                }
            }
        }
        let parent = dirname(&current);
        if parent == current {
            return Ok(normalize_slashes(&start));
        }
        current = parent;
    }
}

async fn push_first_existing(
    fs: &dyn IHostFileSystem,
    out: &mut Vec<AgentFileRoot>,
    dirs: &[&str],
    base: &str,
    source: AgentFileSource,
    warn: Option<AgentRootWarn<'_>>,
) -> Result<(), WireAsyncError> {
    for dir in dirs {
        if push_existing_root(fs, out, &join(base, &[dir]), source, warn).await? {
            return Ok(());
        }
    }
    Ok(())
}

async fn push_existing_root(
    fs: &dyn IHostFileSystem,
    out: &mut Vec<AgentFileRoot>,
    dir: &str,
    source: AgentFileSource,
    warn: Option<AgentRootWarn<'_>>,
) -> Result<bool, WireAsyncError> {
    let result = async {
        if !is_directory_path(fs, dir).await? {
            return Ok(false);
        }
        let resolved = fs.realpath(dir).await?.replace('\\', "/");
        if !out.iter().any(|root| root.path == resolved) {
            out.push(AgentFileRoot {
                path: resolved,
                source,
            });
        }
        Ok(true)
    }
    .await;
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            if is_unavailable(&error) {
                return Err(error);
            }
            if let Some(warn) = warn {
                warn(
                    &format!(
                        "Skipping unreadable agent root {dir}: {}",
                        error_message(&error)
                    ),
                    Some(&error),
                );
            }
            Ok(false)
        }
    }
}

fn is_unavailable(error: &WireAsyncError) -> bool {
    error
        .downcast_ref::<HostFsError>()
        .is_some_and(|error| error.code().as_str() == OS_FS_UNAVAILABLE)
}
