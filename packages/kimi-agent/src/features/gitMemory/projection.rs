use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::types::{CommitRecord, GIT_MEMORY_MAX_FILES_PER_GROUP};

pub fn project_commit(commit: &CommitRecord, seen: &HashSet<String>) -> Vec<String> {
    let short = commit.sha.chars().take(7).collect::<String>();
    if seen.contains(&commit.sha) {
        return vec![format!("see commit {short} above")];
    }
    let mut lines = vec![
        format!("Commit {short} {} {}", commit.date, commit.author),
        format!("  {}", commit.subject),
    ];
    let has_body = !commit.body.is_empty();
    if has_body {
        for line in commit.body.split('\n') {
            lines.push(format!("  {line}"));
        }
    }
    if commit.files.len() > GIT_MEMORY_MAX_FILES_PER_GROUP {
        return lines;
    }
    if has_body && !commit.files.is_empty() {
        lines.push(String::new());
    }
    for file in &commit.files {
        lines.push(format!("  {file}"));
    }
    lines
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntryView {
    pub sha: String,
    pub date: String,
    pub title: String,
    pub body: String,
}

pub fn project_memory_block(entry: &MemoryEntryView) -> String {
    format!(
        "<memory sha=\"{}\" date=\"{}\">\n{}\n{}\n</memory>",
        entry.sha, entry.date, entry.title, entry.body
    )
}

pub fn project_repo_block(commits: &[CommitRecord]) -> String {
    if commits.is_empty() {
        return String::new();
    }
    let lines = commits
        .iter()
        .flat_map(|commit| project_commit(commit, &HashSet::new()))
        .collect::<Vec<_>>();
    format!("<repo-memory>\n{}\n</repo-memory>", lines.join("\n"))
}

pub fn project_repo_tree(dirs: &[String], depth: usize) -> String {
    let lines = dirs
        .iter()
        .filter(|dir| dir.split('/').count() <= depth)
        .cloned()
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "<repo-tree depth=\"{depth}\">\n{}\n</repo-tree>",
        lines.join("\n")
    )
}

pub fn project_file_block(
    path: &str,
    commits: &[CommitRecord],
    dir_path: &str,
    dir_commits: &[CommitRecord],
    seen: &HashSet<String>,
) -> String {
    let mut local = seen.clone();
    let mut lines = Vec::new();
    for commit in commits {
        lines.extend(project_commit(commit, &local));
        local.insert(commit.sha.clone());
    }
    let mut parts = Vec::new();
    if !lines.is_empty() {
        parts.push(lines.join("\n"));
    }
    if !dir_path.is_empty() && !dir_commits.is_empty() {
        parts.push(project_dir_block(dir_path, dir_commits, &local));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!(
        "<file-memory path=\"{path}\">\n{}\n</file-memory>",
        parts.join("\n")
    )
}

pub fn project_dir_block(
    path: &str,
    commits: &[CommitRecord],
    seen: &HashSet<String>,
) -> String {
    if commits.is_empty() {
        return String::new();
    }
    let lines = commits
        .iter()
        .flat_map(|commit| project_commit(commit, seen))
        .collect::<Vec<_>>();
    format!(
        "<dir-memory path=\"{path}\">\n{}\n</dir-memory>",
        lines.join("\n")
    )
}

pub fn project_signal_block(
    path: &str,
    commits: &[CommitRecord],
    seen: &HashSet<String>,
) -> String {
    if commits.is_empty() {
        return String::new();
    }
    let lines = commits
        .iter()
        .flat_map(|commit| project_commit(commit, seen))
        .collect::<Vec<_>>();
    format!(
        "<repo-memory signal=\"{path}\">\n{}\n</repo-memory>",
        lines.join("\n")
    )
}
