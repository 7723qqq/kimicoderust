use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::SecondsFormat;
use kimi_core::primitives::errors::{Error2, Error2Options};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::_base::log::log::ILogService;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::config::config::IConfigService;
use crate::app::event::event::IEventService;
use crate::app::kosong_config::config_section::{
    DEFAULT_MODEL_SECTION, MODELS_SECTION, PROVIDERS_SECTION, THINKING_SECTION,
};
use crate::app::kosong_config::discovery::{
    ModelCatalogChanged, ProviderRefreshChange as CatalogProviderRefreshChange,
    ProviderRefreshFailure as CatalogProviderRefreshFailure, RefreshProviderModelsResponse,
};
use crate::app::region::region::IRegionService;
use crate::app::region::region_service::kimi_region_env;
use crate::app::scopes::LifecycleScope;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::llm_adapter::model::model::{IModelService, ModelRecord};
use crate::llm_adapter::model::model_auth::{
    ModelReadyFailureReason, ModelReadyResolution, ResolveModelAuthMaterialArgs,
    effective_model_config, non_empty, provider_name_from_flat_model, resolve_model_auth_material,
    resolve_model_for_ready,
};
use crate::llm_adapter::provider::provider::{
    IProviderService, OAuthRef, ProviderConfig, ProvidersChangedEvent,
};
use crate::llm_adapter::provider::provider_definition::is_oauth_catalog_vendor;
use crate::region::{KIMI_CODE_PROVIDER_NAME, KimiRegion};
use crate::server::provider_refresh::KIMI_CODE_PLATFORM_ID;
use crate::wire::record::WireAsyncError;

use super::auth::{
    ApplyManagedKimiCodeConfigOptions, AuthCredentialEnvMissingError, AuthManagedUsageResult,
    AuthManagedUserInfoResult, AuthModelNotResolvedError, AuthProvisioningRequiredError, AuthStatus,
    AuthTokenMissingError, BearerTokenProvider, DeviceAuthorization, DeviceCodeTimeoutError,
    IAuthSummaryService, IOAuthService, IOAuthToolkit, KimiCodeLoginAuth, KimiOAuthLoginOptions,
    KimiOAuthLoginResult, KimiOAuthLogoutResult, KimiOAuthTokenRef, OAuthError, OAuthLoginOptions,
    OAuthManagedOptions,
    OAuthSupport, ResolveKimiCodeLoginAuthOptions, ResolveKimiCodeOAuthRefOptions,
    ResolveKimiCodeRuntimeAuthOptions,
};
use super::oauth_protocol::{
    OAuthFlowSnapshot, OAuthFlowStart, OAuthFlowStartAuthenticated,
    OAuthFlowStartAuthenticatedStatus, OAuthFlowStartPending, OAuthFlowStartPendingStatus,
    OAuthFlowStatus, OAuthLoginCancelResponse, OAuthLogoutResponse, ProviderRefreshChange,
    ProviderRefreshFailure, RefreshOAuthProviderModelsResponse,
};

const TERMINAL_RETENTION_MS: u64 = 5 * 60 * 1000;
const DEFAULT_DEVICE_EXPIRES_IN_SEC: u64 = 15 * 60;
const SERVICES_SECTION: &str = "services";

struct FlowMutable {
    device: Option<DeviceAuthorization>,
    status: OAuthFlowStatus,
    token_granted: bool,
    expires_at: i64,
    gc_timer: Option<tokio::task::JoinHandle<()>>,
    error_message: Option<String>,
    resolved_at: Option<String>,
    start_error: Option<WireAsyncError>,
    fast_path_completed: bool,
}

struct FlowState {
    flow_id: String,
    provider: String,
    controller: CancellationToken,
    oauth_ref: Option<OAuthRef>,
    login_base_url: Option<String>,
    started_at: i64,
    mutable: Mutex<FlowMutable>,
    notify: tokio::sync::Notify,
}

impl FlowState {
    fn status(&self) -> OAuthFlowStatus {
        self.mutable.lock().status
    }

    fn device(&self) -> Option<DeviceAuthorization> {
        self.mutable.lock().device.clone()
    }
}

pub struct OAuthService {
    toolkit: Arc<dyn IOAuthToolkit>,
    support: Arc<dyn OAuthSupport>,
    provider_service: Arc<dyn IProviderService>,
    config: Arc<dyn IConfigService>,
    telemetry: Arc<dyn ITelemetryService>,
    log: Arc<dyn ILogService>,
    events: Arc<dyn IEventService>,
    bootstrap: Arc<dyn IBootstrapService>,
    region_service: Arc<dyn IRegionService>,
    flows: Arc<Mutex<HashMap<String, Arc<FlowState>>>>,
    refresh_chain: tokio::sync::Mutex<()>,
    provider_subscription: Mutex<Option<DisposableRef>>,
    self_weak: std::sync::Weak<OAuthService>,
}

