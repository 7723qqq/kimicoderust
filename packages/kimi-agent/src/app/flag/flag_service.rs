use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::Value;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::_base::log::log::ILogService;
use crate::_base::utils::env::parse_boolean_env;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::config::config::IConfigService;
use crate::app::scopes::LifecycleScope;

use super::flag::{
    EXPERIMENTAL_SECTION, ExperimentalFeatureState, ExperimentalFlagConfig, ExperimentalFlagMap,
    ExperimentalFlagSource, IFlagService, state_for_definition,
};
use super::flag_registry::{FlagDefinitionInput, IFlagRegistry};

pub const MASTER_ENV: &str = "KIMI_CODE_EXPERIMENTAL_FLAG";

struct FlagServiceInner {
    bootstrap: Arc<dyn IBootstrapService>,
    config: Arc<dyn IConfigService>,
    registry: Arc<dyn IFlagRegistry>,
    log: Arc<dyn ILogService>,
    config_overrides: Mutex<ExperimentalFlagConfig>,
}

pub struct FlagService {
    inner: Arc<FlagServiceInner>,
    _config_subscription: DisposableRef,
}

impl FlagService {
    pub fn new(
        bootstrap: Arc<dyn IBootstrapService>,
        config: Arc<dyn IConfigService>,
        registry: Arc<dyn IFlagRegistry>,
        log: Arc<dyn ILogService>,
    ) -> Self {
        let overrides = read_config(config.as_ref(), registry.as_ref());
        let inner = Arc::new(FlagServiceInner {
            bootstrap,
            config,
            registry,
            log,
            config_overrides: Mutex::new(overrides),
        });
        let weak = Arc::downgrade(&inner);
        let subscription = inner.config.on_did_section_change(move |event| {
            if event.domain != EXPERIMENTAL_SECTION {
                return;
            }
            if let Some(inner) = weak.upgrade() {
                let overrides = read_config(inner.config.as_ref(), inner.registry.as_ref());
                *inner.config_overrides.lock() = overrides;
            }
        });
        let this = Self {
            inner,
            _config_subscription: subscription,
        };
        this.warn_invalid_env_values();
        this
    }

    pub fn registry(&self) -> Arc<dyn IFlagRegistry> {
        self.inner.registry.clone()
    }

    pub fn set_config_overrides(&self, overrides: Option<ExperimentalFlagConfig>) {
        let canonical = canonicalize_overrides(
            overrides.unwrap_or_default(),
            self.inner.registry.as_ref(),
        );
        *self.inner.config_overrides.lock() = canonical;
    }

    pub fn enabled(&self, id: &str) -> bool {
        self.explain(id).is_some_and(|state| state.enabled)
    }

    pub fn explain(&self, id: &str) -> Option<ExperimentalFeatureState> {
        let definition = self.inner.registry.get(id)?;
        let config_value = self.inner.config_overrides.lock().get(&definition.id).copied();
        let override_value = parse_boolean_env(
            self.inner
                .bootstrap
                .get_env(&definition.env)
                .as_deref(),
        );
        if let Some(override_value) = override_value {
            return Some(state_for_definition(
                &definition,
                override_value,
                ExperimentalFlagSource::Env,
                config_value,
            ));
        }
        if let Some(config_value) = config_value {
            return Some(state_for_definition(
                &definition,
                config_value,
                ExperimentalFlagSource::Config,
                Some(config_value),
            ));
        }
        if parse_boolean_env(self.inner.bootstrap.get_env(MASTER_ENV).as_deref()) == Some(true) {
            return Some(state_for_definition(
                &definition,
                true,
                ExperimentalFlagSource::MasterEnv,
                config_value,
            ));
        }
        Some(state_for_definition(
            &definition,
            definition.default,
            ExperimentalFlagSource::Default,
            None,
        ))
    }

