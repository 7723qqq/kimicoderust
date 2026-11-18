use std::collections::HashMap;
use std::ops::Deref;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::agent::agent_context::agent_context::AgentContext;
use crate::agent::llm_requester::llm_requester::AgentLLMRequestSource;
use crate::human::llm::usage::TokenUsage;
use kimi_core::primitives::errors::{Error2, Error2Options};

pub use super::errors::UsageErrors;

#[derive(Debug)]
pub struct UsageError(pub Error2);

impl UsageError {
    pub fn new(code: &str, message: &str, details: Option<Map<String, Value>>) -> Self {
        Self(
            Error2::with_options(
                code,
                message,
                Error2Options {
                    details,
                    ..Default::default()
                },
            )
            .with_name("UsageError"),
        )
    }
}

impl Deref for UsageError {
    type Target = Error2;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Display for UsageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for UsageError {}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_model: Option<HashMap<String, TokenUsage>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_turn: Option<TokenUsage>,
}

#[derive(Clone)]
pub struct UsageRecordedContext {
    pub agent: AgentContext,
    pub model: String,
    pub usage: TokenUsage,
    pub source: Option<AgentLLMRequestSource>,
    pub first_record: bool,
}
