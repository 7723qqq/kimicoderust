use std::sync::Arc;

use serde_json::Value;

use crate::wire::record::WireAsyncError;

use super::context::TelemetryProperties;
use super::telemetry::{ITelemetryAppender, TelemetryAppenderRecord};

#[derive(Clone, Default)]
pub struct ConsoleAppenderOptions {
    pub prefix: Option<String>,
    pub pretty: Option<bool>,
    pub log: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

const DEFAULT_PREFIX: &str = "[telemetry]";

pub struct ConsoleAppender {
    prefix: String,
    pretty: bool,
    log: Arc<dyn Fn(String) + Send + Sync>,
}

impl ConsoleAppender {
    pub fn new(options: ConsoleAppenderOptions) -> Self {
        Self {
            prefix: options
                .prefix
                .unwrap_or_else(|| DEFAULT_PREFIX.to_string()),
            pretty: options.pretty.unwrap_or(false),
            log: options.log.unwrap_or_else(|| Arc::new(default_log)),
        }
    }
}

impl Default for ConsoleAppender {
    fn default() -> Self {
        Self::new(ConsoleAppenderOptions::default())
    }
}

impl ITelemetryAppender for ConsoleAppender {
    fn track(&self, record: TelemetryAppenderRecord) -> Result<(), WireAsyncError> {
        let payload = if record.properties.is_empty() {
            String::new()
        } else {
            format!(
                " {}",
                stringify_properties(&record.properties, self.pretty)
            )
        };
        (self.log)(format!("{} {}{}", self.prefix, record.event, payload));
        Ok(())
    }
}

fn stringify_properties(properties: &TelemetryProperties, pretty: bool) -> String {
    let value = Value::Object(properties.clone());
    if pretty {
        serde_json::to_string_pretty(&value)
    } else {
        serde_json::to_string(&value)
    }
    .expect("telemetry properties are JSON serializable")
}

fn default_log(message: String) {
    println!("{message}");
}
