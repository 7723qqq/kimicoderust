use std::error::Error;
use std::fmt;
use std::ops::Deref;

use kimi_core::primitives::errors::{Error2, Error2Options};

pub use kimi_core::primitives::errors::domains::state::{
    DOMAIN as STATE_ERRORS, STATE_CYCLE, STATE_DUPLICATE_FOLD, STATE_DURABILITY_MISMATCH,
};

pub type StateErrorCode = &'static str;

#[derive(Debug)]
pub struct StateError {
    inner: Error2,
}

impl StateError {
    pub fn new(code: StateErrorCode, message: impl Into<String>) -> Self {
        Self::with_options(code, message, Error2Options::default())
    }

    pub fn with_options(
        code: StateErrorCode,
        message: impl Into<String>,
        options: Error2Options,
    ) -> Self {
        Self {
            inner: Error2::with_options(code, message, options).with_name("StateError"),
        }
    }
}

impl Deref for StateError {
    type Target = Error2;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl Error for StateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.inner.source()
    }
}
