use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use futures_util::{FutureExt, future::BoxFuture};
use parking_lot::Mutex;

use crate::_base::di::lifecycle::{IDisposable, to_disposable};
use crate::_base::exec_env::shell_path_bridge::ShellPathBridge;
use crate::environment::environment::{
    Environment, EnvironmentCapability, EnvironmentIdentity, EnvironmentPath, EnvironmentStatus,
    EnvironmentWorkspace, StatusListener,
};
use crate::environment::environment_defaults::{
    ENVIRONMENT_PATHS, ENVIRONMENT_WORKSPACES,
};
use crate::os::interface::host_environment::HostEnvironmentInfo;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_process::IHostProcessService;
use crate::wire::record::WireAsyncError;

pub struct FakeEnvironmentOptions {
    pub status: Option<EnvironmentStatus>,
    pub capabilities: Option<Vec<EnvironmentCapability>>,
    pub path_class: Option<String>,
    pub host: Option<HostEnvironmentInfo>,
    pub workspace: Option<EnvironmentWorkspace>,
}

impl Default for FakeEnvironmentOptions {
    fn default() -> Self {
        Self {
            status: None,
            capabilities: None,
            path_class: None,
            host: None,
            workspace: None,
        }
    }
}

pub struct FakeEnvironment {
    identity: EnvironmentIdentity,
    capabilities: HashSet<EnvironmentCapability>,
    host: HostEnvironmentInfo,
    path: EnvironmentPath,
    workspace: EnvironmentWorkspace,
    current_status: Mutex<EnvironmentStatus>,
    listeners: Arc<Mutex<HashMap<u64, StatusListener>>>,
    next_listener_id: AtomicU64,
    pub connect_error: Mutex<Option<String>>,
    pub disposed: AtomicBool,
}

impl FakeEnvironment {
    pub fn new(identity: EnvironmentIdentity, options: FakeEnvironmentOptions) -> Self {
        let path_class = options.path_class.unwrap_or_else(|| "posix".to_string());
        let host = options.host.unwrap_or_else(|| HostEnvironmentInfo {
            os_kind: "fake".to_string(),
            os_arch: "fake".to_string(),
            os_version: "fake".to_string(),
            shell_name: "sh".to_string(),
            shell_path: "/bin/sh".to_string(),
            path_class: if path_class == "win32" {
                "win32".to_string()
            } else {
                "posix".to_string()
            },
            home_dir: if path_class == "win32" {
                "C:\\Users\\fake".to_string()
            } else {
                "/home/fake".to_string()
            },
            cwd: None,
            temp_dir: None,
            js_runtimes: None,
        });
        let path = ENVIRONMENT_PATHS[&host.path_class].clone();
        let workspace = options
            .workspace
            .unwrap_or_else(|| ENVIRONMENT_WORKSPACES[&host.path_class].clone());
        Self {
            identity,
            capabilities: options
                .capabilities
                .unwrap_or_default()
                .into_iter()
                .collect(),
            host,
            path,
            workspace,
            current_status: Mutex::new(options.status.unwrap_or(EnvironmentStatus::Ready)),
            listeners: Arc::new(Mutex::new(HashMap::new())),
            next_listener_id: AtomicU64::new(0),
            connect_error: Mutex::new(None),
            disposed: AtomicBool::new(false),
        }
    }

    pub fn set_status(&self, status: EnvironmentStatus) {
        let mut current = self.current_status.lock();
        if *current == status {
            return;
        }
        *current = status;
        drop(current);
        for listener in self.listeners.lock().values() {
            listener(status);
        }
    }
}

impl Environment for FakeEnvironment {
    fn identity(&self) -> EnvironmentIdentity {
        self.identity.clone()
    }

    fn status(&self) -> EnvironmentStatus {
        *self.current_status.lock()
    }

    fn connect_error(&self) -> Option<String> {
        self.connect_error.lock().clone()
    }

    fn when_ready(&self) -> Option<BoxFuture<'static, Result<(), WireAsyncError>>> {
        None
    }

    fn capabilities(&self) -> HashSet<EnvironmentCapability> {
        self.capabilities.clone()
    }

    fn host(&self) -> Option<HostEnvironmentInfo> {
        Some(self.host.clone())
    }

    fn path(&self) -> Option<EnvironmentPath> {
        Some(self.path.clone())
    }

    fn workspace(&self) -> Option<EnvironmentWorkspace> {
        Some(self.workspace.clone())
    }

    fn shell_path_bridge(&self) -> Option<Arc<dyn ShellPathBridge>> {
        None
    }

    fn connect(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        async { Ok(()) }.boxed()
    }

    fn fs(&self) -> Arc<dyn IHostFileSystem> {
        panic!("fake environment has no file system")
    }

    fn process(&self) -> Arc<dyn IHostProcessService> {
        panic!("fake environment has no process service")
    }

    fn dispose(&self) -> BoxFuture<'static, ()> {
        self.disposed.store(true, Ordering::Relaxed);
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
