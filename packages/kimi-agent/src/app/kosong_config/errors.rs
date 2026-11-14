pub use kimi_core::primitives::errors::domains::models_dev_import::{
    CATALOG_ENTRY_NOT_FOUND, CATALOG_IMPORT_INVALID, CATALOG_UNAVAILABLE, DOMAIN,
    PROVIDER_OAUTH_MANAGED, REGISTRY_IMPORT_INVALID,
};

pub fn register_models_dev_import_errors() {
    let _ = kimi_core::primitives::errors::register_error_domain(&DOMAIN);
}
