
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{OnceLock, RwLock};

use super::domains::{BUILTIN_DOMAINS, core as core_domain};

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct ErrorCode(String);

impl ErrorCode {
    pub fn new(code: impl Into<String>) -> Self {
        Self(code.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ErrorCode {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ErrorCode {
    fn from(code: &str) -> Self {
        Self(code.to_string())
    }
}

impl From<String> for ErrorCode {
    fn from(code: String) -> Self {
        Self(code)
    }
}

impl From<&String> for ErrorCode {
    fn from(code: &String) -> Self {
        Self(code.clone())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorInfo {
    pub title: Option<&'static str>,
    pub retryable: bool,
    pub public: bool,
    pub action: Option<&'static str>,
}

impl ErrorInfo {
    pub const fn new(retryable: bool, public: bool) -> Self {
        Self {
            title: None,
            retryable,
            public,
            action: None,
        }
    }

    pub const fn with_title(mut self, title: &'static str) -> Self {
        self.title = Some(title);
        self
    }

    pub const fn with_action(mut self, action: &'static str) -> Self {
        self.action = Some(action);
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ErrorDomain {
    pub id: &'static str,
    pub codes: &'static [(&'static str, &'static str)],
    pub retryable: &'static [&'static str],
    pub info: &'static [(&'static str, ErrorInfo)],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateCode {
    pub code: &'static str,
}

impl fmt::Display for DuplicateCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "error code '{}' is registered by two different domains",
            self.code
        )
    }
}

impl std::error::Error for DuplicateCode {}

#[derive(Debug, Default)]
struct Registry {
    owners: HashMap<&'static str, &'static str>,
    retryable: HashSet<&'static str>,
    info: HashMap<&'static str, ErrorInfo>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, domain: &'static ErrorDomain) -> Result<(), DuplicateCode> {
        for (_, code) in domain.codes {
            let code = *code;
            match self.owners.get(code) {
                Some(owner) if *owner != domain.id => {
                    return Err(DuplicateCode { code });
                }
                _ => {
                    self.owners.insert(code, domain.id);
                }
            }
        }
        for code in domain.retryable {
            self.retryable.insert(*code);
        }
        for (code, info) in domain.info {
            self.info.insert(*code, *info);
        }
        Ok(())
    }

    pub fn is_error_code(&self, code: &str) -> bool {
        self.owners.contains_key(code)
    }

    pub fn error_info(&self, code: &str) -> ErrorInfo {
        if let Some(info) = self.info.get(code) {
            return *info;
        }
        ErrorInfo::new(self.retryable.contains(code), true)
    }
}

static REGISTRY: OnceLock<RwLock<Registry>> = OnceLock::new();

fn registry() -> &'static RwLock<Registry> {
    REGISTRY.get_or_init(|| RwLock::new(builtin_registry()))
}

fn builtin_registry() -> Registry {
    let mut registry = Registry::new();
    for domain in BUILTIN_DOMAINS {
        if let Err(error) = registry.register(*domain) {
            panic!("{error}");
        }
    }
    registry
}

fn read_registry() -> std::sync::RwLockReadGuard<'static, Registry> {
    registry()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn register_error_domain(domain: &'static ErrorDomain) -> Result<(), DuplicateCode> {
    registry()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .register(domain)
}

pub fn is_error_code(code: &str) -> bool {
    read_registry().is_error_code(code)
}

pub fn error_info(code: &str) -> ErrorInfo {
    read_registry().error_info(code)
}

pub use core_domain::{
    INTERNAL as INTERNAL_CODE, NOT_IMPLEMENTED as NOT_IMPLEMENTED_CODE,
    VALIDATION_FAILED as VALIDATION_FAILED_CODE,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_domain_is_registered_with_its_overrides() {
        assert!(is_error_code("internal"));
        assert!(is_error_code("not_implemented"));
        assert!(is_error_code("validation.failed"));
        assert!(!is_error_code("not.a.registered.code"));

        let internal = error_info("internal");
        assert_eq!(internal, ErrorInfo::new(false, true));
        assert_eq!(error_info("validation.failed"), ErrorInfo::new(false, true));
    }

    #[test]
    fn the_codes_the_engine_already_emits_are_registered() {
        use crate::primitives::errors::domains::{
            agent_lifecycle, agent_loop, config, model_catalog, profile, protocol, session_query,
            skill, spec, wire,
        };

        let registered = [
            agent_lifecycle::AGENT_ALREADY_EXISTS,
            agent_lifecycle::AGENT_NOT_FOUND,
            config::CONFIG_INVALID,
            // `model.config_invalid` and `model.not_configured` are owned by
            // `agent/profile/errors.ts`; `provider.not_found` by the model
            // catalog (`llm-adapter/model/errors.ts`).
            profile::MODEL_CONFIG_INVALID,
            profile::MODEL_NOT_CONFIGURED,
            model_catalog::PROVIDER_NOT_FOUND,
            protocol::PROVIDER_API_ERROR,
            protocol::PROVIDER_AUTH_ERROR,
            protocol::PROVIDER_CONNECTION_ERROR,
            protocol::PROVIDER_FILTERED,
            protocol::PROVIDER_OVERLOADED,
            protocol::PROVIDER_RATE_LIMIT,
            session_query::SESSION_QUERY_INDEX_UNAVAILABLE,
            session_query::SESSION_QUERY_INVALID_FILTER,
            session_query::SESSION_QUERY_SESSION_NOT_FOUND,
            skill::SKILL_NOT_FOUND,
            spec::SPEC_INCOMPLETE,
            spec::SPEC_MODE_INVALID,
            wire::WIRE_CYCLE,
            wire::WIRE_MIGRATION_MISSING,
            wire::WIRE_UNKNOWN_RECORD,
        ];
        for code in registered {
            assert!(is_error_code(code), "{code}");
        }

        // The four provider codes the reference lists as retryable, plus the
        // two advisory-list entries added by their own domains.
        for code in [
            protocol::PROVIDER_RATE_LIMIT,
            protocol::PROVIDER_CONNECTION_ERROR,
            protocol::PROVIDER_OVERLOADED,
            protocol::CONTEXT_OVERFLOW,
            session_query::SESSION_QUERY_INDEX_UNAVAILABLE,
            agent_loop::TURN_AGENT_BUSY,
        ] {
            assert!(error_info(code).retryable, "{code}");
        }

        // `provider.filtered` is in no retryable list and has an explicit
        // false override; the rest are absent from every retryable list.
        for code in [
            protocol::PROVIDER_API_ERROR,
            protocol::PROVIDER_AUTH_ERROR,
            protocol::PROVIDER_FILTERED,
            agent_lifecycle::AGENT_NOT_FOUND,
            wire::WIRE_CYCLE,
        ] {
            assert!(!error_info(code).retryable, "{code}");
        }
    }

    #[test]
    fn every_builtin_domain_is_registered() {
        let mut codes = std::collections::HashSet::new();
        for domain in super::BUILTIN_DOMAINS {
            for (_, code) in domain.codes {
                assert!(is_error_code(code), "{code}");
                codes.insert(*code);
            }
        }
        assert_eq!(super::BUILTIN_DOMAINS.len(), 37);
        assert_eq!(codes.len(), 176);

        for code in [
            "os.fs.not_found",
            "mcp.server_not_found",
            "state.cycle",
            "session.not_found",
            "storage.io_failed",
        ] {
            assert!(is_error_code(code), "{code}");
        }
        assert!(error_info("storage.io_failed").retryable);
        assert!(!error_info("storage.corrupted").retryable);
    }

    #[test]
    fn a_code_can_carry_a_title_and_an_action() {
        let info = error_info(crate::primitives::errors::domains::wire::WIRE_VERSION_TOO_LOW);
        assert_eq!(info.title, Some("Kimi Code upgrade required"));
        assert_eq!(
            info.action,
            Some("Upgrade Kimi Code to the latest version to read this session.")
        );
        assert!(!info.retryable);
        assert!(info.public);
    }

    #[test]
    fn a_code_owned_by_two_domains_is_refused() {
        static FIRST: ErrorDomain = ErrorDomain {
            id: "first",
            codes: &[("SOLO", "solo.code")],
            retryable: &[],
            info: &[],
        };
        static SECOND: ErrorDomain = ErrorDomain {
            id: "second",
            codes: &[("SOLO", "solo.code")],
            retryable: &[],
            info: &[],
        };
        let mut registry = Registry::new();
        registry.register(&FIRST).unwrap();
        assert_eq!(
            registry.register(&SECOND).unwrap_err(),
            DuplicateCode { code: "solo.code" }
        );
    }

    #[test]
    fn the_same_domain_may_be_registered_twice() {
        static DOMAIN: ErrorDomain = ErrorDomain {
            id: "only",
            codes: &[("ONLY", "only.code")],
            retryable: &[],
            info: &[],
        };
        let mut registry = Registry::new();
        registry.register(&DOMAIN).unwrap();
        registry.register(&DOMAIN).unwrap();
        assert!(registry.is_error_code("only.code"));
    }

    #[test]
    fn default_info_reads_the_retryable_list() {
        static DOMAIN: ErrorDomain = ErrorDomain {
            id: "retryable",
            codes: &[("RETRY", "retry.code"), ("PLAIN", "plain.code")],
            retryable: &["retry.code"],
            info: &[],
        };
        let mut registry = Registry::new();
        registry.register(&DOMAIN).unwrap();
        assert_eq!(
            registry.error_info("retry.code"),
            ErrorInfo::new(true, true)
        );
        assert_eq!(
            registry.error_info("plain.code"),
            ErrorInfo::new(false, true)
        );
    }

    #[test]
    fn an_info_override_wins_over_the_retryable_list() {
        static DOMAIN: ErrorDomain = ErrorDomain {
            id: "override",
            codes: &[("FIXED", "fixed.code")],
            retryable: &["fixed.code"],
            info: &[("fixed.code", ErrorInfo::new(false, true))],
        };
        let mut registry = Registry::new();
        registry.register(&DOMAIN).unwrap();
        assert!(!registry.error_info("fixed.code").retryable);
    }
}
