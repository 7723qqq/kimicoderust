use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;
use url::Url;

use super::git::{FsGitStatus, FsGitStatusResponse, FsPullRequest, FsPullRequestState};

pub fn parse_porcelain(
    stdout: &str,
    filter: Option<&HashSet<String>>,
    path_prefix: &str,
) -> FsGitStatusResponse {
    let records = stdout.split('\0').collect::<Vec<_>>();
    let mut branch = String::new();
    let mut ahead = 0;
    let mut behind = 0;
    let mut entries = HashMap::new();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        if record.is_empty() {
            index += 1;
            continue;
        }
        if let Some(header) = record.strip_prefix("## ") {
            let parsed = parse_branch_header(header);
            branch = parsed.branch;
            ahead = parsed.ahead;
            behind = parsed.behind;
            index += 1;
            continue;
        }
        if record.len() < 4 {
            index += 1;
            continue;
        }
        let xy = &record[..2];
        let repo_path = &record[3..];
        let wire_path = if !path_prefix.is_empty() && repo_path.starts_with(path_prefix) {
            &repo_path[path_prefix.len()..]
        } else {
            repo_path
        };
        if xy.starts_with('R') || xy.starts_with('C') {
            index += 1;
        }
        if filter.is_some_and(|filter| !filter.contains(wire_path)) {
            index += 1;
            continue;
        }
        entries.insert(wire_path.to_string(), collapse_xy(xy));
        index += 1;
    }
    FsGitStatusResponse {
        branch,
        ahead,
        behind,
        entries,
        additions: 0,
        deletions: 0,
        pull_request: None,
    }
}

pub fn parse_numstat(stdout: &str) -> (u64, u64) {
    let mut additions = 0u64;
    let mut deletions = 0u64;
    for line in stdout.split('\n') {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        additions = additions.saturating_add(parse_numstat_count(fields.next()));
        deletions = deletions.saturating_add(parse_numstat_count(fields.next()));
    }
    (additions, deletions)
}

fn parse_numstat_count(value: Option<&str>) -> u64 {
    let Some(value) = value else {
        return 0;
    };
    if value == "-" {
        return 0;
    }
    let value = value.trim_start();
    let (negative, unsigned) = match value.strip_prefix('-') {
        Some(value) => (true, value),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    let digits = unsigned
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    if digits.is_empty() || negative {
        return 0;
    }
    let Ok(number) = digits.parse::<f64>() else {
        return 0;
    };
    if !number.is_finite() || number <= 0.0 {
        0
    } else {
        number as u64
    }
}

struct BranchHeader {
    branch: String,
    ahead: u64,
    behind: u64,
}

fn parse_branch_header(rest: &str) -> BranchHeader {
    if rest.starts_with("HEAD (no branch)") {
        return BranchHeader {
            branch: String::new(),
            ahead: 0,
            behind: 0,
        };
    }
    if let Some(branch) = rest.strip_prefix("No commits yet on ") {
        return BranchHeader {
            branch: branch.to_string(),
            ahead: 0,
            behind: 0,
        };
    }
    static AHEAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"ahead (\d+)").unwrap());
    static BEHIND: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"behind (\d+)").unwrap());
    let mut branch = rest;
    let mut ahead = 0;
    let mut behind = 0;
    if let Some(bracket) = rest.find(" [") {
        branch = &rest[..bracket];
        let sliced = &rest[bracket + 2..rest.len().saturating_sub(1)];
        ahead = AHEAD
            .captures(sliced)
            .and_then(|captures| captures.get(1))
            .and_then(|value| value.as_str().parse().ok())
            .unwrap_or(0);
        behind = BEHIND
            .captures(sliced)
            .and_then(|captures| captures.get(1))
            .and_then(|value| value.as_str().parse().ok())
            .unwrap_or(0);
    }
    if let Some(dots) = branch.find("...") {
        branch = &branch[..dots];
    }
    BranchHeader {
        branch: branch.to_string(),
        ahead,
        behind,
    }
}

fn collapse_xy(xy: &str) -> FsGitStatus {
    if xy == "??" {
        return FsGitStatus::Untracked;
    }
    if xy == "!!" {
        return FsGitStatus::Ignored;
    }
    let x = xy.chars().next().unwrap_or(' ');
    let y = xy.chars().nth(1).unwrap_or(' ');
    if matches!(xy, "DD" | "AU" | "UD" | "UA" | "DU" | "AA" | "UU") {
        return FsGitStatus::Conflicted;
    }
    if x == 'D' || y == 'D' {
        return FsGitStatus::Deleted;
    }
    if x == 'M' || y == 'M' || x == 'T' || y == 'T' {
        return FsGitStatus::Modified;
    }
    if x == 'R' || y == 'R' || x == 'C' || y == 'C' {
        return FsGitStatus::Renamed;
    }
    if x == 'A' || y == 'A' {
        return FsGitStatus::Added;
    }
    FsGitStatus::Clean
}

pub fn parse_pull_request(stdout: &str) -> Option<FsPullRequest> {
    let raw = serde_json::from_str::<Value>(stdout).ok()?;
    let record = raw.as_object()?;
    let number = record.get("number")?.as_u64().filter(|number| *number > 0)?;
    let url = record.get("url")?.as_str()?;
    if !is_safe_http_url(url) {
        return None;
    }
    let normalized = record.get("state")?.as_str()?.to_lowercase();
    let state = match normalized.as_str() {
        "open" if record.get("isDraft") == Some(&Value::Bool(true)) => FsPullRequestState::Draft,
        "open" => FsPullRequestState::Open,
        "merged" => FsPullRequestState::Merged,
        "closed" => FsPullRequestState::Closed,
        _ => return None,
    };
    Some(FsPullRequest {
        number,
        state,
        url: url.to_string(),
    })
}

fn is_safe_http_url(value: &str) -> bool {
    if value
        .chars()
        .any(|character| character <= '\u{1f}' || character == '\u{7f}')
    {
        return false;
    }
    Url::parse(value)
        .ok()
        .is_some_and(|url| matches!(url.scheme(), "http" | "https"))
}
