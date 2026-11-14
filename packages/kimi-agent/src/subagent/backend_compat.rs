//! Argument incompatibilities on the Agent tool's external `backend` path.
//!
//! Mirrors `backendCompat.ts` in
//! `upstream/main:packages/agent-core-v2/src/session/subagent/backend/`.
//!
//! An external backend runs a fresh session in another CLI, so it cannot be
//! combined with anything that targets in-process agent state. The five
//! refusals are model-facing and their wording is v2's verbatim, the same
//! contract `fork_incompatibility` follows.

/// v2 `BACKEND_CANNOT_COMBINE_WITH_RESUME`.
pub const BACKEND_CANNOT_COMBINE_WITH_RESUME: &str =
    "Cannot use backend with resume — an external backend runs a fresh session in the external CLI; resume targets an existing in-process agent by id.";
/// v2 `BACKEND_CANNOT_COMBINE_WITH_SUBAGENT_TYPE`.
pub const BACKEND_CANNOT_COMBINE_WITH_SUBAGENT_TYPE: &str =
    "Cannot use backend with subagent_type — external backends have no in-process agent profile. Omit subagent_type to use backend.";
/// v2 `BACKEND_CANNOT_COMBINE_WITH_MODEL`.
pub const BACKEND_CANNOT_COMBINE_WITH_MODEL: &str =
    "Cannot use backend with model — external backends are configured through the [subagentBackend] config section instead of the model parameter.";
/// v2 `BACKEND_CANNOT_COMBINE_WITH_FORK`.
pub const BACKEND_CANNOT_COMBINE_WITH_FORK: &str =
    "Cannot use backend with fork — fork snapshots the in-process conversation history, which an external backend does not share.";
/// v2 `BACKEND_CANNOT_COMBINE_WITH_BACKGROUND`.
pub const BACKEND_CANNOT_COMBINE_WITH_BACKGROUND: &str =
    "Cannot use backend with run_in_background — an external backend runs only in the foreground of the calling turn.";

/// The Agent tool arguments the backend compatibility rules read (v2
/// `BackendCompatInputLike`).
pub struct BackendCompatInput<'a> {
    pub backend: Option<&'a str>,
    pub fork: bool,
    pub resume: Option<&'a str>,
    pub subagent_type: Option<&'a str>,
    pub model: Option<&'a str>,
    pub run_in_background: bool,
}

impl Default for BackendCompatInput<'_> {
    fn default() -> Self {
        Self {
            backend: None,
            fork: false,
            resume: None,
            subagent_type: None,
            model: None,
            run_in_background: false,
        }
    }
}

/// v2 `backendIncompatibility`: the first offending argument, or `None` when the
/// combination is allowed. Only `resume` is trimmed (a whitespace-only resume is
/// absent); every other value counts as soon as it is non-empty.
pub fn backend_incompatibility(input: &BackendCompatInput<'_>) -> Option<&'static str> {
    if input.backend.is_none_or(str::is_empty) {
        return None;
    }
    if input.resume.is_some_and(|value| !value.trim().is_empty()) {
        return Some(BACKEND_CANNOT_COMBINE_WITH_RESUME);
    }
    if input.subagent_type.is_some_and(|value| !value.is_empty()) {
        return Some(BACKEND_CANNOT_COMBINE_WITH_SUBAGENT_TYPE);
    }
    if input.model.is_some_and(|value| !value.is_empty()) {
        return Some(BACKEND_CANNOT_COMBINE_WITH_MODEL);
    }
    if input.fork {
        return Some(BACKEND_CANNOT_COMBINE_WITH_FORK);
    }
    if input.run_in_background {
        return Some(BACKEND_CANNOT_COMBINE_WITH_BACKGROUND);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(backend: Option<&str>) -> BackendCompatInput<'_> {
        BackendCompatInput {
            backend,
            ..Default::default()
        }
    }

    #[test]
    fn no_backend_means_no_incompatibility() {
        assert_eq!(backend_incompatibility(&input(None)), None);
        assert_eq!(backend_incompatibility(&input(Some(""))), None);
    }

    #[test]
    fn a_backend_alone_is_allowed() {
        assert_eq!(backend_incompatibility(&input(Some("codex"))), None);
    }

    #[test]
    fn resume_is_refused() {
        let mut args = input(Some("codex"));
        args.resume = Some("agent-1");
        assert_eq!(
            backend_incompatibility(&args),
            Some(BACKEND_CANNOT_COMBINE_WITH_RESUME)
        );
    }

    #[test]
    fn a_whitespace_only_resume_counts_as_absent() {
        let mut args = input(Some("codex"));
        args.resume = Some("   ");
        assert_eq!(backend_incompatibility(&args), None);
    }

    #[test]
    fn subagent_type_is_refused() {
        let mut args = input(Some("codex"));
        args.subagent_type = Some("coder");
        assert_eq!(
            backend_incompatibility(&args),
            Some(BACKEND_CANNOT_COMBINE_WITH_SUBAGENT_TYPE)
        );
        args.subagent_type = Some("");
        assert_eq!(backend_incompatibility(&args), None);
    }

    #[test]
    fn model_is_refused() {
        let mut args = input(Some("codex"));
        args.model = Some("kimi-k2");
        assert_eq!(
            backend_incompatibility(&args),
            Some(BACKEND_CANNOT_COMBINE_WITH_MODEL)
        );
    }

    #[test]
    fn fork_and_background_are_refused() {
        let mut args = input(Some("codex"));
        args.fork = true;
        assert_eq!(
            backend_incompatibility(&args),
            Some(BACKEND_CANNOT_COMBINE_WITH_FORK)
        );
        args.fork = false;
        args.run_in_background = true;
        assert_eq!(
            backend_incompatibility(&args),
            Some(BACKEND_CANNOT_COMBINE_WITH_BACKGROUND)
        );
    }

    #[test]
    fn the_check_order_is_resume_type_model_fork_background() {
        let mut args = input(Some("codex"));
        args.resume = Some("agent-1");
        args.subagent_type = Some("coder");
        args.model = Some("kimi-k2");
        args.fork = true;
        args.run_in_background = true;
        assert_eq!(
            backend_incompatibility(&args),
            Some(BACKEND_CANNOT_COMBINE_WITH_RESUME)
        );
        args.resume = None;
        assert_eq!(
            backend_incompatibility(&args),
            Some(BACKEND_CANNOT_COMBINE_WITH_SUBAGENT_TYPE)
        );
        args.subagent_type = None;
        assert_eq!(
            backend_incompatibility(&args),
            Some(BACKEND_CANNOT_COMBINE_WITH_MODEL)
        );
        args.model = None;
        assert_eq!(
            backend_incompatibility(&args),
            Some(BACKEND_CANNOT_COMBINE_WITH_FORK)
        );
        args.fork = false;
        assert_eq!(
            backend_incompatibility(&args),
            Some(BACKEND_CANNOT_COMBINE_WITH_BACKGROUND)
        );
    }
}
