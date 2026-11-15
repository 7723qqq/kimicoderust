use std::sync::Arc;

use kimi_core::primitives::errors::domains::fs::FS_PATH_ESCAPES;
use kimi_core::primitives::errors::{Error2, Error2Options};

use crate::_base::exec_env::shell_path_bridge::ShellPathBridge;
use crate::environment::environment::{
    Environment, EnvironmentBinding, EnvironmentPath, EnvironmentWorkspaceRoots,
};
use crate::environment::environment_defaults::{
    DEFAULT_ENVIRONMENT_HOST, POSIX_ENVIRONMENT_PATH, POSIX_ENVIRONMENT_WORKSPACE,
};
use crate::os::interface::host_environment::HostEnvironmentInfo;
use crate::wire::record::WireAsyncError;

pub struct EnvironmentWorkspaceView {
    pub environment: Arc<dyn Environment>,
    pub binding: EnvironmentBinding,
    pub host: HostEnvironmentInfo,
    pub path: EnvironmentPath,
    pub shell_path_bridge: Option<Arc<ShellPathBridge>>,
    pub work_dir: String,
    pub primary_dir: Option<String>,
    pub follows_primary: Option<bool>,
    pub additional_dirs: Vec<String>,
    pub roots: Vec<String>,
}

impl EnvironmentWorkspaceView {
    pub fn new(
        environment: Arc<dyn Environment>,
        roots: EnvironmentWorkspaceRoots,
    ) -> Self {
        let binding = EnvironmentBinding {
            environment_id: environment.identity().environment_id,
            cwd: None,
        };
        let host = environment
            .host()
            .unwrap_or_else(|| DEFAULT_ENVIRONMENT_HOST.clone());
        let path = environment
            .path()
            .unwrap_or_else(|| POSIX_ENVIRONMENT_PATH.clone());
        let shell_path_bridge = environment.shell_path_bridge();
        let workspace = environment
            .workspace()
            .unwrap_or_else(|| POSIX_ENVIRONMENT_WORKSPACE.clone());
        let mapped = workspace.map_roots(&roots);
        let work_dir = path.resolve_single(&mapped.work_dir);
        let primary_dir = mapped
            .primary_dir
            .as_deref()
            .map(|dir| path.resolve_single(dir));
        let mut additional_dirs = Vec::new();
        for root in mapped
            .additional_dirs
            .unwrap_or_default()
            .iter()
            .map(|root| path.resolve_single(root))
        {
            if !additional_dirs.contains(&root) {
                additional_dirs.push(root);
            }
        }
        let mut roots = vec![work_dir.clone()];
        roots.extend(additional_dirs.clone());
        Self {
            environment,
            binding,
            host,
            path,
            shell_path_bridge,
            work_dir,
            primary_dir,
            follows_primary: mapped.follows_primary,
            additional_dirs,
            roots,
        }
    }

    pub fn work_dir(&self) -> &str {
        &self.work_dir
    }

    pub fn resolve(&self, path: &str, cwd: Option<&str>) -> String {
        let bridged = self
            .shell_path_bridge
            .as_ref()
            .map_or_else(|| path.to_string(), |bridge| bridge.from_shell_path(path));
        if self.path.is_absolute(&bridged) {
            self.path.resolve_single(&bridged)
        } else {
            self.path.resolve(cwd.unwrap_or(&self.work_dir), &bridged)
        }
    }

    pub fn assert_allowed(&self, path: &str) -> Result<String, WireAsyncError> {
        let resolved = self.path.resolve_single(path);
        if self
            .roots
            .iter()
            .any(|root| contains(&self.path, root, &resolved))
        {
            return Ok(resolved);
        }
        let mut details = serde_json::Map::new();
        details.insert("path".to_string(), resolved.clone().into());
        Err(Box::new(Error2::with_options(
            FS_PATH_ESCAPES,
            format!(
                "path {path} is outside environment workspace {}",
                self.binding.environment_id
            ),
            Error2Options {
                details: Some(details),
                ..Default::default()
            },
        )))
    }
}

fn contains(path: &EnvironmentPath, root: &str, candidate: &str) -> bool {
    let relative = path.relative(root, candidate);
    if relative.is_empty() {
        return true;
    }
    relative != ".."
        && !relative.starts_with(&format!("..{}", path.separator))
        && !path.is_absolute(&relative)
}
