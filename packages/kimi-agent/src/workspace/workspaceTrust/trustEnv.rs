use crate::_base::utils::env::parse_boolean_env;

pub const TRUST_WORKSPACE_ENV: &str = "KIMI_CODE_TRUST_WORKSPACE";

pub fn trust_workspace_env_trusted(
    get_env: impl Fn(&str) -> Option<String>,
) -> bool {
    parse_boolean_env(get_env(TRUST_WORKSPACE_ENV).as_deref()) == Some(true)
}
