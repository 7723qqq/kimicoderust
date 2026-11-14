use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::join_all;
use indexmap::IndexMap;
use kimi_core::primitives::errors::{Error2, Error2Options};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::instantiation::{
    DependencyKind, ScopeActivation, ServiceDependency, create_decorator,
};
use crate::_base::di::instantiation_service::register_descriptor_dependencies;
use crate::_base::di::lifecycle::IDisposable;
use crate::_base::di::scope::register_scoped_service;
use crate::_base::event::{Emitter, Event};
use crate::_base::log::log::{I_LOG_SERVICE, ILogService};
use crate::app::bootstrap::bootstrap::{I_BOOTSTRAP_SERVICE, IBootstrapService};
use crate::app::plugin::plugin::IPluginService;
use crate::app::region::region::IRegionService;
use crate::app::scopes::LifecycleScope;
use crate::os::interface::host_process::IHostProcessService;
use crate::wire::record::WireAsyncError;

use super::capability::{I_CAPABILITY_SERVICE, ICapabilityService};
use super::entries::context::CapabilityEntryContext;
use super::entries::kimi_cu::create_kimi_cu_entry;
use super::entries::kimi_webbridge::create_kimi_webbridge_entry;
use super::errors::{
    CAPABILITY_INSTALL_IN_PROGRESS, CAPABILITY_NOT_FOUND, CAPABILITY_PROCESSES_RUNNING,
    CAPABILITY_UNSUPPORTED, register_capability_errors,
};
use super::types::{
    CapabilityConfirmationRequired, CapabilityDescriptor, CapabilityEntry, CapabilityId,
    CapabilityInstallChange, CapabilityInstallOptions, CapabilityInstallProgress,
    CapabilityReadiness, CapabilityStatus, CapabilityStep, CapabilityStepState,
};

#[derive(Default)]
struct CapabilityState {
    install_progress: HashMap<CapabilityId, CapabilityInstallProgress>,
    running_installs: HashSet<CapabilityId>,
}

struct CapabilityRuntime {
    state: Mutex<CapabilityState>,
    emitter: Emitter<CapabilityInstallChange>,
}

pub struct CapabilityService {
    entries: IndexMap<CapabilityId, Arc<dyn CapabilityEntry>>,
    runtime: Arc<CapabilityRuntime>,
    log: Arc<dyn ILogService>,
    platform: String,
    arch: String,
}

impl CapabilityService {
    pub fn new(
        bootstrap: Arc<dyn IBootstrapService>,
        plugins: Arc<dyn IPluginService>,
        host_process: Arc<dyn IHostProcessService>,
        log: Arc<dyn ILogService>,
        regions: Arc<dyn IRegionService>,
        entries_override: Option<Vec<Arc<dyn CapabilityEntry>>>,
    ) -> Self {
        register_capability_errors();
        let platform = process_platform();
        let arch = process_arch();
        let entries = if let Some(entries) = entries_override {
            let mut mapped = IndexMap::new();
            for entry in entries {
                mapped.insert(entry.id(), entry);
            }
            mapped
        } else {
            let resolve_regions = regions.clone();
            let ctx = CapabilityEntryContext {
                platform: platform.clone(),
                arch: arch.clone(),
                kimi_home_dir: bootstrap.home_dir(),
                user_home_dir: std::env::home_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                plugins,
                host_process,
                fetch_impl: None,
                applications_dir: None,
                webbridge_base_url: None,
                detect_probe_timeout_ms: None,
                command_timeout_ms: None,
                resolve_region: Some(Arc::new(move || {
                    let regions = resolve_regions.clone();
                    Box::pin(async move { regions.get_region_async().await })
                })),
            };
            IndexMap::from([
                (CapabilityId::KimiCu, create_kimi_cu_entry(ctx.clone())),
                (
                    CapabilityId::KimiWebbridge,
                    create_kimi_webbridge_entry(ctx),
                ),
            ])
        };
        Self {
            entries,
            runtime: Arc::new(CapabilityRuntime {
                state: Mutex::new(CapabilityState::default()),
                emitter: Emitter::new(),
            }),
            log,
            platform,
            arch,
        }
    }

    pub fn on_did_change_install(&self) -> Event<CapabilityInstallChange> {
        self.runtime.emitter.event()
    }

    pub fn describe_capabilities(&self) -> Vec<CapabilityDescriptor> {
        self.entries
            .values()
            .map(|entry| CapabilityDescriptor {
                id: entry.id(),
                plugin_id: entry.plugin_id().map(str::to_string),
                display_name: entry.display_name().to_string(),
                description: entry.description().to_string(),
                supported: entry.supported(),
            })
            .collect()
    }

    pub async fn list_capabilities(&self) -> Vec<CapabilityStatus> {
        join_all(
            self.entries
                .values()
                .cloned()
                .map(|entry| self.status_of_safe(entry)),
        )
        .await
    }

    pub async fn get_capability(&self, id: &str) -> Result<CapabilityStatus, WireAsyncError> {
        self.status_of(self.require_entry(id)?).await
    }

