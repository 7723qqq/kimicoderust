use globset::{GlobBuilder, GlobMatcher};

use super::path_access::{PathClass, canonicalize_path, expand_user_path};

pub struct PermissionPathMatchOptions<'a> {
    pub cwd: Option<&'a str>,
    pub path_class: Option<PathClass>,
    pub home_dir: Option<&'a str>,
    pub case_insensitive_paths: Option<bool>,
}

pub fn glob_match(value: &str, pattern: &str, nocase: bool) -> bool {
    if compile(pattern, nocase).is_some_and(|matcher| matcher.is_match(value)) {
        return true;
    }
    let normalized_value = strip_leading_dot_slash(value);
    let normalized_pattern = strip_leading_dot_slash(pattern);
    if normalized_value == value && normalized_pattern == pattern {
        return false;
    }
    compile(normalized_pattern, nocase).is_some_and(|matcher| matcher.is_match(normalized_value))
}

fn compile(pattern: &str, nocase: bool) -> Option<GlobMatcher> {
    GlobBuilder::new(pattern)
        .case_insensitive(nocase)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .ok()
        .map(|glob| glob.compile_matcher())
}

fn strip_leading_dot_slash(value: &str) -> &str {
    value.strip_prefix("./").unwrap_or(value)
}

pub fn path_glob_match(
    value: &str,
    pattern: &str,
    options: Option<&PermissionPathMatchOptions<'_>>,
) -> bool {
    let path_class = resolve_path_class(value, pattern, options);
    let nocase = options
        .and_then(|options| options.case_insensitive_paths)
        .unwrap_or(true);
    if glob_match(value, pattern, nocase) {
        return true;
    }
    let value_variants = path_variants(value, path_class, options);
    let pattern_variants = path_variants(pattern, path_class, options);
    value_variants.iter().any(|value| {
        pattern_variants
            .iter()
            .any(|pattern| glob_match(value, pattern, nocase))
    })
}

fn resolve_path_class(
    value: &str,
    pattern: &str,
    options: Option<&PermissionPathMatchOptions<'_>>,
) -> PathClass {
    if let Some(path_class) = options.and_then(|options| options.path_class) {
        return path_class;
    }
    let looks_win32 = |candidate: &str| {
        let bytes = candidate.as_bytes();
        (bytes.len() >= 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes.len() == 2 || matches!(bytes[2], b'\\' | b'/')))
            || candidate.starts_with("\\\\")
            || candidate.contains('\\')
    };
    if looks_win32(value) || looks_win32(pattern) {
        PathClass::Win32
    } else {
        PathClass::Posix
    }
}

fn path_variants(
    value: &str,
    path_class: PathClass,
    options: Option<&PermissionPathMatchOptions<'_>>,
) -> Vec<String> {
    let mut variants = vec![value.to_string()];
    push_variant(
        &mut variants,
        strip_leading_dot_path(value, path_class),
        path_class,
    );
    if let Some(canonical) = canonicalize_pattern(value, path_class, options) {
        push_variant(&mut variants, canonical, path_class);
    }
    variants.sort();
    variants.dedup();
    variants
}

fn push_variant(variants: &mut Vec<String>, value: String, path_class: PathClass) {
    variants.push(value.clone());
    if path_class == PathClass::Win32 {
        variants.push(value.replace('\\', "/"));
    }
}

fn strip_leading_dot_path(value: &str, path_class: PathClass) -> String {
    if let Some(stripped) = value.strip_prefix("./") {
        return stripped.to_string();
    }
    if path_class == PathClass::Win32
        && let Some(stripped) = value.strip_prefix(".\\")
    {
        return stripped.to_string();
    }
    value.to_string()
}

fn canonicalize_pattern(
    value: &str,
    path_class: PathClass,
    options: Option<&PermissionPathMatchOptions<'_>>,
) -> Option<String> {
    let expanded = expand_user_path(
        value,
        options.and_then(|options| options.home_dir),
        path_class,
    );
    let cwd = options
        .and_then(|options| options.cwd)
        .map(str::to_string)
        .or_else(|| default_cwd(&expanded, path_class));
    canonicalize_path(&expanded, &cwd?, path_class).ok()
}

