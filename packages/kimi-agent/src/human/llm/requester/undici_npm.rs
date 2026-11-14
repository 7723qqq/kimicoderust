#[derive(Debug, Clone)]
pub struct Agent {
    pub headers_timeout_ms: Option<u64>,
    pub body_timeout_ms: Option<u64>,
}

impl Agent {
    pub fn new(headers_timeout_ms: Option<u64>, body_timeout_ms: Option<u64>) -> Self {
        Self {
            headers_timeout_ms,
            body_timeout_ms,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EnvHttpProxyAgent {
    pub http_proxy: String,
    pub https_proxy: String,
    pub no_proxy: String,
    pub headers_timeout_ms: Option<u64>,
    pub body_timeout_ms: Option<u64>,
}

impl EnvHttpProxyAgent {
    pub fn new(
        http_proxy: String,
        https_proxy: String,
        no_proxy: String,
        headers_timeout_ms: Option<u64>,
        body_timeout_ms: Option<u64>,
    ) -> Result<Self, String> {
        if !http_proxy.is_empty() {
            url::Url::parse(&http_proxy).map_err(|error| error.to_string())?;
        }
        if !https_proxy.is_empty() {
            url::Url::parse(&https_proxy).map_err(|error| error.to_string())?;
        }
        Ok(Self {
            http_proxy,
            https_proxy,
            no_proxy,
            headers_timeout_ms,
            body_timeout_ms,
        })
    }
}

#[derive(Debug, Clone)]
pub enum Dispatcher {
    Agent(Agent),
    EnvHttpProxyAgent(EnvHttpProxyAgent),
}
