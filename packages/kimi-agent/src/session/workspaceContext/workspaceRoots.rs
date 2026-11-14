use std::collections::HashSet;

use crate::_base::utils::workdir_slug::workspace_root_key;
use crate::environment::environment::{EnvironmentBinding, LOCAL_ENVIRONMENT_ID};
use crate::os::interface::host_environment::HostEnvironmentInfo;
use crate::session::session_context::session_context::ISessionContext;
use crate::session::workspace_info::workspace_info::ISessionWorkspaceInfo;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveWorkspaceRoots {
    pub work_dir: String,
    pub primary_dir: Option<String>,
    pub additional_dirs: Vec<String>,
    pub follows_primary: bool,
}

pub fn resolve_effective_workspace_roots(
    session: &dyn ISessionContext,
    binding: Option<&EnvironmentBinding>,
    workspace: &dyn ISessionWorkspaceInfo,
    host: Option<&HostEnvironmentInfo>,
) -> EffectiveWorkspaceRoots {
    let environment_id = binding.map_or(LOCAL_ENVIRONMENT_ID, |binding| binding.environment_id.as_str());
    let local = environment_id == LOCAL_ENVIRONMENT_ID;
    let primary_dir = if local {
        workspace.primary_by_workspace().get(&session.workspace_id()).cloned()
    } else {
        None
    };
    let follows_primary = local && binding.and_then(|binding| binding.cwd.as_deref()).is_none() && primary_dir.is_some();
    let work_dir = binding.and_then(|binding| binding.cwd.clone()).or_else(|| {
        if local {
            primary_dir.clone().or_else(|| Some(session.cwd()))
        } else {
            host.and_then(|host| host.cwd.clone().or_else(|| Some(host.home_dir.clone())))
                .or_else(|| Some(session.cwd()))
        }
    }).unwrap_or_else(|| session.cwd());
    let work_dir_key = workspace_root_key(&work_dir);
    let mut seen = HashSet::from([work_dir_key]);
    let mut additional_dirs = Vec::new();
    for dir in std::iter::once(primary_dir.as_deref()).chain(workspace.additional_dirs().iter().map(String::as_str)) {
        let Some(dir) = dir else {
            continue;
        };
        let key = workspace_root_key(dir);
        if seen.insert(key) {
            additional_dirs.push(dir.to_string());
        }
    }
    EffectiveWorkspaceRoots {
        work_dir,
        primary_dir,
        additional_dirs,
        follows_primary,
    }
}

#[allow(dead_code)]
fn _marker(_: &dyn ISessionContext, _: &dyn ISessionWorkspaceInfo) {}
