use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt};

use crate::agent::permission_policy::types::{
    PermissionPolicy, PermissionPolicyContext, PermissionPolicyResult,
};
use crate::agent::permission_rules::config_section::is_dangerous_command_guard_enabled;
use crate::app::config::config::IConfigService;
use crate::wire::record::WireAsyncError;

use super::path_utils::file_accesses;

#[derive(Clone)]
pub struct SensitiveFileAccessAskPermissionPolicyService {
    config: Arc<dyn IConfigService>,
}

impl SensitiveFileAccessAskPermissionPolicyService {
    pub fn new(config: Arc<dyn IConfigService>) -> Self {
        Self { config }
    }
}

impl PermissionPolicy for SensitiveFileAccessAskPermissionPolicyService {
    fn name(&self) -> &str {
        "sensitive-file-access-ask"
    }

    fn evaluate(
        &self,
        context: PermissionPolicyContext,
    ) -> BoxFuture<'static, Result<Option<PermissionPolicyResult>, WireAsyncError>> {
        let result = (is_dangerous_command_guard_enabled(self.config.as_ref())
            && file_accesses(&context)
                .iter()
                .any(|access| crate::native::file_type::is_sensitive_file(&access.path)))
            .then_some(PermissionPolicyResult::Ask {
                reason: None,
                resolve_approval: None,
                resolve_error: None,
                resolve_non_interactive: None,
            });
        async move { Ok(result) }.boxed()
    }
}

