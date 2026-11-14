use std::sync::LazyLock;

use kimi_core::primitives::errors::{ErrorDomain, register_error_domain};

pub use kimi_core::primitives::errors::domains::web::{
    WEB_FETCH_FAILED, WEB_INVALID_URL, WEB_PRIVATE_ADDRESS,
};

pub static WEB_ERRORS: ErrorDomain = kimi_core::primitives::errors::domains::web::DOMAIN;

static REGISTRATION: LazyLock<()> = LazyLock::new(|| {
    let _ = register_error_domain(&WEB_ERRORS);
});

pub fn register_web_errors() {
    LazyLock::force(&REGISTRATION);
}
