use std::error::Error;
use std::fmt;
use std::ops::Deref;
use std::sync::LazyLock;

use kimi_core::primitives::errors::{Error2, Error2Options, ErrorDomain, register_error_domain};

pub use kimi_core::primitives::errors::domains::event::{
    EVENT_DUPLICATE_EVENT, EVENT_SCHEMA_MISSING,
};

pub static EVENT_ERRORS: ErrorDomain = kimi_core::primitives::errors::domains::event::DOMAIN;

static REGISTRATION: LazyLock<()> = LazyLock::new(|| {
    let _ = register_error_domain(&EVENT_ERRORS);
});

pub fn register_event_errors() {
    LazyLock::force(&REGISTRATION);
}

pub type EventErrorCode = &'static str;

#[derive(Debug)]
pub struct EventError {
    inner: Error2,
}

impl EventError {
    pub fn new(code: EventErrorCode, message: impl Into<String>) -> Self {
        Self::with_options(code, message, Error2Options::default())
    }

    pub fn with_options(
        code: EventErrorCode,
        message: impl Into<String>,
        options: Error2Options,
    ) -> Self {
        Self {
            inner: Error2::with_options(code, message, options).with_name("EventError"),
        }
    }
}

impl Deref for EventError {
    type Target = Error2;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl fmt::Display for EventError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(formatter)
    }
}

impl Error for EventError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.inner.source()
    }
}
