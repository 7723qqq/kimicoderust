use std::error::Error;
use std::pin::Pin;

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

pub type FetchError = Box<dyn Error + Send + Sync>;
pub type FetchByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, FetchError>> + Send>>;

#[derive(Clone, Default)]
pub struct FetchInit {
    pub headers: Vec<(String, String)>,
    pub method: Option<String>,
    pub body: Option<String>,
    pub signal: Option<CancellationToken>,
    pub manual_redirect: bool,
    pub timeout_ms: Option<u64>,
}

pub struct FetchResponse {
    pub ok: bool,
    pub status: u16,
    pub status_text: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    stream: Option<FetchByteStream>,
}

impl FetchResponse {
    pub fn from_bytes(
        status: u16,
        status_text: impl Into<String>,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Self {
        Self {
            ok: (200..300).contains(&status),
            status,
            status_text: status_text.into(),
            headers,
            body: Some(body),
            stream: None,
        }
    }

    pub fn from_stream(
        status: u16,
        status_text: impl Into<String>,
        headers: Vec<(String, String)>,
        stream: FetchByteStream,
    ) -> Self {
        Self {
            ok: (200..300).contains(&status),
            status,
            status_text: status_text.into(),
            headers,
            body: None,
            stream: Some(stream),
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub async fn text(self) -> Result<String, FetchError> {
        let bytes = match (self.body, self.stream) {
            (Some(body), _) => body,
            (None, Some(stream)) => {
                let chunks = stream.collect::<Vec<_>>().await;
                let mut body = Vec::new();
                for chunk in chunks {
                    body.extend(chunk?);
                }
                body
            }
            (None, None) => Vec::new(),
        };
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn stream(self) -> Option<FetchByteStream> {
        if let Some(stream) = self.stream {
            return Some(stream);
        }
        self.body.map(|body| {
            Box::pin(futures_util::stream::once(async move { Ok(body) })) as FetchByteStream
        })
    }
}

#[async_trait]
pub trait FetchLike: Send + Sync {
    async fn fetch(&self, url: &str, init: FetchInit) -> Result<FetchResponse, FetchError>;
}
