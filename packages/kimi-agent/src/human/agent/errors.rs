use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

use crate::wire::record::WireAsyncError;

pub const LOOP_MAX_STEPS_EXCEEDED_ERROR_CODE: &str = "loop.max_steps_exceeded";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnInterruptReason {
    MaxSteps,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaxStepsDetails {
    pub max_steps: u64,
}

#[derive(Debug)]
pub struct MaxStepsExceededError {
    pub code: String,
    pub details: MaxStepsDetails,
    message: String,
}

impl MaxStepsExceededError {
    pub fn new(max_steps: u64, message: Option<String>) -> Self {
        Self {
            code: LOOP_MAX_STEPS_EXCEEDED_ERROR_CODE.to_string(),
            details: MaxStepsDetails { max_steps },
            message: message.unwrap_or_else(|| {
                format!(
                    "Turn exceeded maxSteps={max_steps}. If max_steps_per_turn is too small, raise it in config.toml (loop_control.max_steps_per_turn), or run \"/update-config\" to update it, then \"/reload\"."
                )
            }),
        }
    }

    pub fn name(&self) -> &'static str {
        "MaxStepsExceededError"
    }
}

impl Display for MaxStepsExceededError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MaxStepsExceededError {}

pub fn is_max_steps_exceeded_error(error: &WireAsyncError) -> bool {
    if error.downcast_ref::<MaxStepsExceededError>().is_some() {
        return true;
    }
    error
        .downcast_ref::<kimi_core::primitives::errors::Error2>()
        .is_some_and(|error| error.code().as_str() == LOOP_MAX_STEPS_EXCEEDED_ERROR_CODE)
}

pub fn interrupt_reason_of(error: &WireAsyncError) -> TurnInterruptReason {
    if is_max_steps_exceeded_error(error) {
        TurnInterruptReason::MaxSteps
    } else {
        TurnInterruptReason::Error
    }
}
