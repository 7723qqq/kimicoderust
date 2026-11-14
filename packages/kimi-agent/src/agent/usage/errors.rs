pub use kimi_core::primitives::errors::domains::usage as UsageErrors;

pub fn register_usage_errors() {
    let _ = kimi_core::primitives::errors::register_error_domain(&UsageErrors::DOMAIN);
}