impl OAuthService {
    pub fn new(
        toolkit: Arc<dyn IOAuthToolkit>,
        support: Arc<dyn OAuthSupport>,
        provider_service: Arc<dyn IProviderService>,
        config: Arc<dyn IConfigService>,
        telemetry: Arc<dyn ITelemetryService>,
        log: Arc<dyn ILogService>,
        events: Arc<dyn IEventService>,
        bootstrap: Arc<dyn IBootstrapService>,
        region_service: Arc<dyn IRegionService>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak| {
            let listener_weak = weak.clone();
            let subscription = provider_service.on_did_change_providers(Arc::new(
                move |event: ProvidersChangedEvent| {
                    if let Some(service) = listener_weak.upgrade() {
                        service.invalidate_flows(event);
                    }
                },
            ));
            Self {
                toolkit,
                support,
                provider_service,
                config,
                telemetry,
                log,
                events,
                bootstrap,
                region_service,
                flows: Arc::new(Mutex::new(HashMap::new())),
                refresh_chain: tokio::sync::Mutex::new(()),
                provider_subscription: Mutex::new(Some(subscription)),
                self_weak: weak.clone(),
            }
        })
    }

    fn this(&self) -> Arc<Self> {
        self.self_weak.upgrade().expect("OAuthService is alive")
    }

    pub async fn start_login(
        &self,
        provider: Option<&str>,
        options: Option<OAuthLoginOptions>,
    ) -> Result<OAuthFlowStart, WireAsyncError> {
        let provider = provider.unwrap_or(KIMI_CODE_PROVIDER_NAME).to_string();
        self.log.info(
            "oauth startLogin: enter",
            Some(properties(json!({"provider": provider}))),
        );
        let configured = self.provider_service.get(&provider);
        let region = match options.and_then(|options| options.region) {
            Some(region) => Some(region),
            None if provider == KIMI_CODE_PROVIDER_NAME
                && configured.as_ref().is_none_or(|config| config.oauth.is_none())
                && configured.as_ref().is_none_or(|config| config.base_url.is_none()) =>
            {
                Some(self.region_service.get_region_async().await)
            }
            None => None,
        };
        let login_auth = self.resolve_login_auth(&provider, region);
        self.log.info(
            "oauth startLogin: resolved login auth",
            Some(properties(json!({
                "provider": provider,
                "hasOAuthRef": login_auth.oauth_ref.is_some(),
                "hasBaseUrl": login_auth.base_url.is_some(),
                "hasOAuthHost": login_auth.oauth_host.is_some()
            }))),
        );
        self.abort_existing(&provider);

        let started_at = now_ms();
        let state = Arc::new(FlowState {
            flow_id: format!("oauth_{}", Uuid::new_v4()),
            provider: provider.clone(),
            controller: CancellationToken::new(),
            oauth_ref: login_auth.oauth_ref.clone(),
            login_base_url: login_auth.base_url.clone(),
            started_at,
            mutable: Mutex::new(FlowMutable {
                device: None,
                status: OAuthFlowStatus::Pending,
                token_granted: false,
                expires_at: now_ms() + DEFAULT_DEVICE_EXPIRES_IN_SEC as i64 * 1000,
                gc_timer: None,
                error_message: None,
                resolved_at: None,
                start_error: None,
                fast_path_completed: false,
            }),
            notify: tokio::sync::Notify::new(),
        });
        self.flows.lock().insert(provider.clone(), state.clone());

        self.log.info(
            "oauth startLogin: calling toolkit.login",
            Some(properties(json!({"provider": provider}))),
        );
        let callback_state = state.clone();
        let callback_log = self.log.clone();
        let callback_provider = provider.clone();
        let login_options = KimiOAuthLoginOptions {
            signal: state.controller.clone(),
            oauth_ref: login_auth.oauth_ref.as_ref().map(KimiOAuthTokenRef::from),
            base_url: login_auth.base_url,
            oauth_host: login_auth.oauth_host,
            provision_config: None,
            on_device_code: Arc::new(move |auth| {
                callback_log.info(
                    "oauth startLogin: onDeviceCode fired",
                    Some(properties(json!({"provider": callback_provider}))),
                );
                {
                    let mut mutable = callback_state.mutable.lock();
                    if auth.expires_in.is_some() {
                        mutable.expires_at = now_ms() + auth.expires_in.unwrap() as i64 * 1000;
                    }
                    mutable.device = Some(auth);
                }
                callback_state.notify.notify_waiters();
            }),
        };
        let toolkit = self.toolkit.clone();
        let login_provider_name = provider.clone();
        let login = async move {
            toolkit
                .login(Some(login_provider_name), login_options)
                .await
        };
        let service = self.this();
        let login_state = state.clone();
        let login_provider = provider.clone();
        tokio::spawn(async move {
            let result = login.await;
            match result {
                Ok(_) => {
                    let has_device = login_state.device().is_some();
                    if !has_device {
                        service.log.info(
                            "oauth startLogin: toolkit resolved without device code (already authenticated)",
                            Some(properties(json!({"provider": login_provider}))),
                        );
                    }
                    service.log.info(
                        "oauth startLogin: toolkit.login resolved",
                        Some(properties(json!({
                            "provider": login_provider,
                            "deviceArrived": has_device
                        }))),
                    );
                    if has_device {
                        service.handle_success(login_state.clone());
                    } else {
                        service.complete_already_authenticated_login(login_state.clone()).await;
                        login_state.mutable.lock().fast_path_completed = true;
                        login_state.notify.notify_waiters();
                    }
                }
                Err(error) => {
                    service.log.warn(
                        "oauth startLogin: toolkit.login rejected",
                        Some(properties(json!({
                            "provider": login_provider,
                            "error": error.to_string()
                        }))),
                    );
                    service.handle_failure(&login_state, error.as_ref());
                    login_state.mutable.lock().start_error = Some(error);
                    login_state.notify.notify_waiters();
                }
            }
        });

        self.log.info(
            "oauth startLogin: awaiting device flow start",
            Some(properties(json!({"provider": provider}))),
        );
        loop {
            let notified = state.notify.notified();
            enum StartAction {
                Device(DeviceAuthorization),
                Fast,
                Failure(WireAsyncError),
                Wait,
            }
            let action = {
                let mut mutable = state.mutable.lock();
                if let Some(device) = mutable.device.clone() {
                    StartAction::Device(device)
                } else if let Some(error) = mutable.start_error.take() {
                    StartAction::Failure(error)
                } else if mutable.fast_path_completed {
                    StartAction::Fast
                } else {
                    StartAction::Wait
                }
            };
            match action {
                StartAction::Device(device) => {
                    self.log.info(
                        "oauth startLogin: deviceReady resolved",
                        Some(properties(json!({"provider": provider}))),
                    );
                    return Ok(Self::to_flow_start(&state, &device));
                }
                StartAction::Fast => {
                    self.log.info(
                        "oauth startLogin: fast path returned authenticated",
                        Some(properties(json!({"provider": provider}))),
                    );
                    return Ok(OAuthFlowStart::Authenticated(OAuthFlowStartAuthenticated {
                        flow_id: state.flow_id.clone(),
                        provider: state.provider.clone(),
                        status: OAuthFlowStartAuthenticatedStatus::Authenticated,
                    }));
                }
                StartAction::Failure(error) => return Err(error),
                StartAction::Wait => notified.await,
            }
        }
    }

    pub fn get_flow(&self, provider: Option<&str>) -> Option<OAuthFlowSnapshot> {
        let provider = provider.unwrap_or(KIMI_CODE_PROVIDER_NAME);
        let state = self.flows.lock().get(provider).cloned()?;
        Self::to_snapshot(&state)
    }

    pub async fn cancel_login(
        &self,
        provider: Option<&str>,
    ) -> Result<OAuthLoginCancelResponse, WireAsyncError> {
        let provider = provider.unwrap_or(KIMI_CODE_PROVIDER_NAME);
        let state = self.flows.lock().get(provider).cloned();
        let Some(state) = state else {
            return Ok(OAuthLoginCancelResponse {
                cancelled: false,
                status: OAuthFlowStatus::Cancelled,
            });
        };
        if state.status() != OAuthFlowStatus::Pending {
            return Ok(OAuthLoginCancelResponse {
                cancelled: false,
                status: state.status(),
            });
        }
        state.controller.cancel();
        self.set_terminal(&state, OAuthFlowStatus::Cancelled);
        Ok(OAuthLoginCancelResponse {
            cancelled: true,
            status: OAuthFlowStatus::Cancelled,
        })
    }

    pub async fn logout(
        &self,
        provider: Option<&str>,
    ) -> Result<OAuthLogoutResponse, WireAsyncError> {
        let provider = provider.unwrap_or(KIMI_CODE_PROVIDER_NAME);
        let oauth_ref = if provider == KIMI_CODE_PROVIDER_NAME {
            self.resolve_runtime_oauth_ref(provider, None)
        } else {
            self.read_oauth_ref_optional(provider)
        };
        let token_ref = oauth_ref.as_ref().map(KimiOAuthTokenRef::from);
        let result = self.toolkit.logout(Some(provider), token_ref.as_ref()).await?;
        self.abort_existing(provider);
        self.deprovision_provider(provider).await?;
        Ok(OAuthLogoutResponse {
            logged_out: true,
            provider: result.provider_name,
        })
    }

    pub async fn status(&self, provider: Option<&str>) -> Result<AuthStatus, WireAsyncError> {
        let provider = provider.unwrap_or(KIMI_CODE_PROVIDER_NAME);
        self.log.info(
            "oauth status: enter",
            Some(properties(json!({"provider": provider}))),
        );
        let oauth_ref = self.read_oauth_ref_optional(provider);
        match self.get_cached_access_token(provider, oauth_ref.as_ref()).await {
            Ok(token) => {
                self.log.info(
                    "oauth status: got token",
                    Some(properties(json!({
                        "provider": provider,
                        "hasToken": token.is_some()
                    }))),
                );
                Ok(match token {
                    Some(_) => AuthStatus {
                        logged_in: true,
                        provider: Some(provider.to_string()),
                    },
                    None => AuthStatus {
                        logged_in: false,
                        provider: None,
                    },
                })
            }
            Err(error) => {
                self.log.warn(
                    "oauth status: getCachedAccessToken threw",
                    Some(properties(json!({
                        "provider": provider,
                        "error": error.to_string()
                    }))),
                );
                Err(error)
            }
        }
    }

    pub fn resolve_token_provider(
        &self,
        provider: &str,
        oauth_ref: Option<&OAuthRef>,
    ) -> Option<Arc<dyn BearerTokenProvider>> {
        let oauth_ref = self.resolve_runtime_oauth_ref(provider, oauth_ref);
        let token_ref = oauth_ref.as_ref().map(KimiOAuthTokenRef::from);
        self.toolkit.token_provider(Some(provider), token_ref.as_ref())
    }

    pub async fn get_cached_access_token(
        &self,
        provider: &str,
        oauth_ref: Option<&OAuthRef>,
    ) -> Result<Option<String>, WireAsyncError> {
        let oauth_ref = self.resolve_runtime_oauth_ref(provider, oauth_ref);
        let token_ref = oauth_ref.as_ref().map(KimiOAuthTokenRef::from);
        self.toolkit
            .get_cached_access_token(Some(provider), token_ref.as_ref())
            .await
    }

    pub async fn get_managed_usage(
        &self,
        provider: Option<&str>,
    ) -> Result<AuthManagedUsageResult, WireAsyncError> {
        let provider = provider.unwrap_or(KIMI_CODE_PROVIDER_NAME);
        let configured = self.provider_service.get(provider);
        let auth = self.support.resolve_kimi_code_runtime_auth(ResolveKimiCodeRuntimeAuthOptions {
            configured_base_url: configured.as_ref().and_then(|config| config.base_url.clone()),
            configured_oauth_ref: configured.as_ref().and_then(|config| config.oauth.clone()),
        });
        self.toolkit
            .get_managed_usage(
                Some(provider),
                OAuthManagedOptions {
                    oauth_ref: auth.oauth_ref.as_ref().map(KimiOAuthTokenRef::from),
                    base_url: auth.base_url,
                },
            )
            .await
    }

    pub async fn get_managed_user_info(
        &self,
        provider: Option<&str>,
    ) -> Result<AuthManagedUserInfoResult, WireAsyncError> {
        let provider = provider.unwrap_or(KIMI_CODE_PROVIDER_NAME);
        let configured = self.provider_service.get(provider);
        let auth = self.support.resolve_kimi_code_runtime_auth(ResolveKimiCodeRuntimeAuthOptions {
            configured_base_url: configured.as_ref().and_then(|config| config.base_url.clone()),
            configured_oauth_ref: configured.as_ref().and_then(|config| config.oauth.clone()),
        });
        self.toolkit
            .get_managed_user_info(
                Some(provider),
                OAuthManagedOptions {
                    oauth_ref: auth.oauth_ref.as_ref().map(KimiOAuthTokenRef::from),
                    base_url: auth.base_url,
                },
            )
            .await
    }

    pub async fn refresh_oauth_provider_models(
        &self,
    ) -> Result<RefreshOAuthProviderModelsResponse, WireAsyncError> {
        let _guard = self.refresh_chain.lock().await;
        self.do_refresh_oauth_provider_models().await
    }

    async fn do_refresh_oauth_provider_models(
        &self,
    ) -> Result<RefreshOAuthProviderModelsResponse, WireAsyncError> {
        let mut changed = Vec::new();
        let mut unchanged = Vec::new();
        let mut failed = Vec::new();
        if let Err(error) = self.config.reload().await {
            failed.push(ProviderRefreshFailure {
                provider: KIMI_CODE_PROVIDER_NAME.to_string(),
                reason: error.to_string(),
            });
            self.finish_refresh(&changed, &unchanged, &failed);
            return Err(error);
        }
        let current = self.read_user_config_shape();
        if !is_oauth_catalog_provider(managed_provider(&current)) {
            return Ok(self.finish_refresh(&changed, &unchanged, &failed));
        }

        if let Err(error) = self
            .refresh_current_provider(current, &mut changed, &mut unchanged)
            .await
        {
            failed.push(ProviderRefreshFailure {
                provider: KIMI_CODE_PROVIDER_NAME.to_string(),
                reason: error.to_string(),
            });
        }

        let result = self.finish_refresh(&changed, &unchanged, &failed);
        if !result.changed.is_empty() {
            self.events.publish(model_catalog_changed(&result));
        }
        Ok(result)
    }

    async fn refresh_current_provider(
        &self,
        current: Value,
        changed: &mut Vec<ProviderRefreshChange>,
        unchanged: &mut Vec<String>,
    ) -> Result<(), WireAsyncError> {
        let provider: ProviderConfig = serde_json::from_value(
            managed_provider(&current).cloned().unwrap_or(Value::Null),
        )?;
        let declared = crate::llm_adapter::model::model_auth::declared_provider_credential(
            &provider,
            KIMI_CODE_PROVIDER_NAME,
        );
        if let crate::llm_adapter::model::model_auth::DeclaredProviderCredential::Conflict {
            message,
        } = declared
        {
            return Err(message.into());
        }
        let auth = self.support.resolve_kimi_code_runtime_auth(ResolveKimiCodeRuntimeAuthOptions {
            configured_base_url: provider.base_url.clone(),
            configured_oauth_ref: provider.oauth.clone(),
        });
        let token_provider = self
            .resolve_token_provider(KIMI_CODE_PROVIDER_NAME, auth.oauth_ref.as_ref())
            .ok_or_else(|| oauth_token_provider_missing(KIMI_CODE_PROVIDER_NAME))?;
        let token = token_provider.get_access_token(false).await?;
        let models = self
            .support
            .fetch_managed_kimi_code_models(token, auth.base_url.clone())
            .await?;
        if models.is_empty() {
            return Ok(());
        }

        self.config.reload().await?;
        let fresh = self.read_user_config_shape();
        if !is_oauth_catalog_provider(managed_provider(&fresh)) {
            return Ok(());
        }
        let fresh_provider: ProviderConfig = serde_json::from_value(
            managed_provider(&fresh).cloned().unwrap_or(Value::Null),
        )?;
        let provider_oauth = provider.oauth.as_ref().expect("OAuth catalog provider has oauth");
        let fresh_oauth = fresh_provider
            .oauth
            .as_ref()
            .expect("OAuth catalog provider has oauth");
        if fresh_provider.base_url != provider.base_url
            || fresh_oauth.storage != provider_oauth.storage
            || fresh_oauth.key != provider_oauth.key
            || fresh_oauth.oauth_host != provider_oauth.oauth_host
        {
            return Ok(());
        }

        let mut next = fresh.clone();
        let auth_ref = auth.oauth_ref.as_ref().expect("runtime auth has an OAuth ref");
        self.support.apply_managed_kimi_code_config(
            &mut next,
            ApplyManagedKimiCodeConfigOptions {
                models,
                base_url: auth.base_url.clone(),
                oauth_key: auth_ref.key.clone(),
                oauth_host: auth_ref.oauth_host.clone(),
                preserve_default_model: true,
            },
        );
        let refreshed_alias_keys = provider_refresh_alias_keys(
            &fresh,
            &next,
            KIMI_CODE_PROVIDER_NAME,
            &format!("{KIMI_CODE_PLATFORM_ID}/"),
        );
        let preserved = preserve_user_provider_aliases(
            &fresh,
            KIMI_CODE_PROVIDER_NAME,
            &refreshed_alias_keys,
        );
        restore_provider_aliases(&mut next, preserved);
        let default_model = optional_string(fresh.get("defaultModel"));
        let default_enabled = fresh
            .get("thinking")
            .and_then(Value::as_object)
            .and_then(|thinking| thinking.get("enabled"))
            .and_then(Value::as_bool);
        restore_default_selection(&mut next, default_model, default_enabled);
        clamp_dangling_default(&mut next);

        if provider_models_equal(
            &fresh,
            &next,
            KIMI_CODE_PROVIDER_NAME,
            &refreshed_alias_keys,
        ) {
            unchanged.push(KIMI_CODE_PROVIDER_NAME.to_string());
        } else {
            let (added, removed) = compute_changes(
                &collect_model_ids_for_aliases(&fresh, &refreshed_alias_keys),
                &collect_model_ids_for_aliases(&next, &refreshed_alias_keys),
            );
            self.config
                .replace(PROVIDERS_SECTION, Some(next["providers"].clone()))
                .await?;
            self.config
                .replace(
                    MODELS_SECTION,
                    Some(next.get("models").cloned().unwrap_or_else(|| json!({}))),
                )
                .await?;
            self.config
                .replace(DEFAULT_MODEL_SECTION, optional_value(next.get("defaultModel")))
                .await?;
            self.config
                .replace(THINKING_SECTION, optional_value(next.get("thinking")))
                .await?;
            changed.push(ProviderRefreshChange {
                provider_id: KIMI_CODE_PROVIDER_NAME.to_string(),
                provider_name: "Kimi Code".to_string(),
                added,
                removed,
            });
        }
        Ok(())
    }

    fn finish_refresh(
        &self,
        changed: &[ProviderRefreshChange],
        unchanged: &[String],
        failed: &[ProviderRefreshFailure],
    ) -> RefreshOAuthProviderModelsResponse {
        self.telemetry.track2(
            "oauth_models_refresh_finished",
            Some(properties(json!({
                "changed_count": changed.len(),
                "unchanged_count": unchanged.len(),
                "failed_count": failed.len()
            }))),
        );
        RefreshOAuthProviderModelsResponse {
            changed: changed.to_vec(),
            unchanged: unchanged.to_vec(),
            failed: failed.to_vec(),
        }
    }

    fn read_user_config_shape(&self) -> Value {
        let providers = self
            .config
            .inspect(PROVIDERS_SECTION)
            .user_value
            .unwrap_or_else(|| json!({}));
        let models = self
            .config
            .inspect(MODELS_SECTION)
            .user_value
            .unwrap_or_else(|| json!({}));
        let services = self.config.inspect(SERVICES_SECTION).user_value;
        let default_model = self.config.inspect(DEFAULT_MODEL_SECTION).user_value;
        let thinking = self.config.inspect(THINKING_SECTION).user_value;
        let mut output = Map::new();
        output.insert("providers".to_string(), providers);
        output.insert("models".to_string(), models);
        if let Some(services) = services {
            output.insert("services".to_string(), services);
        }
        if let Some(default_model) = default_model {
            output.insert("defaultModel".to_string(), default_model);
        }
        if let Some(thinking) = thinking {
            output.insert("thinking".to_string(), thinking);
        }
        Value::Object(output)
    }

    pub fn get_region(&self) -> KimiRegion {
        self.region_service.current().region
    }

    pub async fn get_region_async(&self) -> KimiRegion {
        self.region_service.get_region_async().await
    }

    fn resolve_login_auth(
        &self,
        provider: &str,
        region: Option<KimiRegion>,
    ) -> KimiCodeLoginAuth {
        let config = self.provider_service.get(provider);
        if provider != KIMI_CODE_PROVIDER_NAME {
            return KimiCodeLoginAuth {
                oauth_ref: config.and_then(|config| config.oauth),
                base_url: None,
                oauth_host: None,
            };
        }
        let env = kimi_region_env(self.bootstrap.as_ref());
        let hosts = region.and_then(|region| self.support.kimi_region_login_hosts(region, &env));
        let login_auth = self.support.resolve_kimi_code_login_auth(ResolveKimiCodeLoginAuthOptions {
            configured_base_url: config.as_ref().and_then(|config| config.base_url.clone()),
            configured_oauth_ref: config.as_ref().and_then(|config| config.oauth.clone()),
            requested_base_url: hosts.as_ref().map(|hosts| hosts.base_url.clone()),
            requested_oauth_host: hosts.as_ref().map(|hosts| hosts.oauth_host.clone()),
            env,
        });
        let oauth_ref = login_auth.oauth_ref.or_else(|| {
            Some(self.support.resolve_kimi_code_oauth_ref(ResolveKimiCodeOAuthRefOptions {
                oauth_host: login_auth.oauth_host.clone(),
                base_url: login_auth.base_url.clone(),
            }))
        });
        KimiCodeLoginAuth {
            oauth_ref,
            base_url: login_auth.base_url,
            oauth_host: login_auth.oauth_host,
        }
    }

    fn read_oauth_ref_optional(&self, provider: &str) -> Option<OAuthRef> {
        self.provider_service.get(provider).and_then(|config| config.oauth)
    }

    fn resolve_runtime_oauth_ref(
        &self,
        provider: &str,
        oauth_ref: Option<&OAuthRef>,
    ) -> Option<OAuthRef> {
        if provider != KIMI_CODE_PROVIDER_NAME {
            return oauth_ref.cloned();
        }
        let config = self.provider_service.get(provider);
        self.support.resolve_kimi_code_runtime_auth(ResolveKimiCodeRuntimeAuthOptions {
            configured_base_url: config.as_ref().and_then(|config| config.base_url.clone()),
            configured_oauth_ref: oauth_ref
                .cloned()
                .or_else(|| config.and_then(|config| config.oauth)),
        })
        .oauth_ref
    }

    fn abort_existing(&self, provider: &str) {
        let existing = self.flows.lock().get(provider).cloned();
        if let Some(existing) = existing
            && existing.status() == OAuthFlowStatus::Pending
        {
            existing.controller.cancel();
            self.set_terminal(&existing, OAuthFlowStatus::Cancelled);
        }
    }

    fn invalidate_flows(&self, event: ProvidersChangedEvent) {
        let affected: HashSet<String> = event
            .removed
            .into_iter()
            .chain(event.changed)
            .collect();
        if affected.is_empty() {
            return;
        }
        let states = self.flows.lock().values().cloned().collect::<Vec<_>>();
        for state in states {
            if !affected.contains(&state.provider) {
                continue;
            }
            let should_cancel = {
                let mutable = state.mutable.lock();
                mutable.status == OAuthFlowStatus::Pending && !mutable.token_granted
            };
            if should_cancel {
                state.controller.cancel();
                state.mutable.lock().error_message =
                    Some("Provider configuration changed during login.".to_string());
                self.set_terminal(&state, OAuthFlowStatus::Cancelled);
            }
        }
    }

    fn handle_success(self: &Arc<Self>, state: Arc<FlowState>) {
        {
            let mut mutable = state.mutable.lock();
            if mutable.status != OAuthFlowStatus::Pending {
                return;
            }
            mutable.token_granted = true;
        }
        let service = self.clone();
        tokio::spawn(async move {
            service.provision_after_success(state).await;
        });
    }

    async fn complete_already_authenticated_login(self: &Arc<Self>, state: Arc<FlowState>) {
        {
            let mut mutable = state.mutable.lock();
            if mutable.status != OAuthFlowStatus::Pending {
                return;
            }
            mutable.token_granted = true;
        }
        self.provision_after_success(state).await;
    }

    async fn provision_after_success(self: &Arc<Self>, state: Arc<FlowState>) {
        let result = async {
            self.provision_provider(
                &state.provider,
                state.oauth_ref.as_ref(),
                state.login_base_url.as_deref(),
            )
            .await?;
            let current = self
                .flows
                .lock()
                .get(&state.provider)
                .is_some_and(|current| Arc::ptr_eq(current, &state));
            if !current {
                return Ok(());
            }
            if state.provider == KIMI_CODE_PROVIDER_NAME {
                self.refresh_oauth_provider_models_best_effort(&state.provider)
                    .await?;
            }
            Ok::<(), WireAsyncError>(())
        }
        .await;
        if let Err(error) = result {
            self.log.warn(
                "oauth provider provisioning failed",
                Some(properties(json!({
                    "provider": state.provider,
                    "error": error.to_string()
                }))),
            );
        }
        if state.status() == OAuthFlowStatus::Pending {
            self.set_terminal(&state, OAuthFlowStatus::Authenticated);
        }
    }

    async fn provision_provider(
        &self,
        provider: &str,
        oauth_ref: Option<&OAuthRef>,
        login_base_url: Option<&str>,
    ) -> Result<(), WireAsyncError> {
        if oauth_ref.is_none() && provider != KIMI_CODE_PROVIDER_NAME {
            return Ok(());
        }
        let base_url = login_base_url
            .map(str::to_string)
            .or_else(|| {
                self.provider_service
                    .get(provider)
                    .and_then(|config| config.base_url)
            })
            .unwrap_or_else(|| self.support.kimi_code_base_url());
        self.provider_service
            .set(
                provider,
                ProviderConfig {
                    type_: Some("kimi".to_string()),
                    base_url: Some(base_url),
                    api_key: Some(String::new()),
                    oauth: oauth_ref.cloned(),
                    ..Default::default()
                },
            )
            .await
    }

    async fn refresh_oauth_provider_models_best_effort(
        &self,
        provider: &str,
    ) -> Result<(), WireAsyncError> {
        let result = self.refresh_oauth_provider_models().await?;
        if !result.failed.is_empty() {
            self.log.warn(
                "oauth startLogin: model refresh failed on already-authenticated fast path",
                Some(properties(json!({
                    "provider": provider,
                    "failures": result.failed
                }))),
            );
        }
        Ok(())
    }

    async fn deprovision_provider(&self, provider: &str) -> Result<(), WireAsyncError> {
        if provider != KIMI_CODE_PROVIDER_NAME {
            return Ok(());
        }
        let mut next = self.read_user_config_shape();
        let cleanup = self.support.clear_managed_kimi_code_config(&mut next);
        if !cleanup.removed_provider
            && cleanup.removed_models.is_empty()
            && !cleanup.default_model_cleared
            && cleanup.removed_services.is_empty()
        {
            return Ok(());
        }
        if cleanup.default_model_cleared {
            if let Some(object) = next.as_object_mut() {
                object.remove("thinking");
            }
        }
        if cleanup.removed_provider {
            self.config
                .replace(PROVIDERS_SECTION, Some(next["providers"].clone()))
                .await?;
        }
        if !cleanup.removed_models.is_empty() {
            self.config
                .replace(
                    MODELS_SECTION,
                    Some(next.get("models").cloned().unwrap_or_else(|| json!({}))),
                )
                .await?;
        }
        if !cleanup.removed_services.is_empty() {
            self.config
                .replace(SERVICES_SECTION, optional_value(next.get("services")))
                .await?;
        }
        if cleanup.default_model_cleared {
            self.config.replace(DEFAULT_MODEL_SECTION, None).await?;
            self.config.replace(THINKING_SECTION, None).await?;
        }
        Ok(())
    }

    fn handle_failure(&self, state: &FlowState, error: &(dyn Error + Send + Sync + 'static)) {
        let status = {
            let mut mutable = state.mutable.lock();
            if mutable.status != OAuthFlowStatus::Pending {
                return;
            }
            mutable.error_message = Some(error.to_string());
            classify_failure(error)
        };
        self.set_terminal(state, status);
    }

    fn set_terminal(&self, state: &Arc<FlowState>, status: OAuthFlowStatus) {
        {
            let mut mutable = state.mutable.lock();
            mutable.status = status;
            mutable.resolved_at = Some(iso_now());
        }
        self.telemetry.track2(
            "oauth_login_finished",
            Some(properties(json!({
                "provider": state.provider,
                "status": status.as_str(),
                "duration_ms": now_ms() - state.started_at
            }))),
        );
        let flows = Arc::downgrade(&self.flows);
        let timer_state = state.clone();
        let provider = state.provider.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(TERMINAL_RETENTION_MS)).await;
            let Some(flows) = flows.upgrade() else {
                return;
            };
            let mut flows = flows.lock();
            if flows
                .get(&provider)
                .is_some_and(|current| Arc::ptr_eq(current, &timer_state))
            {
                flows.remove(&provider);
            }
        });
        state.mutable.lock().gc_timer = Some(timer);
        state.notify.notify_waiters();
    }

    fn to_flow_start(state: &FlowState, device: &DeviceAuthorization) -> OAuthFlowStart {
        let expires_at = state.mutable.lock().expires_at;
        OAuthFlowStart::Pending(OAuthFlowStartPending {
            flow_id: state.flow_id.clone(),
            provider: state.provider.clone(),
            status: OAuthFlowStartPendingStatus::Pending,
            verification_uri: device.verification_uri.clone(),
            verification_uri_complete: device.verification_uri_complete.clone(),
            user_code: device.user_code.clone(),
            expires_in: device.expires_in.unwrap_or(DEFAULT_DEVICE_EXPIRES_IN_SEC),
            interval: device.interval,
            expires_at: iso_from_ms(expires_at),
        })
    }

    fn to_snapshot(state: &FlowState) -> Option<OAuthFlowSnapshot> {
        let mutable = state.mutable.lock();
        let device = mutable.device.clone()?;
        Some(OAuthFlowSnapshot {
            flow_id: state.flow_id.clone(),
            provider: state.provider.clone(),
            status: mutable.status,
            verification_uri: device.verification_uri,
            verification_uri_complete: device.verification_uri_complete,
            user_code: device.user_code,
            expires_in: device.expires_in.unwrap_or(DEFAULT_DEVICE_EXPIRES_IN_SEC),
            expires_at: iso_from_ms(mutable.expires_at),
            interval: device.interval,
            resolved_at: mutable.resolved_at.clone(),
            error_message: mutable.error_message.clone(),
        })
    }
}