    pub async fn install_capability(
        &self,
        id: &str,
        options: Option<CapabilityInstallOptions>,
    ) -> Result<CapabilityStatus, WireAsyncError> {
        let entry = self.require_entry(id)?;
        if !entry.supported() {
            return Err(capability_error(
                CAPABILITY_UNSUPPORTED,
                format!(
                    "Capability \"{}\" is not supported on {}/{}",
                    entry.id().as_str(),
                    self.platform,
                    self.arch
                ),
                entry.id().as_str(),
            ));
        }
        {
            let mut state = self.runtime.state.lock();
            if state.running_installs.contains(&entry.id()) {
                return Err(capability_error(
                    CAPABILITY_INSTALL_IN_PROGRESS,
                    format!(
                        "Capability \"{}\" is already being installed",
                        entry.id().as_str()
                    ),
                    entry.id().as_str(),
                ));
            }
            state.running_installs.insert(entry.id());
        }
        set_install_progress(
            &self.runtime,
            entry.id(),
            CapabilityInstallProgress {
                running: true,
                ..Default::default()
            },
        );
        let runtime = self.runtime.clone();
        let background_entry = entry.clone();
        let log = self.log.clone();
        tokio::spawn(async move {
            let id = background_entry.id();
            let report_runtime = runtime.clone();
            let reporter = Arc::new(move |step: String, percent: Option<f64>| {
                set_install_progress(
                    &report_runtime,
                    id,
                    CapabilityInstallProgress {
                        running: true,
                        step: Some(step),
                        percent,
                        ..Default::default()
                    },
                );
            });
            let result = background_entry.install(reporter, options).await;
            match result {
                Ok(note) => {
                    set_install_progress(
                        &runtime,
                        id,
                        CapabilityInstallProgress {
                            running: false,
                            note,
                            ..Default::default()
                        },
                    );
                }
                Err(error) => {
                    if error.downcast_ref::<Error2>().is_some_and(|error| {
                        error.code().as_str() == CAPABILITY_PROCESSES_RUNNING
                    }) {
                        set_install_progress(
                            &runtime,
                            id,
                            CapabilityInstallProgress {
                                running: false,
                                confirmation_required: Some(
                                    CapabilityConfirmationRequired::StopProcesses,
                                ),
                                ..Default::default()
                            },
                        );
                    } else {
                        let step = runtime
                            .state
                            .lock()
                            .install_progress
                            .get(&id)
                            .and_then(|progress| progress.step.clone());
                        log.warn(
                            "capability install failed",
                            Some(json!({
                                "capabilityId": id.as_str(),
                                "step": step,
                                "error": error.to_string(),
                            })),
                        );
                        set_install_progress(
                            &runtime,
                            id,
                            CapabilityInstallProgress {
                                running: false,
                                error: Some(error.to_string()),
                                ..Default::default()
                            },
                        );
                    }
                }
            }
            runtime.state.lock().running_installs.remove(&id);
        });
        self.status_of(entry).await
    }

    fn require_entry(&self, id: &str) -> Result<Arc<dyn CapabilityEntry>, WireAsyncError> {
        let parsed = id.parse::<CapabilityId>().ok();
        let entry = parsed.and_then(|id| self.entries.get(&id).cloned());
        entry.ok_or_else(|| capability_error(CAPABILITY_NOT_FOUND, format!("Capability \"{id}\" is not registered"), id))
    }

    async fn status_of(
        &self,
        entry: Arc<dyn CapabilityEntry>,
    ) -> Result<CapabilityStatus, WireAsyncError> {
        let install = current_install(&self.runtime, entry.id());
        if !entry.supported() {
            return Ok(CapabilityStatus {
                id: entry.id(),
                plugin_id: entry.plugin_id().map(str::to_string),
                display_name: entry.display_name().to_string(),
                description: entry.description().to_string(),
                supported: false,
                state: CapabilityReadiness::Unsupported,
                version: None,
                steps: Vec::new(),
                install,
            });
        }
        let detected = entry.detect().await?;
        let required = detected
            .steps
            .iter()
            .filter(|step| step.optional != Some(true))
            .collect::<Vec<_>>();
        let required_ok = !required.is_empty()
            && required
                .iter()
                .all(|step| step.state == CapabilityStepState::Ok);
        let any_ok = detected
            .steps
            .iter()
            .any(|step| step.state == CapabilityStepState::Ok);
        let state = if required_ok {
            CapabilityReadiness::Ready
        } else if any_ok {
            CapabilityReadiness::Partial
        } else {
            CapabilityReadiness::NotInstalled
        };
        Ok(CapabilityStatus {
            id: entry.id(),
            plugin_id: entry.plugin_id().map(str::to_string),
            display_name: entry.display_name().to_string(),
            description: entry.description().to_string(),
            supported: true,
            state,
            version: detected.version,
            steps: detected.steps,
            install,
        })
    }

