use std::sync::LazyLock;

use kimi_core::primitives::errors::{ErrorDomain, ErrorInfo, register_error_domain};

pub const PROVIDER_NOT_FOUND: &str = "provider.not_found";
pub const MODEL_NOT_FOUND: &str = "model.not_found";

pub static MODEL_CATALOG_ERRORS: ErrorDomain = ErrorDomain {
    id: "modelCatalog",
    codes: &[
        ("PROVIDER_NOT_FOUND", PROVIDER_NOT_FOUND),
        ("MODEL_NOT_FOUND", MODEL_NOT_FOUND),
    ],
    retryable: &[],
    info: &[
        (PROVIDER_NOT_FOUND, ErrorInfo::new(false, true)),
        (MODEL_NOT_FOUND, ErrorInfo::new(false, true)),
    ],
};

static REGISTRATION: LazyLock<()> = LazyLock::new(|| {
    let _ = register_error_domain(&MODEL_CATALOG_ERRORS);
});

pub fn register_model_catalog_errors() {
    LazyLock::force(&REGISTRATION);
}
