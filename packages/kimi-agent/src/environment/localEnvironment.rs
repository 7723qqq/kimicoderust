use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::{FutureExt, future::BoxFuture};
use parking_lot::Mutex;

use crate::_base::di::lifecycle::{IDisposable, to_disposable};
use crate::_base::exec_env::shell_path_bridge::{ShellPathBridge, get_shell_path_bridge};
use crate::_base::utils::paths::is_windows_absolute_path;
use crate::environment::environment::{
    Environment, EnvironmentCapability, EnvironmentIdentity, EnvironmentPath, EnvironmentStatus,
    EnvironmentWorkspace, EnvironmentWorkspaceMapper, EnvironmentWorkspaceRoots, StatusListener,
    LOCAL_ENVIRONMENT_ID,
};
use crate::environment::environment_defaults::ENVIRONMENT_PATHS;
use crate::os::interface::host_environment::IHostEnvironment;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_process::IHostProcessService;
use crate::wire::record::WireAsyncError;

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

pub struct LocalEnvironment {
    identity: EnvironmentIdentity,
    capabilities: HashSet<EnvironmentCapability>,
    host: crate::os::interface::host_environment::HostEnvironmentInfo,
    path: EnvironmentPath,
    workspace: EnvironmentWorkspace,
    shell_path_bridge: Option<Arc<dyn ShellPathBridge>>,
    fs: Option<Arc<dyn IHostFileSystem>>,
    process: Option<Arc<dyn IHostProcessService>>,
    current_status: Mutex<EnvironmentStatus>,
    listeners: Arc<Mutex<HashMap<u64, StatusListener>>>,
    next_listener_id: AtomicU64,
}

impl LocalEnvironment {
    pub fn new(
        environment: Arc<dyn IHostEnvironment>,
        fs: Option<Arc<dyn IHostFileSystem>>,
        process: Option<Arc<dyn IHostProcessService>>,
    ) -> Self {
        let identity = EnvironmentIdentity {
            environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
            cwd: None,
            generation: format!(
                "local-{}",
                NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
            ),
        };
        let mut capabilities = HashSet::new();
        if fs.is_some() {
            capabilities.insert(EnvironmentCapability::Fs);
        }
        if process.is_some() {
            capabilities.insert(EnvironmentCapability::Process);
        }
        let host = crate::os::interface::host_environment::HostEnvironmentInfo {
            os_kind: environment.os_kind(),
            os_arch: environment.os_arch(),
            os_version: environment.os_version(),
            shell_name: environment.shell_name(),
            shell_path: environment.shell_path(),
            path_class: environment.path_class(),
            home_dir: environment.home_dir(),
            cwd: None,
            temp_dir: None,
            js_runtimes: None,
        };
        let base_path = ENVIRONMENT_PATHS[&host.path_class].clone();
        let host_path_class = host.path_class.clone();
        let resolver_path = base_path.clone();
        let host_resolve = Arc::new(move |paths: &[&str]| {
            let last = paths.last().copied().unwrap_or("");
            if host_path_class == "win32" || !paths.iter().any(|path| is_windows_absolute_path(path)) {
                return resolver_path.resolve_many(paths);
            }
            if paths.len() == 1 && is_windows_absolute_path(last) {
                return last.to_string();
            }
            EnvironmentPath::new("win32")
                .resolve_many(paths)
                .replace('\\', "/")
        });
        let path = EnvironmentPath::with_resolver(&host.path_class, host_resolve.clone());
        let workspace_resolver = host_resolve.clone();
        let workspace = EnvironmentWorkspaceMapper::custom(move |roots: &EnvironmentWorkspaceRoots| {
            EnvironmentWorkspaceRoots {
                work_dir: workspace_resolver(&[&roots.work_dir]),
                primary_dir: roots
                    .primary_dir
                    .as_deref()
                    .map(|dir| workspace_resolver(&[dir])),
                additional_dirs: roots.additional_dirs.as_ref().map(|dirs| {
                    dirs.iter()
                        .map(|dir| workspace_resolver(&[dir.as_str()]))
                        .collect()
                }),
                follows_primary: roots.follows_primary,
            }
        });
        let shell_path_bridge = if host.path_class == "win32" {
            Some(get_shell_path_bridge(environment.as_ref()))
        } else {
            None
        };
        Self {
            identity,
            capabilities,
            host,
            path,
            workspace,
            shell_path_bridge,
            fs,
            process,
            current_status: Mutex::new(EnvironmentStatus::Ready),
            listeners: Arc::new(Mutex::new(HashMap::new())),
            next_listener_id: AtomicU64::new(0),
        }
    }
}

impl Environment for LocalEnvironment {
    fn identity(&self) -> EnvironmentIdentity {
        self.identity.clone()
    }

    fn status(&self) -> EnvironmentStatus {
        *self.current_status.lock()
    }

    fn connect_error(&self) -> Option<String> {
        None
    }

    fn when_ready(&self) -> Option<BoxFuture<'static, Result<(), WireAsyncError>>> {
        None
    }

    fn capabilities(&self) -> HashSet<EnvironmentCapability> {
        self.capabilities.clone()
    }

    fn host(&self) -> Option<crate::os::interface::host_environment::HostEnvironmentInfo> {
        Some(self.host.clone())
    }

    fn path(&self) -> Option<EnvironmentPath> {
        Some(self.path.clone())
    }

    fn workspace(&self) -> Option<EnvironmentWorkspace> {
        Some(self.workspace.clone())
    }

    fn shell_path_bridge(&self) -> Option<Arc<dyn ShellPathBridge>> {
        self.shell_path_bridge.clone()
    }

    fn connect(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        async { Ok(()) }.boxed()
    }

    fn fs(&self) -> Arc<dyn IHostFileSystem> {
        self.fs
            .clone()
            .expect("local environment has no file system capability")
    }

    fn process(&self) -> Arc<dyn IHostProcessService> {
        self.process
            .clone()
            .expect("local environment has no process capability")
    }

    fn dispose(&self) -> BoxFuture<'static, ()> {
        *self.current_status.lock() = EnvironmentStatus::Disposed;
        for listener in self.listeners.lock().values() {
            listener(EnvironmentStatus::Disposed);
        }
        self.listeners.lock().clear();
        async {}.boxed()
    }

    fn on_did_change_status(&self, listener: StatusListener) -> Box<dyn IDisposable> {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().insert(id, listener);
        let listeners = Arc::downgrade(&self.listeners);
        to_disposable(move || {
            if let Some(listeners) = listeners.upgrade() {
                listeners.lock().remove(&id);
            }
        })
    }
}
