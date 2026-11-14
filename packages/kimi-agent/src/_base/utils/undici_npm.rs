use std::any::Any;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use once_cell::sync::Lazy;
use parking_lot::RwLock;

pub trait Dispatcher: Send + Sync {
    fn as_any(&self) -> &dyn Any;
}

pub struct ConnectorOptions {
    pub hostname: String,
    pub port: Option<u16>,
    pub protocol: String,
    pub http_socket: Option<tokio::net::TcpStream>,
}

pub type Connector = Arc<
    dyn Fn(ConnectorOptions) -> BoxFuture<'static, Result<tokio::net::TcpStream, String>>
        + Send
        + Sync,
>;

pub struct Agent {
    pub connect: Connector,
}

impl Dispatcher for Agent {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Clone, Default)]
pub struct EnvHttpProxyAgentOptions {
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub no_proxy: String,
}

pub struct EnvHttpProxyAgent {
    pub options: EnvHttpProxyAgentOptions,
}

impl EnvHttpProxyAgent {
    pub fn new(options: EnvHttpProxyAgentOptions) -> Self {
        Self { options }
    }
}

impl Dispatcher for EnvHttpProxyAgent {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct ProxyAgent {
    pub proxy: String,
}

impl ProxyAgent {
    pub fn new(proxy: impl Into<String>) -> Self {
        Self {
            proxy: proxy.into(),
        }
    }
}

impl Dispatcher for ProxyAgent {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub fn build_connector() -> Connector {
    Arc::new(|options| {
        async move {
            if let Some(socket) = options.http_socket {
                return Ok(socket);
            }
            let port = options
                .port
                .unwrap_or(if options.protocol == "https:" { 443 } else { 80 });
            tokio::net::TcpStream::connect((options.hostname.as_str(), port))
                .await
                .map_err(|error| error.to_string())
        }
        .boxed()
    })
}

pub async fn fetch(
    url: &str,
    init: Option<reqwest::Request>,
) -> Result<reqwest::Response, reqwest::Error> {
    if let Some(request) = init {
        reqwest::Client::new().execute(request).await
    } else {
        reqwest::Client::new().get(url).send().await
    }
}

static GLOBAL_DISPATCHER: Lazy<RwLock<Option<Arc<dyn Dispatcher>>>> =
    Lazy::new(|| RwLock::new(None));

pub fn set_global_dispatcher(dispatcher: Arc<dyn Dispatcher>) {
    *GLOBAL_DISPATCHER.write() = Some(dispatcher);
}
