use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::turn_loop::types::{
    ExecutableToolResult, FileOperation, ToolAccesses, ToolCall, ToolFileAccess,
    ToolResourceAccess, all_access, read_file_access, read_tree_access, tool_accesses_conflict,
    write_file_access, write_tree_access,
};

#[derive(Debug, Clone)]
pub struct ScheduledToolCall {
    pub tool_call: ToolCall,
    pub accesses: ToolAccesses,
}

#[tracing::instrument(name = "schedule_tool_calls", skip_all, fields(call_count = tool_calls.len()))]
pub fn schedule_tool_calls(tool_calls: Vec<ScheduledToolCall>) -> Vec<Vec<ScheduledToolCall>> {
    if tool_calls.is_empty() {
        return vec![];
    }

    let all_empty = tool_calls.iter().all(|t| t.accesses.is_empty());
    if all_empty {
        return vec![tool_calls];
    }

    let mut batches: Vec<Vec<ScheduledToolCall>> = Vec::new();

    let mut floor = 0;

    for call in tool_calls {
        let mut index = floor;
        loop {
            if index == batches.len() {
                batches.push(Vec::new());
            }
            let conflicts = batches[index]
                .iter()
                .any(|existing| tool_accesses_conflict(&call.accesses, &existing.accesses));
            if !conflicts {
                batches[index].push(call);
                floor = index;
                break;
            }
            index += 1;
        }
    }

    batches
}

pub const SKIPPED_TOOL_OUTPUT: &str = "Tool skipped because a previous tool call stopped the turn.";

#[derive(Debug, Clone)]
pub struct ScheduledToolOutcome {
    pub results: Vec<ExecutableToolResult>,
    pub durations_ms: Vec<Option<u64>>,

    pub cancelled: bool,
}

