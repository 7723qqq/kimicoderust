use std::sync::Arc;

use async_trait::async_trait;
use kimi_core::primitives::errors::{Error2, domains::auth::AUTH_LOGIN_REQUIRED};

use super::config_section::OAuthRef;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AccessTokenOptions {
    pub force: bool,
}

#[async_trait]
pub trait OAuthTokenProvider: Send + Sync {
    async fn get_access_token(&self, options: Option<AccessTokenOptions>) -> Result<String, Error2>;
}

#[async_trait]
pub trait OAuthAccessTokenService: Send + Sync {
    async fn get_cached_access_token(
        &self,
        provider: &str,
        oauth_ref: Option<&OAuthRef>,
    ) -> Result<Option<String>, Error2>;

    fn resolve_token_provider(
        &self,
        provider: &str,
        oauth_ref: Option<&OAuthRef>,
    ) -> Option<Arc<dyn OAuthTokenProvider>>;
}

#[async_trait]
pub trait IModelOAuthTokens: Send + Sync {
    async fn has_cached_access_token(&self, provider: &str, oauth_ref: &OAuthRef) -> bool;
    async fn get_access_token(
        &self,
        provider: &str,
        oauth_ref: &OAuthRef,
        options: Option<AccessTokenOptions>,
    ) -> Result<String, Error2>;
}

pub struct ModelOAuthTokenAdapter {
    oauth: Arc<dyn OAuthAccessTokenService>,
}

impl ModelOAuthTokenAdapter {
    pub fn new(oauth: Arc<dyn OAuthAccessTokenService>) -> Self {
        Self { oauth }
    }
}

#[async_trait]
impl IModelOAuthTokens for ModelOAuthTokenAdapter {
    async fn has_cached_access_token(&self, provider: &str, oauth_ref: &OAuthRef) -> bool {
        self.oauth
            .get_cached_access_token(provider, Some(oauth_ref))
            .await
            .ok()
            .flatten()
            .is_some_and(|token| !token.trim().is_empty())
    }

    async fn get_access_token(
        &self,
        provider: &str,
        oauth_ref: &OAuthRef,
        options: Option<AccessTokenOptions>,
    ) -> Result<String, Error2> {
        let token_provider = self
            .oauth
            .resolve_token_provider(provider, Some(oauth_ref))
            .ok_or_else(|| login_required(provider))?;
        let options = options.filter(|options| options.force);
        let token = token_provider.get_access_token(options).await?;
        if token.trim().is_empty() {
            return Err(login_required(provider));
        }
        Ok(token)
    }
}

fn login_required(provider_key: &str) -> Error2 {
    Error2::new(
        AUTH_LOGIN_REQUIRED,
        format!("OAuth provider \"{provider_key}\" requires login before it can be used."),
    )
}

pub fn register_model_oauth_token_adapter() {
    crate::_base::di::scope::register_scoped_service(
        crate::app::scopes::LifecycleScope::App,
        crate::_base::di::scope::ScopeActivation::OnScopeCreated,
        "kosongConfig",
    );
}
