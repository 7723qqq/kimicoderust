use std::collections::HashMap;
use std::sync::Arc;

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::instantiation::{
    DependencyKind, ServiceDependency, ServiceIdentifier, create_decorator,
    refine_service_decorator,
};
use crate::_base::di::instantiation_service::register_descriptor_dependencies;
use crate::_base::di::scope::{Scope, ScopeOptions, ScopeSeed, create_app_scope};
use crate::_base::di::service_collection::ServiceValue;
use crate::_base::log::log::I_LOG_SERVICE;
use crate::features::skill::catalog::file_skill_discovery::FileSkillDiscovery;
use crate::features::skill::catalog::skill_discovery::ISkillDiscovery;
use crate::persistence::backends::node_fs::file_storage_service::FileStorageService;
use crate::persistence::interface::storage::IFileSystemStorageService;

pub use super::home::{ensure_kimi_home, resolve_config_path, resolve_kimi_home};
use super::home::join;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostUiCapability {
    UpdatePanel,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginNodeRunner {
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_files: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_dirs: Option<Vec<String>>,
    pub request_headers: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_style_guide: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_interactive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_capabilities: Option<Vec<HostUiCapability>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_node_runner: Option<PluginNodeRunner>,
}

pub type BootstrapArgs = HostArgs;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostArgsInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_files: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_dirs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_headers: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_style_guide: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_interactive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_capabilities: Option<Vec<HostUiCapability>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_node_runner: Option<PluginNodeRunner>,
}

