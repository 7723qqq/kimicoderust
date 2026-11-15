use std::sync::Arc;

use once_cell::sync::Lazy;
use futures_util::future::{BoxFuture, FutureExt};
use serde_json::{Map, Value};

use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::instantiation::{
    DependencyKind, ServiceDependency, refine_service_decorator,
};
use crate::_base::di::instantiation_service::register_descriptor_dependencies;
use crate::_base::di::lifecycle::DisposableStore;
use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::app::scopes::LifecycleScope;
use crate::agent::permission_mode::permission_mode::{
    I_AGENT_PERMISSION_MODE_SERVICE, IAgentPermissionModeService,
};
use crate::agent::permission_policy::permission_policy::{
    I_AGENT_PERMISSION_POLICY_SERVICE, IAgentPermissionPolicyService, PermissionPolicyEvaluation,
};
use crate::agent::permission_policy::types::{
    PermissionData, PermissionDecisionReason, PermissionPolicyResult,
};
use crate::agent::permission_rules::permission_rules::{
    I_AGENT_PERMISSION_RULES_SERVICE, IAgentPermissionRulesService,
};
use crate::agent::tool_approval::tool_approval::{
    I_AGENT_TOOL_APPROVAL_SERVICE, IAgentToolApprovalService,
};
use crate::agent::tool_executor::tool_executor::{
    I_AGENT_TOOL_EXECUTOR_SERVICE, IAgentToolExecutorService,
};
use crate::agent::tool_executor::tool_hooks::{BeforeExecuteDecision, ResolvedToolExecutionHookContext};
use crate::app::telemetry::telemetry::{I_TELEMETRY_SERVICE, ITelemetryService};
use crate::wire::record::WireAsyncError;

use super::permission_gate::{I_AGENT_PERMISSION_GATE, IAgentPermissionGate};

struct AgentPermissionGateInner {
    mode_service: Arc<dyn IAgentPermissionModeService>,
    rules_service: Arc<dyn IAgentPermissionRulesService>,
    policy_service: Arc<dyn IAgentPermissionPolicyService>,
    tool_approval: Arc<dyn IAgentToolApprovalService>,
    telemetry: Arc<dyn ITelemetryService>,
}

#[derive(Clone)]
pub struct AgentPermissionGate {
    inner: Arc<AgentPermissionGateInner>,
    _registrations: DisposableStore,
}

impl AgentPermissionGate {
    pub fn new(
        mode_service: Arc<dyn IAgentPermissionModeService>,
        rules_service: Arc<dyn IAgentPermissionRulesService>,
        policy_service: Arc<dyn IAgentPermissionPolicyService>,
        tool_approval: Arc<dyn IAgentToolApprovalService>,
        telemetry: Arc<dyn ITelemetryService>,
        tool_executor: Arc<dyn IAgentToolExecutorService>,
    ) -> Self {
        let inner = Arc::new(AgentPermissionGateInner {
            mode_service,
            rules_service,
            policy_service,
            tool_approval,
            telemetry,
        });
        let registrations = DisposableStore::new();
        let subscription = tool_executor.on_before_execute_tool().subscribe({
            let inner = inner.clone();
            move |event| {
                let context = event.context.clone();
                let inner = inner.clone();
                event.wait_until(Arc::new(move || {
                    let inner = inner.clone();
                    let context = context.clone();
                    async move { inner.adjudicate(context).await }.boxed()
                }));
            }
        });
        registrations.add(Arc::new(subscription));
        Self {
            inner,
            _registrations: registrations,
        }
    }

    fn data(&self) -> PermissionData {
        PermissionData {
            mode: self.inner.mode_service.mode(),
            rules: self.inner.rules_service.rules(),
        }
    }
}

impl AgentPermissionGateInner {
    async fn adjudicate(
        &self,
        context: ResolvedToolExecutionHookContext,
    ) -> Result<Option<BeforeExecuteDecision>, WireAsyncError> {
        let Some(evaluation) = self.policy_service.evaluate(context.clone()).await? else {
            return Ok(None);
        };
        self.track_evaluation(&context, &evaluation);
        let policy_name = evaluation.policy_name;
        match evaluation.result {
            result @ PermissionPolicyResult::Ask { .. } => {
                self.tool_approval
                    .request_tool_approval(context, result, policy_name)
                    .await
            }
            PermissionPolicyResult::Approve {
                execution_metadata,
                ..
            } => Ok(Some(BeforeExecuteDecision {
                veto: None,
                execution_metadata,
            })),
            result => {
                let resolved = self
                    .tool_approval
                    .resolve_permission_resolution(result.into(), context, policy_name)
                    .await?;
                Ok(resolved.and_then(|decision| {
                    decision.veto.map(|veto| BeforeExecuteDecision {
                        veto: Some(veto),
                        execution_metadata: None,
                    })
                }))
            }
        }
    }