pub(crate) fn evaluate_legacy(
    context: &crate::permission::LegacyPolicyContext<'_>,
) -> Option<crate::permission::LocalPermissionVerdict> {
    if !context.snapshot.dangerous_command_guard {
        return None;
    }
    let path = context
        .file_access
        .iter()
        .find(|path| crate::native::file_type::is_sensitive_file(path))?;
    Some(crate::permission::LocalPermissionVerdict {
        decision: crate::permission::VerdictDecision::Ask,
        policy_name: "SensitiveFileAccessAsk".into(),
        session_approval_rule: None,
        reason: Some(
            crate::i18n::LocalizedText::with_params(
                "engine.permission.sensitiveFileAccess",
                crate::i18n::i18n_params!["path" => path],
            )
            .render(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use crate::permission::{PermissionEngine, PermissionMode, PolicySnapshot, VerdictDecision};
    use serde_json::json;

    #[test]
    fn test_guard_off_disables_sensitive_and_git_path_asks() {
        let sensitive = "/workspace/project/.env";
        let git_path = "/workspace/project/.git/config";

        let guarded = PermissionEngine::new(PolicySnapshot::default());
        let verdict = guarded.evaluate("Read", &json!({ "path": sensitive }));
        assert_eq!(verdict.policy_name, "SensitiveFileAccessAsk");
        let verdict = guarded.evaluate("Read", &json!({ "path": git_path }));
        assert_eq!(verdict.policy_name, "GitControlPathAccessAsk");

        let unguarded = PermissionEngine::new(PolicySnapshot {
            dangerous_command_guard: false,
            ..Default::default()
        });
        for path in [sensitive, git_path] {
            let verdict = unguarded.evaluate("Read", &json!({ "path": path }));
            assert_eq!(
                verdict.decision,
                VerdictDecision::Allow,
                "guards off must let the default-approve policy decide {path}"
            );
            assert_eq!(verdict.policy_name, "DefaultToolApprove");
        }

        let verdict = unguarded.evaluate("Bash", &json!({ "command": "rm -rf /" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "FallbackAsk");
    }

    #[test]
    fn test_yolo_mode_still_intercepts_sensitive_files() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });

        let verdict = engine.evaluate("Read", &json!({ "path": ".env" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "SensitiveFileAccessAsk");
        assert_eq!(
            verdict.reason,
            Some("Access to sensitive file requires approval: .env".into())
        );
        assert!(!verdict.is_allow());

        let verdict = engine.evaluate("Grep", &json!({ "path": "config/.env.local" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "SensitiveFileAccessAsk");
        assert_eq!(
            verdict.reason,
            Some("Access to sensitive file requires approval: config/.env.local".into())
        );
        assert!(!verdict.is_allow());

        let verdict = engine.evaluate("Write", &json!({ "path": "~/.ssh/id_rsa" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "SensitiveFileAccessAsk");
        assert_eq!(
            verdict.reason,
            Some("Access to sensitive file requires approval: ~/.ssh/id_rsa".into())
        );
        assert!(!verdict.is_allow());

        let verdict = engine.evaluate("Edit", &json!({ "path": "ssl\\server.key" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
    }

    #[test]
    fn test_sensitive_file_rules_match_v2() {
        assert!(crate::native::file_type::is_sensitive_file(".env"));
        assert!(crate::native::file_type::is_sensitive_file(".env.local"));
        assert!(crate::native::file_type::is_sensitive_file(
            ".env.production"
        ));
        assert!(crate::native::file_type::is_sensitive_file(
            ".env.development.local"
        ));
        assert!(crate::native::file_type::is_sensitive_file(".ENV"));
        assert!(crate::native::file_type::is_sensitive_file(".Env.Test"));
        assert!(crate::native::file_type::is_sensitive_file("config/.env"));
        assert!(crate::native::file_type::is_sensitive_file(
            "backend/.env.production"
        ));

        assert!(crate::native::file_type::is_sensitive_file("id_rsa"));
        assert!(crate::native::file_type::is_sensitive_file("id_ed25519"));
        assert!(crate::native::file_type::is_sensitive_file("id_ecdsa"));
        assert!(crate::native::file_type::is_sensitive_file("~/.ssh/id_rsa"));
        assert!(crate::native::file_type::is_sensitive_file(
            "/root/.ssh/id_ed25519"
        ));
        assert!(crate::native::file_type::is_sensitive_file("ID_RSA"));
        assert!(crate::native::file_type::is_sensitive_file("id_rsa.bak"));
        assert!(crate::native::file_type::is_sensitive_file("id_rsa-prod"));
        assert!(crate::native::file_type::is_sensitive_file(
            "id_ed25519.old"
        ));
        assert!(crate::native::file_type::is_sensitive_file(
            "credentials.bak"
        ));
        assert!(crate::native::file_type::is_sensitive_file(
            "credentials_backup"
        ));

        assert!(crate::native::file_type::is_sensitive_file("credentials"));
        assert!(crate::native::file_type::is_sensitive_file(
            "~/.aws/credentials"
        ));
        assert!(crate::native::file_type::is_sensitive_file(
            "/root/.gcp/credentials"
        ));
        assert!(crate::native::file_type::is_sensitive_file(
            "path/to/.aws/credentials"
        ));
        assert!(crate::native::file_type::is_sensitive_file(
            "path/to/.aws/credentials/extra"
        ));

        assert!(crate::native::file_type::is_sensitive_file(
            "C:\\Users\\admin\\.ssh\\id_rsa"
        ));
        assert!(crate::native::file_type::is_sensitive_file(
            "app\\config\\.env.local"
        ));
        assert!(crate::native::file_type::is_sensitive_file(
            "C:\\path\\.aws\\credentials"
        ));

        assert!(!crate::native::file_type::is_sensitive_file(".env.example"));
        assert!(!crate::native::file_type::is_sensitive_file(".env.sample"));
        assert!(!crate::native::file_type::is_sensitive_file(
            ".env.template"
        ));
        assert!(!crate::native::file_type::is_sensitive_file("id_rsa.pub"));
        assert!(crate::native::file_type::is_sensitive_file("id_rsa"));
        assert!(!crate::native::file_type::is_sensitive_file(
            "id_ed25519.pub"
        ));
        assert!(!crate::native::file_type::is_sensitive_file("id_ecdsa.pub"));
        assert!(!crate::native::file_type::is_sensitive_file(".ENV.example"));

        assert!(!crate::native::file_type::is_sensitive_file(
            "environment.ts"
        ));
        assert!(!crate::native::file_type::is_sensitive_file("dotenv.js"));
        assert!(!crate::native::file_type::is_sensitive_file(
            "environment.json"
        ));
        assert!(!crate::native::file_type::is_sensitive_file("key.txt"));
        assert!(!crate::native::file_type::is_sensitive_file("keyboard.rs"));
        assert!(!crate::native::file_type::is_sensitive_file("README.md"));
        assert!(!crate::native::file_type::is_sensitive_file("src/main.rs"));
        assert!(!crate::native::file_type::is_sensitive_file("server.key"));
        assert!(!crate::native::file_type::is_sensitive_file("cert.pem"));
        assert!(!crate::native::file_type::is_sensitive_file("identity.pfx"));
        assert!(!crate::native::file_type::is_sensitive_file("certs/ca.pem"));
        assert!(!crate::native::file_type::is_sensitive_file(
            "keys/secret.KEY"
        ));
    }
}
