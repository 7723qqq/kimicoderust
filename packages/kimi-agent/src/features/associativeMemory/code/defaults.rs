use super::super::types::{CodeAction, CodeScope, HardRule, MemoryCode};

const CREDENTIAL_PATTERN: &str =
    "(sk-[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|ghp_[A-Za-z0-9]{36}|xox[baprs]-[A-Za-z0-9-]{10,})";
const PRIVATE_KEY_PATTERN: &str =
    "-----BEGIN (RSA |EC |OPENSSH |PGP )?PRIVATE KEY-----";

pub fn default_hard_rules() -> Vec<HardRule> {
    vec![
        HardRule {
            id: "credential".into(),
            pattern: CREDENTIAL_PATTERN.into(),
            regex: true,
            action: CodeAction::BlockWrite,
            scope: CodeScope::Note,
            reason: "credential-like secret".into(),
        },
        HardRule {
            id: "private-key".into(),
            pattern: PRIVATE_KEY_PATTERN.into(),
            regex: true,
            action: CodeAction::BlockWrite,
            scope: CodeScope::Note,
            reason: "private key block".into(),
        },
    ]
}

pub const DEFAULT_GUIDANCE: &str = "Never store credentials, private keys, or other secrets. When the user explicitly asks to remember something, treat it as high priority. When two memories conflict, prefer what the user stated explicitly over an inference, and newer over older. Do not surface a memory the code forbids.";

pub fn default_code() -> MemoryCode {
    MemoryCode {
        rules: default_hard_rules(),
        guidance: DEFAULT_GUIDANCE.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_code_carries_v2s_two_rules_and_guidance() {
        let code = default_code();
        assert_eq!(code.rules.len(), 2);
        assert_eq!(code.rules[0].id, "credential");
        assert_eq!(code.rules[1].id, "private-key");
        assert!(code.rules.iter().all(|rule| rule.regex));
        assert_eq!(code.rules[0].action, CodeAction::BlockWrite);
        assert_eq!(code.rules[0].scope, CodeScope::Note);
        assert!(code.guidance.contains("Never store credentials"));
        assert!(code.guidance.contains("prefer what the user stated explicitly"));
    }

    #[test]
    fn the_default_patterns_match_the_v2_samples() {
        let code = default_code();
        let credential = &code.rules[0].pattern;
        for sample in [
            "sk-abcdefghijklmnopqrstuvwxyz",
            "AKIAIOSFODNN7EXAMPLE",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "xoxb-1234567890-abcd",
        ] {
            let regex = regex::RegexBuilder::new(credential)
                .case_insensitive(true)
                .build()
                .expect("the default pattern compiles");
            assert!(regex.is_match(sample), "{sample}");
        }
        let key = &code.rules[1].pattern;
        let regex = regex::RegexBuilder::new(key)
            .build()
            .expect("the private-key pattern compiles");
        assert!(regex.is_match("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(regex.is_match("-----BEGIN PRIVATE KEY-----"));
        assert!(!regex.is_match("-----END PRIVATE KEY-----"));
    }
}
