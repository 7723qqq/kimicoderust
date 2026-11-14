pub use crate::session::token_counting::*;

use serde::{Deserialize, Serialize};

use crate::human::llm::message::ToolDescription as Tool;
use crate::llm_adapter::contract::message::Message;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextSize {
    pub size: usize,
    pub measured: usize,
    pub estimated: usize,
}

#[derive(Debug, Clone, Default)]
pub struct TokenCountingRequest {
    pub system_prompt: String,
    pub tools: Vec<Tool>,
    pub messages: Vec<Message>,
}

impl TokenCountingStrategy {
    #[allow(non_upper_case_globals)]
    pub const MeasuredEstimated: Self = Self::MeasuredAndEstimated;
}

impl Serialize for TokenCountingStrategy {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TokenCountingStrategy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).ok_or_else(|| serde::de::Error::custom(format!("unknown variant `{raw}`")))
    }
}