#[tracing::instrument(name = "execute_scheduled", skip_all, fields(batch_size = scheduled.len()))]
pub async fn execute_scheduled<F, Fut>(
    cancellation: Option<&Arc<AtomicBool>>,
    mut scheduled: Vec<ScheduledToolCall>,
    execute_fn: F,
    run_count: Option<usize>,
) -> Result<ScheduledToolOutcome, Box<dyn std::error::Error>>
where
    F: Fn(ToolCall) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ExecutableToolResult, String>> + Send,
{
    let skipped_tail = match run_count {
        Some(count) if count < scheduled.len() => scheduled.split_off(count),
        _ => Vec::new(),
    };
    let batches = schedule_tool_calls(scheduled);
    if batches.is_empty() {
        let mut results = Vec::new();
        let mut durations_ms = Vec::new();
        push_skipped(&skipped_tail, &mut results, &mut durations_ms);
        return Ok(ScheduledToolOutcome {
            results,
            durations_ms,
            cancelled: false,
        });
    }
    let execute_fn = Arc::new(execute_fn);
    let mut all_results = Vec::with_capacity(batches.iter().map(Vec::len).sum());
    let mut all_durations = Vec::with_capacity(all_results.capacity());
    let mut batch_stopped = false;
    let total_calls = batches.iter().map(Vec::len).sum::<usize>();
    let call_names: Vec<String> = batches
        .iter()
        .flatten()
        .map(|s| s.tool_call.name.clone())
        .collect();
    for batch in batches {
        if is_cancelled(cancellation) {
            for scheduled in batch {
                all_results.push(ExecutableToolResult {
                    spill: None,
                    parts: Vec::new(),
                    delivery: None,
                    stop_turn: false,
                    content: aborted_tool_output(&scheduled.tool_call.name),
                    is_error: true,
                    note: None,
                    display: None,
                });
                all_durations.push(None);
            }
            push_skipped(&skipped_tail, &mut all_results, &mut all_durations);
            return Ok(ScheduledToolOutcome {
                results: all_results,
                durations_ms: all_durations,
                cancelled: true,
            });
        }
        if batch_stopped {
            for _ in batch {
                all_results.push(ExecutableToolResult {
                    spill: None,
                    parts: Vec::new(),
                    delivery: None,
                    stop_turn: false,
                    content: SKIPPED_TOOL_OUTPUT.to_string(),
                    is_error: true,
                    note: None,
                    display: None,
                });
                all_durations.push(None);
            }
            continue;
        }

        let mut handles = Vec::with_capacity(batch.len());
        for scheduled in batch {
            let tc = scheduled.tool_call;
            let execute_fn = execute_fn.clone();
            handles.push(tokio::spawn(async move {
                let started = std::time::Instant::now();
                let result = execute_fn(tc).await;
                result.map(|result| (result, started.elapsed().as_millis() as u64))
            }));
        }

        let mut failure: Option<String> = None;
        let mut grace_deadline: Option<std::time::Instant> = None;
        let mut pending = handles.into_iter();
        'collect: for mut handle in pending.by_ref() {
            let (joined, won_race) =
                match join_with_abort_grace(&mut handle, cancellation, &mut grace_deadline).await {
                    GraceJoin::Abandoned => {
                        failure = Some("turn cancelled".to_string());
                        break 'collect;
                    }
                    GraceJoin::Settled(joined) => (joined, false),
                    GraceJoin::WonRace(joined) => (joined, true),
                };

            let cancelled_now = !won_race && is_cancelled(cancellation);
            if won_race {
                failure = Some("turn cancelled".to_string());
            }
            match joined {
                Ok(Ok((result, duration_ms))) => {
                    if cancelled_now {
                        failure = Some("turn cancelled".to_string());
                        break;
                    }
                    if result.stop_turn {
                        batch_stopped = true;
                    }
                    all_results.push(result);
                    all_durations.push(Some(duration_ms));
                }
                Ok(Err(e)) => {
                    if cancelled_now {
                        failure = Some("turn cancelled".to_string());
                        break;
                    }
                    all_results.push(ExecutableToolResult {
                        spill: None,
                        parts: Vec::new(),
                        delivery: None,
                        stop_turn: false,
                        content: e,
                        is_error: true,
                        note: None,
                        display: None,
                    });
                    all_durations.push(None);
                }
                Err(e) => {
                    if cancelled_now {
                        failure = Some("turn cancelled".to_string());
                        break;
                    }
                    all_results.push(ExecutableToolResult {
                        spill: None,
                        parts: Vec::new(),
                        delivery: None,
                        stop_turn: false,
                        content: format!("Tool task join error: {e}"),
                        is_error: true,
                        note: None,
                        display: None,
                    });
                    all_durations.push(None);
                }
            }
        }

        match grace_deadline {
            Some(deadline) => {
                for mut handle in pending {
                    let name = call_names
                        .get(all_results.len())
                        .cloned()
                        .unwrap_or_default();
                    match join_until(&mut handle, deadline).await {
                        GraceJoin::WonRace(Ok(Ok((result, duration_ms)))) => {
                            all_results.push(result);
                            all_durations.push(Some(duration_ms));
                        }
                        GraceJoin::WonRace(Ok(Err(e))) => {
                            all_results.push(ExecutableToolResult {
                                spill: None,
                                parts: Vec::new(),
                                delivery: None,
                                stop_turn: false,
                                content: e,
                                is_error: true,
                                note: None,
                                display: None,
                            });
                            all_durations.push(None);
                        }
                        GraceJoin::WonRace(Err(e)) => {
                            all_results.push(ExecutableToolResult {
                                spill: None,
                                parts: Vec::new(),
                                delivery: None,
                                stop_turn: false,
                                content: format!("Tool task join error: {e}"),
                                is_error: true,
                                note: None,
                                display: None,
                            });
                            all_durations.push(None);
                        }

                        GraceJoin::Settled(_) | GraceJoin::Abandoned => {
                            all_results.push(ExecutableToolResult {
                                spill: None,
                                parts: Vec::new(),
                                delivery: None,
                                stop_turn: false,
                                content: aborted_tool_output(&name),
                                is_error: true,
                                note: None,
                                display: None,
                            });
                            all_durations.push(None);
                        }
                    }
                }
            }
            None => {
                for handle in pending {
                    handle.abort();
                }
            }
        }

        if failure.is_some() {
            while all_results.len() < total_calls {
                let index = all_results.len();
                let name = call_names.get(index).cloned().unwrap_or_default();
                all_results.push(ExecutableToolResult {
                    spill: None,
                    parts: Vec::new(),
                    delivery: None,
                    stop_turn: false,
                    content: aborted_tool_output(&name),
                    is_error: true,
                    note: None,
                    display: None,
                });
                all_durations.push(None);
            }
            push_skipped(&skipped_tail, &mut all_results, &mut all_durations);
            return Ok(ScheduledToolOutcome {
                results: all_results,
                durations_ms: all_durations,
                cancelled: true,
            });
        }
    }
    push_skipped(&skipped_tail, &mut all_results, &mut all_durations);
    Ok(ScheduledToolOutcome {
        results: all_results,
        durations_ms: all_durations,
        cancelled: false,
    })
}

