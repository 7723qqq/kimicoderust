/// Aggregate activation for Agent-scope DI descriptors.
///
/// Each agent service registers itself lazily behind its own
/// `ensure_*_registered()` entry; this aggregate forces all of them before an
/// Agent scope is created so `provide_scope_services` can see the descriptors.
pub fn ensure_agent_services_registered() -> Result<(), String> {
    crate::agent::state::agent_state_service::ensure_agent_state_service_registered()?;
    crate::agent::permission_rules::permission_rules_service::ensure_agent_permission_rules_service_registered()?;
    crate::agent::context_projector::context_projector_service::ensure_agent_context_projector_service_registered()?;
    crate::agent::context_memory::context_memory_service::ensure_agent_context_memory_service_registered()?;
    crate::agent::permission_gate::permission_gate_service::ensure_agent_permission_gate_registered()?;
    crate::agent::tool_dedupe::tool_dedupe_service::ensure_agent_tool_dedupe_service_registered()?;
    crate::agent::tool_result_truncation::tool_result_truncation_service::ensure_agent_tool_result_truncation_service_registered()?;
    crate::agent::context_memory::conversation_undo_participants::ensure_agent_conversation_undo_participant_registry_registered()?;
    crate::agent::tool_registry::tool_registry_service::ensure_agent_tool_registry_service_registered()?;
    crate::workspace::sandbox::sandbox_policy_service::ensure_sandbox_policy_service_registered()?;
    Ok(())
}