pub fn resolve_host_args(input: Option<HostArgsInput>) -> HostArgs {
    let input = input.unwrap_or_default();
    HostArgs {
        agent_files: input.agent_files,
        skill_dirs: input.skill_dirs,
        request_headers: input.request_headers.unwrap_or_default(),
        display_name: input.display_name,
        reply_style_guide: input.reply_style_guide,
        non_interactive: input.non_interactive,
        ui_capabilities: input.ui_capabilities,
        plugin_node_runner: input.plugin_node_runner,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KimiHostIdentity {
    pub product_name: String,
    pub version: String,
    pub platform: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent_suffix: Option<String>,
}

#[derive(Debug, Clone)]
pub struct IBootstrapOptions {
    pub home_dir: String,
    pub config_path: String,
    pub os_home_dir: String,
    pub platform: String,
    pub arch: String,
    pub cwd: String,
    pub env: HashMap<String, String>,
    pub client_identity: KimiHostIdentity,
    pub args: HostArgs,
}

pub static I_BOOTSTRAP_OPTIONS: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("bootstrapOptions"));

pub type PersistenceScopeName = str;

pub trait IBootstrapService: Send + Sync {
    fn platform(&self) -> String;
    fn arch(&self) -> String;
    fn cwd(&self) -> String;
    fn os_home_dir(&self) -> String;
    fn home_dir(&self) -> String;
    fn config_path(&self) -> String;
    fn client_identity(&self) -> KimiHostIdentity;
    fn args(&self) -> HostArgs;
    fn sessions_dir(&self) -> String;
    fn blobs_dir(&self) -> String;
    fn store_dir(&self) -> String;
    fn cache_dir(&self) -> String;
    fn logs_dir(&self) -> String;
    fn get_env(&self, name: &str) -> Option<String>;
    fn scope(&self, name: &PersistenceScopeName) -> String;
    fn config_key(&self) -> String;
}

pub static I_BOOTSTRAP_SERVICE: Lazy<ServiceIdentifier<Arc<dyn IBootstrapService>>> =
    Lazy::new(|| create_decorator("bootstrapService"));

#[derive(Debug, Clone)]
pub struct BootstrapInput {
    pub home_dir: Option<String>,
    pub config_path: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub os_home_dir: Option<String>,
    pub platform: Option<String>,
    pub arch: Option<String>,
    pub cwd: Option<String>,
    pub client_identity: KimiHostIdentity,
    pub args: Option<HostArgsInput>,
}

pub fn resolve_bootstrap_options(input: BootstrapInput) -> IBootstrapOptions {
    let BootstrapInput {
        home_dir: input_home_dir,
        config_path: input_config_path,
        env: input_env,
        os_home_dir: input_os_home_dir,
        platform: input_platform,
        arch: input_arch,
        cwd: input_cwd,
        client_identity,
        args,
    } = input;
    let env = input_env.unwrap_or_else(process_env);
    let os_home_dir = input_os_home_dir.unwrap_or_else(os_home_dir);
    let home_dir = resolve_kimi_home(
        input_home_dir.as_deref(),
        Some(&env),
        Some(&os_home_dir),
    );
    let config_path = input_config_path.unwrap_or_else(|| join(&home_dir, "config.toml"));
    IBootstrapOptions {
        home_dir,
        config_path,
        os_home_dir,
        platform: input_platform.unwrap_or_else(process_platform),
        arch: input_arch.unwrap_or_else(process_arch),
        cwd: input_cwd.unwrap_or_else(process_cwd),
        env,
        client_identity,
        args: resolve_host_args(args),
    }
}

pub fn bootstrap_seed(input: BootstrapInput) -> ScopeSeed {
    vec![(
        I_BOOTSTRAP_OPTIONS.clone(),
        ServiceValue::Instance(Arc::new(resolve_bootstrap_options(input)) as DynamicValue),
    )]
}

pub struct BootstrapResult {
    pub app: Arc<Scope>,
}

pub fn bootstrap(
    input: BootstrapInput,
    extra_seeds: Option<ScopeSeed>,
) -> Result<BootstrapResult, String> {
    crate::app::scopes::register_scope_topology().map_err(|error| error.to_string())?;
    let options = resolve_bootstrap_options(input.clone());
    let mut seeds = bootstrap_seed(input);
    seeds.extend(storage_seed(&options));
    seeds.extend(skill_seed());
    seeds.extend(extra_seeds.unwrap_or_default());
    let app = create_app_scope(ScopeOptions {
        seeds: Some(seeds),
        ..Default::default()
    })?;
    Ok(BootstrapResult { app })
}

fn storage_seed(options: &IBootstrapOptions) -> ScopeSeed {
    let home_dir = options.home_dir.clone();
    let ctor: Constructor<DynamicValue> = Arc::new(move |_| {
        let service: Arc<dyn IFileSystemStorageService> = Arc::new(FileStorageService::new(
            home_dir.clone(),
            Some(0o700),
            Some(0o600),
        ));
        Arc::new(service) as DynamicValue
    });
    vec![(
        create_decorator("fileSystemStorageService"),
        ServiceValue::Descriptor(SyncDescriptor::new(ctor, Vec::new())),
    )]
}

fn skill_seed() -> ScopeSeed {
    let ctor: Constructor<DynamicValue> = Arc::new(|args| {
        let log = args
            .first()
            .cloned()
            .and_then(|value| Arc::downcast::<Arc<dyn crate::_base::log::log::ILogService>>(value).ok())
            .expect("FileSkillDiscovery requires the log service")
            .as_ref()
            .clone();
        let service: Arc<dyn ISkillDiscovery> = Arc::new(FileSkillDiscovery::new(log));
        Arc::new(service) as DynamicValue
    });
    let descriptor = SyncDescriptor::new(ctor, Vec::new());
    register_descriptor_dependencies(
        &descriptor,
        vec![ServiceDependency {
            id: refine_service_decorator(&I_LOG_SERVICE),
            index: 0,
            kind: DependencyKind::Instance,
        }],
    );
    vec![(
        create_decorator("skillDiscovery"),
        ServiceValue::Descriptor(descriptor),
    )]
}

fn process_env() -> HashMap<String, String> {
    std::env::vars_os()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

fn os_home_dir() -> String {
    std::env::home_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
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

fn process_cwd() -> String {
    std::env::current_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}
