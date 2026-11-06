//! Behavioral equivalence with the host's `bashTargetDirs`.
//!
//! The corpus is generated from the TypeScript implementation
//! (`packages/node-sdk/src/native/sdk-client/bash-target-dirs.ts`) and carries,
//! per case, both what the host returns (`expected`) and what the same algorithm
//! over the real tree-sitter-bash grammar returns (`reference`). This crate uses
//! the grammar, so every case must equal `reference`; the cases where the host's
//! reimplementation deviates must be listed in [`KNOWN_DIVERGENCES`], and a case
//! that starts or stops diverging fails the suite — a documented deviation
//! cannot drift silently, and a fixed one cannot stay registered.
//!
//! Every divergence in the corpus is a `null` ↔ empty-list pair, which the turn
//! loop treats identically (`run_turn.rs` extends a step's directories only for
//! `Ok(Some(dirs))`), so the reminder is the same either way.

use kimi_agent::tools::bash_target_dirs::bash_target_dirs;
use serde_json::Value;

const CORPUS: &str = include_str!("fixtures/bash-target-dirs.json");

/// Cases where the host's parser reimplementation and the grammar disagree.
///
/// Each is one of the documented deviations in
/// `packages/tree-sitter-bash/test/helpers/known-differences.ts`; the host side
/// returns an empty list where the grammar reports an error (or the reverse),
/// and nothing else differs.
const KNOWN_DIVERGENCES: &[&str] = &[
    "differential/case.txt:72",
    "differential/expansions.txt:47",
    "differential/expansions.txt:48",
    "differential/expansions.txt:81",
    "differential/expansions.txt:83",
    "differential/heredoc.txt:22",
    "differential/heredoc.txt:23",
    "differential/heredoc.txt:31",
    "differential/heredoc.txt:32",
    "differential/redirects.txt:8",
    "differential/redirects.txt:13",
    "differential/statements.txt:100",
    "differential/test-command.txt:92",
    "listing/41@1",
    "listing/41@2",
    "listing/43@1",
    "listing/43@2",
];

fn list(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Null => None,
        Value::Array(items) => Some(
            items
                .iter()
                .map(|item| item.as_str().unwrap_or_default().to_string())
                .collect(),
        ),
        other => panic!("unexpected corpus shape: {other:?}"),
    }
}

