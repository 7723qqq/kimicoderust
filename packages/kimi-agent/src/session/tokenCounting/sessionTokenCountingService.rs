use std::sync::Arc;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::agent::agent_context::agent_context::AgentContext;
use crate::agent::agent_context::agent_space::agent_space_of;
use crate::agent::r#loop::turn_ops::TurnEnded;
use crate::agent::token_counting::config_section::{TOKEN_COUNTING_SECTION, TokenCountingConfig};
use crate::agent::token_counting::token_counting::{
    ContextSize, TokenCountingRequest, TokenCountingStrategy,
};
use crate::app::config::config::IConfigService;
use crate::app::event::event_bus::ISessionEventBus;
use crate::human::llm::message::ToolDescription as Tool;
use crate::human::llm::usage::TokenUsage;
use crate::llm_adapter::contract::message::Message;
use crate::llm_adapter::contract::model_family::ImageTokenPricing;
use crate::llm_adapter::contract::tokens::{
    estimate_tokens, estimate_tokens_for_message, estimate_tokens_for_messages,
    estimate_tokens_for_tools,
};
use crate::human::llm::tokens::EstimableMessage;
use crate::session::agent_lifecycle::agent_lifecycle::IAgentLifecycleService;

use super::session_token_counting::{ISessionTokenCountingService, TokenCountingRebaseInput};
use super::token_counting_agent_model::TOKEN_COUNTING_AGENT_MODEL_DEFINITION;

pub struct SessionTokenCountingService {
    config: Arc<dyn IConfigService>,
    _subscription: DisposableRef,
}

impl SessionTokenCountingService {
    pub fn new(
        config: Arc<dyn IConfigService>,
        event_bus: Arc<dyn ISessionEventBus>,
        agent_lifecycle: Arc<dyn IAgentLifecycleService>,
    ) -> Self {
        let strategy_config = config.clone();
        let subscription = event_bus.subscribe_turn_ended(Arc::new(move |event: TurnEnded| {
            let Some(agent) = agent_lifecycle.get(&event.agent_id) else {
                return;
            };
            let strategy = strategy_from_config(strategy_config.as_ref());
            let future = agent_space_of(&agent).use_async(
                &TOKEN_COUNTING_AGENT_MODEL_DEFINITION,
                move |model| async move { model.record_turn(event.turn_id, strategy).await },
            );
            tokio::spawn(async move {
                let _ = future.await;
            });
        }));
        Self {
            config,
            _subscription: subscription,
        }
    }

    pub fn strategy(&self) -> TokenCountingStrategy {
        strategy_from_config(self.config.as_ref())
    }
}

impl ISessionTokenCountingService for SessionTokenCountingService {
    fn strategy(&self) -> TokenCountingStrategy {
        SessionTokenCountingService::strategy(self)
    }

    fn get(&self, agent: &AgentContext, start: Option<isize>, end: Option<isize>) -> ContextSize {
        agent_space_of(agent).use_(&TOKEN_COUNTING_AGENT_MODEL_DEFINITION, |model| {
            model.get(start, end)
        })
    }

    fn measured(
        &self,
        agent: &AgentContext,
        input: &[Message],
        output: &[Message],
        usage: &TokenUsage,
    ) {
        let input = input.to_vec();
        let output = output.to_vec();
        let usage = usage.clone();
        let future = agent_space_of(agent).use_async(
            &TOKEN_COUNTING_AGENT_MODEL_DEFINITION,
            move |model| async move { model.measured(&input, &output, &usage).await },
        );
        tokio::spawn(async move {
            let _ = future.await;
        });
    }

    fn latest_measured(&self, agent: &AgentContext) -> u64 {
        agent_space_of(agent).use_(&TOKEN_COUNTING_AGENT_MODEL_DEFINITION, |model| {
            model.latest_measured()
        })
    }

    fn status_size(&self, agent: &AgentContext) -> u64 {
        agent_space_of(agent).use_(&TOKEN_COUNTING_AGENT_MODEL_DEFINITION, |model| {
            model.status_size(self.strategy()) as u64
        })
    }

    fn record_truncation(&self, agent: &AgentContext, cut_index: usize) {
        let future = agent_space_of(agent).use_async(
            &TOKEN_COUNTING_AGENT_MODEL_DEFINITION,
            move |model| async move { model.record_truncation(cut_index).await },
        );
        tokio::spawn(async move {
            let _ = future.await;
        });
    }

    fn rebase(&self, agent: &AgentContext, input: TokenCountingRebaseInput) {
        let future = agent_space_of(agent).use_async(
            &TOKEN_COUNTING_AGENT_MODEL_DEFINITION,
            move |model| async move { model.rebase(input).await },
        );
        tokio::spawn(async move {
            let _ = future.await;
        });
    }

    fn request_size(
        &self,
        request: &TokenCountingRequest,
        pricing: Option<&ImageTokenPricing>,
    ) -> u64 {
        self.estimate_text(&request.system_prompt)
            + self.estimate_tools(&request.tools)
            + self.estimate_messages(&request.messages, pricing)
    }

    fn estimate_text(&self, text: &str) -> u64 {
        estimate_tokens(text)
    }

    fn estimate_message(&self, message: &Message, pricing: Option<&ImageTokenPricing>) -> u64 {
        estimate_tokens_for_message(&message.into(), pricing)
    }

    fn estimate_messages(&self, messages: &[Message], pricing: Option<&ImageTokenPricing>) -> u64 {
        estimate_tokens_for_messages(&messages.iter().map(EstimableMessage::from).collect::<Vec<_>>(), pricing)
    }

    fn estimate_tools(&self, tools: &[Tool]) -> u64 {
        estimate_tokens_for_tools(tools)
    }
}

fn strategy_from_config(config: &dyn IConfigService) -> TokenCountingStrategy {
    config
        .get(TOKEN_COUNTING_SECTION)
        .and_then(|value| serde_json::from_value::<TokenCountingConfig>(value).ok())
        .map(|config| config.strategy)
        .unwrap_or(TokenCountingStrategy::MeasuredEstimated)
}
