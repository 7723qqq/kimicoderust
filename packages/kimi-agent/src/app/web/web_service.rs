use std::collections::HashMap;
use std::sync::Arc;

use crate::app::agent_identity::agent_identity::IAgentIdentity;
use crate::app::auth::auth::IOAuthService;
use crate::app::auth::config_section::{SERVICES_SECTION, ServicesConfig};
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::config::config::IConfigService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::llm_adapter::provider::provider::IProviderService;
use crate::llm_adapter::provider::provider_definition::is_oauth_catalog_vendor;
use crate::region::KIMI_CODE_PROVIDER_NAME;

use super::errors::register_web_errors;
use super::providers::local_fetch_url::LocalFetchURLProvider;
use super::providers::moonshot_fetch_url::{
    MoonshotFetchURLProvider, MoonshotFetchURLProviderOptions,
};
use super::tools::fetch_url_types::UrlFetcher;
use super::web::IWebFetchService;

pub struct WebFetchService {
    providers: Arc<dyn IProviderService>,
    oauth: Arc<dyn IOAuthService>,
    bootstrap: Arc<dyn IBootstrapService>,
    config: Arc<dyn IConfigService>,
    identity: Arc<dyn IAgentIdentity>,
    telemetry: Arc<dyn ITelemetryService>,
    local_fetcher: Arc<LocalFetchURLProvider>,
}

impl WebFetchService {
    pub fn new(
        providers: Arc<dyn IProviderService>,
        oauth: Arc<dyn IOAuthService>,
        bootstrap: Arc<dyn IBootstrapService>,
        config: Arc<dyn IConfigService>,
        identity: Arc<dyn IAgentIdentity>,
        telemetry: Arc<dyn ITelemetryService>,
    ) -> Self {
        Self {
            providers,
            oauth,
            bootstrap,
            config,
            identity,
            telemetry,
            local_fetcher: Arc::new(LocalFetchURLProvider::default()),
        }
    }

    fn from_services_config(&self) -> Option<Arc<dyn UrlFetcher>> {
        let services = self
            .config
            .get(SERVICES_SECTION)
            .and_then(|value| serde_json::from_value::<ServicesConfig>(value).ok())?;
        let fetch_config = services.moonshot_fetch?;
        let base_url = fetch_config.base_url.clone()?;
        let token_provider = fetch_config.oauth.as_ref().and_then(|oauth| {
            self.oauth
                .resolve_token_provider(KIMI_CODE_PROVIDER_NAME, oauth)
        });
        let default_headers = self
            .identity
            .current()
            .request_headers
            .into_iter()
            .collect::<HashMap<String, String>>();
        Some(Arc::new(MoonshotFetchURLProvider::new(
            MoonshotFetchURLProviderOptions {
                base_url,
                token_provider,
                api_key: non_empty_string(fetch_config.api_key.as_deref()),
                default_headers: Some(default_headers),
                custom_headers: fetch_config.custom_headers,
                local_fallback: self.local_fetcher.clone(),
                fetch_impl: None,
                telemetry: Some(self.telemetry.clone()),
            },
        )))
    }

    fn from_managed_oauth(&self) -> Option<Arc<dyn UrlFetcher>> {
        let provider = self.providers.get(KIMI_CODE_PROVIDER_NAME)?;
        if !is_oauth_catalog_vendor(provider.type_.as_deref()) {
            return None;
        }
        let oauth = provider.oauth.as_ref()?;
        let token_provider = self
            .oauth
            .resolve_token_provider(KIMI_CODE_PROVIDER_NAME, oauth)?;
        let base_url = provider.base_url.unwrap_or_else(kimi_code_base_url);
        let base_url = format!("{}/fetch", base_url.trim_end_matches('/'));
        Some(Arc::new(MoonshotFetchURLProvider::new(
            MoonshotFetchURLProviderOptions {
                base_url,
                token_provider: Some(token_provider),
                api_key: None,
                default_headers: Some(self.bootstrap.args().request_headers.clone()),
                custom_headers: provider.custom_headers,
                local_fallback: self.local_fetcher.clone(),
                fetch_impl: None,
                telemetry: Some(self.telemetry.clone()),
            },
        )))
    }
}

impl IWebFetchService for WebFetchService {
    fn get_url_fetcher(&self) -> Arc<dyn UrlFetcher> {
        self.from_services_config()
            .or_else(|| self.from_managed_oauth())
            .unwrap_or_else(|| self.local_fetcher.clone())
    }
}

fn non_empty_string(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn kimi_code_base_url() -> String {
    std::env::var("KIMI_CODE_BASE_URL")
        .unwrap_or_else(|_| {
            crate::region::kimi_region_profile(crate::region::KimiRegion::MainlandCn)
                .base_url
                .to_string()
        })
        .trim_end_matches('/')
        .to_string()
}