fn push_skipped(
    skipped: &[ScheduledToolCall],
    results: &mut Vec<ExecutableToolResult>,
    durations: &mut Vec<Option<u64>>,
) {
    for _ in skipped {
        results.push(ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: SKIPPED_TOOL_OUTPUT.to_string(),
            is_error: true,
            note: None,
            display: None,
        });
        durations.push(None);
    }
}

fn aborted_tool_output(tool_name: &str) -> String {
    format!(
        "The user manually interrupted \"{tool_name}\" (and anything else running at the same time). This was a deliberate user action, not a system error, timeout, or capacity limit. Do not retry automatically or guess at the cause — wait for the user's next instruction."
    )
}

const ABORT_GRACE_MS: u64 = 2_000;

fn abort_grace() -> std::time::Duration {
    std::time::Duration::from_millis(ABORT_GRACE_MS)
}

enum GraceJoin<T> {
    Settled(Result<T, tokio::task::JoinError>),

    WonRace(Result<T, tokio::task::JoinError>),

    Abandoned,
}

async fn join_with_abort_grace<T>(
    handle: &mut tokio::task::JoinHandle<T>,
    cancellation: Option<&Arc<AtomicBool>>,
    deadline: &mut Option<std::time::Instant>,
) -> GraceJoin<T> {
    loop {
        let tick = tokio::time::sleep(std::time::Duration::from_millis(20));
        tokio::pin!(tick);
        tokio::select! {
            biased;
            () = &mut tick => {
                if !is_cancelled(cancellation) {
                    continue;
                }
                let dl = *deadline.get_or_insert_with(|| {
                    std::time::Instant::now() + abort_grace()
                });
                return join_until(handle, dl).await;
            }
            done = &mut *handle => return GraceJoin::Settled(done),
        }
    }
}

async fn join_until<T>(
    handle: &mut tokio::task::JoinHandle<T>,
    deadline: std::time::Instant,
) -> GraceJoin<T> {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        handle.abort();
        return GraceJoin::Abandoned;
    }
    let grace = tokio::time::sleep(remaining);
    tokio::pin!(grace);
    tokio::select! {
        biased;
        () = &mut grace => {
            handle.abort();
            GraceJoin::Abandoned
        }
        done = &mut *handle => GraceJoin::WonRace(done),
    }
}

fn is_cancelled(cancellation: Option<&Arc<AtomicBool>>) -> bool {
    cancellation.is_some_and(|flag| flag.load(Ordering::Relaxed))
}

