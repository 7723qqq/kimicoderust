use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared};
use futures_util::{FutureExt, Stream, StreamExt};
use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::exec_env::shell_path_bridge::REMOTE_SHELL_PATH_BRIDGE;
use crate::_base::log::log::ILogService;
use crate::app::environment_declaration::environment_declaration::IEnvironmentDeclarationService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::environment::environment::{
    Environment, EnvironmentCapability, EnvironmentIdentity, EnvironmentStatus,
};
use crate::environment::environment_defaults::{ENVIRONMENT_PATHS, ENVIRONMENT_WORKSPACES};
use crate::environment::environment_provider::{
    EnvironmentProviderAttachment, EnvironmentProviderFactory, EnvironmentProviderHost,
};
use crate::environment::environment_registry::{
    EnvironmentError, EnvironmentRegistrationHandle,
};
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_process::IHostProcessService;
use crate::remote::declaration::remote_environment_declaration::{
    EnvironmentDeclarationSet, RemoteEnvironmentDeclaration,
};
use crate::wire::record::{WireAsyncError, WireRecord};

use super::connect_guidance::{classify_handshake_failure, connect_with_guidance};
use super::connection::{HandshakeError, HandshakeErrorKind};
use super::launchers::{LauncherSpec, declaration_fingerprint, to_launcher_spec};
use super::remote_session::{RemoteSession, RemoteSessionOptions};
use super::ssh_askpass::SshAuthOptions;

static EMPTY_CAPABILITIES: LazyCapabilities = LazyCapabilities::new(&[]);
static REMOTE_CAPABILITIES: LazyCapabilities = LazyCapabilities::new(&["fs", "process"]);

struct LazyCapabilities(&'static [&'static str]);

impl LazyCapabilities {
    const fn new(values: &'static [&'static str]) -> Self {
        Self(values)
    }

    fn values(&self) -> HashSet<EnvironmentCapability> {
        self.0.iter().map(|value| (*value).to_string()).collect()
    }
}

#[derive(Debug, Clone)]
struct SharedRemoteError(Arc<String>);

impl SharedRemoteError {
    fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedRemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for SharedRemoteError {}

type ConnectFuture = Shared<BoxFuture<'static, Result<(), SharedRemoteError>>>;
type StatusListener = Arc<dyn Fn(EnvironmentStatus) + Send + Sync>;

struct DeclaredEnvironmentState {
    generation: String,
    session: Option<Arc<RemoteSession>>,
    launcher: LauncherSpec,
    fingerprint: String,
    status: EnvironmentStatus,
    inflight: Option<ConnectFuture>,
    last_connect_error: Option<String>,
    epoch: u64,
    has_connected: bool,
    connected_at: i64,
    listeners: IndexMap<u64, StatusListener>,
    next_listener_id: u64,
}

struct DeclaredRemoteEnvironmentInner {
    environment_id: String,
    open: Arc<dyn Fn(LauncherSpec) -> BoxFuture<'static, Result<Arc<RemoteSession>, WireAsyncError>> + Send + Sync>,
    telemetry: Arc<dyn ITelemetryService>,
    state: Arc<Mutex<DeclaredEnvironmentState>>,
}

#[derive(Clone)]
struct DeclaredRemoteEnvironment {
    inner: Arc<DeclaredRemoteEnvironmentInner>,
}

impl DeclaredRemoteEnvironment {
    fn new(
        environment_id: String,
        launcher: LauncherSpec,
        fingerprint: String,
        open: Arc<dyn Fn(LauncherSpec) -> BoxFuture<'static, Result<Arc<RemoteSession>, WireAsyncError>> + Send + Sync>,
        telemetry: Arc<dyn ITelemetryService>,
    ) -> Self {
        let inner = Arc::new(DeclaredRemoteEnvironmentInner {
            environment_id: environment_id.clone(),
            open,
            telemetry,
            state: Arc::new(Mutex::new(DeclaredEnvironmentState {
                generation: format!("{environment_id}-{}", uuid::Uuid::new_v4()),
                session: None,
                launcher,
                fingerprint,
                status: EnvironmentStatus::Pending,
                inflight: None,
                last_connect_error: None,
                epoch: 0,
                has_connected: false,
                connected_at: 0,
                listeners: IndexMap::new(),
                next_listener_id: 0,
            })),
        });
        Self { inner }
    }

