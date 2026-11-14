use std::sync::LazyLock;

use kimi_core::primitives::errors::domains::core::INTERNAL;
use kimi_core::primitives::errors::{
    Error2, Error2Options, ErrorDomain, ErrorInfo, register_error_domain,
};

use crate::llm_adapter::contract::errors::{
    CONTEXT_OVERFLOW_ERROR_CODE, PROVIDER_API_ERROR_CODE, PROVIDER_AUTH_ERROR_CODE,
    PROVIDER_CONNECTION_ERROR_CODE, PROVIDER_FILTERED_ERROR_CODE,
    PROVIDER_OVERLOADED_ERROR_CODE, PROVIDER_RATE_LIMIT_ERROR_CODE, throw_if_abort_error,
};
use crate::wire::record::WireAsyncError;

pub use crate::llm_adapter::contract::errors::sanitize_status_error_message;

pub static PROTOCOL_ERRORS: ErrorDomain = ErrorDomain {
    id: "protocol",
    codes: &[
        ("PROVIDER_API_ERROR", PROVIDER_API_ERROR_CODE),
        ("PROVIDER_FILTERED", PROVIDER_FILTERED_ERROR_CODE),
        ("PROVIDER_RATE_LIMIT", PROVIDER_RATE_LIMIT_ERROR_CODE),
        ("PROVIDER_AUTH_ERROR", PROVIDER_AUTH_ERROR_CODE),
        ("PROVIDER_CONNECTION_ERROR", PROVIDER_CONNECTION_ERROR_CODE),
        ("PROVIDER_OVERLOADED", PROVIDER_OVERLOADED_ERROR_CODE),
        ("CONTEXT_OVERFLOW", CONTEXT_OVERFLOW_ERROR_CODE),
    ],
    retryable: &[
        PROVIDER_RATE_LIMIT_ERROR_CODE,
        PROVIDER_CONNECTION_ERROR_CODE,
        PROVIDER_OVERLOADED_ERROR_CODE,
        CONTEXT_OVERFLOW_ERROR_CODE,
    ],
    info: &[
        (PROVIDER_RATE_LIMIT_ERROR_CODE, ErrorInfo::new(true, true)),
        (PROVIDER_FILTERED_ERROR_CODE, ErrorInfo::new(false, true)),
        (PROVIDER_AUTH_ERROR_CODE, ErrorInfo::new(false, true)),
        (PROVIDER_OVERLOADED_ERROR_CODE, ErrorInfo::new(true, true)),
        (CONTEXT_OVERFLOW_ERROR_CODE, ErrorInfo::new(true, true)),
    ],
};

static REGISTRATION: LazyLock<()> = LazyLock::new(|| {
    let _ = register_error_domain(&PROTOCOL_ERRORS);
});

pub fn register_protocol_errors() {
    LazyLock::force(&REGISTRATION);
}

pub fn translate_provider_error(error: WireAsyncError) -> Result<Error2, WireAsyncError> {
    throw_if_abort_error(error.as_ref())?;
    match error.downcast::<Error2>() {
        Ok(error) => Ok(*error),
        Err(error) => {
            let message = error.to_string();
            let name = std::any::type_name_of_val(error.as_ref());
            Ok(Error2::with_options(
                INTERNAL,
                message,
                Error2Options {
                    name: Some(name.to_string()),
                    cause: Some(error),
                    ..Default::default()
                },
            ))
        }
    }
}
