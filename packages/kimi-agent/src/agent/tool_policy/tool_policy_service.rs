use std::collections::HashSet;
use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt};
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::_base::di::lifecycle::DisposableRef;
use crate::agent::profile::profile::{IAgentProfileService, PROFILE_NOT_BOUND, ProfileError};
use crate::agent::tool_executor::tool_executor::{IAgentToolExecutorService, ToolCallGuardInput};
use crate::agent::tool_select::tool_select::SELECT_TOOLS_TOOL_NAME;
use crate::app::config::config::IConfigService;
use crate::session::session_tool_policy::session_tool_policy::ISessionToolPolicy;
use crate::session::session_tool_policy_gate::session_tool_policy_gate::ISessionToolPolicyGate;
use super::evaluate::{
    self, GlobalToolsPolicy, ToolActivationPolicy, ToolPolicyLayers,
    ToolSource as EvaluationToolSource, is_tool_active, is_tool_active_composed,
};
use crate::tool::tool_contract::ToolSource;
use crate::wire::record::WireAsyncError;

use super::config_section::{TOOLS_SECTION, ToolsConfig};
use super::tool_policy::IAgentToolPolicyService;

struct AgentToolPolicyServiceInner {
    profile: Arc<dyn IAgentProfileService>,
    config: Arc<dyn IConfigService>,
    session_tool_policy: Arc<dyn ISessionToolPolicy>,
    tool_policy_gate: Arc<dyn ISessionToolPolicyGate>,
    global_policy: Arc<Mutex<Option<ToolsConfig>>>,
    _registrations: Mutex<Vec<DisposableRef>>,
}

pub struct AgentToolPolicyService {
    inner: Arc<AgentToolPolicyServiceInner>,
}

impl AgentToolPolicyService {
    pub fn new(
        profile: Arc<dyn IAgentProfileService>,
        config: Arc<dyn IConfigService>,
        session_tool_policy: Arc<dyn ISessionToolPolicy>,
        tool_policy_gate: Arc<dyn ISessionToolPolicyGate>,
        tool_executor: Arc<dyn IAgentToolExecutorService>,
    ) -> Self {
        let global_policy = Arc::new(Mutex::new(config_value::<ToolsConfig>(
            config.as_ref(),
        )));
        let config_registration = config.on_did_section_change({
            let config = config.clone();
            let global_policy = global_policy.clone();
            Box::new(move |event| {
                if event.domain != TOOLS_SECTION {
                    return;
                }
                *global_policy.lock() = config_value::<ToolsConfig>(config.as_ref());
            })
        });
        let inner = Arc::new(AgentToolPolicyServiceInner {
            profile,
            config,
            session_tool_policy,
            tool_policy_gate,
            global_policy,
            _registrations: Mutex::new(vec![Arc::from(config_registration)]),
        });
        let weak = Arc::downgrade(&inner);
        let guard_registration = tool_executor.register_tool_call_guard(Arc::new(
            move |input: ToolCallGuardInput| {
                let Some(this) = weak.upgrade() else {
                    return None;
                };
                let active = if input.name == SELECT_TOOLS_TOOL_NAME {
                    this.is_tool_active_for_disclosure(&input.name, Some(input.source))
                } else {
                    this.is_tool_active(&input.name, Some(input.source))
                };
                (!active).then(|| {
                    format!(
                        "Tool \"{}\" is disabled by the active tool policy",
                        input.name
                    )
                })
            },
        ));
        inner._registrations.lock().push(guard_registration);
        Self { inner }
    }

    fn global_policy(&self) -> Option<ToolsConfig> {
        self.inner.global_policy.lock().clone()
    }

    fn layers(&self, profile: ToolActivationPolicy) -> ToolPolicyLayers {
        ToolPolicyLayers {
            workspace_disabled_tools: Some(self.inner.tool_policy_gate.disabled_tools()),
            profile,
            global: self
                .global_policy()
                .map(|policy| GlobalToolsPolicy {
                    enabled: policy.enabled,
                    disabled: policy.disabled,
                }),
            session_disabled_tools: Some(self.inner.session_tool_policy.disabled_tools()),
        }
    }
}

impl AgentToolPolicyServiceInner {
    fn is_tool_active(&self, name: &str, source: Option<ToolSource>) -> bool {
        let profile = self.profile.data();
        is_tool_active_composed(
            &ToolPolicyLayers {
                workspace_disabled_tools: Some(self.tool_policy_gate.disabled_tools()),
                profile: ToolActivationPolicy {
                    tools: profile.active_tool_names,
                    disallowed_tools: profile.disallowed_tools,
                },
                global: self.global_policy.lock().clone().map(|policy| {
                    GlobalToolsPolicy {
                        enabled: policy.enabled,
                        disabled: policy.disabled,
                    }
                }),
                session_disabled_tools: Some(self.session_tool_policy.disabled_tools()),
            },
            name,
            evaluation_source(source.unwrap_or(ToolSource::Builtin)),
        )
    }

