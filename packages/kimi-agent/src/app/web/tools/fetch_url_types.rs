use std::error::Error;
use std::fmt;
use std::ops::Deref;

use async_trait::async_trait;
use kimi_core::primitives::errors::{Error2, Error2Options};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::_base::utils::fetch_like::FetchError;

use super::super::errors::{WEB_FETCH_FAILED, register_web_errors};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UrlFetchKind {
    Passthrough,
    Extracted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlFetchResult {
    pub content: String,
    pub kind: UrlFetchKind,
}

#[derive(Clone, Default)]
pub struct UrlFetchOptions {
    pub tool_call_id: Option<String>,
    pub signal: Option<CancellationToken>,
}

#[async_trait]
pub trait UrlFetcher: Send + Sync {
    async fn fetch(
        &self,
        url: &str,
        options: UrlFetchOptions,
    ) -> Result<UrlFetchResult, FetchError>;
}

#[derive(Debug)]
pub struct HttpFetchError {
    pub status: u16,
    inner: Error2,
}

impl HttpFetchError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        register_web_errors();
        let details = Map::from_iter([("status".to_string(), Value::from(status))]);
        Self {
            status,
            inner: Error2::with_options(
                WEB_FETCH_FAILED,
                message,
                Error2Options {
                    details: Some(details),
                    ..Default::default()
                },
            )
            .with_name("HttpFetchError"),
        }
    }
}

impl Deref for HttpFetchError {
    type Target = Error2;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl fmt::Display for HttpFetchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(formatter)
    }
}

impl Error for HttpFetchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.inner.source()
    }
}
