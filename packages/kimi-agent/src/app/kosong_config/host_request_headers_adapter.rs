use std::collections::BTreeMap;
use std::sync::Arc;

use crate::app::agent_identity::agent_identity::IAgentIdentity;

pub trait IHostRequestHeaders: Send + Sync {
    fn headers(&self) -> BTreeMap<String, String>;
    fn third_party_headers(&self) -> BTreeMap<String, String>;
    fn identity_slug(&self) -> Option<String>;
}

pub struct HostRequestHeadersAdapter {
    headers: BTreeMap<String, String>,
    identity: Arc<dyn IAgentIdentity>,
}

impl HostRequestHeadersAdapter {
    pub fn new(
        headers: impl IntoIterator<Item = (String, String)>,
        identity: Arc<dyn IAgentIdentity>,
    ) -> Self {
        Self {
            headers: headers.into_iter().collect(),
            identity,
        }
    }
}

impl IHostRequestHeaders for HostRequestHeadersAdapter {
    fn headers(&self) -> BTreeMap<String, String> {
        self.headers.clone()
    }

    fn third_party_headers(&self) -> BTreeMap<String, String> {
        self.identity
            .current()
            .third_party_user_agent
            .map(|user_agent| BTreeMap::from([("User-Agent".to_string(), user_agent)]))
            .unwrap_or_default()
    }

    fn identity_slug(&self) -> Option<String> {
        self.identity.current().slug
    }
}
