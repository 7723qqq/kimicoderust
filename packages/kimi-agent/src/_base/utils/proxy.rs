use std::collections::HashMap;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub use super::proxy_env::{SocksProxyConfig, is_proxy_configured, resolve_socks_proxy};
use super::proxy_env::{Env, has_http_proxy, resolve_http_proxy_urls};
use super::undici_npm::{
    Agent, ConnectorOptions, Dispatcher, EnvHttpProxyAgent, EnvHttpProxyAgentOptions,
    build_connector, set_global_dispatcher,
};

const LOOPBACK_NO_PROXY: &[&str] = &["localhost", "127.0.0.1", "::1", "[::1]"];

pub fn resolve_no_proxy(env: &Env) -> String {
    let raw = [env.get("no_proxy"), env.get("NO_PROXY")]
        .into_iter()
        .flatten()
        .find(|value| !value.trim().is_empty())
        .cloned()
        .unwrap_or_default();
    let mut hosts = raw
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if hosts.iter().any(|host| host == "*") {
        return "*".to_string();
    }
    for loopback in LOOPBACK_NO_PROXY {
        if !hosts.iter().any(|host| host == loopback) {
            hosts.push((*loopback).to_string());
        }
    }
    hosts.join(",")
}

struct NoProxyEntry {
    host: String,
    port: Option<String>,
}

pub fn make_no_proxy_matcher(
    no_proxy: &str,
) -> Box<dyn Fn(&str, Option<&str>) -> bool + Send + Sync> {
    let entries = no_proxy
        .split(',')
        .map(|entry| entry.trim().to_lowercase())
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>();
    if entries.iter().any(|entry| entry == "*") {
        return Box::new(|_, _| true);
    }
    let parsed = entries.iter().map(|entry| parse_no_proxy_entry(entry)).collect::<Vec<_>>();
    Box::new(move |host, port| {
        let target = host
            .to_lowercase()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        parsed.iter().any(|entry| {
            (entry.port.is_none() || entry.port.as_deref() == port)
                && (target == entry.host || target.ends_with(&format!(".{}", entry.host)))
        })
    })
}

fn parse_no_proxy_entry(entry: &str) -> NoProxyEntry {
    let mut host = entry.to_string();
    let mut port = None;
    if entry.starts_with('[') {
        let close = entry.find(']').unwrap_or(entry.len().saturating_sub(1));
        host = entry[1..close].to_string();
        let rest = &entry[close + 1..];
        if let Some(value) = rest.strip_prefix(':') {
            port = Some(value.to_string());
        }
    } else if let Some(colon) = entry.find(':')
        && colon == entry.rfind(':').unwrap()
        && entry[colon + 1..].chars().all(|character| character.is_ascii_digit())
    {
        host = entry[..colon].to_string();
        port = Some(entry[colon + 1..].to_string());
    }
    if let Some(value) = host.strip_prefix("*.") {
        host = value.to_string();
    } else if let Some(value) = host.strip_prefix('.') {
        host = value.to_string();
    }
    NoProxyEntry { host, port }
}

#[derive(Clone, Default)]
pub struct HttpAgentOptions {
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub no_proxy: String,
}

pub struct SocksAgentOptions {
    pub proxy: SocksProxyConfig,
    pub no_proxy: String,
}

pub type MakeHttpAgent = Arc<
    dyn Fn(HttpAgentOptions) -> Result<Arc<dyn Dispatcher>, String> + Send + Sync,
>;
pub type MakeSocksAgent = Arc<
    dyn Fn(SocksAgentOptions) -> Result<Arc<dyn Dispatcher>, String> + Send + Sync,
>;

#[derive(Clone)]
pub struct ProxyAgentFactories {
    pub make_http_agent: MakeHttpAgent,
    pub make_socks_agent: MakeSocksAgent,
}

