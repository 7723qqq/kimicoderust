use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use indexmap::IndexMap;
use once_cell::sync::Lazy;
use parking_lot::Mutex;

pub type ErrorCode = String;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ErrorInfo {
    pub title: Option<String>,
    pub retryable: bool,
    pub public: bool,
    pub action: Option<String>,
}

#[derive(Clone)]
pub struct ErrorDomain {
    pub codes: Arc<IndexMap<String, String>>,
    pub retryable: Option<Vec<String>>,
    pub info: Option<IndexMap<String, ErrorInfo>>,
}

static REGISTERED_CODES: Lazy<Mutex<HashMap<String, Arc<IndexMap<String, String>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static RETRYABLE_CODES: Lazy<Mutex<HashSet<String>>> = Lazy::new(|| Mutex::new(HashSet::new()));
static INFO_OVERRIDES: Lazy<Mutex<HashMap<String, ErrorInfo>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub static CORE_ERRORS: Lazy<ErrorDomain> = Lazy::new(|| {
    let codes = IndexMap::from([
        ("INTERNAL".to_string(), "internal".to_string()),
        ("NOT_IMPLEMENTED".to_string(), "not_implemented".to_string()),
        ("VALIDATION_FAILED".to_string(), "validation.failed".to_string()),
    ]);
    let info = IndexMap::from([
        (
            "internal".to_string(),
            ErrorInfo {
                title: None,
                retryable: false,
                public: true,
                action: None,
            },
        ),
        (
            "not_implemented".to_string(),
            ErrorInfo {
                title: None,
                retryable: false,
                public: true,
                action: None,
            },
        ),
    ]);
    ErrorDomain {
        codes: Arc::new(codes),
        retryable: None,
        info: Some(info),
    }
});

static CORE_REGISTERED: Lazy<()> = Lazy::new(|| {
    register_domain(CORE_ERRORS.clone());
});

fn ensure_core_registered() {
    Lazy::force(&CORE_REGISTERED);
}

fn register_domain(domain: ErrorDomain) {
    for code in domain.codes.values() {
        if let Some(owner) = REGISTERED_CODES.lock().get(code)
            && !Arc::ptr_eq(owner, &domain.codes)
        {
            panic!("error code '{code}' is registered by two different domains");
        }
        REGISTERED_CODES
            .lock()
            .insert(code.clone(), domain.codes.clone());
    }
    for code in domain.retryable.unwrap_or_default() {
        RETRYABLE_CODES.lock().insert(code);
    }
    for (code, info) in domain.info.unwrap_or_default() {
        INFO_OVERRIDES.lock().insert(code, info);
    }
}

pub fn register_error_domain(domain: ErrorDomain) {
    ensure_core_registered();
    register_domain(domain);
}

pub fn is_error_code(code: &str) -> bool {
    ensure_core_registered();
    REGISTERED_CODES.lock().contains_key(code)
}

pub fn error_info(code: &str) -> ErrorInfo {
    ensure_core_registered();
    if let Some(info) = INFO_OVERRIDES.lock().get(code) {
        return info.clone();
    }
    ErrorInfo {
        title: None,
        retryable: RETRYABLE_CODES.lock().contains(code),
        public: true,
        action: None,
    }
}
