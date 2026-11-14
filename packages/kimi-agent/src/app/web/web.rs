use std::sync::Arc;

use once_cell::sync::Lazy;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};

pub use super::tools::fetch_url_types::{
    HttpFetchError, UrlFetchKind, UrlFetchResult, UrlFetcher,
};

pub trait IWebFetchService: Send + Sync {
    fn get_url_fetcher(&self) -> Arc<dyn UrlFetcher>;
}

pub static I_WEB_FETCH_SERVICE: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("webFetchService"));
