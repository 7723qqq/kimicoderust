use crate::agent::tool_registry::tool_contribution::AgentToolContributionOptions;

pub trait AgentEnvironment {
    fn is_available(&self, capabilities: &[String]) -> bool;

    fn connect_inspectable(&self) -> bool;
}

pub fn is_tool_available(
    options: &AgentToolContributionOptions,
    environment: &dyn AgentEnvironment,
) -> bool {
    if let Some(required) = options.required_environment_capabilities.as_deref()
        && !environment.is_available(required)
        && !environment.connect_inspectable()
    {
        return false;
    }
    options.when.as_ref().is_none_or(|when| (**when)())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct Environment {
        available: bool,
        inspectable: bool,
    }

    impl AgentEnvironment for Environment {
        fn is_available(&self, _capabilities: &[String]) -> bool {
            self.available
        }

        fn connect_inspectable(&self) -> bool {
            self.inspectable
        }
    }

    #[test]
    fn missing_capabilities_fall_back_to_connect_inspection() {
        let options = AgentToolContributionOptions::new("X")
            .required_environment_capabilities(vec!["browser".to_string()]);
        assert!(!is_tool_available(
            &options,
            &Environment {
                available: false,
                inspectable: false
            }
        ));
        assert!(is_tool_available(
            &options,
            &Environment {
                available: false,
                inspectable: true
            }
        ));
        assert!(is_tool_available(
            &options,
            &Environment {
                available: true,
                inspectable: false
            }
        ));
    }

    #[test]
    fn when_gate_defaults_to_available_and_vetoes_when_false() {
        let base = AgentToolContributionOptions::new("X");
        assert!(is_tool_available(
            &base,
            &Environment {
                available: true,
                inspectable: true
            }
        ));

        let gated = base.when(Arc::new(|| false));
        assert!(!is_tool_available(
            &gated,
            &Environment {
                available: true,
                inspectable: true
            }
        ));

        let allowed = AgentToolContributionOptions::new("X").when(Arc::new(|| true));
        assert!(is_tool_available(
            &allowed,
            &Environment {
                available: false,
                inspectable: false
            }
        ));
    }
}
