use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable, to_disposable};
use crate::agent::agent_context::agent_context::AgentContext;
use crate::agent::agent_context::agent_space::agent_space_or_bug;
use crate::agent::llm_requester::llm_requester::AgentLLMRequestSource;
use crate::agent::usage::usage::{UsageRecordedContext, UsageStatus};
use crate::agent::usage::usage_ops::copy_usage;
use crate::human::llm::usage::TokenUsage;
use crate::wire::record::WireAsyncError;

use super::session_usage::ISessionUsageService;
use super::usage_agent_model::{USAGE_AGENT_MODEL_DEFINITION, UsageAgentModel, UsageRecordInput};

pub struct SessionUsageService {
    listeners: Arc<Mutex<Vec<(u64, Arc<dyn Fn(UsageRecordedContext) + Send + Sync>)>>>,
    next_listener_id: std::sync::atomic::AtomicU64,
}

impl SessionUsageService {
    pub fn new() -> Self {
        Self {
            listeners: Arc::new(Mutex::new(Vec::new())),
            next_listener_id: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl Default for SessionUsageService {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ISessionUsageService for SessionUsageService {
    async fn record(
        &self,
        agent: Arc<AgentContext>,
        model: String,
        usage: TokenUsage,
        source: Option<AgentLLMRequestSource>,
    ) -> Result<(), WireAsyncError> {
        let record_model = model.clone();
        let record_usage = usage.clone();
        let record_source = source.clone();
        let first_record = agent_space_or_bug(&agent)
            .use_async(&USAGE_AGENT_MODEL_DEFINITION, async move |model: &UsageAgentModel| {
                let model_input = UsageRecordInput {
                    model: record_model,
                    usage: record_usage,
                    source: record_source,
                };
                model.record(model_input).await
            })
            .await??;
        let listeners = self.listeners.lock().clone();
        for (_, listener) in listeners {
            listener(UsageRecordedContext {
                agent: (*agent).clone(),
                model: model.clone(),
                usage: copy_usage(&usage),
                source: source.clone(),
                first_record,
            });
        }
        Ok(())
    }

    fn status(&self, agent: &AgentContext) -> UsageStatus {
        agent_space_or_bug(agent).use_(&USAGE_AGENT_MODEL_DEFINITION, |model: &UsageAgentModel| model.status())
    }

    fn on_did_record(
        &self,
        listener: Arc<dyn Fn(UsageRecordedContext) + Send + Sync>,
    ) -> DisposableRef {
        let id = self.next_listener_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.listeners.lock().push((id, listener));
        let listeners = Arc::downgrade(&self.listeners);
        to_disposable(move || {
            if let Some(listeners) = listeners.upgrade() {
                listeners.lock().retain(|(candidate, _)| *candidate != id);
            }
        })
    }
}
