use std::path::{Component, Path, PathBuf};

use crate::tools::tower::paths::{COMMS_DIR, TOWER_ROOT, WORKTREES_DIR};

const MAX_NESTED_PARSE_DEPTH: usize = 4;
const TOWER_GUARD_ESCAPE: &str = "to retire the tower workspace deliberately, turn tower mode off and remove .tower/comms manually in a terminal — this guard only intercepts agent Bash calls.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerBashGuardTarget {
    pub command: String,
    pub cwd: PathBuf,
    pub main_checkout: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerBashScript {
    pub invocations: Vec<TowerBashInvocation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerBashInvocation {
    pub name: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone)]
struct TowerLayout {
    main_checkout: PathBuf,
    tower_root: PathBuf,
    comms_dir: PathBuf,
    worktrees_dir: PathBuf,
}

impl TowerLayout {
    fn new(main_checkout: &Path) -> Self {
        Self {
            main_checkout: main_checkout.to_path_buf(),
            tower_root: main_checkout.join(TOWER_ROOT),
            comms_dir: main_checkout.join(COMMS_DIR),
            worktrees_dir: main_checkout.join(WORKTREES_DIR),
        }
    }
}

pub fn command_needs_tower_guard(command: &str) -> bool {
    let rm = regex::Regex::new(r"\brm\b").expect("valid rm regex");
    let git = regex::Regex::new(r"\bgit\b").expect("valid git regex");
    let clean = regex::Regex::new(r"\bclean\b").expect("valid clean regex");
    let reset = regex::Regex::new(r"\breset\b").expect("valid reset regex");
    rm.is_match(command)
        || (git.is_match(command) && (clean.is_match(command) || reset.is_match(command)))
}

pub fn tower_workspace_owned_json(raw: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|state| {
            state
                .get("sessionId")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .is_some_and(|session_id| !session_id.is_empty())
}

pub fn analyze_tower_bash_command(
    target: &TowerBashGuardTarget,
    parse: fn(&str) -> Result<TowerBashScript, String>,
) -> Option<String> {
    let layout = TowerLayout::new(&target.main_checkout);
    analyze_source(&target.command, target.cwd.clone(), &layout, &parse, 0)
}

pub fn parse_shell_script(source: &str) -> Result<TowerBashScript, String> {
    let mut invocations = Vec::new();
    let mut name: Option<String> = None;
    let mut args = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut skip_redirect_target = false;
    let mut chars = source.chars().peekable();

    fn finish_word(
        word: &mut String,
        name: &mut Option<String>,
        args: &mut Vec<String>,
        skip: &mut bool,
    ) {
        if word.is_empty() {
            return;
        }
        if *skip {
            *skip = false;
        } else if name.is_none() {
            *name = Some(word.clone());
        } else {
            args.push(word.clone());
        }
        word.clear();
    }
    fn finish_invocation(
        word: &mut String,
        name: &mut Option<String>,
        args: &mut Vec<String>,
        invocations: &mut Vec<TowerBashInvocation>,
        skip: &mut bool,
    ) {
        finish_word(word, name, args, skip);
        if let Some(name) = name.take() {
            invocations.push(TowerBashInvocation {
                name,
                args: std::mem::take(args),
            });
        }
    }

    while let Some(ch) = chars.next() {
        if escaped {
            word.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            } else if ch == '`' || (ch == '$' && chars.peek() == Some(&'(') && active == '"') {
                return Err("command substitution needs a full shell parser".into());
            } else {
                word.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '`' => return Err("backtick substitution needs a full shell parser".into()),
            '$' if chars.peek() == Some(&'(') => {
                return Err("command substitution needs a full shell parser".into());
            }
            '>' | '<' => {
                finish_word(&mut word, &mut name, &mut args, &mut skip_redirect_target);
                skip_redirect_target = true;
            }
            '&' | '|' | ';' | '\n' => {
                finish_invocation(
                    &mut word,
                    &mut name,
                    &mut args,
                    &mut invocations,
                    &mut skip_redirect_target,
                );
            }
            ch if ch.is_whitespace() => {
                finish_word(&mut word, &mut name, &mut args, &mut skip_redirect_target)
            }
            _ => word.push(ch),
        }
    }
    if escaped || quote.is_some() {
        return Err("unterminated shell escape or quote".into());
    }
    finish_invocation(
        &mut word,
        &mut name,
        &mut args,
        &mut invocations,
        &mut skip_redirect_target,
    );
    Ok(TowerBashScript { invocations })
}

fn analyze_source(
    source: &str,
    cwd: PathBuf,
    layout: &TowerLayout,
    parse: &fn(&str) -> Result<TowerBashScript, String>,
    depth: usize,
) -> Option<String> {
    let script = match parse(source) {
        Ok(script) => script,
        Err(_) => return Some(unanalyzable_veto()),
    };
    analyze_script(script, cwd, layout, parse, depth).0
}

fn analyze_script(
    script: TowerBashScript,
    mut cwd: PathBuf,
    layout: &TowerLayout,
    parse: &fn(&str) -> Result<TowerBashScript, String>,
    depth: usize,
) -> (Option<String>, PathBuf) {
    for invocation in script.invocations {
        let (veto, next) = analyze_invocation(invocation, cwd, layout, parse, depth);
        if veto.is_some() {
            return (veto, next);
        }
        cwd = next;
    }
    (None, cwd)
}

fn analyze_invocation(
    invocation: TowerBashInvocation,
    cwd: PathBuf,
    layout: &TowerLayout,
    parse: &fn(&str) -> Result<TowerBashScript, String>,
    depth: usize,
) -> (Option<String>, PathBuf) {
    let TowerBashInvocation { name, mut args } = invocation;
    let name = normalize_command_name(&name);
    if name.contains('=') {
        return (None, cwd);
    }
    if matches!(name.as_str(), "sudo" | "command" | "env" | "xargs") {
        while args
            .first()
            .is_some_and(|arg| arg.starts_with('-') || (name == "env" && arg.contains('=')))
        {
            args.remove(0);
        }
        let Some(inner) = args.first().cloned() else {
            return (None, cwd);
        };
        return analyze_invocation(
            TowerBashInvocation {
                name: inner,
                args: args.into_iter().skip(1).collect(),
            },
            cwd,
            layout,
            parse,
            depth,
        );
    }
    if matches!(name.as_str(), "bash" | "sh" | "zsh" | "dash") {
        let payload = args.iter().position(|arg| {
            arg == "-c" || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('c'))
        });
        let Some(index) = payload else {
            return (None, cwd);
        };
        let Some(payload) = args.get(index + 1) else {
            return (None, cwd);
        };
        if depth >= MAX_NESTED_PARSE_DEPTH {
            return (None, cwd);
        }
        return (
            analyze_source(payload, cwd.clone(), layout, parse, depth + 1),
            cwd,
        );
    }
    if name == "eval" {
        if args.is_empty() || depth >= MAX_NESTED_PARSE_DEPTH {
            return (None, cwd);
        }
        return (
            analyze_source(&args.join(" "), cwd.clone(), layout, parse, depth + 1),
            cwd,
        );
    }
    if name == "cd" {
        let target = args
            .iter()
            .find(|arg| !matches!(arg.as_str(), "-L" | "-P" | "--"));
        return (
            None,
            target.map_or(cwd.clone(), |target| resolve_operand(&cwd, target)),
        );
    }
    if name == "git" {
        return analyze_git(&args, cwd, layout);
    }
    if name == "rm" {
        return (analyze_rm(&args, &cwd, layout), cwd);
    }
    (None, cwd)
}

