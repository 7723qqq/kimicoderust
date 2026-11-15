use std::sync::Arc;

use once_cell::sync::Lazy;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::agent::agent_context::agent_context::AgentContext;
use crate::agent::full_compaction::compaction_ops::{
    CompactionCancelled, CompactionCompleted, CompactionStarted,
};
use crate::agent::r#loop::turn_events::{
    AssistantDelta, TurnStarted, TurnStepRetrying, TurnStepStarted,
};
use crate::agent::r#loop::turn_ops::TurnEnded;
use crate::agent::task::task_ops::{TaskStarted, TaskTerminatedNotice};
use crate::agent::undo::undo_service::ContextUndone;
use crate::features::swarm::session::session_swarm_service::SubagentSuspended;
use crate::session::subagent::mirror_agent_run::{
    SubagentCancelled, SubagentCompleted, SubagentFailed, SubagentStarted,
};

use super::event2::{Event2, Event2Class};

pub type Event2Handler = Arc<dyn Fn(Event2) + Send + Sync>;

fn subscribe_typed<B, E>(
    bus: &B,
    cls: Arc<Event2Class>,
    decode: fn(Event2) -> Option<E>,
    handler: Arc<dyn Fn(E) + Send + Sync>,
) -> DisposableRef
where
    B: IEventBus + ?Sized,
    E: 'static,
{
    bus.subscribe_class(
        cls,
        Arc::new(move |event| {
            if let Some(typed) = decode(event) {
                handler(typed);
            }
        }),
    )
}

pub trait IEventBus: Send + Sync {
    fn publish(&self, event: Event2, agent: Option<Arc<AgentContext>>);
    fn subscribe(&self, handler: Event2Handler) -> DisposableRef;
    fn subscribe_class(
        &self,
        cls: Arc<Event2Class>,
        handler: Event2Handler,
    ) -> DisposableRef;
    fn subscribe_type(&self, type_: &str, handler: Event2Handler) -> DisposableRef;

    fn subscribe_turn_started(
        &self,
        handler: Arc<dyn Fn(TurnStarted) + Send + Sync>,
    ) -> DisposableRef {
        subscribe_typed(self, TurnStarted::class(), TurnStarted::from_event, handler)
    }

    fn subscribe_turn_ended(
        &self,
        handler: Arc<dyn Fn(TurnEnded) + Send + Sync>,
    ) -> DisposableRef {
        subscribe_typed(self, TurnEnded::class(), TurnEnded::from_event, handler)
    }

    fn subscribe_turn_step_started(
        &self,
        handler: Arc<dyn Fn(TurnStepStarted) + Send + Sync>,
    ) -> DisposableRef {
        subscribe_typed(
            self,
            TurnStepStarted::class(),
            TurnStepStarted::from_event,
            handler,
        )
    }

    fn subscribe_turn_step_retrying(
        &self,
        handler: Arc<dyn Fn(TurnStepRetrying) + Send + Sync>,
    ) -> DisposableRef {
        subscribe_typed(
            self,
            TurnStepRetrying::class(),
            TurnStepRetrying::from_event,
            handler,
        )
    }

    fn subscribe_assistant_delta(
        &self,
        handler: Arc<dyn Fn(AssistantDelta) + Send + Sync>,
    ) -> DisposableRef {
        subscribe_typed(
            self,
            AssistantDelta::class(),
            AssistantDelta::from_event,
            handler,
        )
    }

    fn subscribe_compaction_started(
        &self,
        handler: Arc<dyn Fn(CompactionStarted) + Send + Sync>,
    ) -> DisposableRef {
        subscribe_typed(
            self,
            CompactionStarted::class(),
            CompactionStarted::from_event,
            handler,
        )
    }

    fn subscribe_compaction_completed(
        &self,
        handler: Arc<dyn Fn(CompactionCompleted) + Send + Sync>,
    ) -> DisposableRef {
        subscribe_typed(
            self,
            CompactionCompleted::class(),
            CompactionCompleted::from_event,
            handler,
        )
    }

