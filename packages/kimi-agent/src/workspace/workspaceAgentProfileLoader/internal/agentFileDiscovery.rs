use std::collections::HashMap;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use parking_lot::Mutex;

use crate::_base::utils::error_message::error_message;
use crate::_base::utils::paths::join;
use crate::app::agent_profile_catalog::agent_profile_contribution::SkippedAgentFile;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_fs_errors::{
    HostFsError, OS_FS_NOT_DIRECTORY, OS_FS_NOT_FOUND, OS_FS_UNAVAILABLE,
};
use crate::wire::record::WireAsyncError;

use super::agent_file::{AgentFileParseError, ParseAgentFileOptions, parse_agent_file_text};
use super::paths::{is_directory_path, is_file_path};
use super::types::{
    AgentFileDefinition, AgentFileDiscoveryResult, AgentFileRoot,
};

const MAX_AGENT_SCAN_DEPTH: u32 = 8;
const MAX_SKIP_WARNINGS: usize = 5;

pub type DiscoverAgentFilesWarn<'a> = &'a dyn Fn(&str, Option<&WireAsyncError>);

struct WarningState {
    emitted: usize,
    suppressed: usize,
    subjects: Vec<String>,
}

pub async fn discover_agent_files(
    fs: &dyn IHostFileSystem,
    roots: &[AgentFileRoot],
    warn: Option<DiscoverAgentFilesWarn<'_>>,
) -> Result<AgentFileDiscoveryResult, WireAsyncError> {
    let mut by_name = HashMap::<String, AgentFileDefinition>::new();
    let mut skipped = Vec::new();
    let warning_state = Arc::new(Mutex::new(WarningState {
        emitted: 0,
        suppressed: 0,
        subjects: Vec::new(),
    }));
    for root in roots {
        let result = walk(
            fs,
            root.path.clone(),
            root,
            0,
            &mut by_name,
            &mut skipped,
            warn,
            &warning_state,
        )
        .await;
        if let Err(error) = result {
            if is_unavailable(&error) {
                return Err(error);
            }
            warn_capped(
                &warning_state,
                warn,
                &root.path,
                &format!(
                    "Skipping unreadable agent root {}: {}",
                    root.path,
                    error_message(&error)
                ),
                Some(&error),
            );
        }
    }
    let warnings = warning_state.lock();
    if warnings.suppressed > 0 {
        if let Some(warn) = warn {
            let examples = warnings
                .subjects
                .iter()
                .map(|subject| format!("\"{subject}\""))
                .collect::<Vec<_>>()
                .join(", ");
            warn(
                &format!(
                    "Suppressed {} further agent-discovery skip warnings (e.g. {examples}); fix or remove the offending files/directories to silence them",
                    warnings.suppressed
                ),
                None,
            );
        }
    }
    drop(warnings);
    let mut agents = by_name.into_values().collect::<Vec<_>>();
    agents.sort_by(|a, b| locale_compare(&a.name, &b.name));
    Ok(AgentFileDiscoveryResult {
        agents,
        skipped,
        scanned_roots: roots.iter().map(|root| root.path.clone()).collect(),
    })
}