impl Drop for OAuthService {
    fn drop(&mut self) {
        if let Some(subscription) = self.provider_subscription.get_mut().take() {
            subscription.dispose();
        }
    }
}

#[async_trait]
impl IOAuthService for OAuthService {
    async fn start_login(
        &self,
        provider: Option<&str>,
        options: Option<OAuthLoginOptions>,
    ) -> Result<OAuthFlowStart, WireAsyncError> {
        OAuthService::start_login(self, provider, options).await
    }

    fn get_flow(&self, provider: Option<&str>) -> Option<OAuthFlowSnapshot> {
        OAuthService::get_flow(self, provider)
    }

    async fn cancel_login(
        &self,
        provider: Option<&str>,
    ) -> Result<OAuthLoginCancelResponse, WireAsyncError> {
        OAuthService::cancel_login(self, provider).await
    }

    async fn logout(&self, provider: Option<&str>) -> Result<OAuthLogoutResponse, WireAsyncError> {
        OAuthService::logout(self, provider).await
    }

    async fn status(&self, provider: Option<&str>) -> Result<AuthStatus, WireAsyncError> {
        OAuthService::status(self, provider).await
    }

    async fn refresh_oauth_provider_models(
        &self,
    ) -> Result<RefreshOAuthProviderModelsResponse, WireAsyncError> {
        OAuthService::refresh_oauth_provider_models(self).await
    }

