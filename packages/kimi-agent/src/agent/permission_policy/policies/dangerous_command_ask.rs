use std::sync::{Arc, LazyLock};

use futures_util::future::{BoxFuture, FutureExt};
use regex::Regex;
use serde_json::Value;

use crate::agent::permission_mode::permission_mode::IAgentPermissionModeService;
use crate::agent::permission_policy::types::{
    PermissionDecisionReason, PermissionMode, PermissionPolicy, PermissionPolicyContext,
    PermissionPolicyResult,
};
use crate::agent::permission_rules::config_section::is_dangerous_command_guard_enabled;
use crate::app::bash_parser::bash_parser::{
    BashParseOptions, BashParseResult, BashSyntaxNode, IBashParserService,
};
use crate::app::bash_parser::bash_parser_service::BashParserService;
use crate::app::config::config::IConfigService;
use crate::wire::record::WireAsyncError;

const PARSE_OPTIONS: BashParseOptions = BashParseOptions {
    timeout_ms: Some(500.0),
    max_nodes: Some(10_000.0),
};
const MAX_NESTED_SHELL_DEPTH: usize = 4;
const SKIPPED_COMMAND_CHILDREN: &[&str] = &[
    "variable_assignment",
    "file_redirect",
    "heredoc_redirect",
];
const SIMPLE_DANGEROUS_COMMANDS: &[&str] = &[
    "shutdown",
    "halt",
    "poweroff",
    "reboot",
    "bcdedit",
    "diskpart",
    "format",
    "restart-computer",
    "stop-computer",
    "mkfs",
    "wipefs",
];
const PRIVILEGE_WRAPPERS: &[&str] = &["sudo", "doas"];
const PRIVILEGE_VALUE_OPTIONS: &[&str] = &[
    "-u",
    "--user",
    "-g",
    "--group",
    "-h",
    "--host",
    "-p",
    "--prompt",
    "-C",
    "--close-from",
    "-T",
    "--command-timeout",
    "-U",
    "--other-user",
    "-r",
    "--role",
    "-t",
    "--type",
];
const NESTED_SHELLS: &[&str] = &["sh", "bash", "dash", "zsh", "ksh", "ash"];
const NESTED_SHELL_VALUE_OPTIONS: &[&str] = &[
    "-o",
    "+o",
    "-O",
    "+O",
    "--init-file",
    "--rcfile",
];
const LAUNCH_WRAPPERS: &[&str] = &["env", "command", "exec", "nohup", "builtin", "nice"];
const WRAPPER_VALUE_OPTIONS: &[&str] = &[
    "-u",
    "--unset",
    "-C",
    "--chdir",
    "-S",
    "--split-string",
    "-a",
    "-n",
    "--adjustment",
];
const SYSTEMCTL_DANGEROUS_SUBCOMMANDS: &[&str] = &["poweroff", "reboot", "halt", "kexec"];
const SYSTEMCTL_VALUE_OPTIONS: &[&str] = &["-H", "--host", "-M", "--machine"];
const DD_SAFE_DEVICE_TARGETS: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/dev/stdin",
    "/dev/stdout",
    "/dev/stderr",
];
const RM_SAFE_TEMP_ROOTS: &[&str] = &["/tmp", "/temp"];

