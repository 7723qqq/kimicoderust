use std::error::Error;
use std::sync::LazyLock;

use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize};

pub const DEFAULT_IDENTITY_SLUG: &str = "agent";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentIdentitySnapshot {
    pub display_name: Option<String>,
    pub slug: Option<String>,
    pub outbound_user_agent: String,
    pub third_party_user_agent: Option<String>,
    pub request_headers: IndexMap<String, String>,
}

pub type AgentIdentityAsyncError = Box<dyn Error + Send + Sync>;
pub type AgentIdentityFuture =
    BoxFuture<'static, Result<AgentIdentitySnapshot, AgentIdentityAsyncError>>;

pub trait IAgentIdentity: Send + Sync {
    fn resolved(&self) -> AgentIdentityFuture;
    fn current(&self) -> AgentIdentitySnapshot;
}

pub fn normalize_identity_slug(raw: &str) -> String {
    static NON_SLUG: LazyLock<Regex> = LazyLock::new(|| Regex::new("[^a-z0-9]+").unwrap());
    static EDGE_DASHES: LazyLock<Regex> = LazyLock::new(|| Regex::new("^-+|-+$").unwrap());
    let lowered = raw.to_lowercase();
    let folded = NON_SLUG.replace_all(&lowered, "-");
    let folded = EDGE_DASHES.replace_all(&folded, "");
    if folded.is_empty() {
        DEFAULT_IDENTITY_SLUG.to_string()
    } else {
        folded.into_owned()
    }
}

#[derive(Debug, Clone, Default)]
pub struct AgentIdentityInput {
    pub name: Option<String>,
    pub slug: Option<String>,
    pub host_display_name: Option<String>,
    pub host_request_headers: IndexMap<String, String>,
}

pub fn build_agent_identity_snapshot(input: AgentIdentityInput) -> AgentIdentitySnapshot {
    let name = declared(input.name.as_deref());
    let raw_slug = declared(input.slug.as_deref()).or_else(|| name.clone());
    let slug = raw_slug.map(|raw| normalize_identity_slug(&raw));
    let user_agent_keys = input
        .host_request_headers
        .keys()
        .filter(|key| key.eq_ignore_ascii_case("user-agent"))
        .cloned()
        .collect::<Vec<_>>();
    let host_user_agent = user_agent_keys
        .first()
        .and_then(|key| input.host_request_headers.get(key))
        .cloned();
    let third_party_user_agent = match (&host_user_agent, &slug) {
        (Some(user_agent), Some(slug)) => Some(replace_user_agent_product(user_agent, slug)),
        _ => host_user_agent.clone(),
    };
    let mut request_headers = input.host_request_headers;
    if let Some(slug) = &slug {
        for key in user_agent_keys {
            if let Some(value) = request_headers.get_mut(&key) {
                *value = replace_user_agent_product(value, slug);
            }
        }
    }
    let outbound_user_agent = third_party_user_agent
        .clone()
        .or_else(|| slug.clone())
        .unwrap_or_else(|| DEFAULT_IDENTITY_SLUG.to_string());
    AgentIdentitySnapshot {
        display_name: name.or_else(|| declared(input.host_display_name.as_deref())),
        slug,
        outbound_user_agent,
        third_party_user_agent,
        request_headers,
    }
}

fn replace_user_agent_product(user_agent: &str, product: &str) -> String {
    match user_agent.find('/') {
        Some(separator) => format!("{product}{}", &user_agent[separator..]),
        None => product.to_string(),
    }
}

fn declared(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}
