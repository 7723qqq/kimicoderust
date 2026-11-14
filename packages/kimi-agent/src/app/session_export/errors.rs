use std::sync::LazyLock;

use kimi_core::primitives::errors::{ErrorDomain, register_error_domain};

pub use kimi_core::primitives::errors::domains::session_export as SessionExportErrors;
pub use kimi_core::primitives::errors::domains::session_export::{
    SESSION_EXPORT_MISSING_VERSION, SESSION_EXPORT_NOT_FOUND, SESSION_EXPORT_OUTPUT_CONFLICT,
    SESSION_EXPORT_TOO_LARGE,
};

pub static SESSION_EXPORT_ERRORS: ErrorDomain =
    kimi_core::primitives::errors::domains::session_export::DOMAIN;

static REGISTRATION: LazyLock<()> = LazyLock::new(|| {
    let _ = register_error_domain(&SESSION_EXPORT_ERRORS);
});

pub fn register_session_export_errors() {
    LazyLock::force(&REGISTRATION);
}