    async fn status_of_safe(&self, entry: Arc<dyn CapabilityEntry>) -> CapabilityStatus {
        match self.status_of(entry.clone()).await {
            Ok(status) => status,
            Err(error) => {
                let install = current_install(&self.runtime, entry.id());
                if !entry.supported() {
                    return CapabilityStatus {
                        id: entry.id(),
                        plugin_id: entry.plugin_id().map(str::to_string),
                        display_name: entry.display_name().to_string(),
                        description: entry.description().to_string(),
                        supported: false,
                        state: CapabilityReadiness::Unsupported,
                        version: None,
                        steps: Vec::new(),
                        install,
                    };
                }
                CapabilityStatus {
                    id: entry.id(),
                    plugin_id: entry.plugin_id().map(str::to_string),
                    display_name: entry.display_name().to_string(),
                    description: entry.description().to_string(),
                    supported: true,
                    state: CapabilityReadiness::Partial,
                    version: None,
                    steps: vec![CapabilityStep {
                        id: "detect".to_string(),
                        state: CapabilityStepState::Failed,
                        detail: Some(error.to_string()),
                        optional: None,
                    }],
                    install,
                }
            }
        }
    }
}

#[async_trait]
impl ICapabilityService for CapabilityService {
    fn on_did_change_install(&self) -> Event<CapabilityInstallChange> {
        CapabilityService::on_did_change_install(self)
    }

    fn describe_capabilities(&self) -> Vec<CapabilityDescriptor> {
        CapabilityService::describe_capabilities(self)
    }

    async fn list_capabilities(&self) -> Vec<CapabilityStatus> {
        CapabilityService::list_capabilities(self).await
    }

    async fn get_capability(&self, id: &str) -> Result<CapabilityStatus, WireAsyncError> {
        CapabilityService::get_capability(self, id).await
    }

    async fn install_capability(
        &self,
        id: &str,
        options: Option<CapabilityInstallOptions>,
    ) -> Result<CapabilityStatus, WireAsyncError> {
        CapabilityService::install_capability(self, id, options).await
    }
}

impl IDisposable for CapabilityService {
    fn dispose(&self) {
        self.runtime.emitter.dispose();
    }
}

fn current_install(
    runtime: &Arc<CapabilityRuntime>,
    id: CapabilityId,
) -> CapabilityInstallProgress {
    runtime
        .state
        .lock()
        .install_progress
        .get(&id)
        .cloned()
        .unwrap_or_default()
}

fn set_install_progress(
    runtime: &Arc<CapabilityRuntime>,
    id: CapabilityId,
    progress: CapabilityInstallProgress,
) {
    runtime
        .state
        .lock()
        .install_progress
        .insert(id, progress.clone());
    runtime.emitter.fire(CapabilityInstallChange { id, install: progress });
}

fn capability_error(code: &'static str, message: String, id: &str) -> WireAsyncError {
    let mut details = Map::new();
    details.insert("id".to_string(), Value::String(id.to_string()));
    Box::new(Error2::with_options(
        code,
        message,
        Error2Options {
            details: Some(details),
            ..Default::default()
        },
    ))
}

fn process_platform() -> String {
    match std::env::consts::OS {
        "windows" => "win32".to_string(),
        "macos" => "darwin".to_string(),
        platform => platform.to_string(),
    }
}

fn process_arch() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "x64".to_string(),
        "aarch64" => "arm64".to_string(),
        "x86" => "ia32".to_string(),
        "powerpc64" => "ppc64".to_string(),
        arch => arch.to_string(),
    }
}

fn dependency<T>(args: &[DynamicValue], index: usize) -> Arc<T>
where
    T: Send + Sync + 'static + ?Sized,
{
    Arc::downcast::<Arc<T>>(args[index].clone())
        .unwrap_or_else(|_| panic!("capability dependency {index} has an incompatible type"))
        .as_ref()
        .clone()
}

static CAPABILITY_SERVICE_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let ctor: Constructor<DynamicValue> = Arc::new(|args| {
        let service: Arc<dyn ICapabilityService> = Arc::new(CapabilityService::new(
            dependency::<dyn IBootstrapService>(&args, 0),
            dependency::<dyn IPluginService>(&args, 1),
            dependency::<dyn IHostProcessService>(&args, 2),
            dependency::<dyn ILogService>(&args, 3),
            dependency::<dyn IRegionService>(&args, 4),
            None,
        ));
        Arc::new(service) as DynamicValue
    });
    let descriptor = SyncDescriptor::new(ctor, Vec::new());
    register_descriptor_dependencies(
        &descriptor,
        vec![
            ServiceDependency {
                id: I_BOOTSTRAP_SERVICE.clone(),
                index: 0,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: create_decorator("pluginService"),
                index: 1,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: create_decorator("hostProcessService"),
                index: 2,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: I_LOG_SERVICE.clone(),
                index: 3,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: create_decorator("regionService"),
                index: 4,
                kind: DependencyKind::Instance,
            },
        ],
    );
    register_scoped_service(
        LifecycleScope::App.as_str().to_string(),
        I_CAPABILITY_SERVICE.clone(),
        descriptor,
        ScopeActivation::OnScopeCreated,
        "capability",
    )
    .map_err(|error| error.to_string())
});

pub fn ensure_capability_service_registered() -> Result<(), String> {
    Lazy::force(&CAPABILITY_SERVICE_REGISTRATION).clone()
}
