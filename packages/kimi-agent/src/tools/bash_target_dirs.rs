//! The AGENTS.md reminder's bash-operand half, implemented in the engine.
//!
//! Ported one-for-one from `packages/node-sdk/src/native/sdk-client/bash-target-dirs.ts`
//! (itself a port of v2 `agent/agentsMdReminder/bashTargets.ts`): which
//! directories a Bash call's *listing* operands resolve to, in first-seen order,
//! so the reminder can name the `AGENTS.md` files those directories carry.
//!
//! # Reference, and the one deliberate difference
//!
//! The host parses with the fork's TypeScript reimplementation of
//! tree-sitter-bash 0.25; this module uses that grammar directly. Trees agree on
//! the documented corpus except where the reimplementation records a deviation
//! (`packages/tree-sitter-bash/test/helpers/known-differences.ts`); at the
//! function's boundary every one of those is a `null` ↔ empty-list pair, which
//! the turn loop treats identically — it extends a step's directories only for
//! `Ok(Some(dirs))`, so neither adds anything. `tests/bash_target_dirs_corpus.rs`
//! pins every case, including the divergent ones, against a corpus generated
//! from the TypeScript implementation.
//!
//! # Failure shape
//!
//! [`bash_target_dirs`] answers `None` when the command does not parse, parses
//! with an error, or exceeds the parse budget — v2's own fallback
//! (`!parsed.ok || parsed.hasError`), which the turn loop mirrors by keeping the
//! declared `cwd` and nothing more. It never returns an error into the turn.
//!
//! # Path normalisation
//!
//! The host normalises with `pathe`, whose rules differ from `std::path` in ways
//! that are observable here (a Windows drive letter is upper-cased, backslashes
//! become forward slashes on every platform, a trailing slash survives, `..`
//! above the root of a relative path is kept). The private `pathe` module below
//! is a direct port of those functions so the two sides produce the same
//! strings, not merely equivalent paths.

use std::collections::HashSet;
use std::time::Instant;

use tree_sitter::{Node, Parser, Tree};

/// v2 `BASH_PARSE_OPTIONS` (`agentsMdReminderService.ts`): 500 ms / 10 000 nodes.
pub const PARSE_TIMEOUT_MS: u128 = 500;
/// See [`PARSE_TIMEOUT_MS`].
pub const PARSE_MAX_NODES: usize = 10_000;

/// The directories a Bash command's listing operands resolve to, in first-seen
/// order.
///
/// `cwd` is the call's effective working directory; `home` answers a bare `cd`.
/// `None` when the command does not parse or parses with an error — the host's
/// own fallback shape, which the turn loop mirrors by keeping only the declared
/// `cwd`.
pub fn bash_target_dirs(command: &str, cwd: &str, home: &str) -> Option<Vec<String>> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .ok()?;
    let started = Instant::now();
    let tree = parser.parse(command, None)?;
    if started.elapsed().as_millis() > PARSE_TIMEOUT_MS {
        return None;
    }
    if tree.root_node().has_error() {
        return None;
    }
    if count_nodes(&tree) > PARSE_MAX_NODES {
        return None;
    }
    Some(extract_target_dirs(tree.root_node(), command, cwd, home))
}

fn count_nodes(tree: &Tree) -> usize {
    let mut cursor = tree.root_node().walk();
    let mut count = 1;
    loop {
        if cursor.goto_first_child() {
            count += 1;
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                count += 1;
                break;
            }
            if !cursor.goto_parent() {
                return count;
            }
        }
    }
}

