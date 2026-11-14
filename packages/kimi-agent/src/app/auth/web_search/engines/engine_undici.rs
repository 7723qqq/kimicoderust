use futures_util::StreamExt;

use crate::_base::utils::abort::AbortError;
use crate::_base::utils::fetch_like::{FetchError, FetchInit, FetchResponse};

pub async fn undici_fetch(url: &str, init: FetchInit) -> Result<FetchResponse, FetchError> {
    let client = build_undici_client(init.manual_redirect, None)?;
    undici_fetch_with_client(&client, url, init).await
}

pub(crate) fn build_undici_client(
    manual_redirect: bool,
    proxy_url: Option<&str>,
) -> Result<reqwest::Client, FetchError> {
    let mut builder = reqwest::Client::builder().no_proxy();
    if manual_redirect {
        builder = builder.redirect(reqwest::redirect::Policy::none());
    }
    if let Some(proxy_url) = proxy_url {
        builder = builder.proxy(reqwest::Proxy::all(proxy_url)?);
    }
    Ok(builder.build()?)
}

pub(crate) async fn undici_fetch_with_client(
    client: &reqwest::Client,
    url: &str,
    init: FetchInit,
) -> Result<FetchResponse, FetchError> {
    let method = init.method.as_deref().unwrap_or("GET");
    let mut request = client.request(reqwest::Method::from_bytes(method.as_bytes())?, url);
    for (name, value) in init.headers {
        request = request.header(&name, &value);
    }
    if let Some(body) = init.body {
        request = request.body(body);
    }
    if let Some(timeout_ms) = init.timeout_ms {
        request = request.timeout(std::time::Duration::from_millis(timeout_ms));
    }
    let send = request.send();
    let response = if let Some(signal) = init.signal {
        tokio::select! {
            result = send => result?,
            _ = signal.cancelled() => {
                return Err(Box::new(AbortError::new("The operation was aborted.")));
            }
        }
    } else {
        send.await?
    };
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect::<Vec<_>>();
    Ok(FetchResponse::from_stream(
        status.as_u16(),
        status.canonical_reason().unwrap_or_default(),
        headers,
        response
            .bytes_stream()
            .map(|chunk| {
                chunk
                    .map(|bytes| bytes.to_vec())
                    .map_err(|error| error.into())
            })
            .boxed(),
    ))
}
