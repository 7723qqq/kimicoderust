use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::os::interface::host_environment::HostEnvironmentInfo;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_process::IHostProcessService;
use crate::wire::record::WireAsyncError;

pub use super::environment_registry::EnvironmentError;

pub type ShellPathBridge = Arc<crate::_base::exec_env::shell_path_bridge::ShellPathBridge>;
pub type StatusListener = Arc<dyn Fn(EnvironmentStatus) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvironmentStatus {
    Pending,
    Connecting,
    Ready,
    Disconnected,
    Disposed,
}

impl EnvironmentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Connecting => "connecting",
            Self::Ready => "ready",
            Self::Disconnected => "disconnected",
            Self::Disposed => "disposed",
        }
    }
}

impl fmt::Display for EnvironmentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvironmentCapability {
    Fs,
    Process,
}

impl EnvironmentCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fs => "fs",
            Self::Process => "process",
        }
    }
}

impl fmt::Display for EnvironmentCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub const LOCAL_ENVIRONMENT_ID: &str = "local";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentBinding {
    pub environment_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

pub fn environment_binding_id(environment_id: &str, cwd: Option<&str>) -> String {
    cwd.map_or_else(
        || environment_id.to_string(),
        |cwd| format!("{environment_id}\0{cwd}"),
    )
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentIdentity {
    pub environment_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub generation: String,
}

#[derive(Clone)]
pub struct EnvironmentPath {
    pub separator: String,
    pub delimiter: String,
    path_class: String,
    resolver: Option<Arc<dyn Fn(&[&str]) -> String + Send + Sync>>,
}

impl EnvironmentPath {
    pub fn new(path_class: &str) -> Self {
        Self {
            separator: if path_class == "win32" { "\\" } else { "/" }.to_string(),
            delimiter: if path_class == "win32" { ";" } else { ":" }.to_string(),
            path_class: path_class.to_string(),
            resolver: None,
        }
    }

    pub fn with_resolver(
        path_class: &str,
        resolver: Arc<dyn Fn(&[&str]) -> String + Send + Sync>,
    ) -> Self {
        Self {
            resolver: Some(resolver),
            ..Self::new(path_class)
        }
    }

    pub fn is_absolute(&self, path: &str) -> bool {
        super::environment_defaults::is_absolute(&self.path_class, path)
    }

    pub fn join(&self, base: &str, paths: &[&str]) -> String {
        super::environment_defaults::join(&self.path_class, base, paths)
    }

    pub fn relative(&self, from: &str, to: &str) -> String {
        super::environment_defaults::relative(&self.path_class, from, to)
    }

    pub fn resolve(&self, base: &str, path: &str) -> String {
        self.resolve_many(&[base, path])
    }

    pub fn resolve_single(&self, path: &str) -> String {
        self.resolve_many(&[path])
    }

    pub fn resolve_many(&self, paths: &[&str]) -> String {
        if let Some(resolver) = &self.resolver {
            return resolver(paths);
        }
        let mut resolved = String::new();
        if paths.is_empty() {
            return super::environment_defaults::resolve(&self.path_class, "", "");
        }
        for path in paths {
            resolved = if resolved.is_empty() {
                super::environment_defaults::resolve(&self.path_class, path, "")
            } else {
                super::environment_defaults::resolve(&self.path_class, &resolved, path)
            };
        }
        resolved
    }

    pub fn basename(&self, path: &str) -> String {
        super::environment_defaults::basename(&self.path_class, path)
    }

    pub fn dirname(&self, path: &str) -> String {
        super::environment_defaults::dirname(&self.path_class, path)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentWorkspaceRoots {
    pub work_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_dirs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follows_primary: Option<bool>,
}

#[derive(Clone)]
pub struct EnvironmentWorkspaceMapper {
    mapper: Arc<dyn Fn(&EnvironmentWorkspaceRoots) -> EnvironmentWorkspaceRoots + Send + Sync>,
}

impl EnvironmentWorkspaceMapper {
    pub fn new(path_class: &str) -> Self {
        let path_class = path_class.to_string();
        Self::custom(move |roots| {
            super::environment_defaults::map_roots(&path_class, roots)
        })
    }

    pub fn custom(
        mapper: impl Fn(&EnvironmentWorkspaceRoots) -> EnvironmentWorkspaceRoots + Send + Sync + 'static,
    ) -> Self {
        Self {
            mapper: Arc::new(mapper),
        }
    }

    pub fn map_roots(&self, roots: &EnvironmentWorkspaceRoots) -> EnvironmentWorkspaceRoots {
        (self.mapper)(roots)
    }
}

pub type EnvironmentWorkspace = EnvironmentWorkspaceMapper;

pub trait Environment: Send + Sync {
    fn identity(&self) -> EnvironmentIdentity;
    fn capabilities(&self) -> HashSet<EnvironmentCapability>;
    fn host(&self) -> Option<HostEnvironmentInfo>;
    fn path(&self) -> Option<EnvironmentPath>;
    fn workspace(&self) -> Option<EnvironmentWorkspace>;
    fn shell_path_bridge(&self) -> Option<ShellPathBridge>;
    fn fs(&self) -> Arc<dyn IHostFileSystem>;
    fn process(&self) -> Arc<dyn IHostProcessService>;
    fn status(&self) -> EnvironmentStatus;
    fn on_did_change_status(&self, listener: StatusListener) -> DisposableRef;
    fn when_ready(&self) -> Option<BoxFuture<'static, Result<(), WireAsyncError>>>;
    fn connect_error(&self) -> Option<String>;
    fn connect(&self) -> BoxFuture<'static, Result<(), WireAsyncError>>;
    fn dispose(&self) -> BoxFuture<'static, ()>;
}

pub trait EnvironmentResource: Send + Sync {
    fn dispose(&self) -> BoxFuture<'static, ()>;
}

type TrackResource = Arc<
    dyn Fn(Arc<dyn EnvironmentResource>, Option<String>) -> Result<(), WireAsyncError>
        + Send
        + Sync,
>;

pub struct EnvironmentLease {
    pub environment: Arc<dyn Environment>,
    track_resource: TrackResource,
    dispose: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl EnvironmentLease {
    pub fn new(
        environment: Arc<dyn Environment>,
        track_resource: TrackResource,
        dispose: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            environment,
            track_resource,
            dispose: Mutex::new(Some(Box::new(dispose))),
        }
    }

    pub fn track<T>(
        &self,
        resource: Arc<T>,
        session_id: Option<String>,
    ) -> Result<Arc<T>, WireAsyncError>
    where
        T: EnvironmentResource + 'static,
    {
        (self.track_resource)(resource.clone(), session_id)?;
        Ok(resource)
    }

    pub fn dispose(&self) {
        if let Some(dispose) = self.dispose.lock().take() {
            dispose();
        }
    }
}

pub async fn realpath_existing_prefix(
    fs: &dyn IHostFileSystem,
    path: &EnvironmentPath,
    abs: &str,
    is_missing: impl Fn(&WireAsyncError) -> bool,
) -> Result<String, WireAsyncError> {
    let mut tail = Vec::new();
    let mut current = abs.to_string();
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
                if !is_missing(&error) {
                    return Err(error);
                }
                let parent = path.dirname(&current);
                if parent == current {
                    return Ok(abs.to_string());
                }
                tail.push(path.basename(&current));
                current = parent;
            }
        }
    }
    Ok(abs.to_string())
}