/// The extraction half, exposed for the corpus test: the caller supplies a tree
/// it has already validated.
pub fn extract_target_dirs(root: Node<'_>, source: &str, cwd: &str, home: &str) -> Vec<String> {
    let mut commands = Vec::new();
    collect_commands(root, &mut commands, false);

    let mut targets: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut base: Option<String> = Some(cwd.to_string());

    for collected in commands {
        let Some((name, args, dropped)) = command_name_and_args(collected.node, source) else {
            continue;
        };
        if name == "cd" {
            if collected.in_pipeline {
                continue;
            }
            if dropped {
                base = None;
                continue;
            }
            match args.first() {
                Some(target) if args.len() == 1 && !target.starts_with('-') => {
                    let resolved = if pathe::is_absolute(target) {
                        pathe::normalize(target)
                    } else {
                        match &base {
                            Some(current) => pathe::join(current, target),
                            None => continue,
                        }
                    };
                    base = Some(resolved);
                }
                None => base = Some(home.to_string()),
                Some(_) => base = None,
            }
            continue;
        }
        if !is_listing_command(&name) {
            continue;
        }
        let operands = if name == "find" {
            take_leading_paths(&args)
        } else {
            drop_flags(&args, &name)
        };
        if operands.is_empty() {
            if !dropped {
                push_operand(".", &base, &mut targets, &mut seen);
            }
            continue;
        }
        for operand in &operands {
            push_operand(operand, &base, &mut targets, &mut seen);
        }
    }
    targets
}

fn is_listing_command(name: &str) -> bool {
    matches!(name, "ls" | "tree" | "find" | "dir" | "exa" | "eza" | "lsd")
}

fn push_operand(
    operand: &str,
    base: &Option<String>,
    targets: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    let dir = if pathe::is_absolute(operand) {
        pathe::normalize(operand)
    } else {
        let Some(current) = base else {
            return;
        };
        pathe::join(current, operand)
    };
    if seen.insert(dir.clone()) {
        targets.push(dir);
    }
}

/// One `command` node, with whether an enclosing `pipeline` put it there.
struct CollectedCommand<'a> {
    node: Node<'a>,
    in_pipeline: bool,
}

fn collect_commands<'a>(node: Node<'a>, out: &mut Vec<CollectedCommand<'a>>, in_pipeline: bool) {
    let kind = node.kind();
    if kind == "command" {
        out.push(CollectedCommand { node, in_pipeline });
        return;
    }
    if !matches!(
        kind,
        "program" | "list" | "pipeline" | "redirected_statement"
    ) {
        return;
    }
    let nested = in_pipeline || kind == "pipeline";
    let mut index = 0;
    while let Some(child) = node.child(index) {
        collect_commands(child, out, nested);
        index += 1;
    }
}

/// The command's word, its literal arguments, and whether anything in the
/// command could not be read as a literal.
///
/// `None` means the command is not a plain executable name at all: a word
/// containing `/` is a path (`./script.sh`), and a name that does not reduce to
/// a literal cannot be matched against the listing-command table.
fn command_name_and_args(command: Node<'_>, source: &str) -> Option<(String, Vec<String>, bool)> {
    let mut name_index: Option<u32> = None;
    let mut index = 0;
    while let Some(child) = command.child(index) {
        if child.kind() == "command_name" {
            name_index = Some(index);
            break;
        }
        index += 1;
    }
    let name_index = name_index?;
    let name_node = command.child(name_index)?;

    let mut name: Option<String> = None;
    let mut child_index = 0;
    while let Some(child) = name_node.child(child_index) {
        if child.is_named() {
            name = literal_text(child, source);
            break;
        }
        child_index += 1;
    }
    let name = name?;
    if name.is_empty() || name.contains('/') {
        return None;
    }

    let mut args = Vec::new();
    let mut dropped = false;
    let mut index = name_index + 1;
    while let Some(child) = command.child(index) {
        match literal_text(child, source) {
            None => dropped = true,
            Some(value) => {
                if !value.is_empty() {
                    args.push(value);
                }
            }
        }
        index += 1;
    }
    Some((name, args, dropped))
}

