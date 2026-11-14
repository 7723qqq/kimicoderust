pub use kimi_core::primitives::errors::domains::plugin as PluginErrors;
pub use kimi_core::primitives::errors::domains::plugin::{PLUGIN_LOAD_FAILED, PLUGIN_NOT_FOUND};

pub fn register_plugin_errors() {
    let _ = kimi_core::primitives::errors::register_error_domain(&PluginErrors::DOMAIN);
}