fn walk<'a>(
    fs: &'a dyn IHostFileSystem,
    dir_path: String,
    root: &'a AgentFileRoot,
    depth: u32,
    by_name: &'a mut HashMap<String, AgentFileDefinition>,
    skipped: &'a mut Vec<SkippedAgentFile>,
    warn: Option<DiscoverAgentFilesWarn<'a>>,
    warning_state: &'a Arc<Mutex<WarningState>>,
) -> BoxFuture<'a, Result<(), WireAsyncError>> {
    async move {
        if depth > MAX_AGENT_SCAN_DEPTH {
            return Ok(());
        }
        let entries = match fs.readdir(&dir_path).await {
            Ok(entries) => {
                let mut entries = entries
                    .into_iter()
                    .map(|entry| entry.name)
                    .collect::<Vec<_>>();
                entries.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
                entries
            }
            Err(error) => {
                if depth > 0 {
                    warn_capped(
                        warning_state,
                        warn,
                        &dir_path,
                        &format!(
                            "Skipping unreadable directory {dir_path}: {}",
                            error_message(&error)
                        ),
                        Some(&error),
                    );
                    return Ok(());
                }
                if has_code(&error, OS_FS_NOT_FOUND) || has_code(&error, OS_FS_NOT_DIRECTORY) {
                    return Ok(());
                }
                return Err(error);
            }
        };
        for entry in entries {
            if entry.starts_with('.') || entry == "node_modules" {
                continue;
            }
            let entry_path = join(&dir_path, &entry);
            let result = async {
                if is_directory_path(fs, &entry_path).await? {
                    walk(
                        fs,
                        entry_path.clone(),
                        root,
                        depth + 1,
                        by_name,
                        skipped,
                        warn,
                        warning_state,
                    )
                    .await?;
                    return Ok(());
                }
                if !entry.ends_with(".md") || !is_file_path(fs, &entry_path).await? {
                    return Ok(());
                }
                parse_and_register(fs, &entry_path, root, by_name, skipped, warn, warning_state).await
            }
            .await;
            if let Err(error) = result {
                if is_unavailable(&error) {
                    return Err(error);
                }
                warn_capped(
                    warning_state,
                    warn,
                    &entry_path,
                    &format!(
                        "Skipping unreadable agent path {entry_path}: {}",
                        error_message(&error)
                    ),
                    Some(&error),
                );
            }
        }
        Ok(())
    }
    .boxed()
}

async fn parse_and_register(
    fs: &dyn IHostFileSystem,
    file_path: &str,
    root: &AgentFileRoot,
    by_name: &mut HashMap<String, AgentFileDefinition>,
    skipped: &mut Vec<SkippedAgentFile>,
    warn: Option<DiscoverAgentFilesWarn<'_>>,
    warning_state: &Arc<Mutex<WarningState>>,
) -> Result<(), WireAsyncError> {
    let result = async {
        let text = fs.read_text(file_path).await?;
        let agent = parse_agent_file_text(ParseAgentFileOptions {
            path: file_path.to_string(),
            source: root.source,
            text,
        })?;
        if !by_name.contains_key(&agent.name) {
            by_name.insert(agent.name.clone(), agent);
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            if is_unavailable(&error) {
                return Err(error);
            }
            if let Some(parse_error) = error.downcast_ref::<AgentFileParseError>() {
                skipped.push(SkippedAgentFile {
                    path: file_path.to_string(),
                    reason: parse_error.to_string(),
                });
                warn_capped(
                    warning_state,
                    warn,
                    file_path,
                    &format!(
                        "Skipping invalid agent file at {file_path}: {}",
                        parse_error.to_string()
                    ),
                    Some(&error),
                );
            } else {
                warn_capped(
                    warning_state,
                    warn,
                    file_path,
                    &format!("Skipping agent file at {file_path} due to unexpected error"),
                    Some(&error),
                );
            }
            Ok(())
        }
    }
}

fn warn_capped(
    state: &Arc<Mutex<WarningState>>,
    warn: Option<DiscoverAgentFilesWarn<'_>>,
    subject: &str,
    message: &str,
    error: Option<&WireAsyncError>,
) {
    let mut state = state.lock();
    if state.emitted < MAX_SKIP_WARNINGS {
        state.emitted += 1;
        drop(state);
        if let Some(warn) = warn {
            warn(message, error);
        }
    } else {
        state.suppressed += 1;
        if state.subjects.len() < 3 {
            state.subjects.push(subject.to_string());
        }
    }
}

fn has_code(error: &WireAsyncError, code: &str) -> bool {
    error
        .downcast_ref::<HostFsError>()
        .is_some_and(|error| error.code().as_str() == code)
}

fn is_unavailable(error: &WireAsyncError) -> bool {
    has_code(error, OS_FS_UNAVAILABLE)
}

fn locale_compare(left: &str, right: &str) -> std::cmp::Ordering {
    let script = format!(
        "String((function (left, right) {{ return left.localeCompare(right); }})({}, {}))",
        serde_json::to_string(left).unwrap(),
        serde_json::to_string(right).unwrap()
    );
    rquickjs::Runtime::new()
        .and_then(|runtime| {
            rquickjs::Context::full(&runtime).and_then(|context| {
                context.with(|ctx| ctx.eval::<String, _>(script))
            })
        })
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .map(|value| value.cmp(&0))
        .unwrap_or_else(|| left.encode_utf16().cmp(right.encode_utf16()))
}