/// The four node shapes whose text is a literal, and nothing else.
fn literal_text(node: Node<'_>, source: &str) -> Option<String> {
    let text = node.utf8_text(source.as_bytes()).ok()?;
    match node.kind() {
        "word" => {
            if is_unsafe_operand(text) {
                return None;
            }
            let unescaped = unescape(text);
            if is_unsafe_operand(&unescaped) {
                return None;
            }
            Some(unescaped)
        }
        "number" => Some(text.to_string()),
        "raw_string" => {
            if text.chars().count() < 2 {
                return None;
            }
            let chars: Vec<char> = text.chars().collect();
            let value: String = chars[1..chars.len() - 1].iter().collect();
            if is_unsafe_operand(&value) {
                return None;
            }
            Some(value)
        }
        "string" => {
            let mut value = String::new();
            let mut index = 0;
            while let Some(child) = node.child(index) {
                if child.kind() == "string_content" {
                    value.push_str(child.utf8_text(source.as_bytes()).ok()?);
                } else if child.is_named() {
                    // A substitution or expansion inside the quotes: the text
                    // is not knowable here, and guessing is how a reminder ends
                    // up naming the wrong directory.
                    return None;
                }
                index += 1;
            }
            if is_unsafe_operand(&value) {
                return None;
            }
            Some(value)
        }
        _ => None,
    }
}

/// `$`, backtick, glob and brace characters, and `~` — a word carrying any of
/// them is a substitution or a glob, not a path this module can name.
fn is_unsafe_operand(text: &str) -> bool {
    text.chars()
        .any(|c| matches!(c, '$' | '`' | '*' | '?' | '[' | ']' | '~'))
}

/// `raw.replaceAll(/\\(.)/gs, '$1')`: drop the backslash, keep what it escaped.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(next) = chars.next()
        {
            out.push(next);
            continue;
        }
        out.push(c);
    }
    out
}

/// Drop the command's options, honouring the ones that consume the next
/// argument. `--opt=value` does not consume; a lone `-` is an operand.
fn drop_flags(args: &[String], command: &str) -> Vec<String> {
    let argument_taking = if command == "ls" || command == "dir" {
        LS_ARG_TAKING_OPTIONS
    } else {
        TREE_LIKE_ARG_TAKING_OPTIONS
    };
    let mut out = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if !arg.starts_with('-') {
            out.push(arg.clone());
            index += 1;
            continue;
        }
        if !arg.contains('=') && argument_taking.contains(&arg.as_str()) {
            index += 1;
        }
        index += 1;
    }
    out
}

/// `find`'s grammar needs its own rule: its paths come *first*, before the
/// expression, so an option-looking argument ends the path list rather than
/// being skipped.
fn take_leading_paths(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for arg in args {
        if FIND_GLOBAL_OPTIONS.contains(&arg.as_str()) || arg == "--" {
            continue;
        }
        if arg.starts_with('-') || arg == "(" || arg == ")" || arg == "!" {
            break;
        }
        out.push(arg.clone());
    }
    out
}

const LS_ARG_TAKING_OPTIONS: &[&str] = &[
    "-w",
    "--width",
    "--sort",
    "--block-size",
    "-I",
    "--ignore",
    "--hide",
    "--format",
    "--time-style",
    "--indicator-style",
    "--quoting-style",
    "-T",
    "--tabsize",
];

const TREE_LIKE_ARG_TAKING_OPTIONS: &[&str] = &[
    "-w",
    "--width",
    "--sort",
    "--block-size",
    "-I",
    "--ignore",
    "--ignore-glob",
    "--hide",
    "--format",
    "--time-style",
    "--indicator-style",
    "--hyperlink",
    "--quoting-style",
    "-T",
    "--tabsize",
    "-L",
    "--level",
    "--depth",
    "-P",
    "-o",
    "--filelimit",
    "--charset",
    "--timefmt",
    "-s",
];

const FIND_GLOBAL_OPTIONS: &[&str] = &["-H", "-L", "-P"];

