use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::agent::context_memory::context_ops::context_memory_key;
use crate::agent::context_memory::types::ContextMessage;
use crate::agent::token_counting::token_counting::{ContextSize, TokenCountingStrategy};
use crate::agent::token_counting::token_counting_ops::{
    TokenCountingMeasured, TokenCountingRebased, TokenCountingTruncated,
    TokenCountingTurnRecorded, TokenAnchor, TokenCountingState, anchors_equal,
    normalize_anchor_length,
};
use crate::agent::usage::usage_events::{AgentStatusUpdated, AgentStatusUpdatedPayload};
use crate::app::event::event2::Event2;
use crate::human::llm::usage::TokenUsage;
use crate::llm_adapter::contract::message::Message;
use crate::llm_adapter::contract::tokens::estimate_tokens_for_messages;
use crate::state::agent_model::{
    AgentModel, AgentModelContext, AgentModelDefinition, AgentModelDefinitionInput,
    AgentModelStateSpec, define_agent_model,
};
use crate::state::errors::StateError;
use crate::wire::record::WireAsyncError;

use super::session_token_counting::TokenCountingRebaseInput;

pub const ZERO_ANCHOR: TokenAnchor = TokenAnchor {
    length: 0,
    tokens: 0,
    measured: true,
};

pub struct TokenCountingAgentModel {
    base: AgentModel,
    state: Arc<Mutex<TokenCountingState>>,
}

impl TokenCountingAgentModel {
    pub fn new(context: AgentModelContext) -> Self {
        let state = Arc::new(Mutex::new(
            serde_json::from_value::<TokenCountingState>(context.bridge.initial_state())
                .unwrap_or_default(),
        ));
        let bridge = context.bridge.clone();
        let mut base = AgentModel::new(context);
        register_appliers(&mut base, state.clone(), bridge);
        Self { base, state }
    }

    pub fn into_base(self) -> AgentModel {
        self.base
    }

    pub fn get(&self, start: Option<isize>, end: Option<isize>) -> ContextSize {
        let context = self.context();
        let from = normalize_slice_index(start.unwrap_or(0), context.len());
        let to = normalize_slice_index(end.unwrap_or(context.len() as isize), context.len());
        let anchor = self.latest_anchor(context.len());
        let measured_end = to.min(anchor.length);
        let estimated_start = from.max(anchor.length);
        let measured = if from == 0 && measured_end == anchor.length {
            anchor.tokens
        } else {
            estimate_tokens_for_messages(slice_messages(&context, from, measured_end))
        };
        let estimated = estimate_tokens_for_messages(slice_messages(&context, estimated_start, to));
        ContextSize {
            size: measured + estimated,
            measured,
            estimated,
        }
    }

    pub async fn measured(
        &mut self,
        input: &[Message],
        _output: &[Message],
        usage: &TokenUsage,
    ) -> Result<(), WireAsyncError> {
        let context = self.context();
        if !matches_context(input, &context) {
            return Ok(());
        }
        self.base
            .emit(
                TokenCountingMeasured::new(json!({
                    "agentId": self.base.agent.agent_id,
                    "length": context.len(),
                    "tokens": token_usage_total(usage),
                }))
                .into(),
            )
            .await
    }

    pub fn latest_measured(&self) -> usize {
        self.state
            .lock()
            .anchors
            .iter()
            .rev()
            .find(|anchor| anchor.measured)
            .map(|anchor| anchor.tokens)
            .unwrap_or(0)
    }

    pub fn status_size(&self, strategy: TokenCountingStrategy) -> usize {
        match strategy {
            TokenCountingStrategy::Measured => self.latest_measured(),
            TokenCountingStrategy::Estimated => estimate_tokens_for_messages(&self.context()),
            _ => self.get(None, None).size.max(self.latest_measured()),
        }
    }

