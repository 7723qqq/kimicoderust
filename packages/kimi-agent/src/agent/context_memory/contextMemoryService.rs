use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt, Shared};
use parking_lot::Mutex;

use crate::_base::di::lifecycle::DisposableStore;
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::agent::state::agent_state_v2::IAgentStateService;
use crate::app::event::event2::Event2;
use crate::session::session_token_counting::session_token_counting::{
    ISessionTokenCountingService, TokenCountingRebaseInput,
};
use crate::state::event_dispatcher::{IEventDispatcher, RestorePhase};

use super::compaction_handoff::{TokenEstimate, build_context_compaction_shape};
use super::context_events::{
    ContextAppendLoopEvent, ContextAppendLoopEventPayload, ContextAppendMessage,
    ContextAppendMessagePayload, ContextApplyCompaction, ContextApplyCompactionKind,
    ContextApplyCompactionPayload, ContextClear, ContextClearPayload, ContextSpliced,
    ContextSplicedPayload,
};
use super::context_memory::{
    ContextCompactionInput, ContextCompactionResult, IAgentContextMemoryService,
};
use super::context_ops::context_memory_key;
use super::loop_event_fold::LoopRecordedEvent;
use super::types::ContextMessage;

pub struct AgentContextMemoryService {
    dispatcher: Arc<dyn IEventDispatcher>,
    scope_context: Arc<dyn IAgentScopeContext>,
    token_counting: Arc<dyn ISessionTokenCountingService>,
    agent_state: Arc<dyn IAgentStateService>,
    revision: Arc<AtomicU64>,
    dispatch_tail: Mutex<Shared<BoxFuture<'static, ()>>>,
    _registrations: DisposableStore,
}

impl AgentContextMemoryService {
    pub fn new(
        dispatcher: Arc<dyn IEventDispatcher>,
        scope_context: Arc<dyn IAgentScopeContext>,
        token_counting: Arc<dyn ISessionTokenCountingService>,
        agent_state: Arc<dyn IAgentStateService>,
    ) -> Self {
        let registrations = DisposableStore::new();
        registrations.add(
            agent_state
                .contribute_state(context_memory_key().state_key())
                .unwrap_or_else(|error| panic!("{error}")),
        );
        let revision = Arc::new(AtomicU64::new(0));
        let revision_on_change = revision.clone();
        let dispatcher_on_change = dispatcher.clone();
        let subscription = agent_state
            .on_did_change(&context_memory_key().state_key())
            .subscribe(move |_| {
                if dispatcher_on_change.restore_phase() == RestorePhase::Restoring {
                    revision_on_change.fetch_add(1, Ordering::Relaxed);
                }
            });
        registrations.add(Arc::new(subscription));
        Self {
            dispatcher,
            scope_context,
            token_counting,
            agent_state,
            revision,
            dispatch_tail: Mutex::new(async {}.boxed().shared()),
            _registrations: registrations,
        }
    }

    fn token_estimate_fns(&self) -> TokenEstimate {
        let text_counter = self.token_counting.clone();
        let message_counter = self.token_counting.clone();
        let messages_counter = self.token_counting.clone();
        TokenEstimate {
            text: Arc::new(move |text| text_counter.estimate_text(text) as u64),
            message: Arc::new(move |message| {
                message_counter.estimate_message(&message.clone().into(), None) as u64
            }),
            messages: Arc::new(move |messages| {
                let messages = messages
                    .iter()
                    .cloned()
                    .map(Into::into)
                    .collect::<Vec<_>>();
                messages_counter.estimate_messages(&messages, None) as u64
            }),
        }
    }

    fn dispatch(&self, event: impl Into<Event2>) {
        let dispatcher = self.dispatcher.clone();
        let event = event.into();
        let previous = self.dispatch_tail.lock().clone();
        let run = async move {
            previous.await;
            let _ = dispatcher.dispatch(event).await;
        }
        .boxed()
        .shared();
        *self.dispatch_tail.lock() = run.clone();
        tokio::spawn(run);
    }

    fn publish_splice(
        &self,
        start: usize,
        delete_count: usize,
        messages: Vec<ContextMessage>,
        tokens: Option<u64>,
    ) {
        if delete_count > 0 {
            self.revision.fetch_add(1, Ordering::Relaxed);
        }
        self.dispatch(ContextSpliced::new(ContextSplicedPayload {
            agent_id: self.scope_context.agent_id(),
            start,
            delete_count,
            messages,
            tokens,
        }));
    }

    fn dispatch_cut_events(&self, cut_index: usize) {
        self.token_counting
            .record_truncation(&self.scope_context.agent_context(), cut_index);
    }
}