impl Default for ProxyAgentFactories {
    fn default() -> Self {
        Self {
            make_http_agent: Arc::new(|options| {
                Ok(Arc::new(EnvHttpProxyAgent::new(EnvHttpProxyAgentOptions {
                    http_proxy: options.http_proxy,
                    https_proxy: options.https_proxy,
                    no_proxy: options.no_proxy,
                })))
            }),
            make_socks_agent: Arc::new(|options| {
                let direct_connect = build_connector();
                let bypass = make_no_proxy_matcher(&options.no_proxy);
                let proxy = options.proxy;
                let connect = Arc::new(move |connect_options: ConnectorOptions| {
                    if bypass(&connect_options.hostname, connect_options.port.map(|port| port.to_string()).as_deref()) {
                        return direct_connect(connect_options);
                    }
                    let direct_connect = direct_connect.clone();
                    let proxy = proxy.clone();
                    async move {
                        let is_tls = connect_options.protocol == "https:";
                        let port = connect_options
                            .port
                            .filter(|port| *port != 0)
                            .unwrap_or(if is_tls { 443 } else { 80 });
                        let socket = socks_connect(
                            &proxy,
                            &connect_options.hostname,
                            port,
                        )
                        .await?;
                        if !is_tls {
                            socket.set_nodelay(true).map_err(|error| error.to_string())?;
                        }
                        direct_connect(ConnectorOptions {
                            hostname: connect_options.hostname,
                            port: connect_options.port,
                            protocol: connect_options.protocol,
                            http_socket: Some(socket),
                        })
                        .await
                    }
                    .boxed()
                });
                Ok(Arc::new(Agent { connect }))
            }),
        }
    }
}

pub fn create_proxy_dispatcher(
    env: &Env,
    factories: Option<ProxyAgentFactories>,
) -> Option<Arc<dyn Dispatcher>> {
    let factories = factories.unwrap_or_default();
    let result = (|| {
        if has_http_proxy(env) {
            let urls = resolve_http_proxy_urls(env);
            return (factories.make_http_agent)(HttpAgentOptions {
                http_proxy: Some(urls.http_proxy.unwrap_or_default()),
                https_proxy: Some(urls.https_proxy.unwrap_or_default()),
                no_proxy: resolve_no_proxy(env),
            })
            .map(Some);
        }
        if let Some(proxy) = resolve_socks_proxy(env) {
            return (factories.make_socks_agent)(SocksAgentOptions {
                proxy,
                no_proxy: resolve_no_proxy(env),
            })
            .map(Some);
        }
        Ok(None)
    })();
    match result {
        Ok(dispatcher) => dispatcher,
        Err(reason) => {
            eprintln!(
                "kimi: ignoring invalid proxy configuration ({reason}); connecting directly"
            );
            None
        }
    }
}

pub struct InstallProxyDeps {
    pub set_global_dispatcher: Arc<dyn Fn(Arc<dyn Dispatcher>) + Send + Sync>,
    pub create_proxy_dispatcher: Arc<dyn Fn(&Env) -> Option<Arc<dyn Dispatcher>> + Send + Sync>,
}

impl Default for InstallProxyDeps {
    fn default() -> Self {
        Self {
            set_global_dispatcher: Arc::new(set_global_dispatcher),
            create_proxy_dispatcher: Arc::new(|env| create_proxy_dispatcher(env, None)),
        }
    }
}

pub fn install_global_proxy_dispatcher(env: &Env, deps: InstallProxyDeps) -> bool {
    let Some(dispatcher) = (deps.create_proxy_dispatcher)(env) else {
        return false;
    };
    (deps.set_global_dispatcher)(dispatcher);
    true
}

pub fn proxy_env_for_child(env: &Env) -> HashMap<String, String> {
    if !has_http_proxy(env) {
        return HashMap::new();
    }
    let no_proxy = resolve_no_proxy(env);
    let mut result = HashMap::from([
        ("NODE_USE_ENV_PROXY".to_string(), "1".to_string()),
        ("NO_PROXY".to_string(), no_proxy.clone()),
        ("no_proxy".to_string(), no_proxy),
    ]);
    let urls = resolve_http_proxy_urls(env);
    if let Some(http_proxy) = urls.http_proxy {
        result.insert("HTTP_PROXY".to_string(), http_proxy.clone());
        result.insert("http_proxy".to_string(), http_proxy);
    }
    if let Some(https_proxy) = urls.https_proxy {
        result.insert("HTTPS_PROXY".to_string(), https_proxy.clone());
        result.insert("https_proxy".to_string(), https_proxy);
    }
    result
}

