use std::sync::{Arc, Weak};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::_base::errors::unexpected_error::on_unexpected_error;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::region::region::{IRegionService, KimiRegionResolution, KimiRegionSource};
use crate::persistence::interface::storage::IFileSystemStorageService;
use crate::region::KimiRegion;
use crate::wire::record::WireAsyncError;

use super::cloud_transport::{
    AccessTokenProvider, CloudContext, CloudPrimitive, CloudProperties, CloudTransport,
    CloudTransportOptions, EnrichedCloudEvent, EndpointResolver, NowImpl, SleepImpl,
    is_cloud_primitive,
};
use super::context::TelemetryProperties;
use super::core_version::resolve_core_version;
use super::privacy::clean_telemetry_properties;
use super::telemetry::{ITelemetryAppender, TelemetryAppenderRecord};

#[derive(Clone)]
pub struct CloudAppenderOptions {
    pub storage: Arc<dyn IFileSystemStorageService>,
    pub bootstrap: Arc<dyn IBootstrapService>,
    pub resolve_region: Arc<dyn Fn() -> KimiRegionResolution + Send + Sync>,
    pub device_id: String,
    pub session_id: Option<String>,
    pub app_name: String,
    pub ui_mode: Option<String>,
    pub model: Option<String>,
    pub build_sha: Option<String>,
    pub terminal: Option<String>,
    pub locale: Option<String>,
    pub get_access_token: Option<AccessTokenProvider>,
    pub endpoint: Option<String>,
    pub flush_threshold: Option<usize>,
    pub flush_interval_ms: Option<u64>,
    pub fetch_impl: Option<Arc<dyn crate::_base::utils::fetch_like::FetchLike>>,
    pub retry_backoffs_ms: Option<Vec<u64>>,
    pub request_timeout_ms: Option<u64>,
    pub max_disk_files: Option<usize>,
    pub max_disk_bytes: Option<u64>,
    pub sleep: Option<SleepImpl>,
    pub now: Option<NowImpl>,
}

#[derive(Clone)]
pub struct CloudAppenderHostOptions {
    pub device_id: String,
    pub app_name: String,
    pub ui_mode: Option<String>,
    pub model: Option<String>,
    pub build_sha: Option<String>,
    pub session_id: Option<String>,
    pub get_access_token: Option<AccessTokenProvider>,
}

pub fn create_cloud_appender(
    storage: Arc<dyn IFileSystemStorageService>,
    bootstrap: Arc<dyn IBootstrapService>,
    regions: Arc<dyn IRegionService>,
    host: CloudAppenderHostOptions,
) -> Arc<CloudAppender> {
    let resolve_region = Arc::new(move || regions.current());
    CloudAppender::new(CloudAppenderOptions {
        storage,
        bootstrap,
        resolve_region,
        device_id: host.device_id,
        session_id: host.session_id,
        app_name: host.app_name,
        ui_mode: host.ui_mode,
        model: host.model,
        build_sha: host.build_sha,
        terminal: None,
        locale: None,
        get_access_token: host.get_access_token,
        endpoint: None,
        flush_threshold: None,
        flush_interval_ms: None,
        fetch_impl: None,
        retry_backoffs_ms: None,
        request_timeout_ms: None,
        max_disk_files: None,
        max_disk_bytes: None,
        sleep: None,
        now: None,
    })
}

const DEFAULT_FLUSH_THRESHOLD: usize = 50;
const DEFAULT_FLUSH_INTERVAL_MS: u64 = 30_000;
const PRUNE_EVERY_N_TICKS: u64 = 20;

struct PeriodicFlush {
    stop: CancellationToken,
    task: JoinHandle<()>,
}

pub struct CloudAppender {
    transport: Arc<CloudTransport>,
    context: Mutex<CloudContext>,
    resolve_region: Arc<dyn Fn() -> KimiRegionResolution + Send + Sync>,
    flush_threshold: usize,
    flush_interval_ms: u64,
    device_id: String,
    session_id: Option<String>,
    buffer: Mutex<Vec<EnrichedCloudEvent>>,
    flush_timer: Mutex<Option<PeriodicFlush>>,
    weak: Weak<Self>,
}

