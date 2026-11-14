pub const EMBEDDINGS_URL_ENV: &str = "KIMI_CODE_ASSOCIATIVE_MEMORY_EMBEDDINGS_URL";
pub const EMBEDDINGS_API_KEY_ENV: &str = "KIMI_CODE_ASSOCIATIVE_MEMORY_EMBEDDINGS_API_KEY";
pub const EMBEDDINGS_MODEL_ENV: &str = "KIMI_CODE_ASSOCIATIVE_MEMORY_EMBEDDINGS_MODEL";
pub const DEFAULT_EMBEDDINGS_MODEL: &str = "text-embedding-3-small";

#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingsOptions {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

pub trait EmbeddingsClient: Send + Sync {
    fn embed(
        &self,
        texts: Vec<String>,
    ) -> crate::rpc::types::BoxFuture<'_, Result<Vec<Vec<f64>>, String>>;
}

pub fn resolve_embeddings_options<F>(env: F) -> Option<EmbeddingsOptions>
where
    F: Fn(&str) -> Option<String>,
{
    let base_url = env(EMBEDDINGS_URL_ENV)?;
    if base_url.is_empty() {
        return None;
    }
    let api_key = env(EMBEDDINGS_API_KEY_ENV)?;
    if api_key.is_empty() {
        return None;
    }
    let model = env(EMBEDDINGS_MODEL_ENV).unwrap_or_else(|| DEFAULT_EMBEDDINGS_MODEL.to_string());
    Some(EmbeddingsOptions {
        base_url: base_url.trim_end_matches('/').to_string(),
        api_key,
        model,
    })
}

pub struct HttpEmbeddingsClient {
    base_url: String,
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl HttpEmbeddingsClient {
    pub fn new(options: EmbeddingsOptions) -> Self {
        Self {
            base_url: options.base_url,
            api_key: options.api_key,
            model: options.model,
            client: reqwest::Client::new(),
        }
    }

    async fn send_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f64>>, String> {
        let response = self
            .client
            .post(format!("{}/embeddings", self.base_url))
            .header("content-type", "application/json")
            .bearer_auth(&self.api_key)
            .json(&serde_json::json!({ "model": self.model, "input": texts }))
            .send()
            .await
            .map_err(|error| format!("embeddings request failed: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "embeddings request failed with status {}",
                status.as_u16()
            ));
        }
        let payload: serde_json::Value = response
            .json()
            .await
            .map_err(|error| format!("decode embeddings response: {error}"))?;
        Ok(payload
            .get("data")
            .and_then(serde_json::Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| {
                        entry
                            .get("embedding")
                            .and_then(serde_json::Value::as_array)
                            .map(|values| {
                                values
                                    .iter()
                                    .filter_map(serde_json::Value::as_f64)
                                    .collect::<Vec<f64>>()
                            })
                            .unwrap_or_default()
                    })
                    .collect::<Vec<Vec<f64>>>()
            })
            .unwrap_or_default())
    }
}