    async fn get_managed_usage(
        &self,
        provider: Option<&str>,
    ) -> Result<AuthManagedUsageResult, WireAsyncError> {
        OAuthService::get_managed_usage(self, provider).await
    }

    async fn get_managed_user_info(
        &self,
        provider: Option<&str>,
    ) -> Result<AuthManagedUserInfoResult, WireAsyncError> {
        OAuthService::get_managed_user_info(self, provider).await
    }

    fn resolve_token_provider(
        &self,
        provider: &str,
        oauth_ref: &OAuthRef,
    ) -> Option<Arc<dyn BearerTokenProvider>> {
        OAuthService::resolve_token_provider(self, provider, Some(oauth_ref))
    }

    fn resolve_token_provider_optional(
        &self,
        provider: &str,
        oauth_ref: Option<&OAuthRef>,
    ) -> Option<Arc<dyn BearerTokenProvider>> {
        OAuthService::resolve_token_provider(self, provider, oauth_ref)
    }

    async fn get_cached_access_token(
        &self,
        provider: &str,
        oauth_ref: Option<&OAuthRef>,
    ) -> Result<Option<String>, WireAsyncError> {
        OAuthService::get_cached_access_token(self, provider, oauth_ref).await
    }

    fn get_region(&self) -> KimiRegion {
        OAuthService::get_region(self)
    }

