use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use serde_json::{Value, json};

use crate::_base::di::lifecycle::DisposableRef;
use crate::_base::log::log::ILogService;
use crate::agent::context_memory::types::ContextMessage;
use crate::agent::state::agent_state_v2::IAgentStateService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::llm_adapter::contract::message::Message;
use crate::state::state::{ReplayableOptions, ReplayableStateKey};
use crate::wire::record::WireAsyncError;

use super::context_projector::{
    IAgentContextProjectorService, MEDIA_DEGRADE_KEEP_RECENT, MediaEvictionPlan, MediaProjection,
    MediaStripSnapshot, ProjectionPolicy, ProjectionStructure,
};
use super::media_projection::{
    capture_media_strip_snapshot, count_media_parts, degrade_leading_media_parts,
    strip_media_parts_by_snapshot,
};
use super::projection::{
    ProjectionAnomaly, project, project_strict, summarize_projection_repairs,
};
use super::request_size::estimate_llm_request_bytes;

pub static CONTEXT_PROJECTOR_LAST_REPAIR_SIGNATURE_KEY: LazyLock<Arc<ReplayableStateKey>> =
    LazyLock::new(|| {
        Arc::new(ReplayableStateKey::new(
            "contextProjector.lastRepairSignature".to_string(),
            Arc::new(|| Value::Null),
            ReplayableOptions {
                schema: json!({}),
                durable: Some(false),
                blobs: None,
                mutable_replay: None,
            },
        ))
    });

pub fn context_projector_last_repair_signature_key() -> Arc<ReplayableStateKey> {
    CONTEXT_PROJECTOR_LAST_REPAIR_SIGNATURE_KEY.clone()
}

pub struct AgentContextProjectorService {
    log: Arc<dyn ILogService>,
    telemetry: Arc<dyn ITelemetryService>,
    states: Arc<dyn IAgentStateService>,
    _registration: DisposableRef,
}

impl AgentContextProjectorService {
    pub fn new(
        log: Arc<dyn ILogService>,
        telemetry: Arc<dyn ITelemetryService>,
        states: Arc<dyn IAgentStateService>,
    ) -> Self {
        let registration = states
            .contribute_state(context_projector_last_repair_signature_key().state_key())
            .unwrap_or_else(|error| panic!("{error}"));
        Self {
            log,
            telemetry,
            states,
            _registration: registration,
        }
    }

    fn last_repair_signature(&self) -> Option<String> {
        match self
            .states
            .get(&context_projector_last_repair_signature_key().state_key())
        {
            Value::String(signature) => Some(signature),
            _ => None,
        }
    }

    fn set_last_repair_signature(&self, value: Option<String>) {
        self.states.set(
            &context_projector_last_repair_signature_key().state_key(),
            value.map_or(Value::Null, Value::String),
        );
    }

    fn project_with_trace(
        &self,
        messages: &[ContextMessage],
        strict: bool,
    ) -> Result<Vec<Message>, WireAsyncError> {
        let mut anomalies = Vec::new();
        let result = {
            let mut on_anomaly = |anomaly| anomalies.push(anomaly);
            if strict {
                project_strict(messages, Some(&mut on_anomaly))?
            } else {
                project(messages, Some(&mut on_anomaly))?
            }
        };
        self.report_projection_repairs(anomalies);
        Ok(result)
    }