    fn identity(&self) -> EnvironmentIdentity {
        EnvironmentIdentity {
            environment_id: self.inner.environment_id.clone(),
            generation: self.inner.state.lock().generation.clone(),
        }
    }

    fn status(&self) -> EnvironmentStatus {
        self.inner.state.lock().status
    }

    fn connect_error(&self) -> Option<String> {
        self.inner.state.lock().last_connect_error.clone()
    }

    fn when_ready(&self) -> Option<BoxFuture<'static, Result<(), WireAsyncError>>> {
        let inflight = self.inner.state.lock().inflight.clone();
        inflight.map(|future| async move { future.await.map_err(SharedRemoteError::boxed) }.boxed())
    }

    fn capabilities(&self) -> HashSet<EnvironmentCapability> {
        if self.inner.state.lock().session.is_some() {
            REMOTE_CAPABILITIES.values()
        } else {
            EMPTY_CAPABILITIES.values()
        }
    }

    fn host(&self) -> Option<crate::os::interface::host_environment::HostEnvironmentInfo> {
        self.inner
            .state
            .lock()
            .session
            .as_ref()
            .map(|session| session.host.host_environment_info())
    }

    fn path(&self) -> Option<crate::environment::environment::EnvironmentPath> {
        let state = self.inner.state.lock();
        let session = state.session.as_ref()?;
        Some(ENVIRONMENT_PATHS[&session.host.path_class].clone())
    }

    fn workspace(&self) -> Option<crate::environment::environment::EnvironmentWorkspace> {
        let state = self.inner.state.lock();
        let session = state.session.as_ref()?;
        Some(ENVIRONMENT_WORKSPACES[&session.host.path_class].clone())
    }

    fn shell_path_bridge(&self) -> Option<crate::environment::environment::ShellPathBridge> {
        let state = self.inner.state.lock();
        let host = state.session.as_ref()?.host.host_environment_info();
        (host.os_kind == "Windows" && host.shell_name == "bash").then_some(REMOTE_SHELL_PATH_BRIDGE.clone())
    }

    fn connected(&self) -> Result<Arc<RemoteSession>, WireAsyncError> {
        let state = self.inner.state.lock();
        if let Some(session) = &state.session {
            if state.status == EnvironmentStatus::Ready {
                return Ok(session.clone());
            }
        }
        Err(Box::new(EnvironmentError::new(
            "environment.unavailable",
            format!("environment {} is {}", self.inner.environment_id, state.status),
            None,
        )))
    }