    async fn get_region_async(&self) -> KimiRegion {
        OAuthService::get_region_async(self).await
    }
}

pub struct AuthSummaryService {
    provider_service: Arc<dyn IProviderService>,
    model_service: Arc<dyn IModelService>,
    config: Arc<dyn IConfigService>,
    oauth: Arc<dyn IOAuthService>,
    telemetry: Arc<dyn ITelemetryService>,
    log: Arc<dyn ILogService>,
}

impl AuthSummaryService {
    pub fn new(
        provider_service: Arc<dyn IProviderService>,
        model_service: Arc<dyn IModelService>,
        config: Arc<dyn IConfigService>,
        oauth: Arc<dyn IOAuthService>,
        telemetry: Arc<dyn ITelemetryService>,
        log: Arc<dyn ILogService>,
    ) -> Self {
        Self {
            provider_service,
            model_service,
            config,
            oauth,
            telemetry,
            log,
        }
    }

    pub async fn summarize(&self) -> Result<Vec<AuthStatus>, WireAsyncError> {
        let providers = self.provider_service.list();
        let oauth_providers = providers
            .iter()
            .filter(|(_, config)| config.oauth.is_some())
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        self.log.info(
            "auth summarize: enter",
            Some(properties(json!({
                "total": providers.len(),
                "oauthProviders": oauth_providers
            }))),
        );
        let mut statuses = Vec::new();
        for name in oauth_providers {
            match self.oauth.status(Some(&name)).await {
                Ok(status) => statuses.push(status),
                Err(error) => {
                    self.log.warn(
                        "auth summarize: status threw",
                        Some(properties(json!({
                            "provider": name,
                            "error": error.to_string()
                        }))),
                    );
                }
            }
        }
        Ok(statuses)
    }

