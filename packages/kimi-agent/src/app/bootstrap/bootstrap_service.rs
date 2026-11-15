use std::collections::HashMap;
use std::sync::Arc;

use once_cell::sync::Lazy;

use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::instantiation::{
    DependencyKind, ScopeActivation, ServiceDependency, refine_service_decorator,
};
use crate::_base::di::instantiation_service::register_descriptor_dependencies;
use crate::_base::di::scope::register_scoped_service;
use crate::app::scopes::LifecycleScope;

use super::bootstrap::{
    HostArgs, I_BOOTSTRAP_OPTIONS, I_BOOTSTRAP_SERVICE, IBootstrapOptions, IBootstrapService,
    KimiHostIdentity, PersistenceScopeName,
};
use super::home::join;

pub struct BootstrapService {
    pub platform: String,
    pub arch: String,
    pub cwd: String,
    pub os_home_dir: String,
    pub home_dir: String,
    pub config_path: String,
    pub client_identity: KimiHostIdentity,
    pub args: HostArgs,
    pub sessions_dir: String,
    pub blobs_dir: String,
    pub store_dir: String,
    pub cache_dir: String,
    pub logs_dir: String,
    pub config_key: String,
    env: HashMap<String, String>,
    scopes: HashMap<String, String>,
}

impl BootstrapService {
    pub fn new(options: IBootstrapOptions) -> Self {
        let sessions_dir = join(&options.home_dir, "sessions");
        let blobs_dir = join(&options.home_dir, "blobs");
        let store_dir = join(&options.home_dir, "store");
        let cache_dir = join(&options.home_dir, "cache");
        let logs_dir = join(&options.home_dir, "logs");
        let config_key = basename(&options.config_path);
        let scopes = HashMap::from([
            ("config".to_string(), String::new()),
            ("sessions".to_string(), "sessions".to_string()),
            ("blobs".to_string(), "blobs".to_string()),
            ("store".to_string(), "store".to_string()),
            ("logs".to_string(), "logs".to_string()),
            ("cache".to_string(), "cache".to_string()),
            ("credentials".to_string(), "credentials".to_string()),
        ]);
        Self {
            platform: options.platform,
            arch: options.arch,
            cwd: options.cwd,
            os_home_dir: options.os_home_dir,
            env: options.env,
            home_dir: options.home_dir,
            config_path: options.config_path,
            client_identity: options.client_identity,
            args: options.args,
            sessions_dir,
            blobs_dir,
            store_dir,
            cache_dir,
            logs_dir,
            config_key,
            scopes,
        }
    }

    pub fn get_env(&self, name: &str) -> Option<String> {
        self.env.get(name).cloned()
    }

    pub fn scope(&self, name: &PersistenceScopeName) -> String {
        self.scopes[name].clone()
    }
}

impl IBootstrapService for BootstrapService {
    fn platform(&self) -> String {
        self.platform.clone()
    }

    fn arch(&self) -> String {
        self.arch.clone()
    }

    fn cwd(&self) -> String {
        self.cwd.clone()
    }

    fn os_home_dir(&self) -> String {
        self.os_home_dir.clone()
    }

    fn home_dir(&self) -> String {
        self.home_dir.clone()
    }

    fn config_path(&self) -> String {
        self.config_path.clone()
    }

    fn client_identity(&self) -> KimiHostIdentity {
        self.client_identity.clone()
    }

    fn args(&self) -> HostArgs {
        self.args.clone()
    }

    fn sessions_dir(&self) -> String {
        self.sessions_dir.clone()
    }

    fn blobs_dir(&self) -> String {
        self.blobs_dir.clone()
    }

    fn store_dir(&self) -> String {
        self.store_dir.clone()
    }

    fn cache_dir(&self) -> String {
        self.cache_dir.clone()
    }

    fn logs_dir(&self) -> String {
        self.logs_dir.clone()
    }

    fn get_env(&self, name: &str) -> Option<String> {
        BootstrapService::get_env(self, name)
    }

    fn scope(&self, name: &PersistenceScopeName) -> String {
        BootstrapService::scope(self, name)
    }

    fn config_key(&self) -> String {
        self.config_key.clone()
    }
}

fn basename(path: &str) -> String {
    path.replace('\\', "/")
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string()
}

static BOOTSTRAP_SERVICE_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let ctor: Constructor<DynamicValue> = Arc::new(|args| {
        let options = args
            .first()
            .cloned()
            .and_then(|value| Arc::downcast::<IBootstrapOptions>(value).ok())
            .expect("BootstrapService requires bootstrap options");
        let service: Arc<dyn IBootstrapService> =
            Arc::new(BootstrapService::new(options.as_ref().clone()));
        Arc::new(service) as DynamicValue
    });
    let descriptor = SyncDescriptor::new(ctor, Vec::new());
    register_descriptor_dependencies(
        &descriptor,
        vec![ServiceDependency {
            id: I_BOOTSTRAP_OPTIONS.clone(),
            index: 0,
            kind: DependencyKind::Instance,
        }],
    );
    register_scoped_service(
        LifecycleScope::App.as_str().to_string(),
        refine_service_decorator(&I_BOOTSTRAP_SERVICE),
        descriptor,
        ScopeActivation::OnScopeCreated,
        "bootstrap",
    )
    .map_err(|error| error.to_string())
});

pub fn ensure_bootstrap_service_registered() -> Result<(), String> {
    Lazy::force(&BOOTSTRAP_SERVICE_REGISTRATION).clone()
}
