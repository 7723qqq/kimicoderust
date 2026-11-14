use std::sync::Arc;

use crate::_base::di::lifecycle::DisposableRef;
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::agent::state::agent_state_v2::IAgentStateService;
use crate::app::event::event2::Event2;
use crate::state::event_dispatcher::IEventDispatcher;

use super::permission_rules::{
    IAgentPermissionRulesService, PermissionApprovalResultRecord, PermissionRule,
};
use super::permission_rules_ops::{
    PermissionRecordApprovalResult, PermissionRulesAdd, PermissionRulesModelState,
    permission_rules_replayable_key,
};

pub struct AgentPermissionRulesService {
    dispatcher: Arc<dyn IEventDispatcher>,
    scope_context: Arc<dyn IAgentScopeContext>,
    agent_state: Arc<dyn IAgentStateService>,
    _registration: DisposableRef,
}

impl AgentPermissionRulesService {
    pub fn new(
        dispatcher: Arc<dyn IEventDispatcher>,
        scope_context: Arc<dyn IAgentScopeContext>,
        agent_state: Arc<dyn IAgentStateService>,
    ) -> Self {
        let registration = agent_state
            .contribute_state(permission_rules_replayable_key().state_key())
            .unwrap_or_else(|error| panic!("{error}"));
        Self {
            dispatcher,
            scope_context,
            agent_state,
            _registration: registration,
        }
    }

    fn state(&self) -> PermissionRulesModelState {
        serde_json::from_value(
            self.agent_state
                .get(&permission_rules_replayable_key().state_key()),
        )
        .unwrap_or_default()
    }

    fn dispatch(&self, event: impl Into<Event2>) {
        let dispatcher = self.dispatcher.clone();
        let event = event.into();
        tokio::spawn(async move {
            let _ = dispatcher.dispatch(event).await;
        });
    }
}

impl IAgentPermissionRulesService for AgentPermissionRulesService {
    fn rules(&self) -> Vec<PermissionRule> {
        self.state().rules
    }

    fn session_approval_rule_patterns(&self) -> Vec<String> {
        self.state().session_approval_rule_patterns
    }

    fn add_rules(&self, rules: &[PermissionRule]) {
        if rules.is_empty() {
            return;
        }
        self.dispatch(PermissionRulesAdd {
            agent_id: self.scope_context.agent_id(),
            rules: rules.to_vec(),
        });
    }

    fn record_approval_result(&self, record: PermissionApprovalResultRecord) {
        self.dispatch(PermissionRecordApprovalResult {
            agent_id: self.scope_context.agent_id(),
            turn_id: record.turn_id,
            tool_call_id: record.tool_call_id,
            tool_name: record.tool_name,
            action: record.action,
            session_approval_rule: record.session_approval_rule,
            result: record.result,
        });
    }
}