    pub async fn record_truncation(&mut self, cut_index: usize) -> Result<(), WireAsyncError> {
        if !self
            .state
            .lock()
            .anchors
            .iter()
            .any(|anchor| anchor.length > cut_index)
        {
            return Ok(());
        }
        let tokens = self.get(Some(0), Some(cut_index as isize)).size;
        self.base
            .emit(
                TokenCountingTruncated::new(json!({
                    "agentId": self.base.agent.agent_id,
                    "length": cut_index,
                    "tokens": tokens,
                }))
                .into(),
            )
            .await
    }

    pub async fn rebase(&mut self, input: TokenCountingRebaseInput) -> Result<(), WireAsyncError> {
        self.base
            .emit(
                TokenCountingRebased::new(json!({
                    "agentId": self.base.agent.agent_id,
                    "length": input.length,
                    "tokens": input.tokens,
                    "measured": input.measured,
                }))
                .into(),
            )
            .await
    }

    pub async fn record_turn(
        &mut self,
        turn_id: u64,
        strategy: TokenCountingStrategy,
    ) -> Result<(), WireAsyncError> {
        let length = self.context().len();
        let tokens = self.status_size(strategy);
        self.base
            .emit(
                TokenCountingTurnRecorded::new(json!({
                    "agentId": self.base.agent.agent_id,
                    "turnId": turn_id,
                    "length": length,
                    "tokens": tokens,
                }))
                .into(),
            )
            .await
    }

    fn context(&self) -> Vec<ContextMessage> {
        serde_json::from_value(self.base.read_legacy(&context_memory_key())).unwrap_or_default()
    }

    fn latest_anchor(&self, context_length: usize) -> TokenAnchor {
        self.state
            .lock()
            .anchors
            .iter()
            .rev()
            .find(|anchor| anchor.length <= context_length)
            .copied()
            .unwrap_or(ZERO_ANCHOR)
    }
}

fn register_appliers(
    base: &mut AgentModel,
    state: Arc<Mutex<TokenCountingState>>,
    bridge: Arc<dyn crate::state::agent_model::AgentModelBridge>,
) {
    let measured_state = state.clone();
    let measured_bridge = bridge.clone();
    let agent = base.agent.clone();
    base.on(
        &TokenCountingMeasured::class(),
        Arc::new(move |event| {
            let length = normalize_anchor_length(number(event, "length"));
            let tokens = number(event, "tokens").max(0) as usize;
            let anchor = TokenAnchor {
                length,
                tokens,
                measured: true,
            };
            let mut state = measured_state.lock();
            let mut anchors = state
                .anchors
                .iter()
                .filter(|anchor| anchor.length < length)
                .copied()
                .collect::<Vec<_>>();
            anchors.push(anchor);
            if state.tokens != tokens || !anchors_equal(&state.anchors, &anchors) {
                state.anchors = anchors;
                state.tokens = tokens;
            }
            drop(state);
            emit_status(
                &measured_bridge,
                &agent.agent_id,
                tokens,
            );
        }),
    )
    .unwrap();
    let truncated_state = state.clone();
    let truncated_bridge = bridge.clone();
    let agent = base.agent.clone();
    base.on(
        &TokenCountingTruncated::class(),
        Arc::new(move |event| {
            let length = normalize_anchor_length(number(event, "length"));
            let tokens = number(event, "tokens").max(0) as usize;
            let mut state = truncated_state.lock();
            let anchors = state
                .anchors
                .iter()
                .filter(|anchor| anchor.length <= length)
                .copied()
                .collect::<Vec<_>>();
            if state.tokens != tokens || !anchors_equal(&state.anchors, &anchors) {
                state.anchors = anchors;
                state.tokens = tokens;
            }
            drop(state);
            emit_status(&truncated_bridge, &agent.agent_id, tokens);
        }),
    )
    .unwrap();
    let rebased_state = state.clone();
    let rebased_bridge = bridge.clone();
    let agent = base.agent.clone();
    base.on(
        &TokenCountingRebased::class(),
        Arc::new(move |event| {
            let length = normalize_anchor_length(number(event, "length"));
            let tokens = number(event, "tokens").max(0) as usize;
            let anchors = vec![TokenAnchor {
                length,
                tokens,
                measured: event
                    .payload
                    .get("measured")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }];
            let mut state = rebased_state.lock();
            if state.tokens != tokens || !anchors_equal(&state.anchors, &anchors) {
                state.anchors = anchors;
                state.tokens = tokens;
            }
            drop(state);
            emit_status(&rebased_bridge, &agent.agent_id, tokens);
        }),
    )
    .unwrap();
    let turn_state = state;
    let turn_bridge = bridge;
    let agent = base.agent.clone();
    base.on(
        &TokenCountingTurnRecorded::class(),
        Arc::new(move |event| {
            let length = normalize_anchor_length(number(event, "length"));
            let tokens = number(event, "tokens").max(0) as usize;
            let mut state = turn_state.lock();
            if state.anchors.iter().any(|anchor| anchor.length == length) {
                state.tokens = tokens;
            } else {
                state.anchors = state
                    .anchors
                    .iter()
                    .filter(|anchor| anchor.length < length)
                    .copied()
                    .chain(std::iter::once(TokenAnchor {
                        length,
                        tokens,
                        measured: false,
                    }))
                    .collect();
                state.tokens = tokens;
            }
            drop(state);
            emit_status(&turn_bridge, &agent.agent_id, tokens);
        }),
    )
    .unwrap();
}

