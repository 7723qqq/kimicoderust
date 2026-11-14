use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::human::llm::model::LlmModel;

#[derive(Debug, Clone)]
pub struct ConnectionContext {
    pub model: LlmModel,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolEndpoint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_base_url: Option<String>,
}

#[derive(Clone, Default)]
pub struct ProviderConnection {
    pub endpoint: Option<Arc<dyn Fn(Option<ConnectionContext>) -> Option<ProtocolEndpoint> + Send + Sync>>,
    pub default_headers: Option<Arc<dyn Fn(ConnectionContext) -> Option<HashMap<String, String>> + Send + Sync>>,
}

impl ProviderConnection {
    pub fn new(
        endpoint: impl Fn(Option<ConnectionContext>) -> Option<ProtocolEndpoint> + Send + Sync + 'static,
    ) -> Self {
        Self {
            endpoint: Some(Arc::new(endpoint)),
            default_headers: None,
        }
    }

    pub fn with_default_headers(
        mut self,
        headers: impl Fn(ConnectionContext) -> Option<HashMap<String, String>> + Send + Sync + 'static,
    ) -> Self {
        self.default_headers = Some(Arc::new(headers));
        self
    }
}

pub fn resolve_model_connection(
    model: &LlmModel,
    connection: Option<&ProviderConnection>,
) -> LlmModel {
    let declaration = connection
        .and_then(|connection| connection.endpoint.as_ref())
        .and_then(|endpoint| endpoint(Some(ConnectionContext { model: model.clone() })));
    let Some(declaration) = declaration else {
        return model.clone();
    };
    let read = |env_name: Option<&str>| {
        env_name
            .and_then(|name| std::env::var(name).ok())
            .filter(|value| !value.is_empty())
    };
    let mut resolved = model.clone();
    resolved.connection.base_url = resolved
        .connection
        .base_url
        .take()
        .or_else(|| read(declaration.base_url_env.as_deref()))
        .or(declaration.default_base_url);
    resolved.connection.api_key = resolved
        .connection
        .api_key
        .take()
        .or_else(|| read(declaration.api_key_env.as_deref()));
    resolved
}