    fn connect(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        if self.status() == EnvironmentStatus::Disposed {
            return async { Err("remote environment is disposed".into()) }.boxed();
        }
        if self.status() == EnvironmentStatus::Ready {
            return async { Ok(()) }.boxed();
        }
        if let Some(inflight) = self.inner.state.lock().inflight.clone() {
            return async move { inflight.await.map_err(SharedRemoteError::boxed) }.boxed();
        }
        let (epoch, launcher, is_reconnect) = {
            let state = self.inner.state.lock();
            (state.epoch, state.launcher.clone(), state.has_connected)
        };
        let started = now_ms();
        self.inner.state.lock().last_connect_error = None;
        self.set_status(EnvironmentStatus::Connecting);
        let this = self.clone();
        let run = async move {
            let result = async {
                let session = (this.inner.open)(launcher.clone()).await?;
                if epoch != this.inner.state.lock().epoch || this.status() == EnvironmentStatus::Disposed {
                    session.dispose().await;
                    return Err("remote environment connect was cancelled".into());
                }
                this.install(session);
                this.inner.state.lock().has_connected = true;
                this.inner.state.lock().connected_at = now_ms();
                this.set_status(EnvironmentStatus::Ready);
                let mut properties = telemetry_properties(&launcher, started, "ok", is_reconnect);
                properties.insert("duration_ms".to_string(), (now_ms() - started).into());
                this.inner.telemetry.track2("environment_connected", Some(properties));
                Ok(())
            }
            .await;
            if let Err(error) = &result {
                let outcome = if epoch != this.inner.state.lock().epoch { "cancelled" } else { "error" };
                let mut properties = telemetry_properties(&launcher, started, outcome, is_reconnect);
                properties.insert("duration_ms".to_string(), (now_ms() - started).into());
                if epoch == this.inner.state.lock().epoch {
                    properties.insert("error_type".to_string(), classify_handshake_failure(error.as_ref()).as_str().into());
                    this.inner.state.lock().last_connect_error = Some(error.to_string());
                    if this.status() == EnvironmentStatus::Connecting {
                        this.set_status(EnvironmentStatus::Disconnected);
                    }
                }
                this.inner.telemetry.track2("environment_connected", Some(properties));
            }
            if this.inner.state.lock().epoch == epoch {
                this.inner.state.lock().inflight = None;
            }
            result.map_err(SharedRemoteError::new)
        }
        .boxed()
        .shared();
        self.inner.state.lock().inflight = Some(run.clone());
        tokio::spawn(run.clone());
        async move { run.await.map_err(SharedRemoteError::boxed) }.boxed()
    }

    fn disconnect(&self) {
        if matches!(self.status(), EnvironmentStatus::Disposed | EnvironmentStatus::Pending) {
            return;
        }
        self.inner.state.lock().epoch += 1;
        self.inner.state.lock().inflight = None;
        let this = self.clone();
        tokio::spawn(async move {
            this.drop_session().await;
        });
        self.set_status(EnvironmentStatus::Disconnected);
    }

    fn apply_launcher(&self, launcher: LauncherSpec, fingerprint: String) {
        if self.inner.state.lock().fingerprint == fingerprint {
            return;
        }
        {
            let mut state = self.inner.state.lock();
            state.fingerprint = fingerprint;
            state.launcher = launcher;
            state.has_connected = false;
            state.generation = format!("{}-{}", self.inner.environment_id, uuid::Uuid::new_v4());
        }
        if self.status() == EnvironmentStatus::Pending {
            return;
        }
        self.disconnect();
    }

    fn install(&self, session: Arc<RemoteSession>) {
        let previous = {
            let mut state = self.inner.state.lock();
            state.generation = format!("{}-{}", self.inner.environment_id, uuid::Uuid::new_v4());
            state.session.replace(session.clone())
        };
        let weak = Arc::downgrade(&self.inner);
        session.connection.on_did_close(Arc::new(move |info| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let this = Self { inner };
            if !this
                .inner
                .state
                .lock()
                .session
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &session))
            {
                return;
            }
            let launcher = this.inner.state.lock().launcher.clone();
            let connected_at = this.inner.state.lock().connected_at;
            let mut properties = serde_json::Map::new();
            properties.insert("environment_type".to_string(), launcher.type_name().into());
            properties.insert("uptime_ms".to_string(), (now_ms() - connected_at).into());
            properties.insert("reason".to_string(), info.kind.map_or("other", |kind| kind.as_str()).into());
            this.inner.telemetry.track2("environment_disconnected", Some(properties));
            this.inner.state.lock().last_connect_error = Some(info.reason);
            this.inner.state.lock().session = None;
            this.set_status(EnvironmentStatus::Disconnected);
            tokio::spawn(async move {
                session.dispose().await;
            });
        }));
        if let Some(previous) = previous {
            if !Arc::ptr_eq(&previous, &session) {
                tokio::spawn(async move {
                    previous.dispose().await;
                });
            }
        }
    }

    async fn drop_session(&self) {
        let session = self.inner.state.lock().session.take();
        if let Some(session) = session {
            session.dispose().await;
        }
    }

    fn set_status(&self, status: EnvironmentStatus) {
        let listeners = {
            let mut state = self.inner.state.lock();
            if state.status == status || state.status == EnvironmentStatus::Disposed {
                return;
            }
            state.status = status;
            state.listeners.values().cloned().collect::<Vec<_>>()
        };
        for listener in listeners {
            listener(status);
        }
    }

    fn on_did_change_status(&self, listener: StatusListener) -> Box<dyn IDisposable> {
        let id = {
            let mut state = self.inner.state.lock();
            let id = state.next_listener_id;
            state.next_listener_id += 1;
            state.listeners.insert(id, listener);
            id
        };
        let state = self.inner.state.clone();
        crate::_base::di::lifecycle::to_disposable(move || {
            state.lock().listeners.remove(&id);
        })
    }

    async fn dispose(&self) {
        if self.status() == EnvironmentStatus::Disposed {
            return;
        }
        self.inner.state.lock().epoch += 1;
        self.inner.state.lock().inflight = None;
        self.set_status(EnvironmentStatus::Disposed);
        self.inner.state.lock().listeners.clear();
        self.drop_session().await;
    }
}