    pub async fn ensure_ready(&self, model_override: Option<&str>) -> Result<(), WireAsyncError> {
        let result = self.ensure_ready_inner(model_override).await;
        if let Err(error) = &result {
            self.telemetry.track2(
                "auth_ensure_ready_failed",
                Some(properties(json!({
                    "reason": ensure_ready_failure_reason(error.as_ref()).unwrap_or("unexpected"),
                    "has_model_override": model_override.is_some()
                }))),
            );
        }
        result
    }

    async fn ensure_ready_inner(&self, model_override: Option<&str>) -> Result<(), WireAsyncError> {
        self.config.reload().await?;
        let providers = self.provider_service.list();
        let models = self.model_service.list();
        let model_id = model_override
            .map(str::to_string)
            .or_else(|| self.model_service.get_default_model());
        let configured = model_id
            .as_ref()
            .filter(|model_id| !model_id.is_empty())
            .and_then(|model_id| models.get(model_id));
        if providers.is_empty() && !is_providerless_model(configured) {
            return Err(Box::new(AuthProvisioningRequiredError::new()));
        }
        let provider_map = providers
            .iter()
            .map(|(name, provider)| (name.clone(), provider.clone()))
            .collect::<HashMap<_, _>>();
        let resolution = resolve_model_for_ready(
            model_id.as_deref(),
            &models,
            &provider_map,
            self.provider_service.get_default_provider().as_deref(),
        );
        let ModelReadyResolution::Resolved = resolution else {
            return Err(Box::new(unresolved_model_error(
                model_id.clone(),
                match resolution {
                    ModelReadyResolution::Failed(reason) => reason,
                    ModelReadyResolution::Resolved => unreachable!(),
                },
                configured,
            )));
        };

        let model = effective_model_config(configured.expect("resolved model is configured"), None);
        let provider_id = model
            .provider_id
            .clone()
            .or_else(|| model.provider.clone())
            .or_else(|| self.provider_service.get_default_provider());
        let provider = provider_id
            .as_ref()
            .and_then(|provider_id| self.provider_service.get(provider_id));
        let provider_name = provider_id
            .or_else(|| provider_name_from_flat_model(&model))
            .expect("resolved model has a provider name");
        let auth = resolve_model_auth_material(ResolveModelAuthMaterialArgs {
            model_id: model_id.as_deref().expect("resolved model has an id"),
            model: &model,
            provider: provider.as_ref(),
            provider_name: &provider_name,
        })?;
        if auth.api_key.is_some() {
            return Ok(());
        }
        if let Some(api_key_env) = auth.api_key_env {
            if std::env::var(&api_key_env)
                .ok()
                .as_deref()
                .and_then(non_empty)
                .is_some()
            {
                return Ok(());
            }
            return Err(Box::new(AuthCredentialEnvMissingError::new(
                provider_name,
                &api_key_env,
            )));
        }
        if let Some(oauth) = auth.oauth {
            let provider_key = auth.oauth_provider_key.unwrap_or(provider_name);
            let token = self
                .oauth
                .get_cached_access_token(&provider_key, Some(&oauth))
                .await?;
            if non_empty(token.as_deref()).is_some() {
                return Ok(());
            }
            return Err(Box::new(AuthTokenMissingError::new(provider_key)));
        }
        Err(Box::new(AuthTokenMissingError::new(provider_name)))
    }
}