    async fn authorize(
        &self,
        context: ResolvedToolExecutionHookContext,
    ) -> Result<Option<BeforeExecuteDecision>, WireAsyncError> {
        let Some(evaluation) = self.policy_service.evaluate(context.clone()).await? else {
            return Ok(None);
        };
        self.track_evaluation(&context, &evaluation);
        self.tool_approval
            .resolve_permission_resolution(
                evaluation.result.into(),
                context,
                evaluation.policy_name,
            )
            .await
    }

    fn track_evaluation(
        &self,
        context: &ResolvedToolExecutionHookContext,
        evaluation: &PermissionPolicyEvaluation,
    ) {
        let (decision, reason) = result_details(&evaluation.result);
        let mut properties = Map::new();
        properties.insert("turn_id".to_string(), Value::from(context.base.turn_id));
        properties.insert(
            "tool_call_id".to_string(),
            Value::String(context.base.tool_call.id.clone()),
        );
        properties.insert(
            "policy_name".to_string(),
            Value::String(evaluation.policy_name.clone()),
        );
        properties.insert(
            "tool_name".to_string(),
            Value::String(context.base.tool_call.name.clone()),
        );
        properties.insert(
            "permission_mode".to_string(),
            serde_json::to_value(self.mode_service.mode()).unwrap_or(Value::Null),
        );
        properties.insert("decision".to_string(), Value::String(decision.to_string()));
        if let Some(reason) = reason {
            properties.extend(reason.clone());
        }
        self.telemetry
            .track2("permission_policy_decision", Some(properties));
    }
}

impl IAgentPermissionGate for AgentPermissionGate {
    fn data(&self) -> PermissionData {
        AgentPermissionGate::data(self)
    }

    fn authorize(
        &self,
        context: ResolvedToolExecutionHookContext,
    ) -> BoxFuture<'static, Result<Option<BeforeExecuteDecision>, WireAsyncError>> {
        let inner = self.inner.clone();
        async move { inner.authorize(context).await }.boxed()
    }
}

fn result_details(
    result: &PermissionPolicyResult,
) -> (&'static str, Option<&PermissionDecisionReason>) {
    match result {
        PermissionPolicyResult::Approve { reason, .. } => ("approve", reason.as_ref()),
        PermissionPolicyResult::Deny { reason, .. } => ("deny", reason.as_ref()),
        PermissionPolicyResult::Ask { reason, .. } => ("ask", reason.as_ref()),
    }
}

fn dependency<T>(args: &[DynamicValue], index: usize) -> Arc<T>
where
    T: Send + Sync + 'static + ?Sized,
{
    Arc::downcast::<Arc<T>>(args[index].clone())
        .unwrap_or_else(|_| panic!("permissionGate dependency {index} has an incompatible type"))
        .as_ref()
        .clone()
}

static AGENT_PERMISSION_GATE_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let ctor: Constructor<DynamicValue> = Arc::new(|args| {
        let service: Arc<dyn IAgentPermissionGate> = Arc::new(AgentPermissionGate::new(
            dependency::<dyn IAgentPermissionModeService>(&args, 0),
            dependency::<dyn IAgentPermissionRulesService>(&args, 1),
            dependency::<dyn IAgentPermissionPolicyService>(&args, 2),
            dependency::<dyn IAgentToolApprovalService>(&args, 3),
            dependency::<dyn ITelemetryService>(&args, 4),
            dependency::<dyn IAgentToolExecutorService>(&args, 5),
        ));
        Arc::new(service) as DynamicValue
    });
    let descriptor = SyncDescriptor::new(ctor, Vec::new());
    register_descriptor_dependencies(
        &descriptor,
        vec![
            ServiceDependency {
                id: refine_service_decorator(&I_AGENT_PERMISSION_MODE_SERVICE),
                index: 0,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_AGENT_PERMISSION_RULES_SERVICE),
                index: 1,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_AGENT_PERMISSION_POLICY_SERVICE),
                index: 2,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_AGENT_TOOL_APPROVAL_SERVICE),
                index: 3,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_TELEMETRY_SERVICE),
                index: 4,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_AGENT_TOOL_EXECUTOR_SERVICE),
                index: 5,
                kind: DependencyKind::Instance,
            },
        ],
    );
    register_scoped_service(
        LifecycleScope::Agent.as_str().to_string(),
        refine_service_decorator(&I_AGENT_PERMISSION_GATE),
        descriptor,
        ScopeActivation::OnScopeCreated,
        "permissionGate",
    )
    .map_err(|error| error.to_string())
});

pub fn ensure_agent_permission_gate_registered() -> Result<(), String> {
    Lazy::force(&AGENT_PERMISSION_GATE_REGISTRATION).clone()
}
