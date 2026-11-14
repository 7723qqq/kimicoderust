use once_cell::sync::Lazy;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};

use super::agent_profile_catalog::AgentProfile;

pub const BUILTIN_AGENT_PROFILE_SOURCE_ID: &str = "builtin";

pub trait IBuiltinAgentProfileLoader: Send + Sync {
    fn get(&self, name: &str) -> Option<AgentProfile>;
    fn get_default(&self) -> AgentProfile;
    fn list(&self) -> Vec<AgentProfile>;
}

pub static I_BUILTIN_AGENT_PROFILE_LOADER: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("builtinAgentProfileLoader"));
