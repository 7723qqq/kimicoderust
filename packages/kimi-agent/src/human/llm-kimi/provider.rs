use std::sync::{Arc, LazyLock};

use crate::human::llm::protocol::base::ProtocolName;
use crate::human::llm::provider::definition::{
    Provider, ProviderDefinition, ProtocolBinding, create_provider,
};
use crate::human::llm::requester::bases::anthropic::requester::{
    AnthropicRequesterOptions, create_anthropic_base,
};
use crate::human::llm::requester::bases::openai::requester::OpenAIBase;
use crate::human::llm::requester::bases::openai_responses::requester::OpenAIResponsesBase;
use crate::human::llm_kimi::errors::classify_kimi_quota_error;
use crate::human::llm_kimi::media::KIMI_MEDIA_CONTRIBUTION;
use crate::human::llm_kimi::r#trait::{
    KIMI_ANTHROPIC_TRAIT, KIMI_CONNECTION, KIMI_OPEN_AI_TRAIT, KIMI_RESPONSES_TRAIT,
};

pub static KIMI_PROVIDER: LazyLock<Arc<dyn Provider>> = LazyLock::new(|| {
    create_provider(ProviderDefinition {
        id: "kimi".to_string(),
        protocols: vec![
            ProtocolBinding::new(
                ProtocolName::OpenAI,
                Arc::new(OpenAIBase),
                Some(KIMI_OPEN_AI_TRAIT.clone()),
                Some(KIMI_CONNECTION.clone()),
                Some(Arc::new(classify_kimi_quota_error)),
                None,
            ),
            ProtocolBinding::new(
                ProtocolName::Anthropic,
                Arc::new(create_anthropic_base(AnthropicRequesterOptions {
                    beta_api: Some(true),
                    ..Default::default()
                })),
                Some(KIMI_ANTHROPIC_TRAIT.clone()),
                Some(KIMI_CONNECTION.clone()),
                Some(Arc::new(classify_kimi_quota_error)),
                None,
            ),
            ProtocolBinding::new(
                ProtocolName::OpenAIResponses,
                Arc::new(OpenAIResponsesBase),
                Some(KIMI_RESPONSES_TRAIT.clone()),
                Some(KIMI_CONNECTION.clone()),
                Some(Arc::new(classify_kimi_quota_error)),
                None,
            ),
        ],
        media: Some(KIMI_MEDIA_CONTRIBUTION.clone()),
        models: None,
    })
    .unwrap()
});
