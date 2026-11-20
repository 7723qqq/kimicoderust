use std::fmt;
use std::sync::Arc;

use futures_util::{FutureExt, future::BoxFuture};
use parking_lot::Mutex;

use crate::_base::exec_env::environment_probe::{
    ProbeShellNotFoundError, probe_host_environment_from_node,
};
use crate::_base::exec_env::login_shell_path::apply_login_shell_path_from_node;
use crate::os::interface::host_environment::{HostEnvironmentInfo, IHostEnvironment, JsRuntimeInfo};
use crate::os::interface::host_process::{
    HostProcessError, SHELL_GIT_BASH_NOT_FOUND,
};
use crate::wire::record::WireAsyncError;

#[derive(Debug)]
struct SharedProbeError(Arc<WireAsyncError>);

impl fmt::Display for SharedProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for SharedProbeError {}

struct HostEnvironmentInner {
    info: Mutex<Option<HostEnvironmentInfo>>,
    probe_error: Mutex<Option<Arc<WireAsyncError>>>,
}

#[derive(Clone)]
pub struct HostEnvironmentService {
    inner: Arc<HostEnvironmentInner>,
}

impl HostEnvironmentService {
    pub fn new() -> Self {
        let inner = Arc::new(HostEnvironmentInner {
            info: Mutex::new(None),
            probe_error: Mutex::new(None),
        });
        let task_inner = inner.clone();
        tokio::spawn(async move {
            let result = async {
                let (info, login) = tokio::join!(
                    probe_host_environment_from_node(),
                    apply_login_shell_path_from_node(),
                );
                let info = info?;
                let _ = login;
                Ok::<HostEnvironmentInfo, WireAsyncError>(info.into())
            }
            .await;
            match result {
                Ok(info) => *task_inner.info.lock() = Some(info),
                Err(error) => {
                    let error = translate_probe_error(error);
                    *task_inner.probe_error.lock() = Some(Arc::new(error));
                }
            }
        });
        Self { inner }
    }

    fn require(&self, field: &str) -> HostEnvironmentInfo {
        if let Some(error) = self.inner.probe_error.lock().as_ref() {
            panic!("{error}");
        }
        self.inner
            .info
            .lock()
            .clone()
            .unwrap_or_else(|| {
                panic!(
                    "IHostEnvironment.{field} accessed before ready — await IHostEnvironment.ready first (composition root should do so before creating a Session scope)."
                )
            })
    }
}

impl Default for HostEnvironmentService {
    fn default() -> Self {
        Self::new()
    }
}

impl IHostEnvironment for HostEnvironmentService {
    fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let inner = self.inner.clone();
        async move {
            loop {
                if let Some(error) = inner.probe_error.lock().as_ref() {
                    return Err(Box::new(SharedProbeError(error.clone())) as WireAsyncError);
                }
                if inner.info.lock().is_some() {
                    return Ok(());
                }
                tokio::task::yield_now().await;
            }
        }
        .boxed()
    }

    fn os_kind(&self) -> String {
        self.require("osKind").os_kind
    }

    fn os_arch(&self) -> String {
        self.require("osArch").os_arch
    }

    fn os_version(&self) -> String {
        self.require("osVersion").os_version
    }

    fn shell_name(&self) -> String {
        self.require("shellName").shell_name
    }

    fn shell_path(&self) -> String {
        self.require("shellPath").shell_path
    }

    fn path_class(&self) -> String {
        self.require("pathClass").path_class
    }

    fn home_dir(&self) -> String {
        self.require("homeDir").home_dir
    }

    fn js_runtimes(&self) -> Option<Vec<JsRuntimeInfo>> {
        self.require("jsRuntimes").js_runtimes
    }
}

fn translate_probe_error(error: WireAsyncError) -> WireAsyncError {
    if let Some(shell_error) = error.downcast_ref::<ProbeShellNotFoundError>() {
        return Box::new(HostProcessError::with_cause(
            SHELL_GIT_BASH_NOT_FOUND,
            shell_error.to_string(),
            Some(serde_json::json!({ "checkedPaths": shell_error.checked })),
            SharedProbeError(Arc::new(error)),
        ));
    }
    error
}