impl Environment for DeclaredRemoteEnvironment {
    fn identity(&self) -> EnvironmentIdentity {
        DeclaredRemoteEnvironment::identity(self)
    }

    fn status(&self) -> EnvironmentStatus {
        DeclaredRemoteEnvironment::status(self)
    }

    fn connect_error(&self) -> Option<String> {
        DeclaredRemoteEnvironment::connect_error(self)
    }

    fn when_ready(&self) -> Option<BoxFuture<'static, Result<(), WireAsyncError>>> {
        DeclaredRemoteEnvironment::when_ready(self)
    }

    fn capabilities(&self) -> HashSet<EnvironmentCapability> {
        DeclaredRemoteEnvironment::capabilities(self)
    }

    fn host(&self) -> Option<crate::os::interface::host_environment::HostEnvironmentInfo> {
        DeclaredRemoteEnvironment::host(self)
    }

    fn path(&self) -> Option<crate::environment::environment::EnvironmentPath> {
        DeclaredRemoteEnvironment::path(self)
    }

    fn workspace(&self) -> Option<crate::environment::environment::EnvironmentWorkspace> {
        DeclaredRemoteEnvironment::workspace(self)
    }

    fn shell_path_bridge(&self) -> Option<crate::environment::environment::ShellPathBridge> {
        DeclaredRemoteEnvironment::shell_path_bridge(self)
    }

    fn connect(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        DeclaredRemoteEnvironment::connect(self)
    }

    fn fs(&self) -> Arc<dyn IHostFileSystem> {
        Arc::new(DeclaredFs { environment: self.clone() })
    }

    fn process(&self) -> Arc<dyn IHostProcessService> {
        Arc::new(DeclaredProcess { environment: self.clone() })
    }

    fn dispose(&self) -> BoxFuture<'static, ()> {
        let this = self.clone();
        async move { this.dispose().await }.boxed()
    }

    fn on_did_change_status(&self, listener: StatusListener) -> Box<dyn IDisposable> {
        DeclaredRemoteEnvironment::on_did_change_status(self, listener)
    }
}

struct DeclaredFs {
    environment: DeclaredRemoteEnvironment,
}

#[async_trait]
impl IHostFileSystem for DeclaredFs {
    async fn read_text(&self, path: &str, options: Option<crate::os::interface::host_file_system::HostReadTextOptions>) -> Result<String, WireAsyncError> {
        self.environment.connected()?.fs.read_text(path, options).await
    }

    async fn write_text(&self, path: &str, data: &str) -> Result<(), WireAsyncError> {
        self.environment.connected()?.fs.write_text(path, data).await
    }

    async fn append_text(&self, path: &str, data: &str) -> Result<(), WireAsyncError> {
        self.environment.connected()?.fs.append_text(path, data).await
    }

    async fn read_bytes(&self, path: &str, max_bytes: u64, offset: Option<u64>) -> Result<Vec<u8>, WireAsyncError> {
        self.environment.connected()?.fs.read_bytes(path, max_bytes, offset).await
    }

