use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::app::auth::auth::IOAuthService;
use crate::app::config::config::IConfigService;
use crate::app::kosong_config::config_section::{
    DEFAULT_MODEL_SECTION, DEFAULT_PROVIDER_SECTION, MODELS_SECTION, ModelsSection,
    PROVIDERS_SECTION, ProvidersSection,
};
use crate::llm_adapter::model::model_auth::{ModelReadyResolution, resolve_model_for_ready};
use crate::region::KIMI_CODE_PROVIDER_NAME;
use crate::wire::record::WireAsyncError;

use super::auth_legacy::{
    AuthSummary, IAuthLegacyService, ManagedProviderStatus, ManagedProviderSummary,
};

const MANAGED_PROVIDER_NAME: &str = KIMI_CODE_PROVIDER_NAME;

pub struct AuthLegacyService {
    config: Arc<dyn IConfigService>,
    oauth: Arc<dyn IOAuthService>,
}

impl AuthLegacyService {
    pub fn new(config: Arc<dyn IConfigService>, oauth: Arc<dyn IOAuthService>) -> Self {
        Self { config, oauth }
    }

    async fn managed_logged_in(&self) -> bool {
        match self.oauth.status(Some(MANAGED_PROVIDER_NAME)).await {
            Ok(status) => status.logged_in,
            Err(_) => false,
        }
    }
}

#[async_trait]
impl IAuthLegacyService for AuthLegacyService {
    async fn get(&self) -> Result<AuthSummary, WireAsyncError> {
        self.config.ready().await?;

        let snapshot = self.config.get_all();
        let providers = snapshot
            .get(PROVIDERS_SECTION)
            .cloned()
            .map(|value| serde_json::from_value::<ProvidersSection>(value).unwrap_or_default())
            .unwrap_or_default();
        let models = snapshot
            .get(MODELS_SECTION)
            .cloned()
            .map(|value| serde_json::from_value::<ModelsSection>(value).unwrap_or_default())
            .unwrap_or_default();
        let default_model = snapshot.get(DEFAULT_MODEL_SECTION).and_then(|value| value.as_str());
        let default_provider = snapshot
            .get(DEFAULT_PROVIDER_SECTION)
            .and_then(|value| value.as_str());
        let providers_count = providers.len();
        let provider_map = providers
            .iter()
            .map(|(name, provider)| (name.clone(), provider.clone()))
            .collect::<HashMap<_, _>>();
        let models_ready = matches!(
            resolve_model_for_ready(default_model, &models, &provider_map, default_provider),
            ModelReadyResolution::Resolved
        );

        let mut managed_provider = None;
        if providers.contains_key(MANAGED_PROVIDER_NAME) {
            let logged_in = self.managed_logged_in().await;
            managed_provider = Some(ManagedProviderSummary {
                name: MANAGED_PROVIDER_NAME.to_string(),
                status: if logged_in {
                    ManagedProviderStatus::Authenticated
                } else {
                    ManagedProviderStatus::Unauthenticated
                },
            });
        }

        Ok(AuthSummary {
            models_ready,
            providers_count,
            managed_provider,
        })
    }
}
