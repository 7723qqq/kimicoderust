pub use kimi_core::primitives::errors::domains::profile as ProfileErrors;

pub fn register_profile_errors() {
    let _ = kimi_core::primitives::errors::register_error_domain(&ProfileErrors::DOMAIN);
}