    async fn read_file_head(&self, path: &str, max_bytes: u64) -> Result<Vec<u8>, WireAsyncError> {
        self.environment.connected()?.fs.read_file_head(path, max_bytes).await
    }

    async fn write_bytes(&self, path: &str, data: &[u8]) -> Result<(), WireAsyncError> {
        self.environment.connected()?.fs.write_bytes(path, data).await
    }

    fn read_lines(&self, path: &str, options: Option<crate::os::interface::host_file_system::HostReadLinesOptions>) -> std::pin::Pin<Box<dyn Stream<Item = Result<String, WireAsyncError>> + Send>> {
        let environment = self.environment.clone();
        let path = path.to_string();
        Box::pin(
            futures_util::stream::once(async move {
                let stream: std::pin::Pin<Box<dyn Stream<Item = Result<String, WireAsyncError>> + Send>> =
                    match environment.connected() {
                        Ok(session) => session.fs.read_lines(&path, options),
                        Err(error) => Box::pin(futures_util::stream::once(async move { Err(error) })),
                    };
                stream
            })
            .flatten(),
        )
    }

    async fn create_exclusive(&self, path: &str, data: &[u8]) -> Result<bool, WireAsyncError> {
        self.environment.connected()?.fs.create_exclusive(path, data).await
    }

    async fn stat(&self, path: &str) -> Result<crate::os::interface::host_file_system::HostFileStat, WireAsyncError> {
        self.environment.connected()?.fs.stat(path).await
    }

    async fn lstat(&self, path: &str) -> Result<crate::os::interface::host_file_system::HostFileStat, WireAsyncError> {
        self.environment.connected()?.fs.lstat(path).await
    }

    async fn readdir(&self, path: &str) -> Result<Vec<crate::os::interface::host_file_system::HostDirEntry>, WireAsyncError> {
        self.environment.connected()?.fs.readdir(path).await
    }

    async fn walk(
        &self,
        path: &str,
        ignore: Option<&[String]>,
        deadline_ms: Option<i64>,
        visit: crate::os::interface::host_file_system::HostWalkVisitor,
    ) -> Result<crate::os::interface::host_file_system::HostWalkResult, WireAsyncError> {
        self.environment.connected()?.fs.walk(path, ignore, deadline_ms, visit).await
    }

    async fn mkdir(&self, path: &str, recursive: bool) -> Result<(), WireAsyncError> {
        self.environment.connected()?.fs.mkdir(path, recursive).await
    }

    async fn remove(&self, path: &str) -> Result<(), WireAsyncError> {
        self.environment.connected()?.fs.remove(path).await
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), WireAsyncError> {
        self.environment.connected()?.fs.rename(from, to).await
    }

    async fn realpath(&self, path: &str) -> Result<String, WireAsyncError> {
        self.environment.connected()?.fs.realpath(path).await
    }
}

struct DeclaredProcess {
    environment: DeclaredRemoteEnvironment,
}

#[async_trait]
impl IHostProcessService for DeclaredProcess {
    async fn spawn(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&str>,
        env: Option<&HashMap<String, String>>,
    ) -> Result<Arc<dyn crate::os::interface::host_process::IHostProcess>, WireAsyncError> {
        self.environment.connected()?.process.spawn(command, args, cwd, env).await
    }
}

#[derive(Clone)]
pub struct SshPasswordOptions {
    pub prompt: Arc<dyn Fn(String) -> BoxFuture<'static, Result<Option<String>, WireAsyncError>> + Send + Sync>,
    pub askpass_command: Vec<String>,
}

#[derive(Clone, Default)]
pub struct RemoteEnvironmentProviderFactoryOptions {
    pub client_version: Option<String>,
    pub connect: Option<Arc<dyn Fn(RemoteSessionOptions) -> BoxFuture<'static, Result<Arc<RemoteSession>, WireAsyncError>> + Send + Sync>>,
    pub ssh_password: Option<SshPasswordOptions>,
}

