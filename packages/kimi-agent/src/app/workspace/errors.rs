use std::sync::LazyLock;

use kimi_core::primitives::errors::{ErrorDomain, register_error_domain};

pub use kimi_core::primitives::errors::domains::workspace::WORKSPACE_NOT_FOUND;

pub static WORKSPACE_ERRORS: ErrorDomain = kimi_core::primitives::errors::domains::workspace::DOMAIN;

static REGISTRATION: LazyLock<()> = LazyLock::new(|| {
    let _ = register_error_domain(&WORKSPACE_ERRORS);
});

pub fn register_workspace_errors() {
    LazyLock::force(&REGISTRATION);
}