static UNSAFE_OPERAND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[$`*?\[\]~]").expect("unsafe operand regex"));
static BACKSLASH_ESCAPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)\\(.)").expect("backslash escape regex"));
static COMMAND_PAYLOAD_OPTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^-[a-zA-Z]*c[a-zA-Z]*$").expect("command payload option regex")
});
static SHORT_OPTION_CLUSTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^-[a-zA-Z]+$").expect("short option cluster regex"));
static ENV_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*=").expect("environment assignment regex")
});

#[derive(Debug, PartialEq, Eq)]
pub enum DangerousVerdict {
    Dangerous(String),
    Unanalyzable(String),
    Safe,
}

#[derive(Clone)]
pub struct DangerousCommandAskPermissionPolicyService {
    bash_parser: Arc<dyn IBashParserService>,
    mode_service: Arc<dyn IAgentPermissionModeService>,
    config: Arc<dyn IConfigService>,
}

impl DangerousCommandAskPermissionPolicyService {
    pub fn new(
        bash_parser: Arc<dyn IBashParserService>,
        mode_service: Arc<dyn IAgentPermissionModeService>,
        config: Arc<dyn IConfigService>,
    ) -> Self {
        Self {
            bash_parser,
            mode_service,
            config,
        }
    }
}

impl PermissionPolicy for DangerousCommandAskPermissionPolicyService {
    fn name(&self) -> &str {
        "dangerous-command-ask"
    }

    fn evaluate(
        &self,
        context: PermissionPolicyContext,
    ) -> BoxFuture<'static, Result<Option<PermissionPolicyResult>, WireAsyncError>> {
        let result = evaluate_dangerous_command(
            self.bash_parser.as_ref(),
            self.config.as_ref(),
            self.mode_service.as_ref(),
            &context,
        );
        async move { Ok(result) }.boxed()
    }
}

fn evaluate_dangerous_command(
    bash_parser: &dyn IBashParserService,
    config: &dyn IConfigService,
    mode_service: &dyn IAgentPermissionModeService,
    context: &PermissionPolicyContext,
) -> Option<PermissionPolicyResult> {
    if !is_dangerous_command_guard_enabled(config)
        || mode_service.mode() == PermissionMode::Auto
        || context.base.tool_call.name != "Bash"
    {
        return None;
    }
    let verdict = match context.base.args.get("command").and_then(Value::as_str) {
        Some(command) => analyze_source(command, 0, bash_parser),
        None => Some(DangerousVerdict::Unanalyzable(String::new())),
    };
    match verdict {
        Some(DangerousVerdict::Dangerous(command)) => {
            let mut reason = PermissionDecisionReason::new();
            reason.insert("dangerous_command".to_string(), Value::String(command));
            Some(PermissionPolicyResult::Ask {
                reason: Some(reason),
                resolve_approval: None,
                resolve_error: None,
                resolve_non_interactive: None,
            })
        }
        Some(DangerousVerdict::Unanalyzable(_)) if mode_service.mode() != PermissionMode::Yolo => {
            let mut reason = PermissionDecisionReason::new();
            reason.insert("unanalyzable_command".to_string(), Value::Bool(true));
            Some(PermissionPolicyResult::Ask {
                reason: Some(reason),
                resolve_approval: None,
                resolve_error: None,
                resolve_non_interactive: None,
            })
        }
        _ => None,
    }
}

pub(crate) fn evaluate_legacy(
    context: &crate::permission::LegacyPolicyContext<'_>,
) -> Option<crate::permission::LocalPermissionVerdict> {
    if context.snapshot.non_interactive
        || !context.snapshot.dangerous_command_guard
        || context.mode == PermissionMode::Auto
        || context.tool_lower != "bash"
    {
        return None;
    }
    let parser = BashParserService::new();
    let verdict = match context.args.get("command").and_then(Value::as_str) {
        Some(command) => analyze_source(command, 0, &parser),
        None => Some(DangerousVerdict::Unanalyzable(String::new())),
    };
    match verdict {
        Some(DangerousVerdict::Dangerous(_)) => Some(crate::permission::LocalPermissionVerdict {
            decision: crate::permission::VerdictDecision::Ask,
            policy_name: "DangerousCommandAsk".into(),
            session_approval_rule: None,
            reason: Some(
                crate::i18n::LocalizedText::new("engine.permission.highRiskShellCommand").render(),
            ),
        }),
        Some(DangerousVerdict::Unanalyzable(_)) if context.mode != PermissionMode::Yolo => {
            Some(crate::permission::LocalPermissionVerdict {
                decision: crate::permission::VerdictDecision::Ask,
                policy_name: "DangerousCommandAsk".into(),
                session_approval_rule: None,
                reason: Some(
                    crate::i18n::LocalizedText::new("engine.permission.shellCommandUnanalyzable")
                        .render(),
                ),
            })
        }
        _ => None,
    }
}

pub fn analyze_bash_command(source: &str) -> DangerousVerdict {
    let parser = BashParserService::new();
    analyze_source(source, 0, &parser).unwrap_or(DangerousVerdict::Safe)
}

fn analyze_source(
    source: &str,
    depth: usize,
    parser: &dyn IBashParserService,
) -> Option<DangerousVerdict> {
    let parsed = parser.parse(source, Some(PARSE_OPTIONS));
    let BashParseResult::Ok { has_error, root } = parsed else {
        return Some(DangerousVerdict::Unanalyzable(source.to_string()));
    };
    if has_error {
        return Some(DangerousVerdict::Unanalyzable(source.to_string()));
    }
    let mut commands = Vec::new();
    collect_commands(&root, &mut commands);
    for command in commands {
        if let Some(verdict) = analyze_command(command, depth, parser) {
            return Some(verdict);
        }
    }
    None
}

fn collect_commands<'a>(node: &'a BashSyntaxNode, out: &mut Vec<&'a BashSyntaxNode>) {
    if node.type_ == "command" {
        out.push(node);
    }
    for child in &node.children {
        collect_commands(child, out);
    }
}

fn analyze_command(
    command: &BashSyntaxNode,
    depth: usize,
    parser: &dyn IBashParserService,
) -> Option<DangerousVerdict> {
    let name_index = command
        .children
        .iter()
        .position(|child| child.type_ == "command_name");
    let name_node = name_index.and_then(|index| command.children.get(index));
    let name_word = name_node.and_then(|node| {
        node.children
            .iter()
            .find(|child| child.is_named)
    });
    let raw_name = name_word.and_then(literal_text);
    let Some(raw_name) = raw_name.filter(|name| !name.is_empty()) else {
        return Some(DangerousVerdict::Unanalyzable(command.text.clone()));
    };
    let mut args = Vec::new();
    let mut dropped = false;
    for child in command
        .children
        .iter()
        .skip(name_index.map(|index| index + 1).unwrap_or(0))
    {
        if SKIPPED_COMMAND_CHILDREN.contains(&child.type_.as_str()) {
            continue;
        }
        match literal_text(child) {
            Some(value) if !value.is_empty() => args.push(value),
            Some(_) => {}
            None => dropped = true,
        }
    }
    analyze_invocation(&normalize_command_name(&raw_name), &args, dropped, depth, parser)
}

fn analyze_invocation(
    name: &str,
    args: &[String],
    dropped: bool,
    depth: usize,
    parser: &dyn IBashParserService,
) -> Option<DangerousVerdict> {
    if PRIVILEGE_WRAPPERS.contains(&name) {
        let rest = drop_leading_options(args, PRIVILEGE_VALUE_OPTIONS);
        let Some(inner) = rest.first() else {
            return dropped.then_some(DangerousVerdict::Unanalyzable(name.to_string()));
        };
        return analyze_invocation(
            &normalize_command_name(inner),
            &rest[1..],
            dropped,
            depth,
            parser,
        );
    }
    if LAUNCH_WRAPPERS.contains(&name) {
        if name == "command" {
            for arg in args {
                if arg == "--" {
                    break;
                }
                if arg == "-" {
                    continue;
                }
                if !arg.starts_with('-') {
                    break;
                }
                if arg.contains(['v', 'V']) {
                    return None;
                }
            }
        }
        let rest = drop_launch_wrapper_operands(name, args);
        let Some(inner) = rest.first() else {
            return dropped.then_some(DangerousVerdict::Unanalyzable(name.to_string()));
        };
        return analyze_invocation(
            &normalize_command_name(inner),
            &rest[1..],
            dropped,
            depth,
            parser,
        );
    }
    if NESTED_SHELLS.contains(&name) {
        let mut payload_index = None;
        let mut index = 0;
        while index < args.len() {
            let arg = &args[index];
            if arg == "--" {
                break;
            }
            if arg.starts_with('-') {
                if NESTED_SHELL_VALUE_OPTIONS.contains(&arg.as_str()) {
                    index += 1;
                } else if COMMAND_PAYLOAD_OPTION.is_match(arg) {
                    payload_index = Some(index + 1);
                }
                index += 1;
                continue;
            }
            break;
        }
        let Some(payload_index) = payload_index else {
            return dropped.then_some(DangerousVerdict::Unanalyzable(name.to_string()));
        };
        let Some(payload) = args.get(payload_index) else {
            return Some(DangerousVerdict::Unanalyzable(name.to_string()));
        };
        if depth >= MAX_NESTED_SHELL_DEPTH {
            return Some(DangerousVerdict::Unanalyzable(payload.clone()));
        }
        return analyze_source(payload, depth + 1, parser);
    }
    if name == "eval" {
        if args.is_empty() {
            return dropped.then_some(DangerousVerdict::Unanalyzable(name.to_string()));
        }
        if dropped || depth >= MAX_NESTED_SHELL_DEPTH {
            return Some(DangerousVerdict::Unanalyzable(args.join(" ")));
        }
        return analyze_source(&args.join(" "), depth + 1, parser);
    }
    if name == "busybox" {
        let Some(applet) = args.first() else {
            return dropped.then_some(DangerousVerdict::Unanalyzable(name.to_string()));
        };
        if applet.starts_with('-') {
            return dropped.then_some(DangerousVerdict::Unanalyzable(name.to_string()));
        }
        return analyze_invocation(
            &normalize_command_name(applet),
            &args[1..],
            dropped,
            depth,
            parser,
        );
    }
    if SIMPLE_DANGEROUS_COMMANDS.contains(&name) || name.starts_with("mkfs.") {
        return Some(DangerousVerdict::Dangerous(name.to_string()));
    }
    if name == "init" || name == "telinit" {
        if args.iter().any(|arg| arg == "0" || arg == "6") {
            return Some(DangerousVerdict::Dangerous(name.to_string()));
        }
        return dropped.then_some(DangerousVerdict::Unanalyzable(name.to_string()));
    }
    if name == "systemctl" {
        let rest = drop_leading_options(args, SYSTEMCTL_VALUE_OPTIONS);
        if let Some(subcommand) = rest.first()
            && SYSTEMCTL_DANGEROUS_SUBCOMMANDS.contains(&subcommand.as_str())
        {
            return Some(DangerousVerdict::Dangerous(format!(
                "systemctl {subcommand}"
            )));
        }
        return dropped.then_some(DangerousVerdict::Unanalyzable(name.to_string()));
    }
    if name == "dd" {
        for arg in args {
            let Some(target) = arg.strip_prefix("of=") else {
                continue;
            };
            if target.starts_with("/dev/") && !DD_SAFE_DEVICE_TARGETS.contains(&target) {
                return Some(DangerousVerdict::Dangerous("dd".to_string()));
            }
        }
        return dropped.then_some(DangerousVerdict::Unanalyzable(name.to_string()));
    }
    if name == "rm" {
        let mut recursive = false;
        let mut force = false;
        let mut operands = Vec::new();
        let mut options_ended = false;
        for arg in args {
            if !options_ended && arg == "--" {
                options_ended = true;
                continue;
            }
            if options_ended {
                operands.push(arg.as_str());
                continue;
            }
            if arg == "--recursive" {
                recursive = true;
            } else if arg == "--force" {
                force = true;
            } else if SHORT_OPTION_CLUSTER.is_match(arg) {
                if arg.contains(['r', 'R']) {
                    recursive = true;
                }
                if arg.contains('f') {
                    force = true;
                }
            } else {
                operands.push(arg.as_str());
            }
        }
        if recursive && force {
            if !dropped
                && !operands.is_empty()
                && operands.iter().all(|operand| is_safe_temp_rm_operand(operand))
            {
                return None;
            }
            return Some(DangerousVerdict::Dangerous("rm -rf".to_string()));
        }
        return dropped.then_some(DangerousVerdict::Unanalyzable(name.to_string()));
    }
    None
}

pub fn normalize_command_name(raw: &str) -> String {
    let mut name = raw;
    if let Some(position) = name.rfind(['/', '\\']) {
        name = &name[position + 1..];
    }
    let mut name = name.to_ascii_lowercase();
    if name.ends_with(".exe") {
        name.truncate(name.len() - 4);
    }
    name
}

fn drop_leading_options(args: &[String], value_options: &[&str]) -> Vec<String> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return args[index + 1..].to_vec();
        }
        if arg == "-" || !arg.starts_with('-') {
            return args[index..].to_vec();
        }
        if !arg.contains('=') && value_options.contains(&arg.as_str()) {
            index += 1;
        }
        index += 1;
    }
    Vec::new()
}

fn drop_launch_wrapper_operands(name: &str, args: &[String]) -> Vec<String> {
    let mut rest = drop_leading_options(args, WRAPPER_VALUE_OPTIONS);
    if name == "env" {
        let mut index = usize::from(rest.first().map(String::as_str) == Some("-"));
        while index < rest.len() && ENV_ASSIGNMENT.is_match(&rest[index]) {
            index += 1;
        }
        rest = rest[index..].to_vec();
    }
    rest
}

fn is_safe_temp_rm_operand(operand: &str) -> bool {
    if operand.split('/').any(|segment| segment == "..") {
        return false;
    }
    RM_SAFE_TEMP_ROOTS.iter().any(|root| {
        operand == *root
            || operand
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

fn literal_text(node: &BashSyntaxNode) -> Option<String> {
    match node.type_.as_str() {
        "word" => {
            let raw = node.text.as_str();
            if UNSAFE_OPERAND.is_match(raw) {
                return None;
            }
            let unescaped = BACKSLASH_ESCAPE.replace_all(raw, "$1");
            if UNSAFE_OPERAND.is_match(&unescaped) {
                return None;
            }
            Some(unescaped.into_owned())
        }
        "number" => Some(node.text.clone()),
        "raw_string" => {
            if node.text.len() < 2 {
                return None;
            }
            let value = node.text.get(1..node.text.len() - 1)?;
            if UNSAFE_OPERAND.is_match(value) {
                return None;
            }
            Some(value.to_string())
        }
        "string" => {
            let mut value = String::new();
            for child in &node.children {
                if child.type_ == "string_content" {
                    value.push_str(&child.text);
                } else if child.is_named {
                    return None;
                }
            }
            if UNSAFE_OPERAND.is_match(&value) {
                return None;
            }
            Some(value)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{PermissionEngine, PolicySnapshot, VerdictDecision};
    use serde_json::json;

    #[test]
    fn test_direct_dangerous_commands() {
        assert_eq!(
            analyze_bash_command("shutdown -h now"),
            DangerousVerdict::Dangerous("shutdown".into())
        );
        assert_eq!(
            analyze_bash_command("reboot"),
            DangerousVerdict::Dangerous("reboot".into())
        );
        assert_eq!(
            analyze_bash_command("format C: /q"),
            DangerousVerdict::Dangerous("format".into())
        );
    }

    #[test]
    fn test_sudo_wrapper_dangerous_commands() {
        assert_eq!(
            analyze_bash_command("sudo reboot"),
            DangerousVerdict::Dangerous("reboot".into())
        );
        assert_eq!(
            analyze_bash_command("sudo -u root poweroff"),
            DangerousVerdict::Dangerous("poweroff".into())
        );
        assert_eq!(
            analyze_bash_command("nohup sudo systemctl reboot &"),
            DangerousVerdict::Dangerous("systemctl reboot".into())
        );
    }

    #[test]
    fn test_nested_shell_dangerous_commands() {
        assert_eq!(
            analyze_bash_command("bash -c 'sudo dd if=/dev/zero of=/dev/sda bs=1M'"),
            DangerousVerdict::Dangerous("dd".into())
        );
    }

    #[test]
    fn test_control_structures_and_substitutions_are_analyzed() {
        for dangerous in [
            "echo $(reboot)",
            "echo `reboot`",
            "(reboot)",
            "if true; then reboot; fi",
            "for x in a; do reboot; done",
            "env FOO=1 reboot",
            "bash -lc 'reboot'",
            "sh -c 'if true; then reboot; fi'",
            "sudo env FOO=1 reboot",
            "dd of=/dev/vda",
            "dd of=/dev/mapper/vg-root",
        ] {
            assert!(
                matches!(
                    analyze_bash_command(dangerous),
                    DangerousVerdict::Dangerous(_)
                ),
                "expected dangerous: {dangerous}"
            );
        }
        for safe in [
            "echo $(date)",
            "echo \"$(date)\"",
            "dd of=/dev/null",
            "dd of=/dev/zero",
            "env FOO=1 echo hi",
            "bash -lc 'echo hi'",
        ] {
            assert_eq!(
                analyze_bash_command(safe),
                DangerousVerdict::Safe,
                "expected safe: {safe}"
            );
        }
    }

    #[test]
    fn test_rm_rf_temp_paths_are_exempt() {
        for allowed in [
            "rm -rf /tmp/build",
            "rm -rf /temp/cache",
            "rm -rf /tmp",
            "rm -rf -- /tmp/build",
            "rm -r -f /tmp/a /tmp/b",
            "sudo rm -rf /tmp/build",
            "bash -c 'rm -rf /tmp/build'",
        ] {
            assert_eq!(
                analyze_bash_command(allowed),
                DangerousVerdict::Safe,
                "expected safe: {allowed}"
            );
        }
    }

    #[test]
    fn test_rm_rf_outside_temp_paths_stay_dangerous() {
        for dangerous in [
            "rm -rf /tmp/build /root",
            "rm -rf /tmp/../etc/passwd",
            "rm -rf /tmpfoo",
            "rm -rf /tmp/$USER",
            "rm -rf /tmp/*",
            "rm -rf /tmp/`whoami`",
            "rm -rf",
            "rm -rf --",
            "rm -rf -",
        ] {
            assert_eq!(
                analyze_bash_command(dangerous),
                DangerousVerdict::Dangerous("rm -rf".into()),
                "expected dangerous: {dangerous}"
            );
        }
    }

    #[test]
    fn test_rm_rf_and_normalization_commands() {
        assert_eq!(
            analyze_bash_command("/bin/rm -rf ./build"),
            DangerousVerdict::Dangerous("rm -rf".into())
        );
        assert_eq!(
            analyze_bash_command("rm -r -f dist"),
            DangerousVerdict::Dangerous("rm -rf".into())
        );
        assert_eq!(
            analyze_bash_command("rm --recursive --force node_modules"),
            DangerousVerdict::Dangerous("rm -rf".into())
        );
        assert_eq!(
            analyze_bash_command("/sbin/mkfs.ext4 /dev/sdb1"),
            DangerousVerdict::Dangerous("mkfs.ext4".into())
        );
        assert_eq!(
            analyze_bash_command("init 0"),
            DangerousVerdict::Dangerous("init".into())
        );
        assert_eq!(
            analyze_bash_command("busybox rm -rf temp"),
            DangerousVerdict::Dangerous("rm -rf".into())
        );
        assert_eq!(
            analyze_bash_command("eval 'rm -rf caches'"),
            DangerousVerdict::Dangerous("rm -rf".into())
        );
        assert_eq!(
            analyze_bash_command("C:\\Windows\\System32\\shutdown.exe /s /t 0"),
            DangerousVerdict::Dangerous("shutdown".into())
        );
    }

    #[test]
    fn test_unanalyzable_shapes() {
        for cmd in ["echo \"unterminated", "$CMD --force"] {
            assert!(
                matches!(analyze_bash_command(cmd), DangerousVerdict::Unanalyzable(_)),
                "must be unanalyzable: {cmd}"
            );
        }
        assert_eq!(
            analyze_bash_command("sudo $CMD reboot"),
            DangerousVerdict::Dangerous("reboot".into())
        );
    }

    #[test]
    fn test_dangerous_wins_over_unanalyzable() {
        assert!(matches!(
            analyze_bash_command("sudo reboot; echo \"unterminated"),
            DangerousVerdict::Unanalyzable(_)
        ));
    }

    #[test]
    fn test_variable_arguments_stay_analyzable() {
        assert_eq!(analyze_bash_command("echo $HOME"), DangerousVerdict::Safe);
        assert_eq!(
            analyze_bash_command("bash -c \"echo $HOME\""),
            DangerousVerdict::Safe
        );
    }

    #[test]
    fn test_safe_commands_allowed() {
        assert_eq!(analyze_bash_command("ls -la"), DangerousVerdict::Safe);
        assert_eq!(analyze_bash_command("git status"), DangerousVerdict::Safe);
        assert_eq!(analyze_bash_command("cargo check"), DangerousVerdict::Safe);
        assert_eq!(
            analyze_bash_command("dd if=/dev/zero of=/dev/null bs=1M"),
            DangerousVerdict::Safe
        );
        assert_eq!(analyze_bash_command("rm file.txt"), DangerousVerdict::Safe);
    }

    #[test]
    fn test_command_query_does_not_execute_its_operand() {
        assert_eq!(analyze_bash_command("command -v reboot"), DangerousVerdict::Safe);
        assert_eq!(analyze_bash_command("command -V rm"), DangerousVerdict::Safe);
        assert_eq!(
            analyze_bash_command("command reboot"),
            DangerousVerdict::Dangerous("reboot".into())
        );
    }

    #[test]
    fn test_nested_shell_value_options_are_skipped() {
        assert_eq!(
            analyze_bash_command("bash --init-file startup -c reboot"),
            DangerousVerdict::Dangerous("reboot".into())
        );
        assert_eq!(
            analyze_bash_command("systemctl -H host reboot"),
            DangerousVerdict::Dangerous("systemctl reboot".into())
        );
    }

    #[test]
    fn test_escaped_and_expanded_operands_follow_literal_rules() {
        assert_eq!(
            analyze_bash_command("rm -rf /tmp/\\$USER"),
            DangerousVerdict::Dangerous("rm -rf".into())
        );
        assert_eq!(
            analyze_bash_command("rm -rf /tmp/$USER"),
            DangerousVerdict::Dangerous("rm -rf".into())
        );
    }

#[test]
    fn a_session_approval_does_not_exempt_a_dangerous_command() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            session_approvals: vec!["Bash".into()],
            ..Default::default()
        });

        let dangerous = engine.evaluate("bash", &json!({ "command": "rm -rf /" }));
        assert_eq!(dangerous.decision, VerdictDecision::Ask);
        assert_eq!(dangerous.policy_name, "DangerousCommandAsk");

        let benign = engine.evaluate("bash", &json!({ "command": "git status" }));
        assert_eq!(benign.decision, VerdictDecision::Allow);
        assert_eq!(benign.policy_name, "SessionApprovalHistory");
    }

#[test]
    fn test_dangerous_bash_command_asks_in_manual_and_yolo() {
        let manual = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });
        let auto = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Auto,
            ..Default::default()
        });
        let yolo = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });

        for cmd in ["sudo reboot", "shutdown -h now", "rm -rf /", "format C: /q"] {
            for engine in [&manual, &yolo] {
                let verdict = engine.evaluate("bash", &json!({ "command": cmd }));
                assert_eq!(verdict.decision, VerdictDecision::Ask, "cmd: {cmd}");
                assert_eq!(verdict.policy_name, "DangerousCommandAsk");
                assert_eq!(
                    verdict.reason.as_deref(),
                    Some("High-risk shell command requires approval")
                );
            }
            let verdict = auto.evaluate("bash", &json!({ "command": cmd }));
            assert_eq!(verdict.decision, VerdictDecision::Allow, "cmd: {cmd}");
            assert_eq!(verdict.policy_name, "AutoModeApprove");
        }

        for engine in [&auto, &yolo] {
            let benign = engine.evaluate("bash", &json!({ "command": "git status" }));
            assert_eq!(benign.decision, VerdictDecision::Allow, "benign command");
        }
        let benign = manual.evaluate("bash", &json!({ "command": "git status" }));
        assert_eq!(benign.decision, VerdictDecision::Ask);
        assert_eq!(benign.policy_name, "FallbackAsk");
    }

#[test]
    fn test_dangerous_command_guard_off_skips_the_policy() {
        let guard_on = PermissionEngine::new(PolicySnapshot {
            dangerous_command_guard: true,
            ..Default::default()
        });
        let guard_off = PermissionEngine::new(PolicySnapshot {
            dangerous_command_guard: false,
            ..Default::default()
        });

        let verdict = guard_on.evaluate("bash", &json!({ "command": "sudo reboot" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "DangerousCommandAsk");

        let verdict = guard_off.evaluate("bash", &json!({ "command": "sudo reboot" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "FallbackAsk");

        let default = PermissionEngine::new(PolicySnapshot::default());
        let verdict = default.evaluate("bash", &json!({ "command": "sudo reboot" }));
        assert_eq!(verdict.policy_name, "DangerousCommandAsk");
    }

#[test]
    fn test_unanalyzable_bash_command_asks_except_in_auto_and_yolo() {
        let manual = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });
        let auto = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Auto,
            ..Default::default()
        });
        let yolo = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });

        for cmd in ["echo \"unterminated", "$CMD --force"] {
            let verdict = manual.evaluate("bash", &json!({ "command": cmd }));
            assert_eq!(verdict.decision, VerdictDecision::Ask, "cmd: {cmd}");
            assert_eq!(verdict.policy_name, "DangerousCommandAsk");
            assert!(
                verdict
                    .reason
                    .as_deref()
                    .is_some_and(|r| r.contains("could not be statically analyzed")),
                "reason: {:?}",
                verdict.reason
            );
        }

        let verdict = auto.evaluate("bash", &json!({ "command": "echo \"unterminated" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "AutoModeApprove");
        let verdict = yolo.evaluate("bash", &json!({ "command": "echo \"unterminated" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");
    }

#[test]
    fn test_non_interactive_session_skips_dangerous_command_ask() {
        let auto = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Auto,
            non_interactive: true,
            ..Default::default()
        });
        let verdict = auto.evaluate("bash", &json!({ "command": "sudo reboot" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "AutoModeApprove");
    }

#[test]
    fn test_benign_command_verdict_tracks_permission_mode() {
        let cmd = r#"ls "D:\work\示例 项目\src\machines" 2>&1 | head -50"#;
        for (mode, policy, expected) in [
            (PermissionMode::Manual, "FallbackAsk", VerdictDecision::Ask),
            (
                PermissionMode::Auto,
                "AutoModeApprove",
                VerdictDecision::Allow,
            ),
            (
                PermissionMode::Yolo,
                "YoloModeApprove",
                VerdictDecision::Allow,
            ),
        ] {
            let engine = PermissionEngine::new(PolicySnapshot {
                mode,
                ..Default::default()
            });
            for tool in ["Bash", "bash"] {
                let verdict = engine.evaluate(tool, &json!({ "command": cmd }));
                assert_eq!(verdict.decision, expected, "{mode:?} / {tool} decision");
                assert_eq!(verdict.policy_name, policy, "{mode:?} / {tool} policy");
            }
        }
    }
}