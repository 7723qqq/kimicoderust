use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Weak};

use async_trait::async_trait;
use futures_util::future::{BoxFuture, FutureExt, join_all};
use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;

use crate::_base::di::lifecycle::DisposableRef;
use once_cell::sync::Lazy;
use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::instantiation::{
    DependencyKind, ServiceDependency, refine_service_decorator,
};
use crate::_base::di::instantiation_service::register_descriptor_dependencies;
use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::app::scopes::LifecycleScope;
use crate::agent::scope_context::scope_context::{
    I_AGENT_SCOPE_CONTEXT, IAgentScopeContext,
};
use crate::agent::state::agent_state::{I_AGENT_STATE_SERVICE, IAgentStateService};
use crate::app::config::config::{I_CONFIG_SERVICE, IConfigService};
use crate::app::event::event2::Event2;
use crate::state::event_dispatcher::{I_EVENT_DISPATCHER, IEventDispatcher};
use crate::agent::permission_rules::project_permission_rules::I_PROJECT_PERMISSION_RULES_SERVICE;
use crate::wire::record::WireAsyncError;

use super::config_section::{PERMISSION_SECTION, PermissionConfig, user_permission_rules};
use super::permission_rules::{
    I_AGENT_PERMISSION_RULES_SERVICE, IAgentPermissionRulesService,
    PermissionApprovalResultRecord, PermissionRule, PermissionRuleDecision,
    PermissionRuleScope,
};
use super::permission_rules_ops::{
    PermissionRecordApprovalResult, PermissionRulesAdd, PermissionRulesModelState,
    permission_rules_replayable_key,
};
use super::project_permission_rules::IProjectPermissionRulesService;

struct UserConfigWriteSlot {
    config: Weak<dyn IConfigService>,
    lock: Arc<AsyncMutex<()>>,
}

static USER_CONFIG_WRITE_LOCKS: LazyLock<Mutex<HashMap<usize, UserConfigWriteSlot>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub struct AgentPermissionRulesService {
    dispatcher: Arc<dyn IEventDispatcher>,
    scope_context: Arc<dyn IAgentScopeContext>,
    agent_state: Arc<dyn IAgentStateService>,
    config: Arc<dyn IConfigService>,
    project_rules: Arc<dyn IProjectPermissionRulesService>,
    _registration: DisposableRef,
}

