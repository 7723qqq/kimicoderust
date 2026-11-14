use std::sync::LazyLock;

use kimi_core::primitives::errors::{ErrorDomain, register_error_domain};

pub use kimi_core::primitives::errors::domains::config::{CONFIG_INVALID, CONFIG_PERSIST_BLOCKED};

pub static CONFIG_ERRORS: ErrorDomain = kimi_core::primitives::errors::domains::config::DOMAIN;

static REGISTRATION: LazyLock<()> = LazyLock::new(|| {
    let _ = register_error_domain(&CONFIG_ERRORS);
});

pub fn register_config_errors() {
    LazyLock::force(&REGISTRATION);
}