    fn report_projection_repairs(&self, anomalies: Vec<ProjectionAnomaly>) {
        let notable = anomalies
            .into_iter()
            .filter(|anomaly| !anomaly.trailing())
            .collect::<Vec<_>>();
        if notable.is_empty() {
            self.set_last_repair_signature(None);
            return;
        }
        let mut signature_parts = notable
            .iter()
            .map(|anomaly| match anomaly.tool_call_id() {
                Some(tool_call_id) => format!("{}:{tool_call_id}", anomaly.kind()),
                None => anomaly.kind().to_string(),
            })
            .collect::<Vec<_>>();
        signature_parts.sort_by_key(|part| part.encode_utf16().collect::<Vec<_>>());
        let signature = signature_parts.join("|");
        if self.last_repair_signature().as_deref() == Some(signature.as_str()) {
            return;
        }
        self.set_last_repair_signature(Some(signature));

        let summary = summarize_projection_repairs(&notable);
        let mut seen = HashSet::new();
        let mut tool_call_ids = Vec::new();
        for tool_call_id in notable.iter().filter_map(ProjectionAnomaly::tool_call_id) {
            if seen.insert(tool_call_id.to_string()) {
                tool_call_ids.push(tool_call_id.to_string());
            }
            if tool_call_ids.len() == 5 {
                break;
            }
        }
        self.log.warn(
            "repaired the request to keep it wire-valid",
            Some(json!({
                "reordered": summary.reordered,
                "synthesized": summary.synthesized,
                "droppedOrphan": summary.dropped_orphan,
                "duplicateCallsDropped": summary.duplicate_calls_dropped,
                "duplicateResultsDropped": summary.duplicate_results_dropped,
                "leadingDropped": summary.leading_dropped,
                "assistantsMerged": summary.assistants_merged,
                "whitespaceDropped": summary.whitespace_dropped,
                "vacuousDropped": summary.vacuous_dropped,
                "toolCallIds": tool_call_ids,
            })),
        );
        let properties = json!({
            "reordered": summary.reordered,
            "synthesized": summary.synthesized,
            "dropped_orphan": summary.dropped_orphan,
            "duplicate_calls_dropped": summary.duplicate_calls_dropped,
            "duplicate_results_dropped": summary.duplicate_results_dropped,
            "leading_dropped": summary.leading_dropped,
            "assistants_merged": summary.assistants_merged,
            "whitespace_dropped": summary.whitespace_dropped,
            "vacuous_dropped": summary.vacuous_dropped,
        });
        self.telemetry.track2(
            "context_projection_repaired",
            properties.as_object().cloned(),
        );
    }
}

impl IAgentContextProjectorService for AgentContextProjectorService {
    fn project(
        &self,
        messages: &[ContextMessage],
        policy: Option<&ProjectionPolicy>,
        media_paths: Option<&HashMap<String, String>>,
    ) -> Result<Vec<Message>, WireAsyncError> {
        let default_policy = ProjectionPolicy::default();
        let policy = policy.unwrap_or(&default_policy);
        let projected = self.project_with_trace(
            messages,
            policy.structure == Some(ProjectionStructure::Strict),
        )?;
        match &policy.media {
            None => Ok(projected),
            Some(MediaProjection::DegradeCount(degrade_count)) => Ok(
                degrade_leading_media_parts(
                    &projected,
                    *degrade_count,
                    None,
                    media_paths,
                )
                .into_owned(),
            ),
            Some(MediaProjection::Strip(snapshot)) => Ok(
                strip_media_parts_by_snapshot(&projected, snapshot, media_paths).into_owned(),
            ),
        }
    }

    fn capture_media_strip_snapshot(
        &self,
        messages: &[ContextMessage],
    ) -> Result<MediaStripSnapshot, WireAsyncError> {
        let projected = self.project_with_trace(messages, false)?;
        Ok(capture_media_strip_snapshot(&projected))
    }

    fn capture_media_degrade_count(
        &self,
        messages: &[ContextMessage],
    ) -> Result<usize, WireAsyncError> {
        let projected = self.project_with_trace(messages, false)?;
        let media_count = count_media_parts(&projected);
        Ok(media_count.saturating_sub(MEDIA_DEGRADE_KEEP_RECENT))
    }

    fn plan_media_eviction(
        &self,
        messages: &[Message],
        estimated_bytes: u64,
        budget_bytes: u64,
    ) -> Option<MediaEvictionPlan> {
        if estimated_bytes <= budget_bytes {
            return None;
        }
        let original_media_count = count_media_parts(messages);
        let degrade_count = original_media_count.saturating_sub(MEDIA_DEGRADE_KEEP_RECENT);
        let degraded = degrade_leading_media_parts(messages, degrade_count, None, None);
        let degraded_bytes = estimate_llm_request_bytes(degraded.as_ref());
        if degraded_bytes <= budget_bytes {
            return Some(MediaEvictionPlan::MediaDegraded {
                degrade_count,
                estimated_bytes: degraded_bytes,
                evicted_media_count: original_media_count
                    - count_media_parts(degraded.as_ref()),
            });
        }
        let snapshot = capture_media_strip_snapshot(messages);
        let stripped = strip_media_parts_by_snapshot(messages, &snapshot, None);
        Some(MediaEvictionPlan::MediaStripped {
            estimated_bytes: estimate_llm_request_bytes(stripped.as_ref()),
            evicted_media_count: original_media_count - count_media_parts(stripped.as_ref()),
        })
    }
}