impl CloudAppender {
    pub fn new(options: CloudAppenderOptions) -> Arc<Self> {
        let mut context = build_context(&options);
        let resolution = (options.resolve_region)();
        set_primitive(
            &mut context,
            "region",
            Some(Value::String(resolution.region.as_str().to_string())),
        );
        set_primitive(
            &mut context,
            "region_source",
            Some(Value::String(
                region_source_name(resolution.source).to_string(),
            )),
        );
        let resolve_endpoint: Option<EndpointResolver> = if options.endpoint.is_none() {
            let resolve_region = options.resolve_region.clone();
            Some(Arc::new(move || {
                telemetry_endpoint(resolve_region().region).map(str::to_string)
            }))
        } else {
            None
        };
        let transport = Arc::new(CloudTransport::new(CloudTransportOptions {
            storage: options.storage,
            device_id: options.device_id.clone(),
            endpoint: options.endpoint,
            resolve_endpoint,
            get_access_token: options.get_access_token,
            fetch_impl: options.fetch_impl,
            retry_backoffs_ms: options.retry_backoffs_ms,
            request_timeout_ms: options.request_timeout_ms,
            max_disk_files: options.max_disk_files,
            max_disk_bytes: options.max_disk_bytes,
            sleep: options.sleep,
            now: options.now,
        }));
        Arc::new_cyclic(|weak| Self {
            transport,
            context: Mutex::new(context),
            resolve_region: options.resolve_region,
            flush_threshold: options.flush_threshold.unwrap_or(DEFAULT_FLUSH_THRESHOLD),
            flush_interval_ms: options.flush_interval_ms.unwrap_or(DEFAULT_FLUSH_INTERVAL_MS),
            device_id: options.device_id,
            session_id: options.session_id,
            buffer: Mutex::new(Vec::new()),
            flush_timer: Mutex::new(None),
            weak: weak.clone(),
        })
    }

    pub fn track(&self, record: TelemetryAppenderRecord) -> Result<(), WireAsyncError> {
        self.refresh_region_context();
        let ambient_session_id = record.context.get("session_id").and_then(Value::as_str);
        let enriched = EnrichedCloudEvent {
            event_id: uuid::Uuid::new_v4().simple().to_string(),
            device_id: Some(self.device_id.clone()),
            session_id: ambient_session_id
                .map(str::to_string)
                .or_else(|| self.session_id.clone()),
            event: record.event,
            timestamp: chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
            properties: clean_telemetry_properties(&sanitize_properties(&record.properties)),
            context: self.envelope_context(&record.context),
        };
        let should_flush = {
            let mut buffer = self.buffer.lock();
            buffer.push(enriched);
            buffer.len() >= self.flush_threshold
        };
        if should_flush {
            self.spawn_flush();
        }
        Ok(())
    }

    fn refresh_region_context(&self) {
        let resolution = (self.resolve_region)();
        let mut context = self.context.lock();
        set_primitive(
            &mut context,
            "region",
            Some(Value::String(resolution.region.as_str().to_string())),
        );
        set_primitive(
            &mut context,
            "region_source",
            Some(Value::String(
                region_source_name(resolution.source).to_string(),
            )),
        );
    }

    fn envelope_context(&self, ambient: &TelemetryProperties) -> CloudContext {
        let mut context = self.context.lock().clone();
        if let Some(model) = ambient
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
        {
            context.insert("model".to_string(), Value::String(model.to_string()));
        }
        context
    }

    pub async fn flush(&self) -> Result<(), WireAsyncError> {
        let events = {
            let mut buffer = self.buffer.lock();
            if buffer.is_empty() {
                return Ok(());
            }
            std::mem::take(&mut *buffer)
        };
        if let Err(error) = self.transport.send(&events, None).await {
            let mut buffer = self.buffer.lock();
            let mut requeued = events;
            requeued.append(&mut buffer);
            *buffer = requeued;
            return Err(error);
        }
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<(), WireAsyncError> {
        self.stop_periodic_flush();
        self.flush().await
    }

    pub fn start_periodic_flush(&self) {
        let mut timer = self.flush_timer.lock();
        if timer.is_some() {
            return;
        }
        self.spawn_prune_disk_events();
        let weak = self.weak.clone();
        let stop = CancellationToken::new();
        let task_stop = stop.clone();
        let interval_ms = self.flush_interval_ms;
        let task = tokio::spawn(async move {
            let mut ticks = 0_u64;
            loop {
                tokio::select! {
                    _ = task_stop.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(interval_ms)) => {}
                }
                let Some(this) = weak.upgrade() else {
                    break;
                };
                let _ = this.flush().await;
                ticks += 1;
                if ticks % PRUNE_EVERY_N_TICKS == 0 {
                    this.prune_disk_events().await;
                }
            }
        });
        *timer = Some(PeriodicFlush { stop, task });
    }

    pub fn stop_periodic_flush(&self) {
        let Some(timer) = self.flush_timer.lock().take() else {
            return;
        };
        timer.stop.cancel();
        drop(timer.task);
    }