fn emit_status(
    bridge: &Arc<dyn crate::state::agent_model::AgentModelBridge>,
    agent_id: &str,
    tokens: usize,
) {
    let bridge = bridge.clone();
    let event: Event2 = AgentStatusUpdated::new(AgentStatusUpdatedPayload {
        agent_id: agent_id.to_string(),
        context_tokens: Some(tokens as u64),
    })
    .into();
    tokio::spawn(async move {
        let _ = bridge.dispatch(event).await;
    });
}

fn number(event: &Event2, key: &str) -> i64 {
    event.payload.get(key).and_then(Value::as_i64).unwrap_or(0)
}

pub static TOKEN_COUNTING_AGENT_MODEL_DEFINITION: LazyLock<Arc<AgentModelDefinition>> =
    LazyLock::new(|| {
        define_agent_model(AgentModelDefinitionInput {
            id: "tokenCounting".to_string(),
            model: Arc::new(|context| TokenCountingAgentModel::new(context).into_base()),
            state: AgentModelStateSpec {
                initial: Arc::new(|| json!({ "anchors": [], "tokens": 0 })),
                schema: json!({"type": "object"}),
            },
            events: vec![
                TokenCountingMeasured::class(),
                TokenCountingTruncated::class(),
                TokenCountingRebased::class(),
                TokenCountingTurnRecorded::class(),
            ],
            undoable: None,
        })
        .expect("tokenCounting agent model registration must succeed")
    });

pub fn register_token_counting_agent_model() -> Result<(), StateError> {
    LazyLock::force(&TOKEN_COUNTING_AGENT_MODEL_DEFINITION);
    Ok(())
}

fn slice_messages(context: &[ContextMessage], start: usize, end: usize) -> &[ContextMessage] {
    if start >= end {
        return &[];
    }
    &context[start..end]
}

fn matches_context(input: &[Message], context: &[ContextMessage]) -> bool {
    input.len() == context.len()
        && input.iter().zip(context).all(|(input, context)| {
            std::ptr::eq(
                input as *const Message as *const (),
                context as *const ContextMessage as *const (),
            )
        })
}

fn token_usage_total(usage: &TokenUsage) -> usize {
    (usage.input_cache_read
        + usage.input_cache_creation
        + usage.input_other
        + usage.output) as usize
}

fn normalize_slice_index(index: isize, length: usize) -> usize {
    if index < 0 {
        return (length as isize + index).max(0) as usize;
    }
    (index as usize).min(length)
}