fn default_cwd(value: &str, _path_class: PathClass) -> Option<String> {
    let bytes = value.as_bytes();
    let absolute_drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    let absolute_slash = match bytes.first() {
        Some(&b'/') | Some(&b'\\') => {
            bytes.len() == 1
                || !matches!(bytes[1], b'/' | b'\\')
                || bytes.get(2) != Some(&b'.')
        }
        _ => false,
    };
    if !absolute_drive && !absolute_slash {
        return None;
    }
    if absolute_drive {
        Some(format!("{}:/", bytes[0] as char))
    } else {
        Some("/".to_string())
    }
}

const GLOB_LITERAL_SPECIAL: &[char] = &[
    '\\', '*', '?', '[', ']', '{', '}', '(', ')', '!', '+', '@', '|',
];

pub fn literal_rule_pattern(tool_name: &str, subject: &str) -> String {
    format!("{tool_name}({})", escape_rule_subject_literal(subject))
}

pub fn escape_rule_subject_literal(subject: &str) -> String {
    let mut escaped = String::with_capacity(subject.len());
    for character in subject.chars() {
        if GLOB_LITERAL_SPECIAL.contains(&character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

pub fn matches_glob_rule_subject(rule_args: &str, subject: &str) -> bool {
    match_rule_subjects(rule_args, &[subject], |pattern, value| {
        glob_match(value, pattern, false)
    })
}

pub fn matches_path_rule_subject(
    rule_args: &str,
    subject: &str,
    options: Option<&PermissionPathMatchOptions<'_>>,
) -> bool {
    match_rule_subjects(rule_args, &[subject], |pattern, value| {
        path_glob_match(value, pattern, options)
    })
}

fn match_rule_subjects(
    rule_args: &str,
    subjects: &[&str],
    matches_positive_pattern: impl Fn(&str, &str) -> bool,
) -> bool {
    if rule_args.is_empty() {
        return true;
    }
    let (negated, pattern) = match rule_args.strip_prefix('!') {
        Some(pattern) => (true, pattern),
        None => (false, rule_args),
    };
    let hit = subjects
        .iter()
        .any(|subject| matches_positive_pattern(pattern, subject));
    if negated { !hit } else { hit }
}

pub struct RuleSubjectVariants {
    pub variants: Vec<String>,
    pub resolved: Option<String>,
}

impl RuleSubjectVariants {
    fn empty() -> Self {
        Self {
            variants: Vec::new(),
            resolved: None,
        }
    }
}

static CWD_MISSING_WARNED: std::sync::Once = std::sync::Once::new();
static SUBJECT_RESOLVE_WARNED: std::sync::Once = std::sync::Once::new();

pub fn rule_subject_variants(
    subject: Option<&crate::agent::permission_rules::matches_rule::RuleSubject>,
    cwd: Option<&str>,
) -> RuleSubjectVariants {
    let Some(subject) = subject else {
        return RuleSubjectVariants::empty();
    };
    let raw = subject.value.clone();
    let mut variants = vec![raw.clone()];
    if subject.kind != crate::agent::permission_rules::matches_rule::RuleSubjectKind::Path {
        return RuleSubjectVariants {
            variants,
            resolved: None,
        };
    }
    let Some(cwd) = cwd.filter(|cwd| !cwd.is_empty()) else {
        CWD_MISSING_WARNED.call_once(|| {
            tracing::warn!(
                subject = %raw,
                "permission path subject has no workspace cwd; only the raw spelling is matched, so relative rules may not apply"
            );
        });
        return RuleSubjectVariants {
            variants,
            resolved: None,
        };
    };
    let path_class = if cwd.starts_with('/') {
        PathClass::Posix
    } else {
        PathClass::Win32
    };
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| home.to_string_lossy().to_string());
    let expanded = expand_user_path(&raw, home.as_deref(), path_class);
    let Ok(lexical) = canonicalize_path(&expanded, cwd, path_class) else {
        SUBJECT_RESOLVE_WARNED.call_once(|| {
            tracing::warn!(
                subject = %raw,
                "permission path subject could not be resolved; only the raw spelling is matched"
            );
        });
        return RuleSubjectVariants {
            variants,
            resolved: None,
        };
    };
    variants.push(lexical.clone());
    let resolved = lexical;
    let slash_variants: Vec<String> = variants
        .iter()
        .filter(|variant| variant.contains('\\'))
        .map(|variant| variant.replace('\\', "/"))
        .collect();
    variants.extend(slash_variants);
    variants.sort();
    variants.dedup();
    RuleSubjectVariants {
        variants,
        resolved: Some(resolved),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{PermissionEngine, PolicySnapshot};
    use serde_json::json;

    #[test]
    fn literals_escape_every_glob_special_character() {
        assert_eq!(escape_rule_subject_literal("a*b?c"), "a\\*b\\?c");
        assert_eq!(
            literal_rule_pattern("Bash", "src/[a].ts"),
            "Bash(src/\\[a\\].ts)"
        );
        assert!(matches_glob_rule_subject(
            &escape_rule_subject_literal("src/[a].ts"),
            "src/[a].ts"
        ));
        assert!(!matches_glob_rule_subject(
            &escape_rule_subject_literal("src/[a].ts"),
            "src/a.ts"
        ));
    }

    #[test]
    fn negated_rule_args_invert_the_match() {
        assert!(matches_glob_rule_subject("src/*", "src/a.ts"));
        assert!(!matches_glob_rule_subject("!src/*", "src/a.ts"));
        assert!(matches_glob_rule_subject("!src/*", "docs/a.md"));
        assert!(matches_glob_rule_subject("", "anything"));
    }

    #[test]
    fn paths_match_with_slash_and_case_variants() {
        assert!(path_glob_match(
            "C:\\work\\src\\a.ts",
            "c:/work/src/*.ts",
            None
        ));
        assert!(path_glob_match("src/a.ts", "src/**/*.ts", None));
        assert!(!path_glob_match("/w/src/a.ts", "*.ts", None));
    }

    #[test]
    fn a_relative_pattern_resolves_against_the_cwd() {
        let options = PermissionPathMatchOptions {
            cwd: Some("/work/project"),
            path_class: Some(PathClass::Posix),
            home_dir: None,
            case_insensitive_paths: None,
        };
        assert!(path_glob_match(
            "/work/project/src/a.ts",
            "src/*.ts",
            Some(&options)
        ));
    }

    #[test]
    fn home_patterns_expand() {
        let options = PermissionPathMatchOptions {
            cwd: Some("/work"),
            path_class: Some(PathClass::Posix),
            home_dir: Some("/home/kimi"),
            case_insensitive_paths: None,
        };
        assert!(path_glob_match(
            "/home/kimi/.config/app.toml",
            "~/.config/*.toml",
            Some(&options)
        ));
    }

#[test]
    fn test_rule_subject_resolves_dot_dot_traversal() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("secret.txt"), "x").unwrap();
        let root = std::fs::canonicalize(ws.path()).unwrap();
        let secret = std::fs::canonicalize(root.join("secret.txt")).unwrap();
        let rule_path = secret.to_string_lossy().replace('\\', "/");
        let engine = PermissionEngine::with_workspace(
            PolicySnapshot {
                mode: PermissionMode::Manual,
                deny_rules: vec![format!("Read({rule_path})")],
                ..Default::default()
            },
            Some(root.to_string_lossy().to_string()),
            Vec::new(),
        );
        let traversal = root.join("sub").join("..").join("secret.txt");
        let verdict = engine.evaluate("Read", &json!({ "path": traversal.to_string_lossy() }));
        assert_eq!(verdict.decision, VerdictDecision::Deny);
        assert_eq!(verdict.policy_name, "UserConfiguredDeny");
    }
}