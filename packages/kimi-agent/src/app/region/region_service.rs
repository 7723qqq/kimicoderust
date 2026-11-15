use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::future::BoxFuture;

use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::kosong_config::config_section::OAuthRef;
use crate::llm_adapter::provider::provider::IProviderService;
use crate::region::KIMI_CODE_PROVIDER_NAME;

use super::region::{IRegionService, KimiRegionResolution};

pub type KimiRegionEnv = HashMap<String, String>;

#[derive(Clone)]
pub enum KimiRegionAuthRead {
    Unreadable,
    LoggedOut,
    LoggedIn(OAuthRef),
}

pub struct KimiRegionResolverOptions {
    pub home_dir: String,
    pub env: KimiRegionEnv,
    pub read_marker: bool,
    pub read_auth: Arc<dyn Fn() -> KimiRegionAuthRead + Send + Sync>,
}

pub trait KimiRegionResolver: Send + Sync {
    fn current(&self) -> KimiRegionResolution;
}

pub type KimiRegionResolverFactory = Arc<dyn Fn(KimiRegionResolverOptions) -> Arc<dyn KimiRegionResolver> + Send + Sync>;

pub fn kimi_region_env(bootstrap: &dyn IBootstrapService) -> KimiRegionEnv {
    let mut env = std::env::vars().collect::<HashMap<_, _>>();
    let oauth_host = bootstrap.get_env("KIMI_CODE_OAUTH_HOST");
    let legacy_oauth_host = bootstrap.get_env("KIMI_OAUTH_HOST");
    let base_url = bootstrap.get_env("KIMI_CODE_BASE_URL");
    if let Some(oauth_host) = oauth_host.clone() {
        env.insert("KIMI_CODE_OAUTH_HOST".to_string(), oauth_host);
    }
    if let Some(legacy_oauth_host) = legacy_oauth_host.clone() {
        env.insert("KIMI_OAUTH_HOST".to_string(), legacy_oauth_host);
    }
    if oauth_host.is_none() && legacy_oauth_host.is_some() {
        env.remove("KIMI_CODE_OAUTH_HOST");
    }
    if let Some(base_url) = base_url {
        env.insert("KIMI_CODE_BASE_URL".to_string(), base_url);
    }
    env
}

pub struct RegionService {
    providers: Arc<dyn IProviderService>,
    resolver: Arc<dyn KimiRegionResolver>,
}

impl RegionService {
    pub fn new(
        providers: Arc<dyn IProviderService>,
        bootstrap: Arc<dyn IBootstrapService>,
        create_resolver: KimiRegionResolverFactory,
    ) -> Self {
        let provider_hydrated = Arc::new(AtomicBool::new(false));
        let auth_hydrated = provider_hydrated.clone();
        let auth_providers = providers.clone();
        let read_auth = Arc::new(move || {
            if !auth_hydrated.load(Ordering::Acquire) {
                return KimiRegionAuthRead::Unreadable;
            }
            match auth_providers.get(KIMI_CODE_PROVIDER_NAME).and_then(|provider| provider.oauth) {
                Some(reference) => KimiRegionAuthRead::LoggedIn(reference),
                None => KimiRegionAuthRead::LoggedOut,
            }
        });
        let read_marker = bootstrap.get_env("KIMI_CODE_REGION_MARKER").or_else(|| std::env::var("KIMI_CODE_REGION_MARKER").ok()).as_deref() != Some("off");
        let resolver = create_resolver(KimiRegionResolverOptions {
            home_dir: bootstrap.home_dir(),
            env: kimi_region_env(bootstrap.as_ref()),
            read_marker,
            read_auth,
        });
        let ready_hydrated = provider_hydrated;
        let ready_providers = providers.clone();
        tokio::spawn(async move {
            let hydrated = ready_providers.ready().await.is_ok();
            ready_hydrated.store(hydrated, Ordering::Release);
        });
        Self {
            providers,
            resolver,
        }
    }
}

impl IRegionService for RegionService {
    fn current(&self) -> KimiRegionResolution {
        self.resolver.current()
    }

    fn get_region_async(&self) -> BoxFuture<'_, crate::region::KimiRegion> {
        Box::pin(async move {
            let _ = self.providers.ready().await;
            self.resolver.current().region
        })
    }
}
