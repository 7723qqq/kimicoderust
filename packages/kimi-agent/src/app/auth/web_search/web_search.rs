use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::_base::utils::fetch_like::FetchError;

pub type WebSearchResult = crate::tools::web_search::WebSearchResultEntry;

#[derive(Clone, Default)]
pub struct WebSearchOptions {
    pub tool_call_id: Option<String>,
    pub signal: Option<CancellationToken>,
}

#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    async fn search(
        &self,
        query: &str,
        options: WebSearchOptions,
    ) -> Result<Vec<WebSearchResult>, FetchError>;
}

pub trait WebSearchProviderService: Send + Sync {
    fn get_web_search_provider(&self) -> Option<&dyn WebSearchProvider>;

    fn has_web_search_provider(&self) -> bool {
        self.get_web_search_provider().is_some()
    }
}
