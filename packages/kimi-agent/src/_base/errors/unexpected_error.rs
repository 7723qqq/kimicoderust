use std::sync::Arc;

use once_cell::sync::Lazy;
use parking_lot::RwLock;

pub type UnexpectedErrorHandler = Arc<dyn Fn(&str) + Send + Sync>;

fn default_handler() -> UnexpectedErrorHandler {
    Arc::new(|error| eprintln!("[unexpected] {error}"))
}

static CURRENT_HANDLER: Lazy<RwLock<UnexpectedErrorHandler>> =
    Lazy::new(|| RwLock::new(default_handler()));

pub fn set_unexpected_error_handler(handler: UnexpectedErrorHandler) {
    *CURRENT_HANDLER.write() = handler;
}

pub fn reset_unexpected_error_handler() {
    *CURRENT_HANDLER.write() = default_handler();
}

pub fn on_unexpected_error(error: impl ToString) {
    let error = error.to_string();
    let handler = CURRENT_HANDLER.read().clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(&error)));
    if let Err(handler_error) = result {
        let handler_error = handler_error
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| handler_error.downcast_ref::<&str>().map(|value| (*value).to_string()))
            .unwrap_or_else(|| "unknown handler error".to_string());
        eprintln!("[unexpected] handler threw {handler_error} while reporting {error}");
    }
}

pub fn safely_call_listener(listener: impl FnOnce()) {
    if let Err(error) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(listener)) {
        let error = error
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| error.downcast_ref::<&str>().map(|value| (*value).to_string()))
            .unwrap_or_else(|| "unknown listener error".to_string());
        on_unexpected_error(error);
    }
}
