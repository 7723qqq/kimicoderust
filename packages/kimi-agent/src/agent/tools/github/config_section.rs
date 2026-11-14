pub use crate::config::GitHubConfig as GithubConfig;

pub const GITHUB_SECTION: &str = "github";

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

pub fn resolve_github_token(config: &crate::config::KimiConfig) -> Option<&str> {
    non_empty(config.github.token.as_deref())
}

pub fn resolve_github_base_url(config: &crate::config::KimiConfig) -> Option<&str> {
    non_empty(config.github.base_url.as_deref())
}

pub fn has_github_token(config: &crate::config::KimiConfig) -> bool {
    resolve_github_token(config).is_some()
}
