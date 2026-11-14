use std::error::Error;
use std::fmt;
use std::ops::Deref;

use kimi_core::primitives::errors::{Error2, Error2Options};

pub use kimi_core::primitives::errors::domains::wire::{
    DOMAIN as WIRE_ERRORS, RECORDS_WRITE_FAILED, WIRE_CYCLE, WIRE_DUPLICATE_OP,
    WIRE_MIGRATION_MISSING, WIRE_UNKNOWN_RECORD, WIRE_VERSION_TOO_LOW,
};

pub type WireErrorCode = &'static str;

#[derive(Debug)]
pub struct WireError {
    inner: Error2,
}

impl WireError {
    pub fn new(code: WireErrorCode, message: impl Into<String>) -> Self {
        Self::with_options(code, message, Error2Options::default())
    }

    pub fn with_options(
        code: WireErrorCode,
        message: impl Into<String>,
        options: Error2Options,
    ) -> Self {
        Self {
            inner: Error2::with_options(code, message, options).with_name("WireError"),
        }
    }

    pub fn into_inner(self) -> Error2 {
        self.inner
    }
}

impl Deref for WireError {
    type Target = Error2;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl Error for WireError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.inner.source()
    }
}
