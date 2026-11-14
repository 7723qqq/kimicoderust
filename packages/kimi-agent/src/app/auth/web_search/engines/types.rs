use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::_base::utils::fetch_like::{FetchError, FetchLike};
use crate::app::auth::web_search::web_search::WebSearchResult;

#[derive(Clone, Default)]
pub struct SearchEngineOptions {
    pub signal: Option<CancellationToken>,
    pub fetch_impl: Option<Arc<dyn FetchLike>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArticleContent {
    pub content: String,
    pub title: Option<String>,
}

pub type SearchEngineFuture = Pin<
    Box<dyn Future<Output = Result<Vec<WebSearchResult>, FetchError>> + Send + 'static>,
>;
pub type SearchEngineFn = Arc<
    dyn Fn(String, usize, SearchEngineOptions) -> SearchEngineFuture + Send + Sync + 'static,
>;
pub type ArticleFetchFuture = Pin<
    Box<dyn Future<Output = Result<Option<ArticleContent>, FetchError>> + Send + 'static>,
>;
pub type ArticleFetchFn = Arc<
    dyn Fn(String, SearchEngineOptions) -> ArticleFetchFuture + Send + Sync + 'static,
>;
