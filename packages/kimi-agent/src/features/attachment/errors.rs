use kimi_core::primitives::errors::{Error2, Error2Options};

pub use kimi_core::primitives::errors::domains::attachment as AttachmentErrors;
pub use kimi_core::primitives::errors::domains::attachment::{
    ATTACHMENT_IMAGE_TOO_LARGE, ATTACHMENT_IMAGE_TOO_MANY_PIXELS,
    ATTACHMENT_IMAGE_TYPE_MISMATCH, ATTACHMENT_INVALID_IMAGE, ATTACHMENT_INVALID_REF,
};

pub struct AttachmentError;

impl AttachmentError {
    pub fn new(
        code: impl Into<kimi_core::primitives::errors::ErrorCode>,
        message: impl Into<String>,
        options: Error2Options,
    ) -> Error2 {
        Error2::with_options(code, message, options).with_name("AttachmentError")
    }
}
