use std::sync::LazyLock;

use kimi_core::primitives::errors::{ErrorDomain, register_error_domain};

pub use kimi_core::primitives::errors::domains::auth::{
    AUTH_LOGIN_REQUIRED, AUTH_MODEL_NOT_RESOLVED, AUTH_PROVISIONING_REQUIRED, AUTH_TOKEN_MISSING,
    AUTH_TOKEN_UNAUTHORIZED,
};

pub static AUTH_ERRORS: ErrorDomain = kimi_core::primitives::errors::domains::auth::DOMAIN;

static REGISTRATION: LazyLock<()> = LazyLock::new(|| {
    if let Err(error) = register_error_domain(&AUTH_ERRORS) {
        std::panic::panic_any(error);
    }
});

pub fn register_auth_errors() {
    LazyLock::force(&REGISTRATION);
}
