use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::agent::llm_requester::llm_requester::AgentLLMRequestSource;
use crate::agent::usage::usage::UsageStatus;
use crate::agent::usage::usage_events::{AgentStatusUpdated, AgentStatusUpdatedPayload};
use crate::agent::usage::usage_ops::{UsageModelState, UsageRecord, copy_usage};
use crate::human::llm::usage::{TokenUsage, add_usage};
use crate::state::agent_model::{
    AgentModel, AgentModelContext, AgentModelDefinition, AgentModelDefinitionInput,
    AgentModelStateSpec, define_agent_model,
};
use crate::wire::record::WireAsyncError;

#[derive(Clone)]
pub struct UsageRecordInput {
    pub model: String,
    pub usage: TokenUsage,
    pub source: Option<AgentLLMRequestSource>,
}

pub struct UsageAgentModel {
    base: AgentModel,
    state: Arc<Mutex<UsageModelState>>,
    current_turn_id: Option<u64>,
    current_turn: Option<TokenUsage>,
}

impl UsageAgentModel {
    pub fn new(context: AgentModelContext) -> Self {
        let state = Arc::new(Mutex::new(
            serde_json::from_value::<UsageModelState>(context.bridge.initial_state())
                .unwrap_or_default(),
        ));
        let mut base = AgentModel::new(context);
        let applier_state = state.clone();
        base.on(
            &UsageRecord::class(),
            Arc::new(move |event| {
                let Some(model) = event.payload.get("model").and_then(Value::as_str) else {
                    return;
                };
                let Ok(usage) = serde_json::from_value::<TokenUsage>(
                    event.payload.get("usage").cloned().unwrap_or(Value::Null),
                ) else {
                    return;
                };
                let mut state = applier_state.lock();
                let current = state.by_model.get(model).cloned();
                state.by_model.insert(
                    model.to_string(),
                    match current {
                        Some(current) => add_usage(&current, &usage),
                        None => copy_usage(&usage),
                    },
                );
            }),
        )
        .unwrap();
        Self {
            base,
            state,
            current_turn_id: None,
            current_turn: None,
        }
    }

    pub fn into_base(self) -> AgentModel {
        self.base
    }

    pub async fn record(&mut self, input: UsageRecordInput) -> Result<bool, WireAsyncError> {
        let first_record = self.state.lock().by_model.is_empty();
        let source = input
            .source
            .as_ref()
            .and_then(|source| serde_json::to_value(source).ok())
            .unwrap_or(Value::Null);
        let is_turn = source.get("type").and_then(Value::as_str) == Some("turn");
        let turn_id = is_turn
            .then(|| source.get("turnId").and_then(Value::as_u64))
            .flatten();
        self.base
            .emit(
                UsageRecord::new(json!({
                    "agentId": self.base.agent.agent_id,
                    "model": input.model,
                    "usage": input.usage,
                    "usageScope": if is_turn { "turn" } else { "session" },
                }))
                .into(),
            )
            .await?;
        if let Some(turn_id) = turn_id {
            if self.current_turn_id != Some(turn_id) {
                self.current_turn_id = Some(turn_id);
                self.current_turn = Some(copy_usage(&input.usage));
            } else {
                self.current_turn = Some(match self.current_turn.take() {
                    Some(current) => add_usage(&current, &input.usage),
                    None => copy_usage(&input.usage),
                });
            }
        }
        let status = self.status();
        self.base
            .emit(
                AgentStatusUpdated::new(AgentStatusUpdatedPayload {
                    agent_id: self.base.agent.agent_id.clone(),
                    context_tokens: None,
                    usage: Some(status),
                })
                .into(),
            )
            .await?;
        Ok(first_record)
    }

    pub fn status(&self) -> UsageStatus {
        let by_model = self
            .state
            .lock()
            .by_model
            .iter()
            .map(|(model, usage)| (model.clone(), copy_usage(usage)))
            .collect::<HashMap<_, _>>();
        let has_by_model = !by_model.is_empty();
        let total = if has_by_model {
            by_model.values().fold(None, |total, usage| {
                Some(match total {
                    Some(total) => add_usage(&total, usage),
                    None => copy_usage(usage),
                })
            })
        } else {
            None
        };
        UsageStatus {
            by_model: has_by_model.then_some(by_model),
            total,
            current_turn: self.current_turn.as_ref().map(copy_usage),
        }
    }
}

pub static USAGE_AGENT_MODEL_DEFINITION: LazyLock<Arc<AgentModelDefinition>> = LazyLock::new(|| {
    define_agent_model(AgentModelDefinitionInput {
        id: "usage".to_string(),
        model: Arc::new(|context| UsageAgentModel::new(context).into_base()),
        state: AgentModelStateSpec {
            initial: Arc::new(|| json!({ "byModel": {} })),
            schema: json!({"type": "object"}),
        },
        events: vec![UsageRecord::class()],
        undoable: None,
    })
    .expect("usage agent model registration must succeed")
});
