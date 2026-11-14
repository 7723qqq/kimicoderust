use std::sync::{Arc, LazyLock};

use crate::human::llm::media::upload::ProviderMediaContribution;
use crate::human::llm::protocol::base::ProtocolName;
use crate::human::llm::protocol::connection::{ProtocolEndpoint, ProviderConnection};
use crate::human::llm::provider::definition::{
    Provider, ProviderDefinition, ProtocolBinding, create_provider,
};
use crate::human::llm::requester::bases::anthropic::requester::{
    AnthropicRequesterOptions, create_anthropic_base,
};
use crate::human::llm::requester::bases::antigravity::requester::AntigravityBase;
use crate::human::llm::requester::bases::antigravity::r#trait::AntigravityTrait;
use crate::human::llm::requester::bases::google_genai::requester::create_google_gen_ai_base;
use crate::human::llm::requester::bases::openai::requester::OpenAIBase;
use crate::human::llm::requester::bases::openai_responses::requester::OpenAIResponsesBase;

fn endpoint(api_key_env: &str, base_url_env: &str) -> Option<ProtocolEndpoint> {
    Some(ProtocolEndpoint {
        api_key_env: Some(api_key_env.to_string()),
        base_url_env: Some(base_url_env.to_string()),
        default_base_url: None,
    })
}

static OPEN_AI_CONNECTION: LazyLock<ProviderConnection> = LazyLock::new(|| {
    ProviderConnection::new(|_| endpoint("OPENAI_API_KEY", "OPENAI_BASE_URL"))
});

static ANTHROPIC_CONNECTION: LazyLock<ProviderConnection> = LazyLock::new(|| {
    ProviderConnection::new(|_| endpoint("ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL"))
});

pub static GOOGLE_GEN_AI_CONNECTION: LazyLock<ProviderConnection> = LazyLock::new(|| {
    ProviderConnection::new(|ctx| {
        if ctx.is_some_and(|ctx| ctx.model.connection.vertexai == Some(true)) {
            endpoint("VERTEXAI_API_KEY", "GOOGLE_VERTEX_BASE_URL")
        } else {
            endpoint("GOOGLE_API_KEY", "GOOGLE_GEMINI_BASE_URL")
        }
    })
});

pub static OPENAI_PROVIDER: LazyLock<Arc<dyn Provider>> = LazyLock::new(|| {
    create_provider(ProviderDefinition {
        id: "openai".to_string(),
        protocols: vec![
            ProtocolBinding::new(
                ProtocolName::OpenAI,
                Arc::new(OpenAIBase),
                None,
                Some(OPEN_AI_CONNECTION.clone()),
                None,
                None,
            ),
            ProtocolBinding::new(
                ProtocolName::OpenAIResponses,
                Arc::new(OpenAIResponsesBase),
                None,
                Some(OPEN_AI_CONNECTION.clone()),
                None,
                None,
            ),
        ],
        media: None,
        models: None,
    })
    .unwrap()
});

pub static ANTHROPIC_PROVIDER: LazyLock<Arc<dyn Provider>> = LazyLock::new(|| {
    create_provider(ProviderDefinition {
        id: "anthropic".to_string(),
        protocols: vec![ProtocolBinding::new(
            ProtocolName::Anthropic,
            Arc::new(create_anthropic_base(AnthropicRequesterOptions::default())),
            None,
            Some(ANTHROPIC_CONNECTION.clone()),
            None,
            None,
        )],
        media: None,
        models: None,
    })
    .unwrap()
});

pub static GOOGLE_PROVIDER: LazyLock<Arc<dyn Provider>> = LazyLock::new(|| {
    create_provider(ProviderDefinition {
        id: "google".to_string(),
        protocols: vec![ProtocolBinding::new(
            ProtocolName::GoogleGenAI,
            Arc::new(create_google_gen_ai_base(None, false)),
            None,
            Some(GOOGLE_GEN_AI_CONNECTION.clone()),
            None,
            None,
        )],
        media: Some(ProviderMediaContribution {
            inline_video: Some(true),
            upload_video: None,
            upload_image: None,
        }),
        models: None,
    })
    .unwrap()
});

pub static ANTIGRAVITY_PROVIDER: LazyLock<Arc<dyn Provider>> = LazyLock::new(|| {
    create_provider(ProviderDefinition {
        id: "antigravity".to_string(),
        protocols: vec![ProtocolBinding::new(
            ProtocolName::Antigravity,
            Arc::new(AntigravityBase),
            Some(AntigravityTrait),
            None,
            None,
            None,
        )],
        media: None,
        models: None,
    })
    .unwrap()
});