    fn subscribe_compaction_cancelled(
        &self,
        handler: Arc<dyn Fn(CompactionCancelled) + Send + Sync>,
    ) -> DisposableRef {
        subscribe_typed(
            self,
            CompactionCancelled::class(),
            CompactionCancelled::from_event,
            handler,
        )
    }

    fn subscribe_task_started(
        &self,
        handler: Arc<dyn Fn(TaskStarted) + Send + Sync>,
    ) -> DisposableRef {
        subscribe_typed(self, TaskStarted::class(), TaskStarted::from_event, handler)
    }

    fn subscribe_task_terminated_notice(
        &self,
        handler: Arc<dyn Fn(TaskTerminatedNotice) + Send + Sync>,
    ) -> DisposableRef {
        subscribe_typed(
            self,
            TaskTerminatedNotice::class(),
            TaskTerminatedNotice::from_event,
            handler,
        )
    }

    fn subscribe_context_undone(
        &self,
        handler: Arc<dyn Fn(ContextUndone) + Send + Sync>,
    ) -> DisposableRef {
        subscribe_typed(
            self,
            ContextUndone::class(),
            ContextUndone::from_event,
            handler,
        )
    }

    fn subscribe_subagent_started(
        &self,
        handler: Arc<dyn Fn(SubagentStarted) + Send + Sync>,
    ) -> DisposableRef {
        subscribe_typed(
            self,
            SubagentStarted::class(),
            SubagentStarted::from_event,
            handler,
        )
    }

    fn subscribe_subagent_completed(
        &self,
        handler: Arc<dyn Fn(SubagentCompleted) + Send + Sync>,
    ) -> DisposableRef {
        subscribe_typed(
            self,
            SubagentCompleted::class(),
            SubagentCompleted::from_event,
            handler,
        )
    }

    fn subscribe_subagent_failed(
        &self,
        handler: Arc<dyn Fn(SubagentFailed) + Send + Sync>,
    ) -> DisposableRef {
        subscribe_typed(
            self,
            SubagentFailed::class(),
            SubagentFailed::from_event,
            handler,
        )
    }

    fn subscribe_subagent_cancelled(
        &self,
        handler: Arc<dyn Fn(SubagentCancelled) + Send + Sync>,
    ) -> DisposableRef {
        subscribe_typed(
            self,
            SubagentCancelled::class(),
            SubagentCancelled::from_event,
            handler,
        )
    }

    fn subscribe_subagent_suspended(
        &self,
        handler: Arc<dyn Fn(SubagentSuspended) + Send + Sync>,
    ) -> DisposableRef {
        subscribe_typed(
            self,
            SubagentSuspended::class(),
            SubagentSuspended::from_event,
            handler,
        )
    }
}

pub static I_EVENT_BUS: Lazy<ServiceIdentifier<Arc<dyn IEventBus>>> =
    Lazy::new(|| create_decorator("eventBus"));

pub trait ISessionEventBus: IEventBus {
    fn activate_agent(&self, agent: Arc<AgentContext>);
    fn deactivate_agent(&self, agent: Arc<AgentContext>);
    fn is_agent_active(&self, agent: &Arc<AgentContext>) -> bool;
    fn source_of(&self, event: &Event2) -> Option<Arc<AgentContext>>;
    fn subscribe_agent(
        &self,
        agent: Arc<AgentContext>,
        handler: Event2Handler,
    ) -> DisposableRef;
    fn subscribe_agent_type(
        &self,
        agent: Arc<AgentContext>,
        type_: &str,
        handler: Event2Handler,
    ) -> DisposableRef;
    fn on_agent_class(
        &self,
        agent: Arc<AgentContext>,
        cls: Arc<Event2Class>,
        handler: Event2Handler,
    ) -> DisposableRef;
    fn on_agent_type(
        &self,
        agent: Arc<AgentContext>,
        type_: &str,
        handler: Event2Handler,
    ) -> DisposableRef;
}

pub static I_SESSION_EVENT_BUS: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("sessionEventBus"));
