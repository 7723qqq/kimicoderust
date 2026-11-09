
pub mod domains;

mod codes;
mod error2;
mod serialize;

pub use codes::{
    DuplicateCode, ErrorCode, ErrorDomain, ErrorInfo, INTERNAL_CODE, NOT_IMPLEMENTED_CODE,
    VALIDATION_FAILED_CODE, error_info, is_error_code, register_error_domain,
};
pub use error2::{
    Error2, Error2Options, ErrorDetails, is_error2, not_implemented, unwrap_error_cause,
};
pub use serialize::{
    ErrorPayload, KimiErrorPayload, MAX_CAUSE_DEPTH, PayloadOptions, from_error_payload,
    is_coded_error, make_error_payload, to_error_payload, to_kimi_error_payload,
};