struct DeclaredEnvironmentRecord {
    handle: EnvironmentRegistrationHandle,
    environment: DeclaredRemoteEnvironment,
    declaration: RemoteEnvironmentDeclaration,
    fingerprint: String,
}

pub struct RemoteEnvironmentProviderFactory {
    options: RemoteEnvironmentProviderFactoryOptions,
    ssh_passwords: Arc<Mutex<HashMap<String, (LauncherSpec, String)>>>,
}

impl RemoteEnvironmentProviderFactory {
    pub fn new(options: RemoteEnvironmentProviderFactoryOptions) -> Self {
        Self {
            options,
            ssh_passwords: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn id(&self) -> &'static str {
        "remote-exec"
    }

    pub async fn attach(&self, host: &dyn EnvironmentProviderHost) -> Result<EnvironmentProviderAttachment, WireAsyncError> {
        let log = host.get::<ILogService>();
        let declarations = host.get::<IEnvironmentDeclarationService>();
        let initial = match declarations.declarations().await {
            Ok(initial) => Some(initial),
            Err(error) => {
                log.warn("remote environment declarations failed to load", Some(serde_json::json!({ "error": error.to_string() })));
                None
            }
        };
        let records = Arc::new(Mutex::new(IndexMap::<String, DeclaredEnvironmentRecord>::new()));
        for declaration in initial.map(|initial| initial.entries).unwrap_or_default() {
            match self.register_declared_environment(host, &declaration) {
                Ok(record) => {
                    records.lock().insert(declaration.id.clone(), record);
                    declarations.report_registration(&declaration.id, None);
                }
                Err(error) => {
                    declarations.report_registration(&declaration.id, Some(error.to_string()));
                    log.warn("remote environment registration failed", Some(serde_json::json!({ "error": error.to_string() })));
                }
            }
        }
        let disposed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tail = Arc::new(Mutex::new(None::<Shared<BoxFuture<'static, Result<(), SharedRemoteError>>>>));
        let reconcile = {
            let records = records.clone();
            let declarations = declarations.clone();
            let disposed = disposed.clone();
            let tail = tail.clone();
            let this = self.clone_factory();
            move |resolved: EnvironmentDeclarationSet| {
                let previous = tail.lock().clone();
                let records = records.clone();
                let declarations = declarations.clone();
                let disposed = disposed.clone();
                let this = this.clone();
                let next_tail = async move {
                    if let Some(previous) = previous {
                        previous.await;
                    }
                    if disposed.load(std::sync::atomic::Ordering::Relaxed) {
                        return Err(SharedRemoteError(Arc::new(
                            "remote environment provider is disposed".to_string(),
                        )));
                    }
                    let mut failures = Vec::new();
                    let next = resolved.entries.iter().map(|declaration| (declaration.id.clone(), declaration.clone())).collect::<HashMap<_, _>>();
                    let existing = records.lock().keys().cloned().collect::<Vec<_>>();
                    for id in existing {
                        if next.contains_key(&id) {
                            continue;
                        }
                        this.ssh_passwords.lock().remove(&id);
                        let record = records.lock().remove(&id).unwrap();
                        declarations.report_registration(&id, None);
                        if let Err(error) = record.handle.remove().await {
                            failures.push(error.to_string());
                        }
                    }
                    for declaration in resolved.entries {
                        if disposed.load(std::sync::atomic::Ordering::Relaxed) {
                            return Err(SharedRemoteError(Arc::new(
                                "remote environment provider is disposed".to_string(),
                            )));
                        }
                        let fingerprint = declaration_fingerprint(&declaration.entry);
                        let existing = records.lock().contains_key(&declaration.id);
                        if !existing {
                            match this.register_declared_environment(host, &declaration) {
                                Ok(record) => {
                                    records.lock().insert(declaration.id.clone(), record);
                                    declarations.report_registration(&declaration.id, None);
                                }
                                Err(error) => {
                                    declarations.report_registration(&declaration.id, Some(error.to_string()));
                                    failures.push(error.to_string());
                                }
                            }
                            continue;
                        }
                        let mut records = records.lock();
                        let record = records.get_mut(&declaration.id).unwrap();
                        record.declaration = declaration.clone();
                        if record.fingerprint == fingerprint {
                            continue;
                        }
                        record.fingerprint = fingerprint.clone();
                        this.ssh_passwords.lock().remove(&declaration.id);
                        record.environment.apply_launcher(to_launcher_spec(&declaration.entry), fingerprint);
                    }
                    if !failures.is_empty() {
                        return Err(SharedRemoteError(Arc::new(failures.join("; "))));
                    }
                    Ok(())
                }
                .boxed()
                .shared();
                *tail.lock() = Some(next_tail.clone());
                tokio::spawn(next_tail.clone());
                next_tail
            }
        };
        let listener = declarations.on_did_change({
            let reconcile = reconcile.clone();
            let log = log.clone();
            move |event| {
                let future = reconcile(event.clone());
                let log = log.clone();
                event.wait_until(Box::pin(async move {
                    if let Err(error) = future.await {
                        log.warn(
                            &format!("remote environment declarations failed to reload: {error}"),
                            None,
                        );
                    }
                    Ok(())
                }));
            }
        });
        Ok(EnvironmentProviderAttachment {
            dispose: Box::new(move || {
                let disposed = disposed.clone();
                let tail = tail.lock().clone();
                let ssh_passwords = self.ssh_passwords.clone();
                let records = records.clone();
                async move {
                    disposed.store(true, std::sync::atomic::Ordering::Relaxed);
                    drop(listener);
                    if let Some(tail) = tail {
                        tail.await;
                    }
                    ssh_passwords.lock().clear();
                    records.lock().clear();
                }
                .boxed()
            }),
        })
    }

    fn clone_factory(&self) -> Self {
        Self {
            options: RemoteEnvironmentProviderFactoryOptions {
                client_version: self.options.client_version.clone(),
                connect: self.options.connect.clone(),
                ssh_password: self.options.ssh_password.clone(),
            },
            ssh_passwords: self.ssh_passwords.clone(),
        }
    }

    fn register_declared_environment(
        &self,
        host: &dyn EnvironmentProviderHost,
        declaration: &RemoteEnvironmentDeclaration,
    ) -> Result<DeclaredEnvironmentRecord, WireAsyncError> {
        let launcher = to_launcher_spec(&declaration.entry);
        let fingerprint = declaration_fingerprint(&declaration.entry);
        let id = declaration.id.clone();
        let open = {
            let this = self.clone_factory();
            Arc::new(move |launcher| this.open(&id, launcher))
        };
        let environment = DeclaredRemoteEnvironment::new(
            declaration.id.clone(),
            launcher,
            fingerprint.clone(),
            open,
            host.get::<ITelemetryService>(),
        );
        match host.register_environment(environment.clone()) {
            Ok(handle) => Ok(DeclaredEnvironmentRecord {
                handle,
                environment,
                declaration: declaration.clone(),
                fingerprint,
            }),
            Err(error) => {
                tokio::spawn(environment.dispose());
                Err(error)
            }
        }
    }

    fn open(&self, environment_id: &str, launcher: LauncherSpec) -> BoxFuture<'static, Result<Arc<RemoteSession>, WireAsyncError>> {
        let this = self.clone_factory();
        let environment_id = environment_id.to_string();
        async move {
            let connect = this.options.connect.clone().unwrap_or_else(|| {
                Arc::new(|options| async move { RemoteSession::connect(options).await }.boxed())
            });
            let ssh_password = this.options.ssh_password.clone();
            let cache_launcher = launcher.clone();
            let attempt = {
                let this = this.clone();
                let environment_id = environment_id.clone();
                let connect = connect.clone();
                move |spec: LauncherSpec| {
                    let this = this.clone();
                    let environment_id = environment_id.clone();
                    let connect = connect.clone();
                    let cache_launcher = cache_launcher.clone();
                    let ssh_password = ssh_password.clone();
                    async move {
                        let connect_with_password = {
                            let this = this.clone();
                            let environment_id = environment_id.clone();
                            let connect = connect.clone();
                            let spec = spec.clone();
                            move |password: Option<String>| {
                                let ssh_auth = match (&password, &ssh_password) {
                                    (Some(password), Some(options)) => Some(SshAuthOptions {
                                        password: password.clone(),
                                        command: options.askpass_command.clone(),
                                    }),
                                    _ => None,
                                };
                                connect(RemoteSessionOptions {
                                    environment_id: environment_id.clone(),
                                    launcher: spec.clone(),
                                    client_version: this.options.client_version.clone(),
                                    min_executor_version: None,
                                    initialize_timeout_ms: None,
                                    ssh_auth,
                                })
                            }
                        };
                        let cached = this.ssh_passwords.lock().get(&environment_id).cloned();
                        let initial_password = cached
                            .filter(|(cached_launcher, _)| *cached_launcher == cache_launcher)
                            .map(|(_, password)| password);
                        match connect_with_password(initial_password).await {
                            Ok(session) => Ok(session),
                            Err(error) => {
                                if spec.type_name() != "ssh"
                                    || error.downcast_ref::<HandshakeError>().is_none_or(|error| {
                                        error.kind != Some(HandshakeErrorKind::Authentication)
                                    })
                                    || ssh_password.is_none()
                                {
                                    return Err(error);
                                }
                                this.ssh_passwords.lock().remove(&environment_id);
                                let password = ssh_password
                                    .as_ref()
                                    .unwrap()
                                    .prompt(environment_id.clone())
                                    .await?;
                                let Some(password) = password else {
                                    return Err(error);
                                };
                                this.ssh_passwords.lock().insert(
                                    environment_id.clone(),
                                    (cache_launcher.clone(), password.clone()),
                                );
                                match connect_with_password(Some(password)).await {
                                    Ok(session) => Ok(session),
                                    Err(retry_error) => {
                                        if retry_error.downcast_ref::<HandshakeError>().is_some_and(|error| {
                                            error.kind == Some(HandshakeErrorKind::Authentication)
                                        }) {
                                            this.ssh_passwords.lock().remove(&environment_id);
                                        }
                                        Err(retry_error)
                                    }
                                }
                            }
                        }
                    }
                    .boxed()
                }
            };
            connect_with_guidance(attempt, super::connect_guidance::ConnectWithGuidanceOptions {
                launcher,
                min_executor_version: None,
            })
            .await
        }
        .boxed()
    }
}

#[async_trait]
impl EnvironmentProviderFactory for RemoteEnvironmentProviderFactory {
    fn id(&self) -> &'static str {
        RemoteEnvironmentProviderFactory::id(self)
    }

    async fn attach(&self, host: &dyn EnvironmentProviderHost) -> Result<EnvironmentProviderAttachment, WireAsyncError> {
        RemoteEnvironmentProviderFactory::attach(self, host).await
    }
}

fn telemetry_properties(launcher: &LauncherSpec, started: i64, outcome: &str, is_reconnect: bool) -> serde_json::Map<String, serde_json::Value> {
    let _ = started;
    serde_json::Map::from_iter([
        ("environment_type".to_string(), launcher.type_name().into()),
        ("outcome".to_string(), outcome.into()),
        ("is_reconnect".to_string(), is_reconnect.into()),
    ])
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

trait FailureClassStr {
    fn as_str(self) -> &'static str;
}

impl FailureClassStr for super::connect_guidance::HandshakeFailureClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Timeout => "timeout",
            Self::Incompatible => "incompatible",
            Self::Other => "other",
        }
    }
}

trait CloseKindStr {
    fn as_str(self) -> &'static str;
}

impl CloseKindStr for crate::remote::protocol::channel::ConnectionCloseKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Eof => "eof",
            Self::PipeError => "pipe_error",
            Self::ProtocolError => "protocol_error",
            Self::ClientClosed => "client_closed",
            Self::Liveness => "liveness",
            Self::Other => "other",
        }
    }
}