fn analyze_git(args: &[String], cwd: PathBuf, layout: &TowerLayout) -> (Option<String>, PathBuf) {
    let mut git_cwd = cwd.clone();
    let mut subcommand = None;
    let mut subargs = &args[0..0];
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-C" => {
                let Some(dir) = args.get(index + 1) else {
                    return (None, cwd);
                };
                git_cwd = resolve_operand(&git_cwd, dir);
                index += 2;
            }
            "-c" => index += 2,
            arg if arg.starts_with('-') => index += 1,
            arg => {
                subcommand = Some(arg);
                subargs = &args[index + 1..];
                break;
            }
        }
    }
    match subcommand {
        Some("clean") => (analyze_git_clean(subargs, &git_cwd, layout), cwd),
        Some("reset") => (analyze_git_reset(subargs, &git_cwd, layout), cwd),
        _ => (None, cwd),
    }
}

fn analyze_git_clean(args: &[String], cwd: &Path, layout: &TowerLayout) -> Option<String> {
    let mut force = 0;
    let mut ignored = false;
    let mut dry_run = false;
    let mut interactive = false;
    let mut pathspecs = Vec::new();
    let mut options = true;
    for arg in args {
        if options && arg == "--" {
            options = false;
        } else if options && arg.starts_with("--") {
            match arg.as_str() {
                "--force" => force += 1,
                "--dry-run" => dry_run = true,
                "--interactive" => interactive = true,
                _ => {}
            }
        } else if options && arg.starts_with('-') {
            force += arg.matches('f').count();
            dry_run |= arg.contains('n');
            interactive |= arg.contains('i');
            ignored |= arg.contains('x') || arg.contains('X');
        } else {
            pathspecs.push(arg);
        }
    }
    if dry_run || interactive || force == 0 || !ignored {
        return None;
    }
    if pathspecs.is_empty() {
        return layout.comms_dir.starts_with(cwd).then(git_clean_veto);
    }
    let cwd_depth = slot_depth(cwd, &layout.worktrees_dir);
    for spec in pathspecs {
        let absolute = resolve_operand(cwd, spec);
        if cwd_depth.is_some_and(|depth| depth >= 1)
            && slot_depth(&absolute, &layout.worktrees_dir).is_some_and(|depth| depth >= 1)
        {
            continue;
        }
        if operand_bucket(&absolute, layout).is_some() {
            return Some(git_clean_veto());
        }
    }
    None
}

