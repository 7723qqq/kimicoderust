use std::fmt::{Display, Formatter};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use parking_lot::Mutex;

use crate::human::llm::model::LlmModel;
use crate::human::llm::requester::recovery::{
    LlmRecovery, LlmRecoveryContext, LlmRecoveryProposalRecord,
};
use crate::human::llm::requester::requester::{
    LlmCredential, LlmCredentialProvider, merge_request_headers,
};
use crate::wire::record::WireAsyncError;

pub type AccessTokenResolver = Arc<
    dyn Fn(bool) -> BoxFuture<'static, Result<Option<String>, WireAsyncError>> + Send + Sync,
>;

struct StaticCredentialProvider {
    api_key: Option<String>,
}

#[async_trait]
impl LlmCredentialProvider for StaticCredentialProvider {
    async fn resolve(&self) -> Result<Option<LlmCredential>, WireAsyncError> {
        Ok(self.api_key.as_ref().and_then(|api_key| {
            (!api_key.trim().is_empty()).then(|| LlmCredential {
                api_key: Some(api_key.clone()),
                headers: None,
            })
        }))
    }
}

pub fn create_static_credential_provider(
    api_key: Option<String>,
) -> Arc<dyn LlmCredentialProvider> {
    Arc::new(StaticCredentialProvider { api_key })
}

type SharedTokenFuture = Shared<BoxFuture<'static, Result<Option<String>, String>>>;

struct OAuthCredentialProvider {
    get_token: AccessTokenResolver,
    refreshed: Mutex<Option<SharedTokenFuture>>,
}

#[derive(Debug)]
struct TokenError(String);

impl Display for TokenError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TokenError {}

#[derive(Debug)]
struct CredentialStatusError {
    status_code: Option<u16>,
}

impl Display for CredentialStatusError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "credential status: {:?}", self.status_code)
    }
}

impl std::error::Error for CredentialStatusError {}

fn token_future(
    get_token: &AccessTokenResolver,
    force: bool,
) -> SharedTokenFuture {
    get_token(force)
        .map(|result| result.map_err(|error| error.to_string()))
        .boxed()
        .shared()
}

fn map_token_result(result: Result<Option<String>, String>) -> Result<Option<LlmCredential>, WireAsyncError> {
    result
        .map(|api_key| {
            api_key.map(|api_key| LlmCredential {
                api_key: Some(api_key),
                headers: None,
            })
        })
        .map_err(|error| TokenError(error).into())
}

#[async_trait]
impl LlmCredentialProvider for OAuthCredentialProvider {
    async fn resolve(&self) -> Result<Option<LlmCredential>, WireAsyncError> {
        let pending = self.refreshed.lock().take();
        match pending {
            Some(pending) => map_token_result(pending.await),
            None => map_token_result(
                (self.get_token)(false)
                    .await
                    .map_err(|error| error.to_string()),
            ),
        }
    }

    fn can_recover(&self, error: &WireAsyncError) -> bool {
        error
            .downcast_ref::<CredentialStatusError>()
            .and_then(|error| error.status_code)
            .or_else(|| {
                error
                    .downcast_ref::<reqwest::Error>()
                    .and_then(|error| error.status())
                    .map(|status| status.as_u16())
            })
            == Some(401)
    }

    fn invalidate(&self) {
        let pending = {
            let mut refreshed = self.refreshed.lock();
            if refreshed.is_none() {
                *refreshed = Some(token_future(&self.get_token, true));
            }
            refreshed.as_ref().unwrap().clone()
        };
        tokio::spawn(async move {
            let _ = pending.await;
        });
    }
}

pub fn create_oauth_credential_provider(
    get_token: AccessTokenResolver,
) -> Arc<dyn LlmCredentialProvider> {
    Arc::new(OAuthCredentialProvider {
        get_token,
        refreshed: Mutex::new(None),
    })
}

pub fn apply_credential(model: &LlmModel, credential: Option<LlmCredential>) -> LlmModel {
    let Some(credential) = credential else {
        return model.clone();
    };
    let mut model = model.clone();
    if credential.api_key.is_some() {
        model.connection.api_key = credential.api_key;
    }
    model.connection.default_headers = merge_request_headers(
        model.connection.default_headers.as_ref(),
        credential.headers.as_ref(),
    );
    model
}

pub const CREDENTIALS_RECOVERY_ID: &str = "credentials";

pub struct CredentialsRecovery;

impl LlmRecovery for CredentialsRecovery {
    fn propose(&self, ctx: LlmRecoveryContext) -> Option<LlmRecoveryProposalRecord> {
        let provider = ctx.credential_provider?;
        let error: WireAsyncError = Box::new(CredentialStatusError {
            status_code: ctx.error.status_code,
        });
        if !provider.can_recover(&error)
            || ctx
                .applied_recoveries
                .iter()
                .any(|record| record.strategy == CREDENTIALS_RECOVERY_ID)
        {
            return None;
        }
        Some(LlmRecoveryProposalRecord {
            strategy: CREDENTIALS_RECOVERY_ID.to_string(),
            action: "refresh".to_string(),
            attempt_message_override: None,
            before_next_attempt: Some(Box::new(move || provider.invalidate())),
        })
    }
}

pub static CREDENTIALS_RECOVERY: CredentialsRecovery = CredentialsRecovery;
