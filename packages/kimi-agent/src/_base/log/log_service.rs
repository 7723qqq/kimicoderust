use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::future::{BoxFuture, FutureExt, Shared, join_all};
use once_cell::sync::Lazy;
use parking_lot::Mutex;

use super::file_log::{FileLogWriter, FileLogWriterOptions, create_file_log_writer};
use super::log::{
    ILogWriter, ILogger, ILogService, LogContext, LogEntry, LogEntryError, LogEntryLevel, LogLevel,
    LogPayload, level_enabled,
};
use super::log_config::LoggingConfig;
use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::instantiation::ScopeActivation;
use crate::_base::di::lifecycle::IDisposable;
use crate::_base::di::scope::{LifecycleScope, register_scoped_service};

type CloseFuture = Shared<BoxFuture<'static, ()>>;
static PENDING_LOG_CLOSES: Lazy<Mutex<HashMap<u64, CloseFuture>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static NEXT_CLOSE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn track_log_close(close: BoxFuture<'static, ()>) {
    let close = close.shared();
    let id = NEXT_CLOSE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    PENDING_LOG_CLOSES.lock().insert(id, close.clone());
    tokio::spawn(async move {
        close.await;
        PENDING_LOG_CLOSES.lock().remove(&id);
    });
}

pub async fn drain_log_closes() {
    loop {
        let closes = PENDING_LOG_CLOSES.lock().values().cloned().collect::<Vec<_>>();
        if closes.is_empty() {
            return;
        }
        join_all(closes).await;
    }
}

struct ExtractedPayload {
    ctx: Option<LogContext>,
    error: Option<LogEntryError>,
}

fn error_entry(error: &dyn std::error::Error) -> LogEntryError {
    LogEntryError {
        message: error.to_string(),
        stack: Some(std::backtrace::Backtrace::capture().to_string()),
    }
}

fn stringify_payload(payload: &LogPayload) -> String {
    if let serde_json::Value::String(payload) = payload {
        return payload.clone();
    }
    serde_json::to_string(payload).unwrap_or_else(|_| payload.to_string())
}

fn extract_payload(payload: Option<&LogPayload>) -> Option<ExtractedPayload> {
    let Some(payload) = payload else {
        return Some(ExtractedPayload {
            ctx: None,
            error: None,
        });
    };
    match payload {
        serde_json::Value::Object(values) => {
            let ctx = values
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<LogContext>();
            Some(ExtractedPayload {
                ctx: (!ctx.is_empty()).then_some(ctx),
                error: None,
            })
        }
        serde_json::Value::Array(values) => {
            let ctx = values
                .iter()
                .enumerate()
                .map(|(index, value)| (index.to_string(), value.clone()))
                .collect::<LogContext>();
            Some(ExtractedPayload {
                ctx: (!ctx.is_empty()).then_some(ctx),
                error: None,
            })
        }
        _ => Some(ExtractedPayload {
            ctx: Some(LogContext::from([(
                "reason".to_string(),
                serde_json::Value::String(stringify_payload(payload)),
            )])),
            error: None,
        }),
    }
}

pub struct LogLevelState {
    pub level: Mutex<LogLevel>,
}

pub struct BoundLogger {
    writer: Arc<dyn ILogWriter>,
    level_state: Arc<LogLevelState>,
    bound: LogContext,
}

impl BoundLogger {
    pub fn new(
        writer: Arc<dyn ILogWriter>,
        level_state: Arc<LogLevelState>,
        bound: LogContext,
    ) -> Self {
        Self {
            writer,
            level_state,
            bound,
        }
    }

    pub fn emit_error(&self, message: &str, error: &dyn std::error::Error) {
        self.emit_with_extracted(
            LogEntryLevel::Error,
            message,
            ExtractedPayload {
                ctx: None,
                error: Some(error_entry(error)),
            },
        );
    }

    fn emit(&self, level: LogEntryLevel, message: &str, payload: Option<LogPayload>) {
        let Some(extracted) = extract_payload(payload.as_ref()) else {
            return;
        };
        self.emit_with_extracted(level, message, extracted);
    }