    pub async fn retry_disk_events(&self) -> Result<(), WireAsyncError> {
        self.transport.retry_disk_events().await
    }

    pub async fn prune_disk_events(&self) {
        let _ = self.transport.prune_expired_disk_events().await;
    }

    fn spawn_flush(&self) {
        let Some(this) = self.weak.upgrade() else {
            return;
        };
        tokio::spawn(async move {
            let _ = this.flush().await;
        });
    }

    fn spawn_prune_disk_events(&self) {
        let Some(this) = self.weak.upgrade() else {
            return;
        };
        tokio::spawn(async move {
            this.prune_disk_events().await;
        });
    }
}

#[async_trait]
impl ITelemetryAppender for CloudAppender {
    fn track(&self, record: TelemetryAppenderRecord) -> Result<(), WireAsyncError> {
        CloudAppender::track(self, record)
    }

    async fn flush(&self) -> Result<(), WireAsyncError> {
        CloudAppender::flush(self).await
    }

    async fn shutdown(&self) -> Result<(), WireAsyncError> {
        CloudAppender::shutdown(self).await
    }
}

fn sanitize_properties(input: &TelemetryProperties) -> CloudProperties {
    let mut out = CloudProperties::new();
    for (key, value) in input {
        if is_cloud_primitive(value) {
            out.insert(key.clone(), value.clone());
        } else {
            on_unexpected_error(format!(
                "telemetry property \"{key}\" is not a primitive and was dropped"
            ));
        }
    }
    out
}

fn is_bun_runtime() -> bool {
    std::env::var_os("BUN_VERSION").is_some()
}

fn runtime_version() -> String {
    std::env::var("BUN_VERSION")
        .or_else(|_| std::env::var("NODE_VERSION"))
        .unwrap_or_else(|_| "unknown".to_string())
}

fn os_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|release| release.trim().to_string())
        .unwrap_or_else(|_| std::env::consts::OS.to_string())
}

fn build_context(options: &CloudAppenderOptions) -> CloudContext {
    let bootstrap = &options.bootstrap;
    let client_version = bootstrap.client_identity().version.clone();
    let mut context = CloudContext::from_iter([
        ("app_name".to_string(), Value::String(options.app_name.clone())),
        ("client_version".to_string(), Value::String(client_version.clone())),
        ("version".to_string(), Value::String(client_version)),
        ("core_version".to_string(), Value::String(resolve_core_version())),
        (
            "runtime".to_string(),
            Value::String(if is_bun_runtime() { "bun" } else { "node" }.to_string()),
        ),
        ("platform".to_string(), Value::String(bootstrap.platform().to_string())),
        ("arch".to_string(), Value::String(bootstrap.arch().to_string())),
        ("node_version".to_string(), Value::String(runtime_version())),
        ("os_version".to_string(), Value::String(os_release())),
        ("ci".to_string(), Value::Bool(bootstrap.get_env("CI").is_some())),
        (
            "locale".to_string(),
            Value::String(
                options
                    .locale
                    .clone()
                    .or_else(|| bootstrap.get_env("LANG"))
                    .unwrap_or_default(),
            ),
        ),
        (
            "terminal".to_string(),
            Value::String(
                options
                    .terminal
                    .clone()
                    .or_else(|| bootstrap.get_env("TERM_PROGRAM"))
                    .unwrap_or_default(),
            ),
        ),
        (
            "ui_mode".to_string(),
            Value::String(options.ui_mode.clone().unwrap_or_else(|| "shell".to_string())),
        ),
    ]);
    set_primitive(
        &mut context,
        "model",
        options.model.clone().map(Value::String),
    );
    set_primitive(
        &mut context,
        "build_sha",
        options.build_sha.clone().map(Value::String),
    );
    context
}

fn set_primitive(target: &mut CloudContext, key: &str, value: Option<CloudPrimitive>) {
    let Some(value) = value else {
        return;
    };
    if value.as_str().is_some_and(str::is_empty) {
        return;
    }
    target.insert(key.to_string(), value);
}

fn region_source_name(source: KimiRegionSource) -> &'static str {
    match source {
        KimiRegionSource::Env => "env",
        KimiRegionSource::Login => "login",
        KimiRegionSource::Marker => "marker",
        KimiRegionSource::LocalSignals => "local-signals",
        KimiRegionSource::Fallback => "fallback",
        KimiRegionSource::Default => "default",
    }
}

fn telemetry_endpoint(region: KimiRegion) -> Option<&'static str> {
    match region {
        KimiRegion::MainlandCn => Some("https://telemetry-logs.kimi.com/v1/event"),
        KimiRegion::Global => Some("https://telemetry-logs.kimi.ai/v1/event"),
    }
}
