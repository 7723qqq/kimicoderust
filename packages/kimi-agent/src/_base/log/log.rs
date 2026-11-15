use std::sync::Arc;

use futures_util::future::BoxFuture;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
}

impl LogLevel {
    fn order(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Error => 1,
            Self::Warn => 2,
            Self::Info => 3,
            Self::Debug => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogEntryLevel {
    Error,
    Warn,
    Info,
    Debug,
}

impl From<LogEntryLevel> for LogLevel {
    fn from(level: LogEntryLevel) -> Self {
        match level {
            LogEntryLevel::Error => Self::Error,
            LogEntryLevel::Warn => Self::Warn,
            LogEntryLevel::Info => Self::Info,
            LogEntryLevel::Debug => Self::Debug,
        }
    }
}

pub type LogContext = indexmap::IndexMap<String, serde_json::Value>;
pub type LogPayload = serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntryError {
    pub message: String,
    pub stack: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub t: u64,
    pub level: LogEntryLevel,
    pub msg: String,
    pub ctx: Option<LogContext>,
    pub error: Option<LogEntryError>,
}

pub trait ILogWriter: Send + Sync {
    fn write(&self, entry: LogEntry);

    fn flush(&self) -> BoxFuture<'static, ()> {
        async {}.boxed()
    }

    fn close(&self) -> BoxFuture<'static, ()> {
        async {}.boxed()
    }

    fn flush_sync(&self) {}
}

pub trait ILogger: Send + Sync {
    fn error(&self, message: &str, payload: Option<LogPayload>);
    fn warn(&self, message: &str, payload: Option<LogPayload>);
    fn info(&self, message: &str, payload: Option<LogPayload>);
    fn debug(&self, message: &str, payload: Option<LogPayload>);
    fn child(&self, ctx: LogContext) -> Arc<dyn ILogger>;
}

pub trait ILogService: ILogger {
    fn level(&self) -> LogLevel;
    fn set_level(&self, level: LogLevel);
    fn flush(&self) -> BoxFuture<'static, ()>;
}

pub static I_LOG_SERVICE: Lazy<ServiceIdentifier<Arc<dyn ILogService>>> =
    Lazy::new(|| create_decorator("logService"));

pub fn level_enabled(level: LogLevel, configured: LogLevel) -> bool {
    if level == LogLevel::Off || configured == LogLevel::Off {
        return false;
    }
    level.order() <= configured.order()
}

use futures_util::FutureExt;
