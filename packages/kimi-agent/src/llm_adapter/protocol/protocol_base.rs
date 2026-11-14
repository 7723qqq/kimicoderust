use std::sync::{Arc, LazyLock};

use crate::human::llm::provider::definition::ProtocolBinding;
use crate::human::llm::protocol::base::ProtocolName;
use crate::human::llm::requester::bases::anthropic::requester::create_anthropic_base;
use crate::human::llm::requester::bases::antigravity::requester::create_antigravity_base;
use crate::human::llm::requester::bases::google_genai::requester::create_google_gen_ai_base;
use crate::human::llm::requester::bases::openai::requester::OpenAIBase;
use crate::human::llm::requester::bases::openai_responses::requester::OpenAIResponsesBase;
use crate::human::llm::requester::bases::anthropic::r#trait::AnthropicTrait;
use crate::human::llm::requester::bases::antigravity::r#trait::AntigravityTrait;
use crate::human::llm::requester::bases::google_genai::r#trait::GoogleGenAITrait;
use crate::human::llm::requester::bases::openai::openai_trait::OpenAITrait;
use crate::human::llm::requester::bases::openai_responses::openai_responses_trait::OpenAIResponsesTrait;
use crate::llm_adapter::protocol::protocol::Protocol;

pub type ProtocolBaseId = Protocol;

#[derive(Debug, Clone, Default)]
pub struct ProtocolTraitMetadata {
    pub thinking: Option<()>,
    pub strict_thinking_validation: Option<bool>,
}

#[derive(Clone)]
pub enum ProtocolTrait {
    OpenAI(OpenAITrait),
    OpenAIResponses(OpenAIResponsesTrait),
    Anthropic(AnthropicTrait),
    GoogleGenAI(GoogleGenAITrait),
    Antigravity(AntigravityTrait),
}

impl ProtocolTrait {
    pub fn metadata(&self) -> ProtocolTraitMetadata {
        match self {
            Self::OpenAI(trait_) => ProtocolTraitMetadata {
                thinking: trait_.thinking.as_ref().map(|_| ()),
                strict_thinking_validation: trait_.strict_thinking_validation,
            },
            Self::OpenAIResponses(trait_) => ProtocolTraitMetadata {
                thinking: trait_.thinking.as_ref().map(|_| ()),
                strict_thinking_validation: trait_.strict_thinking_validation,
            },
            Self::Anthropic(trait_) => ProtocolTraitMetadata {
                thinking: trait_.thinking.as_ref().map(|_| ()),
                strict_thinking_validation: None,
            },
            Self::GoogleGenAI(trait_) => ProtocolTraitMetadata {
                thinking: trait_.thinking.as_ref().map(|_| ()),
                strict_thinking_validation: None,
            },
            Self::Antigravity(_) => ProtocolTraitMetadata::default(),
        }
    }
}

#[derive(Clone)]
pub struct ProtocolBaseDefinition {
    pub id: ProtocolBaseId,
    pub binding: ProtocolBinding,
}

#[derive(Clone)]
pub struct ResolvedAdapterIdentity {
    pub base_id: ProtocolBaseId,
    pub trait_: Option<ProtocolTraitMetadata>,
    pub binding: ProtocolBinding,
}

fn binding<TTrait>(
    protocol: ProtocolName,
    base: Arc<dyn crate::human::llm::protocol::base::ProtocolBase<TTrait>>,
) -> ProtocolBinding
where
    TTrait: Clone + Send + Sync + 'static,
{
    ProtocolBinding::new(protocol, base, None, None, None, None)
}

pub static PROTOCOL_BASES: LazyLock<Vec<ProtocolBaseDefinition>> = LazyLock::new(|| {
    vec![
        ProtocolBaseDefinition {
            id: Protocol::OpenAI,
            binding: binding(ProtocolName::OpenAI, Arc::new(OpenAIBase)),
        },
        ProtocolBaseDefinition {
            id: Protocol::OpenAIResponses,
            binding: binding(ProtocolName::OpenAIResponses, Arc::new(OpenAIResponsesBase)),
        },
        ProtocolBaseDefinition {
            id: Protocol::Anthropic,
            binding: binding(
                ProtocolName::Anthropic,
                Arc::new(create_anthropic_base(Default::default())),
            ),
        },
        ProtocolBaseDefinition {
            id: Protocol::GoogleGenAI,
            binding: binding(
                ProtocolName::GoogleGenAI,
                Arc::new(create_google_gen_ai_base(None, false)),
            ),
        },
        ProtocolBaseDefinition {
            id: Protocol::Antigravity,
            binding: binding(ProtocolName::Antigravity, Arc::new(create_antigravity_base())),
        },
    ]
});

pub fn get_protocol_base(id: ProtocolBaseId) -> Option<&'static ProtocolBaseDefinition> {
    PROTOCOL_BASES.iter().find(|definition| definition.id == id)
}

pub fn list_protocol_bases() -> &'static [ProtocolBaseDefinition] {
    PROTOCOL_BASES.as_slice()
}
