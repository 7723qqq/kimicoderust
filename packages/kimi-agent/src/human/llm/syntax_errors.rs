use std::fmt;

use crate::human::llm::errors::{LlmErrorKind, LlmErrorMessage, LlmSyntaxErrorCode};
use crate::wire::record::WireAsyncError;

#[derive(Debug)]
pub struct SyntaxRequestFormatError {
    message: String,
    code: LlmSyntaxErrorCode,
}

impl SyntaxRequestFormatError {
    pub fn new(message: impl Into<String>) -> Self {
        Self::with_code(LlmSyntaxErrorCode::RequestFormat, message)
    }

    pub fn with_code(code: LlmSyntaxErrorCode, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code,
        }
    }

    pub fn to_llm_error_message(&self) -> LlmErrorMessage {
        LlmErrorMessage {
            kind: LlmErrorKind::Syntax,
            code: Some(self.code),
            message: self.message.clone(),
            status_code: None,
            request_id: None,
            retry_after_ms: None,
            headers: None,
            retryable: None,
            finish_reason: None,
            raw_finish_reason: None,
        }
    }
}

impl fmt::Display for SyntaxRequestFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SyntaxRequestFormatError {}

pub fn to_llm_syntax_error_message(error: &WireAsyncError) -> LlmErrorMessage {
    if let Some(error) = error.downcast_ref::<crate::human::llm::thinking::ThinkingConfigError>() {
        return error.to_llm_error_message();
    }
    if let Some(error) = error.downcast_ref::<SyntaxRequestFormatError>() {
        return error.to_llm_error_message();
    }
    LlmErrorMessage {
        kind: LlmErrorKind::Syntax,
        code: Some(LlmSyntaxErrorCode::Internal),
        message: error.to_string(),
        status_code: None,
        request_id: None,
        retry_after_ms: None,
        headers: None,
        retryable: None,
        finish_reason: None,
        raw_finish_reason: None,
    }
}