impl IAgentContextMemoryService for AgentContextMemoryService {
    fn get(&self) -> Vec<ContextMessage> {
        serde_json::from_value(self.agent_state.get(&context_memory_key().state_key()))
            .unwrap_or_else(|error| panic!("{error}"))
    }

    fn prefix_revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }

    fn append(&self, messages: &[ContextMessage]) {
        if messages.is_empty() {
            return;
        }
        let start = self.get().len();
        for message in messages {
            self.dispatch(ContextAppendMessage::new(ContextAppendMessagePayload {
                agent_id: self.scope_context.agent_id(),
                message: message.clone(),
            }));
        }
        self.publish_splice(start, 0, messages.to_vec(), None);
    }

    fn append_loop_event(&self, event: LoopRecordedEvent) {
        self.dispatch(ContextAppendLoopEvent::new(
            ContextAppendLoopEventPayload {
                agent_id: self.scope_context.agent_id(),
                event,
            },
        ));
    }

    fn publish_trailing_removal(&self, previous: &[ContextMessage]) -> bool {
        let Some(cut_index) = previous.len().checked_sub(1) else {
            return false;
        };
        let current = self.get();
        if current.len() != cut_index
            || current
                .iter()
                .zip(previous)
                .any(|(message, previous)| message != previous)
        {
            return false;
        }
        self.dispatch_cut_events(cut_index);
        self.publish_splice(cut_index, 1, Vec::new(), None);
        true
    }

    fn clear(&self) {
        let delete_count = self.get().len();
        if delete_count == 0 {
            return;
        }
        self.dispatch(ContextClear::new(ContextClearPayload {
            agent_id: self.scope_context.agent_id(),
        }));
        self.token_counting.rebase(
            &self.scope_context.agent_context(),
            TokenCountingRebaseInput {
                length: 0,
                tokens: 0,
                measured: true,
            },
        );
        self.publish_splice(0, delete_count, Vec::new(), None);
    }

    fn apply_compaction(&self, input: ContextCompactionInput) -> ContextCompactionResult {
        let history = self.get();
        let estimate = self.token_estimate_fns();
        let shape_input = super::compaction_handoff::ContextCompactionShapeInput {
            summary: input.summary,
            legacy_summary_message: None,
            context_summary: input.context_summary,
            in_flight_turn: input.in_flight_turn,
            compacted_count: input.compacted_count,
            tokens_before: input.tokens_before,
            tokens_after: input.tokens_after,
            summary_output_tokens: input.summary_output_tokens,
            request_overhead_tokens: input.request_overhead_tokens,
            kept_user_message_count: input.kept_user_message_count,
            kept_head_user_message_count: input.kept_head_user_message_count,
            dropped_count: input.dropped_count,
            legacy_tail: None,
        };
        let summary_output_tokens = shape_input.summary_output_tokens;
        let wire_lines = input.wire_lines;
        let result = build_context_compaction_shape(&history, shape_input, &estimate);
        self.dispatch(ContextApplyCompaction::new(
            ContextApplyCompactionPayload {
                agent_id: self.scope_context.agent_id(),
                compaction: ContextApplyCompactionKind::ContextSummary {
                    context_summary: result.context_summary.clone(),
                    compacted_count: result.compacted_count,
                    summary: Some(result.summary.clone()),
                },
                tokens_before: Some(result.tokens_before),
                tokens_after: Some(result.tokens_after),
                summary_output_tokens,
                kept_user_message_count: Some(result.kept_user_message_count),
                kept_head_user_message_count: result.kept_head_user_message_count,
                dropped_count: result.dropped_count,
                legacy_tail: None,
                wire_lines: wire_lines.map(|range| {
                    super::context_events::ContextCompactionWireLines {
                        start: range.start as usize,
                        end: range.end as usize,
                    }
                }),
            },
        ));
        self.token_counting.rebase(
            &self.scope_context.agent_context(),
            TokenCountingRebaseInput {
                length: result.messages.len(),
                tokens: result.tokens_after as usize,
                measured: false,
            },
        );
        self.publish_splice(
            0,
            history.len(),
            result.messages.to_vec(),
            Some(result.tokens_after),
        );
        ContextCompactionResult {
            summary: result.summary,
            context_summary: result.context_summary,
            compacted_count: result.compacted_count,
            tokens_before: result.tokens_before,
            tokens_after: result.tokens_after,
            kept_user_message_count: result.kept_user_message_count,
            kept_head_user_message_count: result.kept_head_user_message_count,
            dropped_count: result.dropped_count,
        }
    }
}