/// A direct port of the `pathe` functions the host normalises with.
///
/// `std::path` would answer a different string for the same input on Windows
/// (it keeps backslashes, lower-cases nothing, and drops a trailing separator),
/// and the reminder compares these strings with the ones the host produced.
mod pathe {
    pub fn is_absolute(path: &str) -> bool {
        let bytes: Vec<char> = path.chars().collect();
        if bytes.is_empty() {
            return false;
        }
        // `^[/\\](?![/\\])` — one separator, not two.
        if matches!(bytes[0], '/' | '\\') {
            if bytes.len() == 1 || !matches!(bytes[1], '/' | '\\') {
                return true;
            }
            // `^[/\\]{2}(?!\.)` — a UNC prefix, unless it is `//.`. Two
            // separators with nothing after them are an absolute root too.
            return bytes.len() == 2 || bytes[2] != '.';
        }
        // `^[A-Za-z]:[/\\]`
        bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == ':'
            && matches!(bytes[2], '/' | '\\')
    }

    fn is_unc(path: &str) -> bool {
        let bytes: Vec<char> = path.chars().collect();
        bytes.len() >= 2 && matches!(bytes[0], '/' | '\\') && matches!(bytes[1], '/' | '\\')
    }

    fn normalize_windows_path(input: &str) -> String {
        if input.is_empty() {
            return String::new();
        }
        let replaced: Vec<char> = input
            .chars()
            .map(|c| if c == '\\' { '/' } else { c })
            .collect();
        let mut out: String = replaced.iter().collect();
        if replaced.len() >= 2 && replaced[0].is_ascii_alphabetic() && replaced[1] == ':' {
            let upper = replaced[0].to_ascii_uppercase();
            out = format!("{upper}{}", &out[replaced[0].len_utf8()..]);
        }
        out
    }

    fn is_drive_letter(path: &str) -> bool {
        let chars: Vec<char> = path.chars().collect();
        chars.len() == 2 && chars[0].is_ascii_alphabetic() && chars[1] == ':'
    }

    fn normalize_string(path: &str, allow_above_root: bool) -> String {
        let chars: Vec<char> = path.chars().collect();
        let mut res = String::new();
        let mut last_segment_length = 0usize;
        let mut last_slash: isize = -1;
        let mut dots: isize = 0;
        let mut previous: Option<char> = None;
        let mut index = 0usize;
        while index <= chars.len() {
            let current = if index < chars.len() {
                chars[index]
            } else if previous == Some('/') {
                break;
            } else {
                '/'
            };
            if current == '/' {
                if last_slash == index as isize - 1 || dots == 1 {
                    // A repeated separator, or `.` — nothing to add.
                } else if dots == 2 {
                    let ends_with_dotdot = res.len() >= 2
                        && last_segment_length == 2
                        && res.ends_with('.')
                        && res.chars().rev().nth(1) == Some('.');
                    if !ends_with_dotdot {
                        if res.chars().count() > 2 {
                            if let Some(slash_index) = res.rfind('/') {
                                res.truncate(slash_index);
                                last_segment_length = res
                                    .chars()
                                    .count()
                                    .saturating_sub(1)
                                    .saturating_sub(res.rfind('/').map(|i| i + 1).unwrap_or(0));
                            } else {
                                res.clear();
                                last_segment_length = 0;
                            }
                            last_slash = index as isize;
                            dots = 0;
                            index += 1;
                            previous = Some(current);
                            continue;
                        } else if !res.is_empty() {
                            res.clear();
                            last_segment_length = 0;
                            last_slash = index as isize;
                            dots = 0;
                            index += 1;
                            previous = Some(current);
                            continue;
                        }
                    }
                    if allow_above_root {
                        if res.is_empty() {
                            res.push_str("..");
                        } else {
                            res.push_str("/..");
                        }
                        last_segment_length = 2;
                    }
                } else {
                    let start = (last_slash + 1) as usize;
                    let segment: String = chars[start..index].iter().collect();
                    if res.is_empty() {
                        res = segment;
                    } else {
                        res.push('/');
                        res.push_str(&segment);
                    }
                    last_segment_length = index.saturating_sub(start);
                }
                last_slash = index as isize;
                dots = 0;
            } else if current == '.' && dots != -1 {
                dots += 1;
            } else {
                dots = -1;
            }
            previous = Some(current);
            index += 1;
        }
        res
    }