#[async_trait]
impl IAuthSummaryService for AuthSummaryService {
    async fn summarize(&self) -> Result<Vec<AuthStatus>, WireAsyncError> {
        AuthSummaryService::summarize(self).await
    }

    async fn ensure_ready(&self, model_override: Option<&str>) -> Result<(), WireAsyncError> {
        AuthSummaryService::ensure_ready(self, model_override).await
    }
}

fn unresolved_model_error(
    model_id: Option<String>,
    reason: ModelReadyFailureReason,
    configured: Option<&ModelRecord>,
) -> AuthModelNotResolvedError {
    if reason == ModelReadyFailureReason::NoDefault {
        return AuthModelNotResolvedError::new(None, None);
    }
    if reason == ModelReadyFailureReason::ProviderMissing
        && let Some(configured) = configured
    {
        let model = effective_model_config(configured, None);
        return AuthModelNotResolvedError::new(
            model_id,
            model.provider_id.or(model.provider),
        );
    }
    AuthModelNotResolvedError::new(model_id, None)
}

fn ensure_ready_failure_reason(
    error: &(dyn Error + Send + Sync + 'static),
) -> Option<&'static str> {
    if error.downcast_ref::<AuthProvisioningRequiredError>().is_some() {
        return Some("provisioning_required");
    }
    if error.downcast_ref::<AuthModelNotResolvedError>().is_some() {
        return Some("model_not_resolved");
    }
    if error.downcast_ref::<AuthTokenMissingError>().is_some()
        || error.downcast_ref::<AuthCredentialEnvMissingError>().is_some()
    {
        return Some("token_missing");
    }
    None
}