impl EmbeddingsClient for HttpEmbeddingsClient {
    fn embed(
        &self,
        texts: Vec<String>,
    ) -> crate::rpc::types::BoxFuture<'_, Result<Vec<Vec<f64>>, String>> {
        Box::pin(async move { self.send_embeddings(&texts).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn serve_once(status: u16, body: &'static str) -> (String, Arc<Mutex<Vec<u8>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_task = seen.clone();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let Ok(read) = sock.read(&mut chunk).await else {
                    return;
                };
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&buffer);
                let Some(header_end) = text.find("\r\n\r\n") else {
                    continue;
                };
                let content_length = text[..header_end]
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if buffer.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            seen_task.lock().unwrap().extend_from_slice(&buffer);
            let reason = match status {
                200 => "OK",
                429 => "Too Many Requests",
                _ => "Error",
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        (format!("http://{addr}"), seen)
    }

    #[test]
    fn options_need_both_a_url_and_a_key() {
        let empty_url = |name: &str| if name == EMBEDDINGS_URL_ENV { Some(String::new()) } else { Some("k".into()) };
        assert!(resolve_embeddings_options(empty_url).is_none());

        let empty_key = |name: &str| if name == EMBEDDINGS_API_KEY_ENV { Some(String::new()) } else { Some("u".into()) };
        assert!(resolve_embeddings_options(empty_key).is_none());

        let missing_url = |name: &str| if name == EMBEDDINGS_URL_ENV { None } else { Some("k".into()) };
        assert!(resolve_embeddings_options(missing_url).is_none());
    }

    #[test]
    fn the_url_loses_its_trailing_slashes_and_the_model_defaults() {
        let env = |name: &str| match name {
            EMBEDDINGS_URL_ENV => Some("https://example.test/v1///".into()),
            EMBEDDINGS_API_KEY_ENV => Some("secret".into()),
            _ => None,
        };
        let options = resolve_embeddings_options(env).unwrap();
        assert_eq!(options.base_url, "https://example.test/v1");
        assert_eq!(options.model, DEFAULT_EMBEDDINGS_MODEL);
    }

    #[test]
    fn an_explicit_model_wins() {
        let env = |name: &str| match name {
            EMBEDDINGS_URL_ENV => Some("https://example.test".into()),
            EMBEDDINGS_API_KEY_ENV => Some("secret".into()),
            EMBEDDINGS_MODEL_ENV => Some("custom-model".into()),
            _ => None,
        };
        assert_eq!(
            resolve_embeddings_options(env).unwrap().model,
            "custom-model"
        );
    }

    #[tokio::test]
    async fn the_client_posts_the_openai_shape_and_decodes_the_data_array() {
        let (base, seen) = serve_once(
            200,
            r#"{"data":[{"embedding":[0.5,0.25]},{"embedding":[1.0]}]}"#,
        )
        .await;
        let options = EmbeddingsOptions {
            base_url: base,
            api_key: "secret".into(),
            model: DEFAULT_EMBEDDINGS_MODEL.into(),
        };
        let client = HttpEmbeddingsClient::new(options);
        let out = client
            .embed(vec!["a".to_string(), "b".to_string()])
            .await
            .unwrap();
        assert_eq!(out, vec![vec![0.5, 0.25], vec![1.0]]);

        let request = String::from_utf8(seen.lock().unwrap().clone()).unwrap();
        assert!(request.starts_with("POST /embeddings HTTP/1.1\r\n"));
        assert!(request.to_lowercase().contains("authorization: bearer secret"));
        assert!(request.contains(r#""input":["a","b"]"#));
        assert!(request.contains(r#""model":"text-embedding-3-small""#));
    }

    #[tokio::test]
    async fn a_missing_embedding_yields_an_empty_vector() {
        let (base, _seen) = serve_once(200, r#"{"data":[{"embedding":null},{}]}"#).await;
        let options = EmbeddingsOptions {
            base_url: base,
            api_key: "secret".into(),
            model: DEFAULT_EMBEDDINGS_MODEL.into(),
        };
        let client = HttpEmbeddingsClient::new(options);
        assert_eq!(
            client.embed(vec!["a".to_string()]).await.unwrap(),
            vec![Vec::<f64>::new(), Vec::<f64>::new()]
        );
    }

    #[tokio::test]
    async fn a_non_success_status_is_an_error() {
        let (base, _seen) = serve_once(429, "").await;
        let options = EmbeddingsOptions {
            base_url: base,
            api_key: "secret".into(),
            model: DEFAULT_EMBEDDINGS_MODEL.into(),
        };
        let client = HttpEmbeddingsClient::new(options);
        let error = client.embed(vec!["a".to_string()]).await.unwrap_err();
        assert_eq!(error, "embeddings request failed with status 429");
    }
}