#[test]
fn every_corpus_case_matches_the_grammar() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus is valid json");
    let cases = corpus["cases"].as_array().expect("cases array");
    assert!(cases.len() > 700, "the corpus carries the whole sample set");

    let mut failures = Vec::new();
    for case in cases {
        let name = case["name"].as_str().unwrap_or_default();
        let command = case["command"].as_str().unwrap_or_default();
        let cwd = case["cwd"].as_str().unwrap_or_default();
        let home = case["home"].as_str().unwrap_or_default();
        let expected = list(&case["reference"]);
        let actual = bash_target_dirs(command, cwd, home);
        if actual != expected {
            failures.push(format!(
                "{name}: {command:?} cwd={cwd:?} expected={expected:?} got={actual:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} case(s) disagree with the grammar:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn the_registered_divergences_are_exactly_the_ones_that_diverge() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus is valid json");
    let cases = corpus["cases"].as_array().expect("cases array");

    let mut diverging = Vec::new();
    for case in cases {
        let name = case["name"].as_str().unwrap_or_default().to_string();
        let command = case["command"].as_str().unwrap_or_default();
        let cwd = case["cwd"].as_str().unwrap_or_default();
        let home = case["home"].as_str().unwrap_or_default();
        let host = list(&case["expected"]);
        let grammar = list(&case["reference"]);
        if host == grammar {
            continue;
        }
        diverging.push(name.clone());
        assert!(
            KNOWN_DIVERGENCES.contains(&name.as_str()),
            "{name} diverges ({command:?}: host={host:?} grammar={grammar:?}) but is not registered"
        );
        assert_eq!(
            bash_target_dirs(command, cwd, home),
            grammar,
            "{name}: this crate must answer what the grammar answers"
        );
        assert!(
            host.as_ref().is_none_or(|dirs| dirs.is_empty()),
            "{name}: a divergence must never hide a directory the host names"
        );
        assert!(
            grammar.as_ref().is_none_or(|dirs| dirs.is_empty()),
            "{name}: a divergence must never hide a directory this crate names"
        );
    }

    let stale: Vec<&str> = KNOWN_DIVERGENCES
        .iter()
        .copied()
        .filter(|name| !diverging.iter().any(|seen| seen == name))
        .collect();
    assert!(
        stale.is_empty(),
        "these registered cases no longer diverge; drop them: {stale:?}"
    );
}

/// The host's own contract cases (`packages/node-sdk/test/bash-target-dirs.test.ts`),
/// restated here so the two implementations are held to one list.
#[test]
fn the_hosts_contract_cases_hold() {
    const CWD: &str = "/repo";
    const HOME: &str = "/home/user";
    let dirs = |command: &str| bash_target_dirs(command, CWD, HOME);

    assert_eq!(dirs("ls sub"), Some(vec!["/repo/sub".to_string()]));
    assert_eq!(
        dirs("cd sub && ls other"),
        Some(vec!["/repo/sub/other".to_string()])
    );
    assert_eq!(
        dirs("find src tests -name \"*.ts\""),
        Some(vec!["/repo/src".to_string(), "/repo/tests".to_string()])
    );
    assert_eq!(
        dirs("ls \"some dir\""),
        Some(vec!["/repo/some dir".to_string()])
    );
    assert_eq!(
        dirs("ls 'some dir'"),
        Some(vec!["/repo/some dir".to_string()])
    );
    assert_eq!(dirs("ls /etc"), Some(vec!["/etc".to_string()]));
    assert_eq!(
        dirs("cd /abs && ls rel"),
        Some(vec!["/abs/rel".to_string()])
    );
    assert_eq!(dirs("ls $DIR"), Some(vec![]));
    assert_eq!(dirs("ls *.ts"), Some(vec![]));
    assert_eq!(dirs("cd $X && ls sub"), Some(vec![]));
    assert_eq!(
        dirs("cd && ls sub"),
        Some(vec!["/home/user/sub".to_string()])
    );
    assert_eq!(dirs("ls -la sub"), Some(vec!["/repo/sub".to_string()]));
    assert_eq!(
        dirs("ls --sort size sub"),
        Some(vec!["/repo/sub".to_string()])
    );
    assert_eq!(
        dirs("ls --sort=size sub"),
        Some(vec!["/repo/sub".to_string()])
    );
    assert_eq!(dirs("tree -L 2 sub"), Some(vec!["/repo/sub".to_string()]));
    assert_eq!(dirs("ls sub | grep x"), Some(vec!["/repo/sub".to_string()]));
    assert_eq!(
        dirs("ls sub > out.txt"),
        Some(vec!["/repo/sub".to_string()])
    );
    assert_eq!(
        dirs("ls sub sub src"),
        Some(vec!["/repo/sub".to_string(), "/repo/src".to_string()])
    );
    assert_eq!(dirs("ls"), Some(vec!["/repo".to_string()]));
    assert_eq!(dirs("/bin/ls sub"), Some(vec![]));
    assert_eq!(dirs("$TOOL sub"), Some(vec![]));
    assert_eq!(dirs("ls 123"), Some(vec!["/repo/123".to_string()]));
    assert_eq!(dirs("ls \"unbalanced"), None);
}

/// The parse budget is part of the contract: a command past the node cap is
/// "did not parse" and the turn loop keeps the declared cwd.
#[test]
fn a_command_over_the_node_budget_answers_none() {
    let mut command = String::from("ls");
    for index in 0..12_000 {
        command.push_str(&format!(" dir{index}"));
    }
    assert_eq!(bash_target_dirs(&command, "/repo", "/home/user"), None);

    let mut small = String::from("ls");
    for index in 0..100 {
        small.push_str(&format!(" dir{index}"));
    }
    let dirs = bash_target_dirs(&small, "/repo", "/home/user").expect("a small command parses");
    assert_eq!(dirs.len(), 100);
}
