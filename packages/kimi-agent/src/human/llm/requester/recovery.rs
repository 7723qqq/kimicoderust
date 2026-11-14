use std::sync::Arc;

use crate::human::llm::errors::LlmRemoteErrorMessage;
use crate::human::llm::message::Message;
use crate::human::llm::requester::requester::LlmCredentialProvider;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmRecoveryRecord {
    pub strategy: String,
    pub action: String,
}

pub struct LlmRecoveryContext {
    pub error: LlmRemoteErrorMessage,
    pub messages: Vec<Message>,
    pub applied_recoveries: Vec<LlmRecoveryRecord>,
    pub credential_provider: Option<Arc<dyn LlmCredentialProvider>>,
}

pub struct LlmRecoveryProposal {
    pub action: String,
    pub attempt_message_override: Option<Vec<Message>>,
    pub before_next_attempt: Option<Box<dyn FnOnce() + Send>>,
}

pub struct LlmRecoveryProposalRecord {
    pub strategy: String,
    pub action: String,
    pub attempt_message_override: Option<Vec<Message>>,
    pub before_next_attempt: Option<Box<dyn FnOnce() + Send>>,
}

pub trait LlmRecovery: Send + Sync {
    fn propose(&self, ctx: LlmRecoveryContext) -> Option<LlmRecoveryProposalRecord>;
}