    pub fn snapshot(&self) -> ExperimentalFlagMap {
        self.inner
            .registry
            .list()
            .into_iter()
            .map(|definition| {
                let enabled = self.enabled(&definition.id);
                (definition.id, enabled)
            })
            .collect()
    }

    pub fn enabled_ids(&self) -> Vec<String> {
        self.inner
            .registry
            .list()
            .into_iter()
            .filter(|definition| self.enabled(&definition.id))
            .map(|definition| definition.id)
            .collect()
    }

    pub fn exposed_ids(&self) -> Vec<String> {
        self.inner
            .registry
            .list()
            .into_iter()
            .filter(|definition| self.enabled(&definition.id))
            .filter(|definition| {
                definition
                    .is_exposed
                    .as_ref()
                    .is_none_or(|is_exposed| is_exposed(self))
            })
            .map(|definition| definition.id)
            .collect()
    }

    pub fn explain_all(&self) -> Vec<ExperimentalFeatureState> {
        self.inner
            .registry
            .list()
            .into_iter()
            .filter_map(|definition| self.explain(&definition.id))
            .collect()
    }

    fn warn_invalid_env_values(&self) {
        let mut names = self
            .inner
            .registry
            .list()
            .into_iter()
            .map(|definition| definition.env)
            .collect::<Vec<_>>();
        names.push(MASTER_ENV.to_string());
        for name in names {
            let Some(value) = self.inner.bootstrap.get_env(&name) else {
                continue;
            };
            if value.trim().is_empty() || parse_boolean_env(Some(&value)).is_some() {
                continue;
            }
            let rendered = serde_json::to_string(&value).unwrap();
            self.inner.log.warn(
                &format!(
                    "Ignoring invalid boolean value {rendered} for {name}; expected 1/true/yes/on or 0/false/no/off"
                ),
                None,
            );
        }
    }
}

impl IFlagService for FlagService {
    fn registry(&self) -> Arc<dyn IFlagRegistry> {
        FlagService::registry(self)
    }

    fn enabled(&self, id: &str) -> bool {
        FlagService::enabled(self, id)
    }

    fn snapshot(&self) -> ExperimentalFlagMap {
        FlagService::snapshot(self)
    }

    fn enabled_ids(&self) -> Vec<String> {
        FlagService::enabled_ids(self)
    }

    fn exposed_ids(&self) -> Vec<String> {
        FlagService::exposed_ids(self)
    }

    fn explain(&self, id: &str) -> Option<ExperimentalFeatureState> {
        FlagService::explain(self, id)
    }

    fn explain_all(&self) -> Vec<ExperimentalFeatureState> {
        FlagService::explain_all(self)
    }

    fn set_config_overrides(&self, overrides: Option<ExperimentalFlagConfig>) {
        FlagService::set_config_overrides(self, overrides)
    }
}

fn read_config(
    config: &dyn IConfigService,
    registry: &dyn IFlagRegistry,
) -> ExperimentalFlagConfig {
    let raw = config
        .get(EXPERIMENTAL_SECTION)
        .map(|value| {
            serde_json::from_value::<ExperimentalFlagConfig>(value)
                .expect("invalid experimental flag configuration")
        })
        .unwrap_or_default();
    canonicalize_overrides(raw, registry)
}

fn canonicalize_overrides(
    raw: ExperimentalFlagConfig,
    registry: &dyn IFlagRegistry,
) -> ExperimentalFlagConfig {
    raw.into_iter()
        .map(|(key, value)| (canonical_flag_id(&key, registry), value))
        .collect()
}

fn canonical_flag_id(key: &str, registry: &dyn IFlagRegistry) -> String {
    let normalized = key.replace('-', "_");
    if registry.get(&normalized).is_some() {
        normalized
    } else {
        key.to_string()
    }
}

pub fn register_flag_service() {
    register_scoped_service(
        LifecycleScope::App,
        ScopeActivation::OnScopeCreated,
        "flag",
    );
}