pub fn infer_tool_accesses(tool_name: &str, args: &serde_json::Value) -> ToolAccesses {
    let name = tool_name.to_ascii_lowercase();
    let path = args.get("path").and_then(|p| p.as_str());
    match name.as_str() {
        "read" => path.map(read_file_access).into_iter().collect(),
        "write" => path.map(write_file_access).into_iter().collect(),
        "edit" => path
            .map(|p| {
                ToolResourceAccess::File(ToolFileAccess {
                    operation: FileOperation::ReadWrite,
                    path: p.to_string(),
                    recursive: false,
                })
            })
            .into_iter()
            .collect(),
        "grep" | "glob" | "listdirectory" | "list_directory" => {
            path.map(read_tree_access).into_iter().collect()
        }

        "fetchurl" | "fetch_url" | "websearch" | "web_search" | "agent" => vec![],
        _ if crate::agent::tools::github::github_legacy::is_github_tool(tool_name) => vec![],

        "bash" => vec![all_access()],

        "agentswarm" | "agent_swarm" => vec![all_access()],
        "towermerge" | "tower_merge" | "towerteardown" | "tower_teardown" => {
            vec![write_tree_access("/")]
        }

        _ => vec![all_access()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_loop::types::{
        FileOperation, ToolFileAccess, ToolResourceAccess, all_access, read_file_access,
        write_file_access,
    };

    #[test]
    fn test_empty_calls() {
        let result = schedule_tool_calls(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_no_accesses_all_parallel() {
        let calls = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![],
            },
        ];
        let batches = schedule_tool_calls(calls);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 2);
    }

    #[test]
    fn test_conflicting_writes_same_file() {
        let calls = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/foo/bar.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/foo/bar.txt")],
            },
        ];
        let batches = schedule_tool_calls(calls);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[1].len(), 1);
    }

    #[test]
    fn test_read_write_same_file_conflict() {
        let calls = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![read_file_access("/foo/bar.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/foo/bar.txt")],
            },
        ];
        let batches = schedule_tool_calls(calls);
        assert_eq!(batches.len(), 2);
    }

    #[test]
    fn test_read_read_same_file_no_conflict() {
        let calls = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![read_file_access("/foo/bar.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![read_file_access("/foo/bar.txt")],
            },
        ];
        let batches = schedule_tool_calls(calls);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 2);
    }

    #[test]
    fn test_all_access_conflicts_with_everything() {
        let calls = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "global".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![all_access()],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![read_file_access("/foo/bar.txt")],
            },
        ];
        let batches = schedule_tool_calls(calls);
        assert_eq!(batches.len(), 2);
    }

    #[test]
    fn test_non_overlapping_paths_no_conflict() {
        let calls = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/foo/a.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/bar/b.txt")],
            },
        ];
        let batches = schedule_tool_calls(calls);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 2);
    }

    #[test]
    fn test_recursive_contains_non_recursive() {
        let calls = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "write_tree".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![ToolResourceAccess::File(
                    crate::turn_loop::types::ToolFileAccess {
                        operation: crate::turn_loop::types::FileOperation::Write,
                        path: "/foo".to_string(),
                        recursive: true,
                    },
                )],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "write_file".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/foo/bar.txt")],
            },
        ];
        let batches = schedule_tool_calls(calls);
        assert_eq!(batches.len(), 2);
    }

    #[test]
    fn test_three_way_conflict() {
        let calls = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "w".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/file.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "w".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/file.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "3".into(),
                    name: "w".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/file.txt")],
            },
        ];
        let batches = schedule_tool_calls(calls);
        assert_eq!(batches.len(), 3);
        for batch in &batches {
            assert_eq!(batch.len(), 1);
        }
    }

    #[test]
    fn test_mixed_conflict_and_non_conflict() {
        let calls = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "w".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/a.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "w".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/b.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "3".into(),
                    name: "w".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/a.txt")],
            },
        ];
        let batches = schedule_tool_calls(calls);

        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 2);
        assert_eq!(batches[1].len(), 1);
    }

    #[test]
    fn test_search_no_conflict_with_read() {
        let calls = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "search".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![ToolResourceAccess::File(ToolFileAccess {
                    operation: FileOperation::Search,
                    path: "/foo".to_string(),
                    recursive: true,
                })],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![read_file_access("/foo/bar.txt")],
            },
        ];
        let batches = schedule_tool_calls(calls);

        assert_eq!(batches.len(), 1);
    }

    #[test]
    fn test_search_conflicts_with_write() {
        let calls = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "search".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![ToolResourceAccess::File(ToolFileAccess {
                    operation: FileOperation::Search,
                    path: "/foo".to_string(),
                    recursive: true,
                })],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/foo/bar.txt")],
            },
        ];
        let batches = schedule_tool_calls(calls);

        assert_eq!(batches.len(), 2);
    }

    #[test]
    fn test_readwrite_conflicts_with_both() {
        let calls = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "rw".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![ToolResourceAccess::File(ToolFileAccess {
                    operation: FileOperation::ReadWrite,
                    path: "/foo".to_string(),
                    recursive: false,
                })],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![read_file_access("/foo")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "3".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/foo")],
            },
        ];
        let batches = schedule_tool_calls(calls);

        assert_eq!(batches.len(), 3);
    }

    #[test]
    fn test_windows_path_normalization() {
        let calls = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "w".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("C:\\foo\\bar.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "w".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("C:/foo/bar.txt")],
            },
        ];
        let batches = schedule_tool_calls(calls);
        assert_eq!(batches.len(), 2);
    }

    #[test]
    fn test_multiple_accesses_per_tool() {
        let calls = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "multi".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![read_file_access("/a.txt"), write_file_access("/b.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "w".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/a.txt")],
            },
        ];
        let batches = schedule_tool_calls(calls);

        assert_eq!(batches.len(), 2);
    }

    #[test]
    fn test_all_access_blocks_all() {
        let calls = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "all".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![all_access()],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "r".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![read_file_access("/x.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "3".into(),
                    name: "w".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/y.txt")],
            },
        ];
        let batches = schedule_tool_calls(calls);

        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[1].len(), 2);
    }

    #[test]
    fn test_single_call_no_conflict() {
        let calls = vec![ScheduledToolCall {
            tool_call: ToolCall {
                id: "1".into(),
                name: "only".into(),
                arguments: serde_json::json!({}),
                extras: None,
                args_parse_error: None,
            },
            accesses: vec![write_file_access("/x.txt")],
        }];
        let batches = schedule_tool_calls(calls);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 1);
    }

    #[test]
    fn test_different_dirs_no_conflict() {
        let calls = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "w".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/project/a/src/main.rs")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "w".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/project/b/src/lib.rs")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "3".into(),
                    name: "w".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/project/c/README.md")],
            },
        ];
        let batches = schedule_tool_calls(calls);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 3);
    }

    #[test]
    fn test_infer_read() {
        let accesses = infer_tool_accesses("read", &serde_json::json!({"path": "/a.txt"}));
        assert_eq!(accesses, vec![read_file_access("/a.txt")]);
    }

    #[test]
    fn test_infer_write() {
        let accesses = infer_tool_accesses("Write", &serde_json::json!({"path": "/b.txt"}));
        assert_eq!(accesses, vec![write_file_access("/b.txt")]);
    }

    #[test]
    fn test_infer_edit_is_readwrite() {
        let accesses = infer_tool_accesses("edit", &serde_json::json!({"path": "/c.txt"}));
        assert_eq!(
            accesses,
            vec![ToolResourceAccess::File(ToolFileAccess {
                operation: FileOperation::ReadWrite,
                path: "/c.txt".into(),
                recursive: false,
            })]
        );
    }

    #[test]
    fn test_infer_grep_and_glob_are_tree_reads() {
        let accesses = infer_tool_accesses("grep", &serde_json::json!({"path": "/repo"}));
        assert_eq!(accesses, vec![read_tree_access("/repo")]);
        let accesses = infer_tool_accesses("Glob", &serde_json::json!({"path": "/repo"}));
        assert_eq!(accesses, vec![read_tree_access("/repo")]);
    }

    #[test]
    fn test_infer_bash_conflicts_with_everything() {
        let accesses = infer_tool_accesses("Bash", &serde_json::json!({"command": "echo hi"}));
        assert_eq!(accesses, vec![all_access()]);
        assert!(tool_accesses_conflict(
            &accesses,
            &vec![write_file_access("/any/file.txt")],
        ));
        assert!(tool_accesses_conflict(
            &accesses,
            &vec![read_file_access("/any/file.txt")],
        ));
    }

    #[test]
    fn test_infer_undeclared_tool_conflicts_with_everything() {
        let accesses = infer_tool_accesses("mcp__server__tool", &serde_json::json!({"x": 1}));
        assert_eq!(accesses, vec![all_access()]);
        assert!(tool_accesses_conflict(
            &accesses,
            &vec![read_file_access("/any/file.txt")],
        ));
        assert!(tool_accesses_conflict(
            &accesses,
            &infer_tool_accesses("mcp__other__tool", &serde_json::json!({})),
        ));
    }

    #[test]
    fn test_infer_declared_none_tools_stay_parallel_safe() {
        for name in ["FetchURL", "WebSearch", "GitHubGetRepo", "Agent"] {
            let accesses = infer_tool_accesses(name, &serde_json::json!({}));
            assert!(
                accesses.is_empty(),
                "{name} declares none() and must stay parallel-safe"
            );
        }
        assert!(!tool_accesses_conflict(
            &infer_tool_accesses("GitHubGetRepo", &serde_json::json!({})),
            &vec![write_file_access("/any/file.txt")],
        ));
    }

    #[test]
    fn test_infer_missing_path_has_no_accesses() {
        let accesses = infer_tool_accesses("read", &serde_json::json!({}));
        assert!(accesses.is_empty());
    }

    #[tokio::test]
    async fn test_execute_scheduled_conflicting_writes_serialized() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let scheduled = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/f.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/f.txt")],
            },
        ];
        let executor = {
            let active = active.clone();
            let max_active = max_active.clone();
            move |_tc: ToolCall| {
                let active = active.clone();
                let max_active = max_active.clone();
                async move {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(ExecutableToolResult {
                        spill: None,
                        parts: Vec::new(),
                        delivery: None,
                        stop_turn: false,
                        content: "ok".into(),
                        is_error: false,
                        note: None,
                        display: None,
                    })
                }
            }
        };
        let results = execute_scheduled(None, scheduled, executor, None)
            .await
            .unwrap()
            .results;
        assert_eq!(results.len(), 2);
        assert_eq!(
            max_active.load(Ordering::SeqCst),
            1,
            "conflicting calls must never overlap"
        );
    }

    #[tokio::test]
    async fn test_execute_scheduled_preserves_call_order() {
        let scheduled = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/a.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/a.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "3".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/b.txt")],
            },
        ];
        let executor = move |tc: ToolCall| async move {
            Ok(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: tc.id.clone(),
                is_error: false,
                note: None,
                display: None,
            })
        };
        let results = execute_scheduled(None, scheduled, executor, None)
            .await
            .unwrap()
            .results;
        let ids: Vec<&str> = results.iter().map(|r| r.content.as_str()).collect();
        assert_eq!(
            ids,
            vec!["1", "2", "3"],
            "results must come back in call order"
        );
    }

    #[tokio::test]
    async fn test_execute_scheduled_stops_on_cancellation() {
        let cancel = Arc::new(AtomicBool::new(true));
        let scheduled = vec![ScheduledToolCall {
            tool_call: ToolCall {
                id: "1".into(),
                name: "write".into(),
                arguments: serde_json::json!({}),
                extras: None,
                args_parse_error: None,
            },
            accesses: vec![write_file_access("/a.txt")],
        }];
        let executor = move |_tc: ToolCall| async move {
            Ok(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: "ok".into(),
                is_error: false,
                note: None,
                display: None,
            })
        };

        let outcome = execute_scheduled(Some(&cancel), scheduled, executor, None)
            .await
            .expect("a cancelled turn settles, it does not error the batch");
        assert!(outcome.cancelled);
        assert!(!outcome.results.is_empty());
        assert!(
            outcome.results[0]
                .content
                .contains("The user manually interrupted"),
            "unexpected output: {:?}",
            outcome.results[0].content
        );
    }

    #[tokio::test]
    async fn test_execute_scheduled_hung_tool_does_not_block_cancel() {
        let scheduled = vec![ScheduledToolCall {
            tool_call: ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({}),
                extras: None,
                args_parse_error: None,
            },
            accesses: vec![],
        }];
        let executor = move |_tc: ToolCall| async move {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: "too late".into(),
                is_error: false,
                note: None,
                display: None,
            })
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            canceller.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();
        let outcome = execute_scheduled(Some(&cancel), scheduled, executor, None)
            .await
            .expect("cancellation must win over the hung tool");
        assert!(outcome.cancelled);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "cancel took {:?}, the hung tool pinned the collector",
            started.elapsed()
        );
    }

    fn ok_result(content: &str) -> ExecutableToolResult {
        ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: content.into(),
            is_error: false,
            note: None,
            display: None,
        }
    }

    fn one_call(name: &str, accesses: Vec<ToolResourceAccess>) -> Vec<ScheduledToolCall> {
        vec![ScheduledToolCall {
            tool_call: ToolCall {
                id: "1".into(),
                name: name.into(),
                arguments: serde_json::json!({}),
                extras: None,
                args_parse_error: None,
            },
            accesses,
        }]
    }

    #[tokio::test]
    async fn a_tool_finishing_inside_the_grace_keeps_its_real_result() {
        let cancel = Arc::new(AtomicBool::new(false));
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            canceller.store(true, Ordering::Relaxed);
        });

        let executor = move |_tc: ToolCall| async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            Ok(ok_result("the real answer"))
        };

        let outcome = execute_scheduled(Some(&cancel), one_call("bash", vec![]), executor, None)
            .await
            .expect("the grace resolves the batch");

        assert!(outcome.cancelled, "the turn is still reported as cancelled");
        assert_eq!(
            outcome.results[0].content, "the real answer",
            "a result inside the grace window wins over the synthetic abort"
        );
        assert!(
            !outcome.results[0].is_error,
            "the tool did not fail; the user just stopped waiting"
        );
    }

    #[tokio::test]
    async fn a_tool_overrunning_the_grace_is_replaced_by_the_abort_output() {
        let cancel = Arc::new(AtomicBool::new(false));
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            canceller.store(true, Ordering::Relaxed);
        });
        let executor = move |_tc: ToolCall| async move {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(ok_result("far too late"))
        };

        let started = std::time::Instant::now();
        let outcome = execute_scheduled(Some(&cancel), one_call("bash", vec![]), executor, None)
            .await
            .expect("the grace resolves the batch");
        let elapsed = started.elapsed();

        assert!(outcome.cancelled);
        assert!(
            outcome.results[0]
                .content
                .contains("The user manually interrupted"),
            "past the window the abort stands: {:?}",
            outcome.results[0].content
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(ABORT_GRACE_MS - 100),
            "the grace must actually be waited out, took {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the grace must not be open-ended, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn the_grace_deadline_is_shared_across_the_batch() {
        let scheduled = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/a.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/b.txt")],
            },
        ];
        let cancel = Arc::new(AtomicBool::new(false));
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            canceller.store(true, Ordering::Relaxed);
        });
        let executor = move |_tc: ToolCall| async move {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(ok_result("far too late"))
        };

        let started = std::time::Instant::now();
        let outcome = execute_scheduled(Some(&cancel), scheduled, executor, None)
            .await
            .expect("the grace resolves the batch");
        let elapsed = started.elapsed();

        assert!(outcome.cancelled);
        assert_eq!(outcome.results.len(), 2, "both calls are still paired");
        for r in &outcome.results {
            assert!(
                r.content.contains("The user manually interrupted"),
                "both calls are past the shared window: {:?}",
                r.content
            );
        }
        assert!(
            elapsed < std::time::Duration::from_millis(ABORT_GRACE_MS * 2),
            "two calls must share one 2 s window, not get 2 s each: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn test_execute_scheduled_single_failure_keeps_siblings() {
        let scheduled = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/a.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/b.txt")],
            },
        ];
        let executor = move |tc: ToolCall| async move {
            if tc.id == "1" {
                Err("transport failed".to_string())
            } else {
                Ok(ExecutableToolResult {
                    spill: None,
                    parts: Vec::new(),
                    delivery: None,
                    stop_turn: false,
                    content: "ok-2".into(),
                    is_error: false,
                    note: None,
                    display: None,
                })
            }
        };
        let results = execute_scheduled(None, scheduled, executor, None)
            .await
            .unwrap()
            .results;
        assert_eq!(results.len(), 2);
        assert!(results[0].is_error, "call 1 must carry an error result");
        assert!(results[0].content.contains("transport failed"));
        assert!(!results[1].is_error, "sibling result must be kept");
        assert_eq!(results[1].content, "ok-2");
    }

    #[tokio::test]
    async fn test_execute_scheduled_non_conflicting_parallel() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let scheduled = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/a.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/b.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "3".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/c.txt")],
            },
        ];
        let executor = {
            let active = active.clone();
            let max_active = max_active.clone();
            move |_tc: ToolCall| {
                let active = active.clone();
                let max_active = max_active.clone();
                async move {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(ExecutableToolResult {
                        spill: None,
                        parts: Vec::new(),
                        delivery: None,
                        stop_turn: false,
                        content: "ok".into(),
                        is_error: false,
                        note: None,
                        display: None,
                    })
                }
            }
        };
        let results = execute_scheduled(None, scheduled, executor, None)
            .await
            .unwrap()
            .results;
        assert_eq!(results.len(), 3);
        assert!(
            max_active.load(Ordering::SeqCst) > 1,
            "non-conflicting calls should overlap"
        );
    }

    #[tokio::test]
    async fn test_execute_scheduled_empty() {
        let results = execute_scheduled(
            None,
            vec![],
            |_tc: ToolCall| async move {
                Ok(ExecutableToolResult {
                    spill: None,
                    parts: Vec::new(),
                    delivery: None,
                    stop_turn: false,
                    content: "x".into(),
                    is_error: false,
                    note: None,
                    display: None,
                })
            },
            None,
        )
        .await
        .unwrap()
        .results;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_execute_scheduled_preserves_order_across_batches() {
        let scheduled = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/f.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/g.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "3".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/f.txt")],
            },
        ];
        let results = execute_scheduled(
            None,
            scheduled,
            |tc: ToolCall| async move {
                Ok(ExecutableToolResult {
                    spill: None,
                    parts: Vec::new(),
                    delivery: None,
                    stop_turn: false,
                    content: tc.id,
                    is_error: false,
                    note: None,
                    display: None,
                })
            },
            None,
        )
        .await
        .unwrap()
        .results;
        let ids: Vec<&str> = results.iter().map(|r| r.content.as_str()).collect();
        assert_eq!(ids, vec!["1", "2", "3"]);
    }

    #[tokio::test]
    async fn test_execute_scheduled_predicted_stop_skips_the_tail() {
        let scheduled = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "UpdateGoal".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![all_access()],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/g.txt")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "3".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![],
            },
        ];
        let executed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = executed.clone();
        let results = execute_scheduled(
            None,
            scheduled,
            move |tc: ToolCall| {
                let seen = seen.clone();
                async move {
                    seen.lock().unwrap().push(tc.id.clone());
                    Ok(ExecutableToolResult {
                        spill: None,
                        parts: Vec::new(),
                        delivery: None,
                        stop_turn: false,
                        content: tc.id,
                        is_error: false,
                        note: None,
                        display: None,
                    })
                }
            },
            Some(1),
        )
        .await
        .unwrap()
        .results;

        assert_eq!(*executed.lock().unwrap(), vec!["1".to_string()]);
        assert_eq!(results[0].content, "1");
        assert_eq!(results[1].content, SKIPPED_TOOL_OUTPUT);
        assert!(results[1].is_error);
        assert_eq!(results[2].content, SKIPPED_TOOL_OUTPUT);
    }

    #[tokio::test]
    async fn test_execute_scheduled_records_durations_per_call() {
        let scheduled = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![all_access()],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![all_access()],
            },
        ];
        let outcome = execute_scheduled(
            None,
            scheduled,
            |tc: ToolCall| async move {
                if tc.id == "1" {
                    Ok(ExecutableToolResult {
                        spill: None,
                        parts: Vec::new(),
                        delivery: None,
                        stop_turn: true,
                        content: "stop".into(),
                        is_error: false,
                        note: None,
                        display: None,
                    })
                } else {
                    Ok(ExecutableToolResult {
                        spill: None,
                        parts: Vec::new(),
                        delivery: None,
                        stop_turn: false,
                        content: "skipped".into(),
                        is_error: true,
                        note: None,
                        display: None,
                    })
                }
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.results.len(), 2);
        assert_eq!(outcome.durations_ms.len(), 2);
        assert!(
            outcome.durations_ms[0].is_some(),
            "an executed call must carry its duration"
        );
        assert_eq!(
            outcome.durations_ms[1], None,
            "a skipped call never executed, so it has no duration"
        );
    }

    #[tokio::test]
    async fn test_execute_scheduled_stop_batch_skips_later_batches() {
        let scheduled = vec![
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "1".into(),
                    name: "exit_plan_mode".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/plan.md")],
            },
            ScheduledToolCall {
                tool_call: ToolCall {
                    id: "2".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                accesses: vec![write_file_access("/plan.md")],
            },
        ];
        let results = execute_scheduled(
            None,
            scheduled,
            |tc: ToolCall| async move {
                if tc.id == "1" {
                    Ok(ExecutableToolResult {
                        spill: None,
                        parts: Vec::new(),
                        delivery: None,
                        stop_turn: true,
                        content: "plan exited".into(),
                        is_error: false,
                        note: None,
                        display: None,
                    })
                } else {
                    Ok(ExecutableToolResult {
                        spill: None,
                        parts: Vec::new(),
                        delivery: None,
                        stop_turn: false,
                        content: "written".into(),
                        is_error: false,
                        note: None,
                        display: None,
                    })
                }
            },
            None,
        )
        .await
        .unwrap()
        .results;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].content, "plan exited");
        assert!(results[0].stop_turn);
        assert!(!results[0].is_error);

        assert_eq!(results[1].content, SKIPPED_TOOL_OUTPUT);
        assert!(results[1].is_error);
    }
}