    fn is_tool_active_for_disclosure(&self, name: &str, source: Option<ToolSource>) -> bool {
        let profile = self.profile.data();
        is_tool_active_composed(
            &ToolPolicyLayers {
                workspace_disabled_tools: Some(self.tool_policy_gate.disabled_tools()),
                profile: ToolActivationPolicy {
                    tools: None,
                    disallowed_tools: profile.disallowed_tools,
                },
                global: self.global_policy.lock().clone().map(|policy| {
                    GlobalToolsPolicy {
                        enabled: policy.enabled,
                        disabled: policy.disabled,
                    }
                }),
                session_disabled_tools: Some(self.session_tool_policy.disabled_tools()),
            },
            name,
            evaluation_source(source.unwrap_or(ToolSource::Builtin)),
        )
    }
}

impl IAgentToolPolicyService for AgentToolPolicyService {
    fn is_tool_active(&self, name: &str, source: Option<ToolSource>) -> bool {
        self.inner.is_tool_active(name, source)
    }

    fn is_tool_denied(&self, name: &str, source: Option<ToolSource>) -> bool {
        let source = source.unwrap_or_else(|| {
            if evaluate::is_mcp_tool_name(name) {
                ToolSource::Mcp
            } else {
                ToolSource::Builtin
            }
        });
        let source = evaluation_source(source);
        let denied_by = |list: Option<Vec<String>>| {
            list.is_some_and(|list| {
                !is_tool_active(
                    &ToolActivationPolicy {
                        tools: None,
                        disallowed_tools: Some(list),
                    },
                    name,
                    source,
                )
            })
        };
        denied_by(Some(self.inner.tool_policy_gate.disabled_tools()))
            || denied_by(self.inner.profile.data().disallowed_tools)
            || denied_by(self.global_policy().and_then(|policy| policy.disabled))
            || denied_by(Some(self.inner.session_tool_policy.disabled_tools()))
    }

    fn get_policy_fingerprint(&self) -> String {
        let profile = self.inner.profile.data();
        let global = self.global_policy();
        let layers = [
            Some(self.inner.tool_policy_gate.disabled_tools()),
            profile.active_tool_names,
            profile.disallowed_tools,
            global.as_ref().and_then(|policy| policy.enabled.clone()),
            global.and_then(|policy| policy.disabled),
            Some(self.inner.session_tool_policy.disabled_tools()),
        ];
        let values = layers
            .into_iter()
            .map(|names| match names {
                None => Value::Null,
                Some(names) => {
                    let unique = names.into_iter().collect::<HashSet<_>>();
                    let mut names = unique.into_iter().collect::<Vec<_>>();
                    names.sort_by(|left, right| {
                        left.encode_utf16()
                            .cmp(right.encode_utf16())
                    });
                    json!(names)
                }
            })
            .collect::<Vec<_>>();
        serde_json::to_string(&values).expect("tool policy fingerprint is serializable")
    }

    fn is_tool_active_for_disclosure(&self, name: &str, source: Option<ToolSource>) -> bool {
        self.inner.is_tool_active_for_disclosure(name, source)
    }

    fn is_tool_active_for_profile(
        &self,
        profile: &ToolActivationPolicy,
        name: &str,
        source: Option<ToolSource>,
    ) -> bool {
        is_tool_active_composed(
            &self.layers(profile.clone()),
            name,
            evaluation_source(source.unwrap_or(ToolSource::Builtin)),
        )
    }

    fn set_session_disabled_tools(
        &self,
        names: Vec<String>,
    ) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        if self.inner.profile.data().base.profile_name.is_none() {
            let error = ProfileError::new(
                PROFILE_NOT_BOUND,
                "Cannot set session disabled tools: agent profile is not bound",
                None,
            );
            return async move { Err(Box::new(error) as WireAsyncError) }.boxed();
        }
        let session_tool_policy = self.inner.session_tool_policy.clone();
        async move { session_tool_policy.set_disabled_tools(names).await }.boxed()
    }
}

fn config_value<T: serde::de::DeserializeOwned>(config: &dyn IConfigService) -> Option<T> {
    config
        .get(TOOLS_SECTION)
        .and_then(|value| serde_json::from_value(value).ok())
}

fn evaluation_source(source: ToolSource) -> EvaluationToolSource {
    match source {
        ToolSource::Mcp => EvaluationToolSource::Mcp,
        ToolSource::Host | ToolSource::User => EvaluationToolSource::User,
        ToolSource::Builtin | ToolSource::Plugin => EvaluationToolSource::Builtin,
    }
}