fn classify_failure(error: &(dyn Error + Send + Sync + 'static)) -> OAuthFlowStatus {
    if error.downcast_ref::<DeviceCodeTimeoutError>().is_some() {
        return OAuthFlowStatus::Expired;
    }
    if let Some(error) = error.downcast_ref::<OAuthError>() {
        return if error.to_string().to_lowercase().contains("aborted") {
            OAuthFlowStatus::Cancelled
        } else {
            OAuthFlowStatus::Denied
        };
    }
    OAuthFlowStatus::Denied
}

fn is_providerless_model(model: Option<&ModelRecord>) -> bool {
    let Some(model) = model else {
        return false;
    };
    let effective = effective_model_config(model, None);
    effective.provider_id.is_none()
        && effective.provider.is_none()
        && provider_name_from_flat_model(&effective).is_some()
}

fn managed_provider(config: &Value) -> Option<&Value> {
    config
        .get("providers")
        .and_then(Value::as_object)
        .and_then(|providers| providers.get(KIMI_CODE_PROVIDER_NAME))
}

fn is_oauth_catalog_provider(provider: Option<&Value>) -> bool {
    let Some(provider) = provider.and_then(Value::as_object) else {
        return false;
    };
    let type_ = provider.get("type").and_then(Value::as_str);
    is_oauth_catalog_vendor(type_) && provider.get("oauth").is_some_and(|oauth| !oauth.is_null())
}

fn collect_model_ids_for_aliases(config: &Value, alias_keys: &HashSet<String>) -> HashSet<String> {
    let mut ids = HashSet::new();
    for alias_key in alias_keys {
        if let Some(model) = managed_model(config, alias_key)
            && let Some(id) = model.get("model").and_then(Value::as_str)
            && !id.is_empty()
        {
            ids.insert(id.to_string());
        }
    }
    ids
}

fn provider_alias_keys(config: &Value, provider_id: &str) -> HashSet<String> {
    let mut keys = HashSet::new();
    for (alias, model) in models_object(config) {
        if model.get("provider").and_then(Value::as_str) == Some(provider_id) {
            keys.insert(alias.clone());
        }
    }
    keys
}

fn generated_provider_alias_keys(
    config: &Value,
    provider_id: &str,
    alias_prefix: &str,
) -> HashSet<String> {
    let mut keys = HashSet::new();
    for (alias, model) in models_object(config) {
        if model.get("provider").and_then(Value::as_str) == Some(provider_id)
            && alias.starts_with(alias_prefix)
        {
            keys.insert(alias.clone());
        }
    }
    keys
}

fn compute_changes(old_ids: &HashSet<String>, new_ids: &HashSet<String>) -> (u64, u64) {
    let added = new_ids.difference(old_ids).count() as u64;
    let removed = old_ids.difference(new_ids).count() as u64;
    (added, removed)
}

fn provider_models_equal(
    config: &Value,
    next_config: &Value,
    provider_id: &str,
    alias_keys: &HashSet<String>,
) -> bool {
    provider_model_snapshot(config, provider_id, alias_keys)
        == provider_model_snapshot(next_config, provider_id, alias_keys)
}

fn provider_model_snapshot(
    config: &Value,
    provider_id: &str,
    alias_keys: &HashSet<String>,
) -> String {
    let mut snapshots = Vec::new();
    for alias in alias_keys {
        let Some(model) = managed_model(config, alias).cloned() else {
            continue;
        };
        if model.get("provider").and_then(Value::as_str) != Some(provider_id) {
            continue;
        }
        let mut model = model;
        if let Some(capabilities) = model
            .get("capabilities")
            .and_then(Value::as_array)
            .cloned()
        {
            let mut capabilities = capabilities;
            capabilities.sort_by(|left, right| {
                left.as_str()
                    .unwrap_or_default()
                    .cmp(right.as_str().unwrap_or_default())
            });
            model["capabilities"] = Value::Array(capabilities);
        }
        snapshots.push(json!({"alias": alias, "model": model}));
    }
    snapshots.sort_by(|left, right| {
        left["alias"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["alias"].as_str().unwrap_or_default())
    });
    json!({
        "defaultModel": optional_value(config.get("defaultModel")),
        "models": snapshots
    })
    .to_string()
}

fn provider_refresh_alias_keys(
    config: &Value,
    next_config: &Value,
    provider_id: &str,
    alias_prefix: &str,
) -> HashSet<String> {
    let mut keys = generated_provider_alias_keys(config, provider_id, alias_prefix);
    keys.extend(provider_alias_keys(next_config, provider_id));
    keys
}

fn preserve_user_provider_aliases(
    config: &Value,
    provider_id: &str,
    refreshed_alias_keys: &HashSet<String>,
) -> Map<String, Value> {
    let mut preserved = Map::new();
    for (alias, model) in models_object(config) {
        if model.get("provider").and_then(Value::as_str) != Some(provider_id)
            || refreshed_alias_keys.contains(alias)
        {
            continue;
        }
        preserved.insert(alias.clone(), model.clone());
    }
    preserved
}

fn restore_provider_aliases(config: &mut Value, aliases: Map<String, Value>) {
    if aliases.is_empty() {
        return;
    }
    let models = config
        .as_object_mut()
        .expect("managed config is an object")
        .entry("models")
        .or_insert_with(|| json!({}));
    if !models.is_object() {
        *models = json!({});
    }
    let models = models.as_object_mut().expect("models is an object");
    for (alias, model) in aliases {
        models.insert(alias, model);
    }
}

fn restore_default_selection(
    config: &mut Value,
    default_model: Option<String>,
    default_enabled: Option<bool>,
) {
    let Some(default_model) = default_model else {
        return;
    };
    if managed_model(config, &default_model).is_none() {
        return;
    }
    config["defaultModel"] = Value::String(default_model.clone());
    let capabilities = managed_model(config, &default_model)
        .and_then(|model| model.get("capabilities"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let enabled = if capabilities
        .iter()
        .any(|capability| capability.as_str() == Some("always_thinking"))
    {
        Some(true)
    } else {
        default_enabled
    };
    if let Some(enabled) = enabled {
        let thinking = config
            .as_object_mut()
            .expect("managed config is an object")
            .entry("thinking")
            .or_insert_with(|| json!({}));
        if !thinking.is_object() {
            *thinking = json!({});
        }
        thinking["enabled"] = Value::Bool(enabled);
    }
}

fn clamp_dangling_default(config: &mut Value) {
    let dangling = optional_string(config.get("defaultModel"))
        .is_some_and(|default_model| managed_model(config, &default_model).is_none());
    if dangling {
        if let Some(object) = config.as_object_mut() {
            object.remove("defaultModel");
            object.remove("thinking");
        }
    }
}

fn managed_model<'a>(config: &'a Value, alias: &str) -> Option<&'a Value> {
    config
        .get("models")
        .and_then(Value::as_object)
        .and_then(|models| models.get(alias))
}

fn models_object(config: &Value) -> Map<String, Value> {
    config
        .get("models")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn optional_string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

fn optional_value(value: Option<&Value>) -> Option<Value> {
    value.filter(|value| !value.is_null()).cloned()
}

fn oauth_token_provider_missing(provider: &str) -> WireAsyncError {
    let mut details = Map::new();
    details.insert("provider_id".to_string(), Value::String(provider.to_string()));
    Box::new(Error2::with_options(
        super::errors::AUTH_TOKEN_MISSING,
        "OAuth token provider is not configured.",
        Error2Options {
            details: Some(details),
            ..Default::default()
        },
    ))
}

fn model_catalog_changed(result: &RefreshOAuthProviderModelsResponse) -> ModelCatalogChanged {
    ModelCatalogChanged::new(RefreshProviderModelsResponse {
        changed: result
            .changed
            .iter()
            .map(|change| CatalogProviderRefreshChange {
                provider_id: change.provider_id.clone(),
                provider_name: change.provider_name.clone(),
                added: change.added,
                removed: change.removed,
            })
            .collect(),
        unchanged: result.unchanged.clone(),
        failed: result
            .failed
            .iter()
            .map(|failure| CatalogProviderRefreshFailure {
                provider: failure.provider.clone(),
                reason: failure.reason.clone(),
            })
            .collect(),
        retired_default: None,
    })
}

fn properties(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn iso_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn iso_from_ms(milliseconds: i64) -> String {
    chrono::DateTime::from_timestamp_millis(milliseconds)
        .expect("valid OAuth expiry timestamp")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub struct OAuthToolkitService {
    inner: Arc<dyn IOAuthToolkit>,
}

impl OAuthToolkitService {
    pub fn new(inner: Arc<dyn IOAuthToolkit>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl IOAuthToolkit for OAuthToolkitService {
    async fn login(
        &self,
        provider_name: Option<String>,
        options: KimiOAuthLoginOptions,
    ) -> Result<KimiOAuthLoginResult, WireAsyncError> {
        self.inner.login(provider_name, options).await
    }

    async fn logout(
        &self,
        provider_name: Option<&str>,
        oauth_ref: Option<&KimiOAuthTokenRef>,
    ) -> Result<KimiOAuthLogoutResult, WireAsyncError> {
        self.inner.logout(provider_name, oauth_ref).await
    }

    async fn get_cached_access_token(
        &self,
        provider_name: Option<&str>,
        oauth_ref: Option<&KimiOAuthTokenRef>,
    ) -> Result<Option<String>, WireAsyncError> {
        self.inner
            .get_cached_access_token(provider_name, oauth_ref)
            .await
    }

    fn token_provider(
        &self,
        provider_name: Option<&str>,
        oauth_ref: Option<&KimiOAuthTokenRef>,
    ) -> Option<Arc<dyn BearerTokenProvider>> {
        self.inner.token_provider(provider_name, oauth_ref)
    }

    async fn get_managed_usage(
        &self,
        provider_name: Option<&str>,
        options: OAuthManagedOptions,
    ) -> Result<AuthManagedUsageResult, WireAsyncError> {
        self.inner.get_managed_usage(provider_name, options).await
    }

    async fn get_managed_user_info(
        &self,
        provider_name: Option<&str>,
        options: OAuthManagedOptions,
    ) -> Result<AuthManagedUserInfoResult, WireAsyncError> {
        self.inner.get_managed_user_info(provider_name, options).await
    }
}

pub fn register_auth_services() {
    register_scoped_service(
        LifecycleScope::App,
        ScopeActivation::OnScopeCreated,
        "auth",
    );
    register_scoped_service(
        LifecycleScope::App,
        ScopeActivation::OnScopeCreated,
        "auth",
    );
    register_scoped_service(
        LifecycleScope::App,
        ScopeActivation::OnScopeCreated,
        "auth",
    );
}
