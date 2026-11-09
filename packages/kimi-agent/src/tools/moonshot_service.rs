use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

const SERVICE_TIMEOUT_SECS: u64 = 30;

#[derive(Clone, Debug)]
pub struct MoonshotServiceConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub custom_headers: HashMap<String, String>,
}

pub fn install(
    slot: &LazyLock<Mutex<Option<MoonshotServiceConfig>>>,
    config: Option<MoonshotServiceConfig>,
) {
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = config;
}

pub fn current(
    slot: &LazyLock<Mutex<Option<MoonshotServiceConfig>>>,
) -> Option<MoonshotServiceConfig> {
    slot.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn non_blank_key(api_key: &Option<String>) -> Option<String> {
    api_key
        .as_deref()
        .filter(|key| !key.trim().is_empty())
        .map(str::to_string)
}

pub fn bearer_headers(
    api_key: &str,
    custom_headers: &HashMap<String, String>,
    extra: &[(&str, &str)],
    tool_call_id: Option<&str>,
) -> Vec<(String, String)> {
    let mut headers = Vec::with_capacity(
        2 + extra.len() + usize::from(tool_call_id.is_some()) + custom_headers.len(),
    );
    headers.push(("Authorization".to_string(), format!("Bearer {api_key}")));
    for (key, value) in extra {
        headers.push((key.to_string(), value.to_string()));
    }
    headers.push(("Content-Type".to_string(), "application/json".to_string()));
    if let Some(id) = tool_call_id.filter(|id| !id.is_empty()) {
        headers.push(("X-Msh-Tool-Call-Id".to_string(), id.to_string()));
    }
    for (key, value) in custom_headers {
        headers.push((key.clone(), value.clone()));
    }
    headers
}

pub fn build_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(SERVICE_TIMEOUT_SECS))
        .build()
}

pub fn apply_headers(
    mut request: reqwest::RequestBuilder,
    headers: Vec<(String, String)>,
) -> reqwest::RequestBuilder {
    for (key, value) in headers {
        request = request.header(key, value);
    }
    request
}