pub fn reconcile_child_no_proxy(child_env: &mut HashMap<String, String>, config_env: Option<&HashMap<String, String>>) {
    let override_value = [
        config_env.and_then(|env| env.get("no_proxy")),
        config_env.and_then(|env| env.get("NO_PROXY")),
    ]
    .into_iter()
    .flatten()
    .find(|value| !value.trim().is_empty())
    .cloned();
    let Some(override_value) = override_value else {
        return;
    };
    let env = HashMap::from([
        ("no_proxy".to_string(), override_value.clone()),
        ("NO_PROXY".to_string(), override_value),
    ]);
    let no_proxy = resolve_no_proxy(&env);
    child_env.insert("NO_PROXY".to_string(), no_proxy.clone());
    child_env.insert("no_proxy".to_string(), no_proxy);
}

async fn socks_connect(
    proxy: &SocksProxyConfig,
    destination_host: &str,
    destination_port: u16,
) -> Result<TcpStream, String> {
    let mut socket = TcpStream::connect((proxy.host.as_str(), proxy.port))
        .await
        .map_err(|error| error.to_string())?;
    if proxy.proxy_type == 5 {
        let has_auth = proxy.user_id.is_some() || proxy.password.is_some();
        if has_auth {
            socket
                .write_all(&[5, 2, 0, 2])
                .await
                .map_err(|error| error.to_string())?;
        } else {
            socket
                .write_all(&[5, 1, 0])
                .await
                .map_err(|error| error.to_string())?;
        }
        let mut response = [0_u8; 2];
        socket.read_exact(&mut response).await.map_err(|error| error.to_string())?;
        if response[0] != 5 || response[1] == 0xff {
            return Err("SOCKS5 authentication method was rejected".to_string());
        }
        if response[1] == 2 {
            let user = proxy.user_id.as_deref().unwrap_or_default().as_bytes();
            let password = proxy.password.as_deref().unwrap_or_default().as_bytes();
            let mut request = vec![1, user.len() as u8];
            request.extend(user);
            request.push(password.len() as u8);
            request.extend(password);
            socket.write_all(&request).await.map_err(|error| error.to_string())?;
            socket.read_exact(&mut response).await.map_err(|error| error.to_string())?;
            if response[1] != 0 {
                return Err("SOCKS5 authentication failed".to_string());
            }
        }
        let mut request = vec![5, 1, 0];
        push_address(&mut request, destination_host);
        request.extend(destination_port.to_be_bytes());
        socket.write_all(&request).await.map_err(|error| error.to_string())?;
        let mut head = [0_u8; 4];
        socket.read_exact(&mut head).await.map_err(|error| error.to_string())?;
        if head[0] != 5 || head[1] != 0 {
            return Err(format!("SOCKS5 connect failed with status {}", head[1]));
        }
        read_address(&mut socket, head[3]).await?;
        let mut port = [0_u8; 2];
        socket.read_exact(&mut port).await.map_err(|error| error.to_string())?;
    } else {
        let mut request = vec![4, 1];
        request.extend(destination_port.to_be_bytes());
        let ip = destination_host.parse::<std::net::Ipv4Addr>().ok();
        request.extend(ip.map(|ip| ip.octets()).unwrap_or([0, 0, 0, 1]));
        request.extend(proxy.user_id.as_deref().unwrap_or_default().as_bytes());
        request.push(0);
        if ip.is_none() {
            request.extend(destination_host.as_bytes());
            request.push(0);
        }
        socket.write_all(&request).await.map_err(|error| error.to_string())?;
        let mut response = [0_u8; 8];
        socket.read_exact(&mut response).await.map_err(|error| error.to_string())?;
        if response[1] != 0x5a {
            return Err(format!("SOCKS4 connect failed with status {}", response[1]));
        }
    }
    Ok(socket)
}

fn push_address(request: &mut Vec<u8>, host: &str) {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        match ip {
            std::net::IpAddr::V4(ip) => {
                request.push(1);
                request.extend(ip.octets());
            }
            std::net::IpAddr::V6(ip) => {
                request.push(4);
                request.extend(ip.octets());
            }
        }
    } else {
        request.push(3);
        request.push(host.len() as u8);
        request.extend(host.as_bytes());
    }
}

async fn read_address(socket: &mut TcpStream, address_type: u8) -> Result<(), String> {
    let length = match address_type {
        1 => 4,
        4 => 16,
        3 => {
            let mut length = [0_u8; 1];
            socket.read_exact(&mut length).await.map_err(|error| error.to_string())?;
            length[0] as usize
        }
        _ => return Err("SOCKS5 response has an invalid address type".to_string()),
    };
    let mut address = vec![0_u8; length];
    socket.read_exact(&mut address).await.map_err(|error| error.to_string())?;
    Ok(())
}