    fn emit_with_extracted(&self, level: LogEntryLevel, message: &str, extracted: ExtractedPayload) {
        if !level_enabled(level.into(), *self.level_state.level.lock()) {
            return;
        }
        let ctx = if extracted.ctx.is_some() || !self.bound.is_empty() {
            let mut ctx = extracted.ctx.unwrap_or_default();
            ctx.extend(self.bound.clone());
            Some(ctx)
        } else {
            None
        };
        self.writer.write(LogEntry {
            t: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            level,
            msg: message.to_string(),
            ctx,
            error: extracted.error,
        });
    }
}

impl ILogger for BoundLogger {
    fn error(&self, message: &str, payload: Option<LogPayload>) {
        self.emit(LogEntryLevel::Error, message, payload);
    }

    fn warn(&self, message: &str, payload: Option<LogPayload>) {
        self.emit(LogEntryLevel::Warn, message, payload);
    }

    fn info(&self, message: &str, payload: Option<LogPayload>) {
        self.emit(LogEntryLevel::Info, message, payload);
    }

    fn debug(&self, message: &str, payload: Option<LogPayload>) {
        self.emit(LogEntryLevel::Debug, message, payload);
    }

    fn child(&self, ctx: LogContext) -> Arc<dyn ILogger> {
        let mut bound = self.bound.clone();
        bound.extend(ctx);
        Arc::new(Self::new(
            self.writer.clone(),
            self.level_state.clone(),
            bound,
        ))
    }
}

pub struct AppLogService {
    sink: Arc<FileLogWriter>,
    root_level: Arc<LogLevelState>,
    logger: BoundLogger,
}

impl AppLogService {
    pub fn new(options: &LoggingConfig) -> Self {
        let sink = Arc::new(create_file_log_writer(FileLogWriterOptions {
            path: options.global_log_path.clone(),
            max_bytes: options.global_max_bytes,
            files: options.global_files,
            format: None,
        }));
        let root_level = Arc::new(LogLevelState {
            level: Mutex::new(options.level),
        });
        let logger = BoundLogger::new(sink.clone(), root_level.clone(), LogContext::new());
        Self {
            sink,
            root_level,
            logger,
        }
    }

    pub fn dispose(&self) {
        self.sink.flush_sync();
        track_log_close(self.sink.close());
    }
}

impl ILogger for AppLogService {
    fn error(&self, message: &str, payload: Option<LogPayload>) {
        self.logger.error(message, payload);
    }

    fn warn(&self, message: &str, payload: Option<LogPayload>) {
        self.logger.warn(message, payload);
    }

    fn info(&self, message: &str, payload: Option<LogPayload>) {
        self.logger.info(message, payload);
    }

    fn debug(&self, message: &str, payload: Option<LogPayload>) {
        self.logger.debug(message, payload);
    }

    fn child(&self, ctx: LogContext) -> Arc<dyn ILogger> {
        self.logger.child(ctx)
    }
}

impl ILogService for AppLogService {
    fn level(&self) -> LogLevel {
        *self.root_level.level.lock()
    }

    fn set_level(&self, level: LogLevel) {
        *self.root_level.level.lock() = level;
    }

    fn flush(&self) -> BoxFuture<'static, ()> {
        self.sink.flush()
    }
}

impl IDisposable for AppLogService {
    fn dispose(&self) {
        AppLogService::dispose(self);
    }
}

static LOG_SERVICE_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let ctor: Constructor<DynamicValue> = Arc::new(|args| {
        let options = args
            .first()
            .cloned()
            .and_then(|value| Arc::downcast::<LoggingConfig>(value).ok())
            .unwrap_or_else(|| panic!("AppLogService requires log options"));
        Arc::new(AppLogService::new(options.as_ref())) as DynamicValue
    });
    register_scoped_service(
        LifecycleScope::App.as_str().to_string(),
        super::log::I_LOG_SERVICE.clone(),
        SyncDescriptor::new(ctor, Vec::new()),
        ScopeActivation::OnScopeCreated,
        "log",
    )
    .map_err(|error| error.to_string())
});

pub fn ensure_log_service_registered() -> Result<(), String> {
    Lazy::force(&LOG_SERVICE_REGISTRATION).clone()
}
