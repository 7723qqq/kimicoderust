use std::sync::{Arc, OnceLock};

use async_trait::async_trait;

use crate::_base::utils::fetch_like::FetchError;
use crate::app::agent_identity::agent_identity::IAgentIdentity;
use crate::app::auth::auth::{BearerTokenProvider, IOAuthService};
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::config::config::IConfigService;
use crate::llm_adapter::provider::provider::{IProviderService, ProviderConfig};
use crate::llm_adapter::provider::provider_definition::is_oauth_catalog_vendor;
use crate::region::{KIMI_CODE_PROVIDER_NAME, KimiRegion, kimi_region_profile};

use crate::app::auth::config_section::{MoonshotServiceConfig, SERVICES_SECTION, ServicesConfig};
use super::providers::local_web_search::{LocalWebSearchProvider, LocalWebSearchProviderOptions};
use super::providers::moonshot_web_search::{MoonshotWebSearchProvider, MoonshotWebSearchProviderOptions};
use super::web_search::{
    WebSearchOptions, WebSearchProvider, WebSearchProviderService as IWebSearchProviderService,
    WebSearchResult,
};

pub struct WebSearchProviderService {
    providers: Arc<dyn IProviderService>,
    oauth: Arc<dyn IOAuthService>,
    bootstrap: Arc<dyn IBootstrapService>,
    config: Arc<dyn IConfigService>,
    identity: Arc<dyn IAgentIdentity>,
    local: OnceLock<Arc<LocalWebSearchProvider>>,
    provider: OnceLock<Arc<dyn WebSearchProvider>>,
}

impl WebSearchProviderService {
    pub fn new(
        providers: Arc<dyn IProviderService>,
        oauth: Arc<dyn IOAuthService>,
        bootstrap: Arc<dyn IBootstrapService>,
        config: Arc<dyn IConfigService>,
        identity: Arc<dyn IAgentIdentity>,
    ) -> Self {
        Self {
            providers,
            oauth,
            bootstrap,
            config,
            identity,
            local: OnceLock::new(),
            provider: OnceLock::new(),
        }
    }

    fn local_provider(&self) -> Arc<dyn WebSearchProvider> {
        let local = self
            .local
            .get_or_init(|| Arc::new(LocalWebSearchProvider::new(LocalWebSearchProviderOptions::default())))
            .clone();
        local
    }

    fn configured_search(&self) -> Option<MoonshotServiceConfig> {
        let services = self
            .config
            .get(SERVICES_SECTION)
            .and_then(|value| serde_json::from_value::<ServicesConfig>(value).ok())?;
        let search = services.moonshot_search?;
        if search.base_url.is_none() {
            return None;
        }
        Some(search)
    }

    fn managed_token_provider(
        &self,
    ) -> Option<(ProviderConfig, Arc<dyn BearerTokenProvider>)> {
        let provider = self.providers.get(KIMI_CODE_PROVIDER_NAME)?;
        if !is_oauth_catalog_vendor(provider.type_.as_deref()) {
            return None;
        }
        let token_provider = self
            .oauth
            .resolve_token_provider(KIMI_CODE_PROVIDER_NAME, provider.oauth.as_ref()?)?;
        Some((provider, token_provider))
    }

    fn from_services_config(&self) -> Option<Arc<dyn WebSearchProvider>> {
        let search = self.configured_search()?;
        let token_provider = search.oauth.as_ref().and_then(|oauth| {
            self.oauth
                .resolve_token_provider(KIMI_CODE_PROVIDER_NAME, oauth)
        });
        Some(Arc::new(MoonshotWebSearchProvider::new(
            MoonshotWebSearchProviderOptions {
                base_url: search.base_url.clone()?,
                token_provider,
                api_key: non_empty_string(search.api_key.as_deref()),
                default_headers: Some(
                    self.identity
                        .current()
                        .request_headers
                        .into_iter()
                        .collect(),
                ),
                custom_headers: search.custom_headers,
                fetch_impl: None,
            },
        )))
    }

    fn from_managed_oauth(&self) -> Option<Arc<dyn WebSearchProvider>> {
        let (provider, token_provider) = self.managed_token_provider()?;
        let configured_base_url = provider.base_url.unwrap_or_else(kimi_code_base_url);
        let base_url = format!("{}/search", configured_base_url.trim_end_matches('/'));
        let args = self.bootstrap.args();
        let default_headers = args.request_headers.clone().into_iter().collect();
        Some(Arc::new(MoonshotWebSearchProvider::new(
            MoonshotWebSearchProviderOptions {
                base_url,
                token_provider: Some(token_provider),
                api_key: None,
                default_headers: Some(default_headers),
                custom_headers: provider.custom_headers,
                fetch_impl: None,
            },
        )))
    }
}

impl IWebSearchProviderService for WebSearchProviderService {
    fn get_web_search_provider(&self) -> Option<&dyn WebSearchProvider> {
        let provider = self.provider.get_or_init(|| {
            let mut candidates: Vec<Arc<dyn WebSearchProvider>> = Vec::new();
            if let Some(services) = self.from_services_config() {
                candidates.push(services);
            } else if let Some(managed) = self.from_managed_oauth() {
                candidates.push(managed);
            }
            candidates.push(self.local_provider());
            create_failover_web_search_provider(candidates)
        });
        Some(provider.as_ref())
    }

    fn has_web_search_provider(&self) -> bool {
        true
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
            kimi_region_profile(KimiRegion::MainlandCn)
                .base_url
                .to_string()
        })
        .trim_end_matches('/')
        .to_string()
}

struct FailoverWebSearchProvider {
    candidates: Vec<Arc<dyn WebSearchProvider>>,
}

pub fn create_failover_web_search_provider(
    candidates: Vec<Arc<dyn WebSearchProvider>>,
) -> Arc<dyn WebSearchProvider> {
    Arc::new(FailoverWebSearchProvider { candidates })
}

#[async_trait]
impl WebSearchProvider for FailoverWebSearchProvider {
    async fn search(
        &self,
        query: &str,
        options: WebSearchOptions,
    ) -> Result<Vec<WebSearchResult>, FetchError> {
        let mut last_error = None;
        for candidate in &self.candidates {
            match candidate.search(query, options.clone()).await {
                Ok(results) if !results.is_empty() => return Ok(results),
                Ok(_) => {}
                Err(error) => {
                    if options
                        .signal
                        .as_ref()
                        .is_some_and(|signal| signal.is_cancelled())
                    {
                        return Err(error);
                    }
                    last_error = Some(error);
                }
            }
        }
        match last_error {
            Some(error) => Err(error),
            None => Ok(Vec::new()),
        }
    }
}
