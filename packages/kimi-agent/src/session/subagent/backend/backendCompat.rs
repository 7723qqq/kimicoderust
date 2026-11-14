use serde::{Deserialize, Serialize};

pub const BACKEND_CANNOT_COMBINE_WITH_RESUME: &str = "Cannot use backend with resume — an external backend runs a fresh session in the external CLI; resume targets an existing in-process agent by id.";
pub const BACKEND_CANNOT_COMBINE_WITH_SUBAGENT_TYPE: &str = "Cannot use backend with subagent_type — external backends have no in-process agent profile. Omit subagent_type to use backend.";
pub const BACKEND_CANNOT_COMBINE_WITH_MODEL: &str = "Cannot use backend with model — external backends are configured through the [subagentBackend] config section instead of the model parameter.";
pub const BACKEND_CANNOT_COMBINE_WITH_FORK: &str = "Cannot use backend with fork — fork snapshots the in-process conversation history, which an external backend does not share.";
pub const BACKEND_CANNOT_COMBINE_WITH_BACKGROUND: &str = "Cannot use backend with run_in_background — an external backend runs only in the foreground of the calling turn.";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendCompatInputLike {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_in_background: Option<bool>,
}

pub fn backend_incompatibility(args: &BackendCompatInputLike) -> Option<&'static str> {
    if args.backend.as_deref().is_none_or(str::is_empty) {
        return None;
    }
    if args
        .resume
        .as_deref()
        .is_some_and(|resume| !resume.trim().is_empty())
    {
        return Some(BACKEND_CANNOT_COMBINE_WITH_RESUME);
    }
    if args
        .subagent_type
        .as_deref()
        .is_some_and(|subagent_type| !subagent_type.is_empty())
    {
        return Some(BACKEND_CANNOT_COMBINE_WITH_SUBAGENT_TYPE);
    }
    if args.model.as_deref().is_some_and(|model| !model.is_empty()) {
        return Some(BACKEND_CANNOT_COMBINE_WITH_MODEL);
    }
    if args.fork == Some(true) {
        return Some(BACKEND_CANNOT_COMBINE_WITH_FORK);
    }
    if args.run_in_background == Some(true) {
        return Some(BACKEND_CANNOT_COMBINE_WITH_BACKGROUND);
    }
    None
}
