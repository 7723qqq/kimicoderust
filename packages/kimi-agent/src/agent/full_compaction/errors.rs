use std::sync::{Arc, LazyLock};

use indexmap::IndexMap;

use crate::_base::errors::codes::{ErrorDomain, register_error_domain};

pub use crate::compaction::*;

pub const COMPACTION_FAILED: &str = "compaction.failed";
pub const COMPACTION_UNABLE: &str = "compaction.unable";

pub static FULL_COMPACTION_ERRORS: LazyLock<ErrorDomain> = LazyLock::new(|| ErrorDomain {
    codes: Arc::new(IndexMap::from([
        ("COMPACTION_FAILED".to_string(), COMPACTION_FAILED.to_string()),
        ("COMPACTION_UNABLE".to_string(), COMPACTION_UNABLE.to_string()),
    ])),
    retryable: None,
    info: None,
});

pub fn register_full_compaction_errors() {
    register_error_domain(FULL_COMPACTION_ERRORS.clone());
}