fn analyze_git_reset(args: &[String], cwd: &Path, layout: &TowerLayout) -> Option<String> {
    if !args.iter().any(|arg| arg == "--hard") || !cwd.starts_with(&layout.main_checkout) {
        return None;
    }
    if cwd.starts_with(&layout.worktrees_dir) && cwd != layout.worktrees_dir {
        return None;
    }
    Some(git_reset_veto())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperandBucket {
    Comms,
    TowerAncestor,
    ProtectedInterior,
}

fn analyze_rm(args: &[String], cwd: &Path, layout: &TowerLayout) -> Option<String> {
    let mut recursive = false;
    let mut operands = Vec::new();
    let mut options = true;
    for arg in args {
        if options && arg == "--" {
            options = false;
        } else if options && arg.starts_with("--") {
            recursive |= arg == "--recursive";
        } else if options && arg.starts_with('-') {
            recursive |= arg.contains('r') || arg.contains('R');
        } else {
            operands.push(arg);
        }
    }
    for operand in operands {
        match operand_bucket(&resolve_operand(cwd, operand), layout) {
            Some(OperandBucket::Comms) => return Some(rm_veto(operand)),
            Some(OperandBucket::TowerAncestor | OperandBucket::ProtectedInterior) if recursive => {
                return Some(rm_veto(operand));
            }
            _ => {}
        }
    }
    None
}

fn operand_bucket(path: &Path, layout: &TowerLayout) -> Option<OperandBucket> {
    if path.starts_with(&layout.comms_dir) {
        return Some(OperandBucket::Comms);
    }
    if layout.tower_root.starts_with(path) {
        return Some(OperandBucket::TowerAncestor);
    }
    if !path.starts_with(&layout.tower_root) {
        return None;
    }
    if slot_depth(path, &layout.worktrees_dir).is_some_and(|depth| depth >= 2) {
        None
    } else {
        Some(OperandBucket::ProtectedInterior)
    }
}

fn slot_depth(path: &Path, worktrees: &Path) -> Option<usize> {
    path.strip_prefix(worktrees)
        .ok()
        .map(|relative| relative.components().count())
}

fn resolve_operand(cwd: &Path, operand: &str) -> PathBuf {
    let path = Path::new(operand);
    let joined = if path.has_root() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    normalize_path(&joined)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn normalize_command_name(name: &str) -> String {
    Path::new(name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(name)
        .to_ascii_lowercase()
}

fn git_clean_veto() -> String {
    format!(
        "`git clean` with force and -x/-X is not allowed when its scope covers the tower main checkout — it would delete .tower/comms, the tower protocol state, which git does not track and cannot restore. Run the clean inside your own tower worktree or a temporary `git worktree add` checkout instead; {TOWER_GUARD_ESCAPE}"
    )
}

fn git_reset_veto() -> String {
    format!(
        "`git reset --hard` is not allowed in the tower main checkout — it would silently discard base-checkout work the tower fleet depends on. Run it inside your own tower worktree or a temporary worktree instead; {TOWER_GUARD_ESCAPE}"
    )
}

fn rm_veto(operand: &str) -> String {
    format!(
        "`rm` is not allowed on tower protocol paths (\"{operand}\") — .tower/comms holds the tower protocol state and deleting it orphans the fleet. Remove paths inside your own worktree instead; {TOWER_GUARD_ESCAPE}"
    )
}

fn unanalyzable_veto() -> String {
    "this Bash command mentions git clean / git reset / rm and the tower guard could not parse it safely, so it is blocked conservatively — a misread command could delete .tower/comms, the tower protocol state. Split it into simpler commands or run it inside your tower worktree.".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn target(command: &str, cwd: &Path, main: &Path) -> TowerBashGuardTarget {
        TowerBashGuardTarget {
            command: command.into(),
            cwd: cwd.to_path_buf(),
            main_checkout: main.to_path_buf(),
        }
    }

    #[test]
    fn prefilter_only_targets_dangerous_invocations() {
        assert!(command_needs_tower_guard("rm -rf .tower/comms"));
        assert!(command_needs_tower_guard("git clean -fx"));
        assert!(command_needs_tower_guard("git reset --hard"));
        assert!(!command_needs_tower_guard("git status"));
        assert!(!command_needs_tower_guard("echo hello"));
    }

    #[test]
    fn rm_guard_protects_comms_and_main_but_allows_slot_contents() {
        let main = Path::new("/repo");
        assert!(
            analyze_tower_bash_command(
                &target("rm .tower/comms/state.json", main, main),
                parse_shell_script,
            )
            .is_some()
        );
        assert!(
            analyze_tower_bash_command(&target("rm -rf /repo", main, main), parse_shell_script)
                .is_some()
        );
        assert!(
            analyze_tower_bash_command(
                &target("rm -rf .tower/worktrees", main, main),
                parse_shell_script,
            )
            .is_some()
        );
        assert!(
            analyze_tower_bash_command(
                &target("rm -rf .tower/worktrees/wt-1/src/file.rs", main, main),
                parse_shell_script,
            )
            .is_none()
        );
        assert!(
            analyze_tower_bash_command(
                &target("rm -rf /tmp/other", main, main),
                parse_shell_script
            )
            .is_none()
        );
    }

    #[test]
    fn git_clean_requires_force_and_ignored_file_deletion() {
        let main = Path::new("/repo");
        assert!(
            analyze_tower_bash_command(&target("git clean -fx", main, main), parse_shell_script)
                .is_some()
        );
        assert!(
            analyze_tower_bash_command(&target("git clean -ff", main, main), parse_shell_script)
                .is_none()
        );
        assert!(
            analyze_tower_bash_command(&target("git clean -nx", main, main), parse_shell_script)
                .is_none()
        );
        assert!(
            analyze_tower_bash_command(&target("git clean -f", main, main), parse_shell_script)
                .is_none()
        );
        let slot = main.join(".tower/worktrees/wt-1");
        assert!(
            analyze_tower_bash_command(&target("git clean -fx", &slot, main), parse_shell_script)
                .is_none()
        );
        assert!(
            analyze_tower_bash_command(
                &target(
                    "git -C /repo/.tower/worktrees/wt-1 clean -fx -- ../../comms",
                    &slot,
                    main,
                ),
                parse_shell_script,
            )
            .is_some()
        );
    }

    #[test]
    fn reset_and_git_c_follow_the_effective_checkout() {
        let main = Path::new("/repo");
        assert!(
            analyze_tower_bash_command(&target("git reset --hard", main, main), parse_shell_script)
                .is_some()
        );
        assert!(
            analyze_tower_bash_command(&target("git reset", main, main), parse_shell_script)
                .is_none()
        );
        let slot = main.join(".tower/worktrees/wt-1");
        assert!(
            analyze_tower_bash_command(
                &target("git reset --hard", &slot, main),
                parse_shell_script
            )
            .is_none()
        );
        assert!(
            analyze_tower_bash_command(
                &target("git -C /repo reset --hard", &slot, main),
                parse_shell_script,
            )
            .is_some()
        );
    }

    #[test]
    fn nested_shell_eval_and_cd_are_analyzed() {
        let main = Path::new("/repo");
        assert!(
            analyze_tower_bash_command(
                &target("cd /tmp && bash -c 'rm -rf /repo/.tower/comms'", main, main),
                parse_shell_script,
            )
            .is_some()
        );
        assert!(
            analyze_tower_bash_command(
                &target("eval 'git -C /repo reset --hard'", main, main),
                parse_shell_script,
            )
            .is_some()
        );
        assert!(
            analyze_tower_bash_command(
                &target("cd /tmp && rm -rf /tmp/safe", main, main),
                parse_shell_script,
            )
            .is_none()
        );
    }

    #[test]
    fn parse_failures_veto_but_quoted_keywords_are_safe() {
        let main = Path::new("/repo");
        let call = target("rm -rf /tmp/safe", main, main);
        let error = analyze_tower_bash_command(&call, |_| -> Result<TowerBashScript, String> {
            Err("bad parse".into())
        });
        assert!(error.is_some());
        assert!(
            analyze_tower_bash_command(
                &target("echo 'rm -rf /repo/.tower/comms'", main, main),
                parse_shell_script,
            )
            .is_none()
        );
    }

    #[test]
    fn ownership_requires_a_nonempty_session_id() {
        assert!(tower_workspace_owned_json(r#"{"sessionId":"session-1"}"#));
        assert!(!tower_workspace_owned_json(r#"{"sessionId":""}"#));
        assert!(!tower_workspace_owned_json(r#"{"sessionId":null}"#));
        assert!(!tower_workspace_owned_json("not json"));
    }
}
