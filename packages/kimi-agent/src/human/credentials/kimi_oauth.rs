use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::FutureExt;

use crate::human::credentials::credentials::create_oauth_credential_provider;
use crate::human::llm::requester::requester::LlmCredentialProvider;
use crate::wire::record::WireAsyncError;

#[async_trait]
pub trait BearerTokenProvider: Send + Sync {
    async fn get_access_token(&self, force: bool) -> Result<Option<String>, WireAsyncError>;
}

pub fn create_kimi_oauth_credential_provider(
    tokens: Arc<dyn BearerTokenProvider>,
) -> Arc<dyn LlmCredentialProvider> {
    create_oauth_credential_provider(Arc::new(move |force| {
        let tokens = tokens.clone();
        async move { tokens.get_access_token(force).await }.boxed()
    }))
}