impl AgentPermissionRulesService {
    pub fn new(
        dispatcher: Arc<dyn IEventDispatcher>,
        scope_context: Arc<dyn IAgentScopeContext>,
        agent_state: Arc<dyn IAgentStateService>,
        config: Arc<dyn IConfigService>,
        project_rules: Arc<dyn IProjectPermissionRulesService>,
    ) -> Self {
        let registration = agent_state
            .contribute_state(permission_rules_replayable_key().state_key())
            .unwrap_or_else(|error| panic!("{error}"));
        Self {
            dispatcher,
            scope_context,
            agent_state,
            config,
            project_rules,
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

#[async_trait]
impl IAgentPermissionRulesService for AgentPermissionRulesService {
    fn rules(&self) -> Vec<PermissionRule> {
        let mut merged = self.state().rules;
        merged.extend(self.project_rules.rules());
        merged.extend(user_permission_rules(self.config.as_ref()));
        dedupe_permission_rules(merged)
    }

    fn session_approval_rule_patterns(&self) -> Vec<String> {
        self.state().session_approval_rule_patterns
    }

    async fn add_rules(&self, rules: &[PermissionRule]) -> Result<(), WireAsyncError> {
        if rules.is_empty() {
            return Ok(());
        }
        let runtime_rules = permission_rules_with_scope(
            rules,
            &[
                PermissionRuleScope::TurnOverride,
                PermissionRuleScope::SessionRuntime,
            ],
        );
        let user_rules = permission_rules_with_scope(rules, &[PermissionRuleScope::User]);
        let project_rules = missing_permission_rules(
            &self.project_rules.rules(),
            &permission_rules_with_scope(rules, &[PermissionRuleScope::Project]),
        );
        let mut writes: Vec<BoxFuture<'static, Result<(), WireAsyncError>>> = Vec::new();
        if !runtime_rules.is_empty() {
            let dispatcher = self.dispatcher.clone();
            let event = PermissionRulesAdd {
                agent_id: self.scope_context.agent_id(),
                rules: runtime_rules,
            };
            writes.push(async move { dispatcher.dispatch(event.into()).await }.boxed());
        }
        if !user_rules.is_empty() {
            let config = self.config.clone();
            writes.push(append_user_permission_rules(config, user_rules).boxed());
        }
        if !project_rules.is_empty() {
            let project_rules_service = self.project_rules.clone();
            writes.push(
                async move { project_rules_service.append_rules(&project_rules).await }.boxed(),
            );
        }
        for result in join_all(writes).await {
            result?;
        }
        Ok(())
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

fn permission_rules_with_scope(
    rules: &[PermissionRule],
    scopes: &[PermissionRuleScope],
) -> Vec<PermissionRule> {
    rules
        .iter()
        .filter(|rule| scopes.contains(&rule.scope))
        .cloned()
        .collect()
}

fn permission_rule_key(rule: &PermissionRule) -> String {
    format!(
        "{}\u{0}{}\u{0}{}\u{0}{}",
        decision_key(rule.decision),
        scope_key(rule.scope),
        rule.pattern,
        rule.reason.as_deref().unwrap_or_default()
    )
}

fn decision_key(decision: PermissionRuleDecision) -> &'static str {
    match decision {
        PermissionRuleDecision::Allow => "allow",
        PermissionRuleDecision::Deny => "deny",
        PermissionRuleDecision::Ask => "ask",
    }
}

fn scope_key(scope: PermissionRuleScope) -> &'static str {
    match scope {
        PermissionRuleScope::TurnOverride => "turn-override",
        PermissionRuleScope::SessionRuntime => "session-runtime",
        PermissionRuleScope::Project => "project",
        PermissionRuleScope::User => "user",
    }
}

fn missing_permission_rules(
    existing: &[PermissionRule],
    rules: &[PermissionRule],
) -> Vec<PermissionRule> {
    let mut seen: std::collections::HashSet<String> =
        existing.iter().map(permission_rule_key).collect();
    let mut out = Vec::new();
    for rule in rules {
        if seen.insert(permission_rule_key(rule)) {
            out.push(rule.clone());
        }
    }
    out
}

fn dedupe_permission_rules(rules: Vec<PermissionRule>) -> Vec<PermissionRule> {
    missing_permission_rules(&[], &rules)
}

fn user_config_write_lock(config: &Arc<dyn IConfigService>) -> Arc<AsyncMutex<()>> {
    let key = Arc::as_ptr(config) as *const () as usize;
    let mut locks = USER_CONFIG_WRITE_LOCKS.lock();
    locks.retain(|_, slot| slot.config.strong_count() > 0);
    if let Some(slot) = locks.get(&key) {
        return slot.lock.clone();
    }
    let lock = Arc::new(AsyncMutex::new(()));
    locks.insert(
        key,
        UserConfigWriteSlot {
            config: Arc::downgrade(config),
            lock: lock.clone(),
        },
    );
    lock
}

async fn append_user_permission_rules(
    config: Arc<dyn IConfigService>,
    rules: Vec<PermissionRule>,
) -> Result<(), WireAsyncError> {
    let lock = user_config_write_lock(&config);
    let _guard = lock.lock().await;
    let current = config.inspect(PERMISSION_SECTION).user_value;
    let existing = match current.as_ref() {
        Some(value) => serde_json::from_value::<PermissionConfig>(value.clone())
            .map_err(|error| Box::new(error) as WireAsyncError)?
            .rules
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let appended = missing_permission_rules(&existing, &rules);
    if appended.is_empty() {
        return Ok(());
    }
    let mut value = current
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let mut merged = existing;
    merged.extend(appended);
    value.insert(
        "rules".to_string(),
        serde_json::to_value(&merged).map_err(|error| Box::new(error) as WireAsyncError)?,
    );
    config
        .replace(
            PERMISSION_SECTION,
            Some(serde_json::Value::Object(value)),
            None,
        )
        .await
}

fn dependency<T>(args: &[DynamicValue], index: usize) -> Arc<T>
where
    T: Send + Sync + 'static + ?Sized,
{
    Arc::downcast::<Arc<T>>(args[index].clone())
        .unwrap_or_else(|_| panic!("permissionRules dependency {index} has an incompatible type"))
        .as_ref()
        .clone()
}

static AGENT_PERMISSION_RULES_SERVICE_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let ctor: Constructor<DynamicValue> = Arc::new(|args| {
        let service: Arc<dyn IAgentPermissionRulesService> =
            Arc::new(AgentPermissionRulesService::new(
                dependency::<dyn IEventDispatcher>(&args, 0),
                dependency::<dyn IAgentScopeContext>(&args, 1),
                dependency::<dyn IAgentStateService>(&args, 2),
                dependency::<dyn IConfigService>(&args, 3),
                dependency::<dyn IProjectPermissionRulesService>(&args, 4),
            ));
        Arc::new(service) as DynamicValue
    });
    let descriptor = SyncDescriptor::new(ctor, Vec::new());
    register_descriptor_dependencies(
        &descriptor,
        vec![
            ServiceDependency {
                id: refine_service_decorator(&I_EVENT_DISPATCHER),
                index: 0,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_AGENT_SCOPE_CONTEXT),
                index: 1,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_AGENT_STATE_SERVICE),
                index: 2,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_CONFIG_SERVICE),
                index: 3,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_PROJECT_PERMISSION_RULES_SERVICE),
                index: 4,
                kind: DependencyKind::Instance,
            },
        ],
    );
    register_scoped_service(
        LifecycleScope::Agent.as_str().to_string(),
        refine_service_decorator(&I_AGENT_PERMISSION_RULES_SERVICE),
        descriptor,
        ScopeActivation::OnScopeCreated,
        "permissionRules",
    )
    .map_err(|error| error.to_string())
});

pub fn ensure_agent_permission_rules_service_registered() -> Result<(), String> {
    Lazy::force(&AGENT_PERMISSION_RULES_SERVICE_REGISTRATION).clone()
}
