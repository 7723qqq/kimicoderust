use std::sync::Arc;

use serde_json::Value;

#[derive(Clone)]
pub struct LlmLogger {
    warn: Arc<dyn Fn(&str, Option<Value>) + Send + Sync>,
}

impl LlmLogger {
    pub fn new(
        warn: impl Fn(&str, Option<Value>) + Send + Sync + 'static,
    ) -> Self {
        Self { warn: Arc::new(warn) }
    }

    pub fn warn(&self, message: &str, payload: Option<Value>) {
        (self.warn)(message, payload);
    }
}