    pub fn normalize(path: &str) -> String {
        if path.is_empty() {
            return ".".to_string();
        }
        let path = normalize_windows_path(path);
        let unc = is_unc(&path);
        let absolute = is_absolute(&path);
        let trailing_separator = path.ends_with('/');
        let mut out = normalize_string(&path, !absolute);
        if out.is_empty() {
            return if absolute {
                "/".to_string()
            } else if trailing_separator {
                "./".to_string()
            } else {
                ".".to_string()
            };
        }
        if trailing_separator {
            out.push('/');
        }
        if is_drive_letter(&out) {
            out.push('/');
        }
        if unc {
            return if absolute {
                format!("//{out}")
            } else {
                format!("//./{out}")
            };
        }
        if absolute && !is_absolute(&out) {
            return format!("/{out}");
        }
        out
    }

    pub fn join(base: &str, segment: &str) -> String {
        let mut path = String::new();
        for piece in [base, segment] {
            if piece.is_empty() {
                continue;
            }
            if path.is_empty() {
                path.push_str(piece);
                continue;
            }
            let trailing = path.ends_with('/');
            let leading = piece.starts_with('/');
            if trailing && leading {
                path.push_str(&piece[1..]);
            } else if trailing || leading {
                path.push_str(piece);
            } else {
                path.push('/');
                path.push_str(piece);
            }
        }
        normalize(&path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_listing_names_the_cwd() {
        assert_eq!(
            bash_target_dirs("ls", "/work/project", "/home/user"),
            Some(vec!["/work/project".to_string()])
        );
    }

    #[test]
    fn a_relative_operand_resolves_against_the_cwd() {
        assert_eq!(
            bash_target_dirs("ls src", "/work/project", "/home/user"),
            Some(vec!["/work/project/src".to_string()])
        );
    }

    #[test]
    fn an_absolute_operand_is_normalised() {
        assert_eq!(
            bash_target_dirs("ls /a/b/../c", "/work/project", "/home/user"),
            Some(vec!["/a/c".to_string()])
        );
    }

    #[test]
    fn a_quoted_operand_loses_its_quotes() {
        assert_eq!(
            bash_target_dirs("ls 'src dir'", "/w", "/h"),
            Some(vec!["/w/src dir".to_string()])
        );
        assert_eq!(
            bash_target_dirs("ls \"src\"", "/w", "/h"),
            Some(vec!["/w/src".to_string()])
        );
    }

    #[test]
    fn an_expansion_is_not_a_nameable_operand() {
        assert_eq!(bash_target_dirs("ls $HOME", "/w", "/h"), Some(vec![]));
        assert_eq!(bash_target_dirs("ls \"$HOME\"", "/w", "/h"), Some(vec![]));
        assert_eq!(bash_target_dirs("ls src/*.ts", "/w", "/h"), Some(vec![]));
    }

    #[test]
    fn a_redirect_leaves_the_command_with_no_operand() {
        // The redirect is a sibling of `command` under `redirected_statement`,
        // not an argument, so the listing falls back to the current directory.
        assert_eq!(
            bash_target_dirs("ls > out.txt", "/w", "/h"),
            Some(vec!["/w".to_string()])
        );
    }

    #[test]
    fn cd_moves_the_base_for_later_listings() {
        assert_eq!(
            bash_target_dirs("cd src && ls", "/w", "/h"),
            Some(vec!["/w/src".to_string()])
        );
        assert_eq!(
            bash_target_dirs("cd && ls", "/w", "/h"),
            Some(vec!["/h".to_string()])
        );
        assert_eq!(bash_target_dirs("cd - && ls", "/w", "/h"), Some(vec![]));
    }

    #[test]
    fn a_cd_inside_a_pipeline_does_not_move_the_base() {
        assert_eq!(
            bash_target_dirs("cd src | ls", "/w", "/h"),
            Some(vec!["/w".to_string()]),
            "the listing still resolves against the original cwd"
        );
    }

    #[test]
    fn an_option_that_takes_a_value_does_not_become_an_operand() {
        assert_eq!(
            bash_target_dirs("ls -w 40 src", "/w", "/h"),
            Some(vec!["/w/src".to_string()])
        );
        assert_eq!(
            bash_target_dirs("ls --width=40 src", "/w", "/h"),
            Some(vec!["/w/src".to_string()])
        );
    }

    #[test]
    fn find_reads_its_paths_before_its_expression() {
        assert_eq!(
            bash_target_dirs("find src -name x", "/w", "/h"),
            Some(vec!["/w/src".to_string()])
        );
        assert_eq!(
            bash_target_dirs("find -H src .", "/w", "/h"),
            Some(vec!["/w/src".to_string(), "/w".to_string()])
        );
        assert_eq!(
            bash_target_dirs("find -name x", "/w", "/h"),
            Some(vec!["/w".to_string()]),
            "no path means the current directory"
        );
    }

    #[test]
    fn duplicates_collapse_but_order_is_first_seen() {
        assert_eq!(
            bash_target_dirs("ls b a b", "/w", "/h"),
            Some(vec!["/w/b".to_string(), "/w/a".to_string()])
        );
    }

    #[test]
    fn a_command_that_does_not_parse_answers_none() {
        assert_eq!(bash_target_dirs("ls \"unterminated", "/w", "/h"), None);
    }

    #[test]
    fn a_windows_drive_letter_is_upper_cased_and_forward_slashed() {
        assert_eq!(
            bash_target_dirs("ls src", "c:\\work", "C:/Users/dev"),
            Some(vec!["C:/work/src".to_string()])
        );
    }

    mod pathe_tests {
        use super::super::pathe;

        #[test]
        fn normalize_matches_the_host_rules() {
            assert_eq!(pathe::normalize(""), ".");
            assert_eq!(pathe::normalize("/a/b/../c"), "/a/c");
            assert_eq!(pathe::normalize("/a//b/"), "/a/b/");
            assert_eq!(pathe::normalize("a/./b"), "a/b");
            assert_eq!(pathe::normalize("../a"), "../a");
            assert_eq!(pathe::normalize("c:\\work"), "C:/work");
            assert_eq!(pathe::normalize("C:"), "C:/");
            assert_eq!(pathe::normalize("/"), "/");
            assert_eq!(
                pathe::normalize("//"),
                "/",
                "two separators are an absolute UNC root"
            );
        }

        #[test]
        fn join_matches_the_host_rules() {
            assert_eq!(pathe::join("/w", "src"), "/w/src");
            assert_eq!(pathe::join("/w/", "src"), "/w/src");
            assert_eq!(
                pathe::join("/w", "/src"),
                "/w/src",
                "pathe concatenates rather than replacing"
            );
            assert_eq!(pathe::join("", "src"), "src");
            assert_eq!(pathe::join("/w", "a/../b"), "/w/b");
            assert_eq!(pathe::join("/w", ".."), "/");
        }

        #[test]
        fn is_absolute_matches_the_host_regex() {
            assert!(pathe::is_absolute("/a"));
            assert!(pathe::is_absolute("C:/a"));
            assert!(pathe::is_absolute("C:\\a"));
            assert!(pathe::is_absolute("//server/share"));
            assert!(!pathe::is_absolute("a/b"));
            assert!(!pathe::is_absolute("./a"));
            assert!(!pathe::is_absolute("C:"));
            assert!(!pathe::is_absolute("//."));
        }
    }
}
