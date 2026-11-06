//! Large-session resume benchmark — the B-16 baseline (RUST-PORT-GAP §5 阶段 3).
//!
//! The upstream optimization this measures against is `256ce61f0c` ("speed up
//! large session resume"): v2 made resuming a multi-thousand-event session
//! cheaper. The fork cannot port (or reject) that work without knowing its own
//! numbers, and `tests/turn_bench.rs` measures the turn loop, not resume.
//!
//! What resume actually is on this engine, measured here end to end:
//!
//! 1. `append_event` — writing N wire rows (the cost is paid while the session
//!    runs, but it is what makes resume slow, so it is reported);
//! 2. `fold_projection` — the cold projection: read every row past the last
//!    compaction boundary, parse each payload, fold into conversation
//!    messages. This is the number "resume feels slow" is about.
//!
//! Run with:
//!   cargo test --release --test resume_bench -- --ignored --nocapture
//!
//! No `#[cfg(feature = "cli")]` gate: turn_bench has none either, and a cfg
//! gate silently compiles the file to zero tests under a plain
//! `cargo test --release` (measured: `--list` reported 0 tests twice before
//! this note).
//!
//! Scenario sizes mirror a long-lived session: mostly assistant/user message
//! pairs, a tool call every few turns, one compaction checkpoint.

use std::time::Instant;

use kimi_agent::native::event_store::{EventStore, RawWireEvent, SqliteEventStore};

/// One assistant/user exchange: the dominant row type in a real transcript.
fn message_rows(session: &str, turn: usize, seq: &mut u64, now: i64) -> Vec<RawWireEvent> {
    let mut rows = Vec::new();
    for (role, content) in [
        (
            "user",
            format!("question {turn} about the build and its config"),
        ),
        (
            "assistant",
            format!("answer {turn}: the build is two steps, see the tool output above"),
        ),
    ] {
        *seq += 1;
        rows.push(RawWireEvent {
            id: ulid::Ulid::new().to_string(),
            session_id: session.to_string(),
            event_type: format!("message.{role}"),
            payload: serde_json::json!({ "content": content }),
            is_checkpoint: false,
            is_compaction: false,
            created_at: now,
        });
    }
    rows
}

/// One tool call + result pair, the way a native tool records itself.
fn tool_rows(session: &str, turn: usize, seq: &mut u64, now: i64) -> Vec<RawWireEvent> {
    let call_id = format!("call_{turn}");
    *seq += 1;
    let started = RawWireEvent {
        id: ulid::Ulid::new().to_string(),
        session_id: session.to_string(),
        event_type: "tool.call.started".into(),
        payload: serde_json::json!({
            "toolCallId": call_id, "name": "Read", "arguments": {"path": format!("src/{turn}.rs")}
        }),
        is_checkpoint: false,
        is_compaction: false,
        created_at: now,
    };
    *seq += 1;
    let native = RawWireEvent {
        id: ulid::Ulid::new().to_string(),
        session_id: session.to_string(),
        event_type: "tool.native".into(),
        payload: serde_json::json!({
            "toolCallId": call_id, "content": format!("fn turn_{turn}() {{}}"), "isError": false
        }),
        is_checkpoint: false,
        is_compaction: false,
        created_at: now,
    };
    vec![started, native]
}

/// Build a session of `turns` exchanges (every 5th turn adds a tool pair) and
/// return the append cost alongside the store.
fn build_session(store: &SqliteEventStore, session: &str, turns: usize) -> std::time::Duration {
    let now = 1_700_000_000_000i64;
    let mut seq = 0u64;
    let started = Instant::now();
    for turn in 0..turns {
        for row in message_rows(session, turn, &mut seq, now) {
            store.append_event(&row).unwrap();
        }
        if turn % 5 == 0 {
            for row in tool_rows(session, turn, &mut seq, now) {
                store.append_event(&row).unwrap();
            }
        }
    }
    started.elapsed()
}

fn report(
    label: &str,
    turns: usize,
    write: std::time::Duration,
    fold: std::time::Duration,
    messages: usize,
) {
    println!(
        "{label:>18}: {turns:>6} turns | append {write:>10.1?} | fold {fold:>10.1?} | {messages:>5} messages"
    );
}

/// The B-16 numbers. `--ignored` keeps them out of every default `cargo test`
/// run; the assertions inside pin the invariant that matters (the fold returns
/// every message) so the bench cannot silently measure nothing.
#[test]
#[ignore = "benchmark: run with --release -- --ignored --nocapture"]
fn large_session_resume_baseline() {
    for turns in [500usize, 2_000, 8_000] {
        let session = format!("bench-{turns}");
        let store = SqliteEventStore::new_in_memory().unwrap();
        let write = build_session(&store, &session, turns);

        let started = Instant::now();
        let messages = store.fold_projection(&session).expect("cold projection");
        let fold = started.elapsed();

        // Every turn contributes a user + assistant message; the fold must
        // return them all. A resume that silently drops messages would show up
        // here as a shorter fold, i.e. a *better* number for the wrong reason.
        assert_eq!(messages.len(), turns * 2, "every message survives the fold");

        report(
            &format!("{turns} turns"),
            turns,
            write,
            fold,
            messages.len(),
        );
    }
}

/// The compaction-boundary path: rows before the last checkpoint are skipped
/// by `read_fold_rows`, so a compacted session folds faster than its row count
/// suggests. Measured separately because it is the lever a port of `256ce61f0c`
/// would actually pull.
#[test]
#[ignore = "benchmark: run with --release -- --ignored --nocapture"]
fn compacted_session_resume_baseline() {
    let turns = 4_000usize;
    let session = "bench-compacted";
    let store = SqliteEventStore::new_in_memory().unwrap();
    let write = build_session(&store, session, turns);

    // Compact halfway: the boundary row is what read_fold_rows folds from.
    store
        .append_event(&RawWireEvent {
            id: ulid::Ulid::new().to_string(),
            session_id: session.to_string(),
            event_type: "compaction.checkpoint".into(),
            payload: serde_json::json!({ "summary": "halfway" }),
            is_checkpoint: false,
            is_compaction: true,
            created_at: 1_700_000_000_001i64,
        })
        .unwrap();

    let started = Instant::now();
    let messages = store.fold_projection(session).expect("cold projection");
    let fold = started.elapsed();

    // Halfway compacted: the pre-boundary rows are skipped.
    assert!(
        messages.len() < turns * 2,
        "a compacted fold is shorter than the full log: {} < {}",
        messages.len(),
        turns * 2
    );
    report("compacted", turns, write, fold, messages.len());
}
