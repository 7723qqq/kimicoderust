use std::sync::Arc;

use crate::agent::llm_requester::llm_requester::{
    AgentLLMRequestSource, AgentLLMTurnRequestSnapshot,
};
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::llm_adapter::model::catalog::IModelCatalog;
use crate::session::cache_observation::{CacheProbeObservation, CacheProbeSource};

use super::cache_probe::IAgentCacheProbeService;
use super::usage::UsageRecordedContext;

pub struct AgentCacheProbeService {
    parent_id: Option<String>,
    parent_request: Option<AgentLLMTurnRequestSnapshot>,
    parent_revision: Arc<dyn Fn() -> Option<u64> + Send + Sync>,
    own_request: Arc<dyn Fn() -> Option<AgentLLMTurnRequestSnapshot> + Send + Sync>,
    telemetry: Arc<dyn ITelemetryService>,
    models: Arc<dyn IModelCatalog>,
}

impl AgentCacheProbeService {
    pub fn new(
        parent_id: Option<String>,
        parent_request: Option<AgentLLMTurnRequestSnapshot>,
        parent_revision: Arc<dyn Fn() -> Option<u64> + Send + Sync>,
        own_request: Arc<dyn Fn() -> Option<AgentLLMTurnRequestSnapshot> + Send + Sync>,
        telemetry: Arc<dyn ITelemetryService>,
        models: Arc<dyn IModelCatalog>,
    ) -> Self {
        Self {
            parent_id,
            parent_request,
            parent_revision,
            own_request,
            telemetry,
            models,
        }
    }

    pub fn probe(&self, event: &UsageRecordedContext) {
        if !event.first_record {
            return;
        }
        let Some(AgentLLMRequestSource::Turn { turn_id, .. }) = &event.source else {
            return;
        };
        let parent_revision = self.parent_id.as_ref().map(|_| (self.parent_revision)());
        let parent = self.parent_id.as_ref().and_then(|_| {
            self.parent_request.as_ref().filter(|request| {
                Some(request.context_revision) == parent_revision
            })
        });
        let (provider_type, protocol) = match self.models.get(&event.model) {
            Ok(model) => (
                Some(
                    model
                        .provider_type
                        .unwrap_or_else(|| model.protocol.to_string()),
                ),
                Some(model.protocol.to_string()),
            ),
            Err(_) => (None, None),
        };
        let own = (self.own_request)();
        let same_model = parent.is_some_and(|parent| parent.model_alias == event.model);
        let compared = same_model && own.is_some();
        let observation = CacheProbeObservation {
            source: CacheProbeSource::Fork,
            turn_id: Some(turn_id.to_string()),
            provider_type,
            protocol,
            input_tokens: crate::human::llm::usage::input_total(&event.usage),
            input_cache_read: event.usage.input_cache_read,
            input_cache_creation: event.usage.input_cache_creation,
            output_tokens: event.usage.output,
            model_match: None,
            tools_match: compared.then(|| {
                own.as_ref().unwrap().tools_hash == parent.as_ref().unwrap().tools_hash
            }),
            system_prompt_match: compared.then(|| {
                own.as_ref().unwrap().system_prompt_hash
                    == parent.as_ref().unwrap().system_prompt_hash
            }),
            history_prefix_match: None,
            parent_input_tokens: same_model
                .then(|| parent.and_then(|parent| parent.input_tokens))
                .flatten(),
            parent_request_gap_ms: compared.then(|| {
                own.as_ref().unwrap().started_at - parent.as_ref().unwrap().finished_at
            }),
        };
        let payload = observation.telemetry_payload();
        let properties = payload.as_object().cloned().map(|mut properties| {
            properties.remove("type");
            properties.insert("turn_id".to_string(), serde_json::Value::from(*turn_id));
            properties
        });
        self.telemetry.track2("prompt_cache_probe", properties);
    }
}

impl IAgentCacheProbeService for AgentCacheProbeService {}
