use super::*;

/// A panic while the connection guard is held must stay confined to the
/// call that caused it.
///
/// This is the property `parking_lot` buys and `std::sync` does not. With a
/// poisoning std mutex, the panic below would mark the connection guard
/// poisoned, and then **every** later `lock()` on this single connection
/// would panic too — the whole session store dead for the process
/// lifetime, with no recovery and nothing in the log. The test pins the
/// non-poisoning behaviour itself, so swapping the lock type back cannot
/// pass silently.
#[test]
fn a_panic_under_the_connection_guard_does_not_poison_the_store() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-alive", None).unwrap();

    // Silence the panic message so a passing run stays quiet; restore the
    // default hook afterwards because the process is shared.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = store.conn.lock();
        panic!("a panic inside the connection guard");
    }));
    std::panic::set_hook(previous);

    assert!(caught.is_err(), "the panic must actually have happened");
    // The point of the whole change: the store still answers.
    let sessions = store
        .list_sessions()
        .expect("a read after a guarded panic must still work");
    assert!(
        sessions.iter().any(|s| s.session_id == "sess-alive"),
        "the session written before the panic is still readable: {sessions:?}"
    );
}

/// One unparseable `wire_events` payload must not decide the whole cold
/// rebuild: the session still opens from the rows around it, and the bad
/// row is left on disk rather than repaired behind the user's back.
#[test]
fn test_fold_projection_skips_a_corrupt_wire_event_row() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-fold", None).unwrap();
    for (id, event_type, content) in [
        ("evt_1", "message.user", "first"),
        ("evt_2", "message.assistant", "second"),
        ("evt_3", "message.user", "third"),
    ] {
        store
            .append_wire_event(&RawWireEvent {
                id: id.into(),
                session_id: "sess-fold".into(),
                event_type: event_type.into(),
                payload: json!({ "content": content }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 1,
            })
            .unwrap();
    }
    assert_eq!(store.fold_projection("sess-fold").unwrap().len(), 3);

    // A crash mid-write leaves a half-serialized payload in one row.
    let truncated = "{\"content\": \"sec";
    {
        let conn = store.conn.lock();
        conn.execute(
            "UPDATE wire_events SET payload = ?1 WHERE id = 'evt_2'",
            params![truncated],
        )
        .unwrap();
    }

    let projected = store
        .fold_projection("sess-fold")
        .expect("one corrupt row must not fail the whole fold");
    let contents: Vec<&str> = projected.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, vec!["first", "third"]);

    // The row is still there, unrepaired: a repair entry point stays a
    // decision for the caller, not something a read may do.
    let conn = store.conn.lock();
    let payload: String = conn
        .query_row(
            "SELECT payload FROM wire_events WHERE id = 'evt_2'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(payload, truncated);
}

/// A file over the byte cap is recorded as change metadata plus the
/// `oversize` marker; its content never reaches the table.
#[test]
fn test_file_history_caps_content_bytes() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-big", None).unwrap();
    let big = "x".repeat(FILE_HISTORY_MAX_CONTENT_BYTES + 1);

    store
        .record_file_change("sess-big", 1, "big.bin", Some("small before"), Some(&big))
        .unwrap();
    store
        .record_file_change("sess-big", 2, "small.txt", Some("a"), Some("b"))
        .unwrap();

    // The over-cap row keeps the change metadata and is marked.
    let (changes, _) = store.get_file_history_changes("sess-big", Some(1)).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].status, "modified");
    assert_eq!(changes[0].oversize, Some(true));

    // No content on either side, and the content endpoint says why.
    let stored: (Option<String>, Option<String>, i64) = {
        let conn = store.conn.lock();
        conn.query_row(
            "SELECT content_before, content_after, oversize FROM session_file_history \
                 WHERE session_id = 'sess-big' AND turn_id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
    };
    assert!(stored.0.is_none(), "before content was stored");
    assert!(stored.1.is_none(), "after content was stored");
    assert_eq!(stored.2, 1);
    for phase in ["start", "end"] {
        let content = store
            .get_file_history_content("sess-big", 1, "big.bin", phase)
            .unwrap()
            .unwrap();
        assert_eq!(content.oversize, Some(true), "phase {phase}");
        assert!(content.content.is_none(), "phase {phase}");
    }

    // A file under the cap is untouched: the marker is false and the
    // content is still there.
    let (small, _) = store.get_file_history_changes("sess-big", Some(2)).unwrap();
    assert_eq!(small[0].oversize, Some(false));
    let small_end = store
        .get_file_history_content("sess-big", 2, "small.txt", "end")
        .unwrap()
        .unwrap();
    assert_eq!(small_end.oversize, None, "no marker key for stored content");
    assert_eq!(small_end.content.as_deref(), Some("b"));
}

/// Pruning has to give the WAL back: a `DELETE` on its own leaves the
/// whole deleted payload sitting in the log until SQLite checkpoints on
/// its own schedule. (The main file's high-water mark is a separate,
/// still-open question — see `reclaim_file_history_space`.)
#[test]
fn test_prune_file_history_reclaims_the_wal() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("sessions.db");
    let store = SqliteSessionStore::open(&db_path).unwrap();
    store.create_session("sess-reclaim", None).unwrap();
    let body = "y".repeat(200_000);
    for turn in 1..=8usize {
        store
            .record_file_change("sess-reclaim", turn, "big.txt", Some(&body), Some(&body))
            .unwrap();
    }
    let size = |path: &Path| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let wal_path = db_path.with_extension("db-wal");
    // The store is in WAL mode, so every byte of those rows is still in
    // the log until a prune checkpoints it.
    let wal_before = size(&wal_path);
    assert!(
        wal_before > 1_000_000,
        "the rows must really be in the wal: {wal_before}"
    );

    assert_eq!(store.prune_file_history("sess-reclaim", 1).unwrap(), 7);

    let wal_after = size(&wal_path);
    assert!(
        wal_after < wal_before / 4,
        "the wal must be checkpointed and truncated, not left to grow: \
             {wal_before} -> {wal_after}"
    );
    // The pages themselves are free for reuse even though the main file
    // keeps its size until something runs a full VACUUM.
    let freelist: i64 = {
        let conn = store.conn.lock();
        conn.query_row("PRAGMA freelist_count", [], |row| row.get(0))
            .unwrap()
    };
    assert!(freelist > 0, "the deleted pages must be reusable");
}

#[test]
fn test_sqlite_session_lifecycle() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session("sess-1", Some("Test Session"))
        .unwrap();

    let s_init = store.get_session("sess-1").unwrap().unwrap();
    assert_eq!(s_init.session_id, "sess-1");
    assert_eq!(s_init.title.as_deref(), Some("Test Session"));
    assert!(s_init.created_at > 0);
    assert!(s_init.updated_at >= s_init.created_at);

    // Updating session without title preserves original title and updates updated_at
    store.create_session("sess-1", None).unwrap();
    let s_preserved = store.get_session("sess-1").unwrap().unwrap();
    assert_eq!(s_preserved.title.as_deref(), Some("Test Session"));
    assert!(s_preserved.updated_at >= s_init.updated_at);

    // Save turn with TokenUsage
    let msgs = vec![
        LLMMessage::user("Hello"),
        LLMMessage::assistant("Hi there!"),
    ];
    let usage = TokenUsage {
        input_tokens: 150,
        output_tokens: 42,
        total_tokens: 192,
        input_cache_creation: 10,
        input_cache_read: 5,
    };
    store
        .save_turn("sess-1", "turn-1", 1, &msgs, Some(&usage), None)
        .unwrap();

    // Verify loaded messages
    let loaded = store.load_session_history("sess-1").unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].role, "user");
    assert_eq!(loaded[0].content, "Hello");
    assert_eq!(loaded[1].role, "assistant");
    assert_eq!(loaded[1].content, "Hi there!");

    // Verify turn record and persisted token usage
    {
        let conn = store.conn.lock();
        let (status, usage_json): (String, Option<String>) = conn
            .query_row(
                "SELECT status, usage FROM turns WHERE turn_id = 'turn-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "completed");
        let parsed_usage: TokenUsage =
            serde_json::from_str(&usage_json.expect("usage serialized")).unwrap();
        assert_eq!(parsed_usage.input_tokens, 150);
        assert_eq!(parsed_usage.output_tokens, 42);
        assert_eq!(parsed_usage.input_cache_creation, 10);
        assert_eq!(parsed_usage.input_cache_read, 5);
    }

    // Save a second turn and verify chronological order
    let msgs_turn2 = vec![
        LLMMessage::user("Second question"),
        LLMMessage::assistant("Second answer"),
    ];
    store
        .save_turn("sess-1", "turn-2", 2, &msgs_turn2, None, None)
        .unwrap();
    let loaded2 = store.load_session_history("sess-1").unwrap();
    assert_eq!(loaded2.len(), 4);
    assert_eq!(loaded2[2].content, "Second question");
    assert_eq!(loaded2[3].content, "Second answer");

    let list = store.list_sessions().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].session_id, "sess-1");
    assert_eq!(list[0].title.as_deref(), Some("Test Session"));
}

#[test]
fn test_sqlite_state_entries_and_checkpoints() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-1", None).unwrap();

    // 1. Put and get state with complex object
    let state_val = serde_json::json!({
        "name": "commit",
        "flags": ["staged", "amend"],
        "count": 42
    });
    store.put_state("skill", "commit", &state_val).unwrap();
    let read = store.get_state("skill", "commit").unwrap().unwrap();
    assert_eq!(read, state_val);
    assert_eq!(read["flags"][0], "staged");
    assert_eq!(read["count"], 42);

    // State overwrite updates value
    let updated_val = serde_json::json!({ "name": "commit", "count": 43 });
    store.put_state("skill", "commit", &updated_val).unwrap();
    assert_eq!(
        store.get_state("skill", "commit").unwrap().unwrap()["count"],
        43
    );

    // Non-existent key returns None
    assert!(store.get_state("skill", "unknown").unwrap().is_none());
    assert!(store.get_state("unknown_domain", "key").unwrap().is_none());

    // 2. Save and verify checkpoint
    let chk_data = serde_json::json!({ "step": 1, "status": "in_progress" });
    store
        .save_checkpoint("sess-1", "chk-1", "Step 1", &chk_data)
        .unwrap();

    // Verify checkpoint in sqlite table
    {
        let conn = store.conn.lock();
        let (sess_id, name, data_str, created_at): (String, String, String, i64) = conn
            .query_row(
                "SELECT session_id, name, data, created_at FROM checkpoints WHERE id = 'chk-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(sess_id, "sess-1");
        assert_eq!(name, "Step 1");
        assert_eq!(data_str, serde_json::to_string(&chk_data).unwrap());
        assert!(created_at > 0);
    }

    // Updating checkpoint on conflict
    let chk_data2 = serde_json::json!({ "step": 2, "status": "done" });
    store
        .save_checkpoint("sess-1", "chk-1", "Step 2 Done", &chk_data2)
        .unwrap();
    {
        let conn = store.conn.lock();
        let (name, data_str): (String, String) = conn
            .query_row(
                "SELECT name, data FROM checkpoints WHERE id = 'chk-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(name, "Step 2 Done");
        assert_eq!(data_str, serde_json::to_string(&chk_data2).unwrap());
    }

    // Foreign key constraint: saving checkpoint for non-existent session fails
    let bad_chk = store.save_checkpoint(
        "non-existent-sess",
        "chk-bad",
        "Bad",
        &serde_json::json!({}),
    );
    assert!(
        bad_chk.is_err(),
        "foreign key constraint must reject non-existent session"
    );
}

#[test]
fn test_sqlite_structured_tool_calls_and_blocks() {
    use crate::turn_loop::types::{ContentBlock, ToolCall};

    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-structured", None).unwrap();

    let msgs = vec![
        LLMMessage {
            role: "user".into(),
            content: "show me image".into(),
            blocks: vec![
                ContentBlock::Text {
                    text: "show me image".into(),
                },
                ContentBlock::ImageUrl {
                    url: "https://example.com/cat.png".into(),
                    id: None,
                    name: None,
                },
            ],
            tool_calls: vec![],
            tool_call_id: None,

            prompt_id: None,
            origin: None,
        },
        LLMMessage {
            role: "assistant".into(),
            content: "calling tools".into(),
            blocks: vec![],
            tool_calls: vec![
                ToolCall {
                    id: "call_read_1".into(),
                    name: "Read".into(),
                    arguments: serde_json::json!({ "path": "file.txt" }),
                    extras: None,
                },
                ToolCall {
                    id: "call_read_2".into(),
                    name: "Grep".into(),
                    arguments: serde_json::json!({ "pattern": "TODO" }),
                    extras: None,
                },
            ],
            tool_call_id: None,

            prompt_id: None,
            origin: None,
        },
        LLMMessage {
            role: "tool".into(),
            content: "file contents".into(),
            blocks: vec![],
            tool_calls: vec![],
            tool_call_id: Some("call_read_1".into()),
            prompt_id: None,
            origin: None,
        },
        LLMMessage {
            role: "tool".into(),
            content: "matches found".into(),
            blocks: vec![],
            tool_calls: vec![],
            tool_call_id: Some("call_read_2".into()),
            prompt_id: None,
            origin: None,
        },
    ];

    store
        .save_turn("sess-structured", "turn-1", 1, &msgs, None, None)
        .unwrap();

    let loaded = store.load_session_history("sess-structured").unwrap();
    assert_eq!(loaded.len(), 4);

    // Turn 1 user message with multiple blocks
    assert_eq!(loaded[0].role, "user");
    assert_eq!(loaded[0].blocks.len(), 2);
    assert_eq!(
        loaded[0].blocks[0],
        ContentBlock::Text {
            text: "show me image".into()
        }
    );
    assert_eq!(
        loaded[0].blocks[1],
        ContentBlock::ImageUrl {
            url: "https://example.com/cat.png".into(),
            id: None,
            name: None,
        }
    );

    // Turn 1 assistant message with multiple structured tool calls
    assert_eq!(loaded[1].role, "assistant");
    assert_eq!(loaded[1].tool_calls.len(), 2);
    assert_eq!(loaded[1].tool_calls[0].id, "call_read_1");
    assert_eq!(loaded[1].tool_calls[0].name, "Read");
    assert_eq!(loaded[1].tool_calls[0].arguments["path"], "file.txt");
    assert_eq!(loaded[1].tool_calls[1].id, "call_read_2");
    assert_eq!(loaded[1].tool_calls[1].name, "Grep");
    assert_eq!(loaded[1].tool_calls[1].arguments["pattern"], "TODO");

    // Turn 1 tool results with corresponding tool_call_ids
    assert_eq!(loaded[2].role, "tool");
    assert_eq!(loaded[2].content, "file contents");
    assert_eq!(loaded[2].tool_call_id.as_deref(), Some("call_read_1"));

    assert_eq!(loaded[3].role, "tool");
    assert_eq!(loaded[3].content, "matches found");
    assert_eq!(loaded[3].tool_call_id.as_deref(), Some("call_read_2"));
}

#[test]
fn test_fork_session_copies_history_and_creates_new_session() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session_with_workspace("sess-orig", Some("Original"), Some("wd_test_123"))
        .unwrap();
    let msgs = vec![
        LLMMessage::user("hello from original"),
        LLMMessage::assistant("assistant reply"),
    ];
    store
        .save_turn("sess-orig", "turn-1", 1, &msgs, None, None)
        .unwrap();

    // Fork to sess-fork with explicit title
    let ok = store
        .fork_session("sess-orig", "sess-fork", Some("Forked"), None)
        .unwrap();
    assert!(ok);

    let forked = store.get_session("sess-fork").unwrap().unwrap();
    assert_eq!(forked.session_id, "sess-fork");
    assert_eq!(forked.title.as_deref(), Some("Forked"));
    assert_eq!(forked.workspace_id.as_deref(), Some("wd_test_123"));

    let forked_history = store.load_session_history("sess-fork").unwrap();
    assert_eq!(forked_history.len(), 2);
    assert_eq!(forked_history[0].content, "hello from original");
    assert_eq!(forked_history[1].content, "assistant reply");

    // Subsequent turns in original do not leak into forked session (isolation)
    store
        .save_turn(
            "sess-orig",
            "turn-2",
            2,
            &[LLMMessage::user("isolated prompt")],
            None,
            None,
        )
        .unwrap();
    assert_eq!(store.load_session_history("sess-orig").unwrap().len(), 3);
    assert_eq!(store.load_session_history("sess-fork").unwrap().len(), 2);

    // Fork without title inherits original title
    let ok_inherit = store
        .fork_session("sess-orig", "sess-inherit", None, None)
        .unwrap();
    assert!(ok_inherit);
    let inherited = store.get_session("sess-inherit").unwrap().unwrap();
    assert_eq!(inherited.title.as_deref(), Some("Original"));
    assert_eq!(inherited.workspace_id.as_deref(), Some("wd_test_123"));

    // Fork non-existent returns false
    let missing = store
        .fork_session("sess-missing", "sess-none", None, None)
        .unwrap();
    assert!(!missing);
}

/// A fork keeps the source's turn structure instead of collapsing it into one
/// synthetic row (v2 `sliceMainRecordsAtTurn` / `copyAgentWire`).
///
/// Before the cut, `fork_session` called
/// `save_turn(new, "turn-fork", 1, &history)` — a four-message, two-turn
/// session came back as four messages under a single turn, so `list_turns`
/// reported one row and turn-scoped reads (`/undo`'s plan, file history)
/// had nothing to scope to.
#[test]
fn a_fork_preserves_the_source_turn_structure() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session_with_workspace("sess-src", Some("Src"), Some("wd_fork"))
        .unwrap();
    store
        .save_turn(
            "sess-src",
            "turn-a",
            1,
            &[LLMMessage::user("u1"), LLMMessage::assistant("a1")],
            None,
            None,
        )
        .unwrap();
    store
        .save_turn(
            "sess-src",
            "turn-b",
            2,
            &[LLMMessage::user("u2"), LLMMessage::assistant("a2")],
            None,
            None,
        )
        .unwrap();

    store
        .fork_session("sess-src", "sess-whole", None, None)
        .unwrap();

    let turns = store.list_turns("sess-whole").unwrap();
    assert_eq!(
        turns.iter().map(|t| t.turn_number).collect::<Vec<_>>(),
        vec![1, 2],
        "each source turn is copied as its own row, numbered consecutively"
    );
    assert_eq!(store.load_session_history("sess-whole").unwrap().len(), 4);
    // The copy is independent: a later source turn must not leak in.
    store
        .save_turn(
            "sess-src",
            "turn-c",
            3,
            &[LLMMessage::user("u3")],
            None,
            None,
        )
        .unwrap();
    assert_eq!(store.load_session_history("sess-whole").unwrap().len(), 4);
}

/// `turnIndex` names the last retained turn: v2 slices
/// `records.slice(0, turnStarts[turnIndex + 1])`, so the named turn *and
/// everything before it* are kept and later turns are dropped.
#[test]
fn a_turn_index_cuts_the_history_at_that_turn() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session_with_workspace("sess-src", Some("Src"), Some("wd_cut"))
        .unwrap();
    for (n, id) in ["turn-a", "turn-b", "turn-c"].iter().enumerate() {
        store
            .save_turn(
                "sess-src",
                id,
                n as u32 + 1,
                &[LLMMessage::user(*id), LLMMessage::assistant("reply")],
                None,
                None,
            )
            .unwrap();
    }

    store
        .fork_session("sess-src", "sess-cut", None, Some(1))
        .unwrap();

    let turns = store.list_turns("sess-cut").unwrap();
    assert_eq!(
        turns.iter().map(|t| t.turn_number).collect::<Vec<_>>(),
        vec![1, 2],
        "index 1 keeps turns 0 and 1 and drops turn 2"
    );
    let history = store.load_session_history("sess-cut").unwrap();
    assert_eq!(history.len(), 4);
    assert!(
        history.iter().all(|m| m.content != "turn-c"),
        "the cut turn's messages are absent"
    );
}

/// The compaction turn is a recognised key, not just a label: `undo` refuses
/// to cross it by string comparison. Copying a compacted session must keep
/// that exact id, or the fork silently loses the guard and an undo could
/// delete a turn out from under the summary that explains it.
#[test]
fn a_fork_keeps_the_compaction_undo_guard() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session_with_workspace("sess-c", Some("Src"), Some("wd_compact"))
        .unwrap();
    store
        .save_turn(
            "sess-c",
            COMPACT_TURN_ID,
            1,
            &[LLMMessage::assistant("summary of earlier work")],
            None,
            None,
        )
        .unwrap();
    store
        .save_turn(
            "sess-c",
            "turn-real",
            2,
            &[LLMMessage::user("q"), LLMMessage::assistant("a")],
            None,
            None,
        )
        .unwrap();

    store
        .fork_session("sess-c", "sess-c-fork", None, None)
        .unwrap();

    let forked_ids: Vec<String> = store
        .list_turns("sess-c-fork")
        .unwrap()
        .into_iter()
        .map(|t| t.turn_id)
        .collect();
    assert_eq!(
        forked_ids.len(),
        2,
        "both the summary and the real turn are copied, got {forked_ids:?}"
    );
    assert_eq!(
        forked_ids
            .iter()
            .filter(|id| turn_is_compaction(id))
            .count(),
        1,
        "the fork carries exactly one recognisable compaction turn"
    );
    // The source must be untouched: reusing its key would have upserted
    // the source's rows instead of inserting the fork's.
    assert_eq!(
        store.list_turns("sess-c").unwrap().len(),
        2,
        "the source keeps both of its rows"
    );
    // And the guard still fires on the fork: undoing both turns would cross it.
    assert!(
        store.plan_undo_turns("sess-c-fork", 2).is_err(),
        "the fork must refuse an undo that crosses the compaction boundary"
    );
    // Undoing only the real turn is still fine.
    assert_eq!(
        store.plan_undo_turns("sess-c-fork", 1).unwrap(),
        vec![2],
        "the compaction guard does not block a same-side undo"
    );
}

/// An index past the last turn is a client error, and it must not leave a
/// half-created session behind.
#[test]
fn an_out_of_range_turn_index_is_refused_without_creating_the_child() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session_with_workspace("sess-src", Some("Src"), Some("wd_range"))
        .unwrap();
    store
        .save_turn(
            "sess-src",
            "turn-a",
            1,
            &[LLMMessage::user("u1")],
            None,
            None,
        )
        .unwrap();

    let err = store
        .fork_session("sess-src", "sess-none", None, Some(7))
        .unwrap_err();
    assert!(
        matches!(
            err,
            ForkError::TurnNotFound {
                turn_index: 7,
                available_turns: 1
            }
        ),
        "expected TurnNotFound naming the real count, got {err:?}"
    );
    assert!(
        store.get_session("sess-none").unwrap().is_none(),
        "a rejected index must not create the child session"
    );
}

/// `turnIndex` counts *visible* turns (v2 `isUserVisibleTurnRecord`), so an
/// invisible turn sitting between two visible ones must not shift the
/// numbering the caller sees.
#[test]
fn an_invisible_turn_does_not_shift_the_turn_index() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session_with_workspace("sess-src", Some("Src"), Some("wd_vis"))
        .unwrap();
    let user_origin = json!({ "kind": "user" });
    // An auto-injected shell command is not a user-visible turn.
    let shell_origin = json!({ "kind": "shell_command", "phase": "output" });
    store
        .save_turn(
            "sess-src",
            "turn-user-1",
            1,
            &[LLMMessage::user("q1")],
            None,
            Some(&user_origin),
        )
        .unwrap();
    store
        .save_turn(
            "sess-src",
            "turn-shell",
            2,
            &[LLMMessage::user("auto")],
            None,
            Some(&shell_origin),
        )
        .unwrap();
    store
        .save_turn(
            "sess-src",
            "turn-user-2",
            3,
            &[LLMMessage::user("q2")],
            None,
            Some(&user_origin),
        )
        .unwrap();

    // Visible turns are index 0 (q1) and index 1 (q2); the shell turn sits
    // between them and must be carried along as part of the prefix.
    store
        .fork_session("sess-src", "sess-cut", None, Some(1))
        .unwrap();

    let turns = store.list_turns("sess-cut").unwrap();
    assert_eq!(
        turns.iter().map(|t| t.turn_number).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "cutting at visible turn 1 keeps the invisible turn before it"
    );
    let history = store.load_session_history("sess-cut").unwrap();
    assert!(
        history.iter().any(|m| m.content == "auto"),
        "the invisible shell turn rides along inside the retained prefix"
    );

    // The invisible turn is not nameable on its own: index 2 is past the end.
    let err = store
        .fork_session("sess-src", "sess-cut2", None, Some(2))
        .unwrap_err();
    assert!(
        matches!(
            err,
            ForkError::TurnNotFound {
                available_turns: 2,
                ..
            }
        ),
        "only two turns are nameable, got {err:?}"
    );
}

/// v2's *first* visibility guard is the message `role`, checked before
/// `origin.kind` (`forkTurnSlice.ts:89-91`): a turn whose opening message is
/// not a user message is never a nameable cut point, whatever its origin
/// says. Without that guard a `kind: "user"` turn opened by the engine
/// itself would be offered as a cut point and silently consume index 0.
#[test]
fn a_non_user_opening_message_is_not_a_visible_turn() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session_with_workspace("sess-role", Some("Src"), Some("wd_role"))
        .unwrap();
    let user_origin = json!({ "kind": "user" });
    // A genuine user turn.
    store
        .save_turn(
            "sess-role",
            "turn-user",
            1,
            &[LLMMessage::user("q1")],
            None,
            Some(&user_origin),
        )
        .unwrap();
    // Claims `kind: "user"` — the most permissive origin there is — but
    // opened by the assistant, so v2 excludes it on the role check alone.
    store
        .save_turn(
            "sess-role",
            "turn-assistant",
            2,
            &[LLMMessage::assistant("auto-reply")],
            None,
            Some(&user_origin),
        )
        .unwrap();
    store
        .save_turn(
            "sess-role",
            "turn-user-2",
            3,
            &[LLMMessage::user("q2")],
            None,
            Some(&user_origin),
        )
        .unwrap();

    // Only the two real user turns are nameable, so index 2 is past the end
    // even though three rows exist.
    let err = store
        .fork_session("sess-role", "sess-role-cut", None, Some(2))
        .unwrap_err();
    assert!(
        matches!(
            err,
            ForkError::TurnNotFound {
                available_turns: 2,
                ..
            }
        ),
        "the assistant-opened turn must not be nameable, got {err:?}"
    );

    // Cutting at visible index 1 keeps rows 1..=3 — the assistant turn rides
    // along inside the retained prefix, exactly as an invisible turn does.
    store
        .fork_session("sess-role", "sess-role-ok", None, Some(1))
        .unwrap();
    let turns = store.list_turns("sess-role-ok").unwrap();
    assert_eq!(
        turns.iter().map(|t| t.turn_number).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "the retained prefix carries the role-excluded turn along"
    );
}

#[test]
fn test_workspaces_crud_trust_and_session_count() {
    let store = SqliteSessionStore::in_memory().unwrap();

    // Test workdir key encoding algorithm
    let key1 = encode_workdir_key("G:\\kimi\\kimi-code\\");
    assert!(key1.starts_with("wd_kimi-code_"));
    assert_eq!(key1.len(), "wd_kimi-code_".len() + 12);

    let key_empty = encode_workdir_key("");
    assert!(key_empty.starts_with("wd_workspace_"));
    assert_eq!(key_empty.len(), "wd_workspace_".len() + 12);

    let key_dot = encode_workdir_key(".");
    assert!(key_dot.starts_with("wd_workspace_"));

    // 1. Create workspace
    let ws1 = store
        .create_workspace("/path/to/my-project", Some("My Project"))
        .unwrap();
    assert_eq!(ws1.id, encode_workdir_key("/path/to/my-project"));
    assert_eq!(ws1.root, "/path/to/my-project");
    assert_eq!(ws1.name, "My Project");
    assert_eq!(ws1.session_count, 0);

    // Validate timestamp formats are valid ISO-8601
    let created_dt =
        chrono::DateTime::parse_from_rfc3339(&ws1.created_at).expect("valid ISO-8601 created_at");
    let last_opened_dt = chrono::DateTime::parse_from_rfc3339(&ws1.last_opened_at)
        .expect("valid ISO-8601 last_opened_at");
    assert!(last_opened_dt >= created_dt);

    // 2. Default trust is true
    assert!(store.is_workspace_trusted(&ws1.id).unwrap());
    // Toggle trust to false
    assert!(store.set_workspace_trusted(&ws1.id, false).unwrap());
    assert!(!store.is_workspace_trusted(&ws1.id).unwrap());
    assert!(store.set_workspace_trusted(&ws1.id, true).unwrap());
    assert!(store.is_workspace_trusted(&ws1.id).unwrap());

    // 3. Create session linked to workspace
    store
        .create_session_with_workspace("s1", Some("Session 1"), Some(&ws1.id))
        .unwrap();
    store
        .create_session_with_workspace("s2", Some("Session 2"), Some(&ws1.id))
        .unwrap();

    // 4. Query workspace again -> session_count == 2
    let ws1_updated = store.get_workspace(&ws1.id).unwrap().unwrap();
    assert_eq!(ws1_updated.session_count, 2);

    // 5. Update workspace with new name on conflict
    let ws1_renamed = store
        .create_workspace("/path/to/my-project", Some("Renamed Project"))
        .unwrap();
    assert_eq!(ws1_renamed.name, "Renamed Project");
    assert_eq!(ws1_renamed.session_count, 2);

    // 6. List workspaces
    let list = store.list_workspaces().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, ws1.id);
    assert_eq!(list[0].name, "Renamed Project");

    // 7. Delete workspace
    let deleted = store.delete_workspace(&ws1.id).unwrap();
    assert!(deleted);
    assert!(store.get_workspace(&ws1.id).unwrap().is_none());
    assert_eq!(store.list_workspaces().unwrap().len(), 0);

    // Deleting non-existent workspace returns false
    assert!(!store.delete_workspace(&ws1.id).unwrap());
}

#[test]
fn test_export_session_full_snapshot() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session("sess-exp", Some("Export Session"))
        .unwrap();
    let msgs = vec![
        LLMMessage::user("Please export this"),
        LLMMessage::assistant("Exporting now"),
    ];
    store
        .save_turn("sess-exp", "turn-1", 1, &msgs, None, None)
        .unwrap();

    // Export existing
    let export = store.export_session("sess-exp").unwrap().unwrap();
    assert_eq!(export.session.session_id, "sess-exp");
    assert_eq!(export.session.title.as_deref(), Some("Export Session"));
    assert_eq!(export.messages.len(), 2);
    assert_eq!(export.messages[0].content, "Please export this");
    assert_eq!(export.messages[1].content, "Exporting now");
    assert_eq!(export.turns_count, 1);

    // Validate exported_at is a concrete, valid recent ISO-8601 timestamp
    let parsed_dt = chrono::DateTime::parse_from_rfc3339(&export.exported_at)
        .expect("exported_at must be valid RFC 3339 timestamp");
    assert!(parsed_dt.timestamp_millis() <= chrono::Utc::now().timestamp_millis());

    // Add second turn and export again
    store
        .save_turn(
            "sess-exp",
            "turn-2",
            2,
            &[LLMMessage::user("turn 2")],
            None,
            None,
        )
        .unwrap();
    let export2 = store.export_session("sess-exp").unwrap().unwrap();
    assert_eq!(export2.turns_count, 2);
    assert_eq!(export2.messages.len(), 3);

    // Export non-existent returns None
    assert!(store.export_session("sess-missing").unwrap().is_none());
}

#[test]
fn test_update_session_title() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session("sess-title-test", Some("Initial Title"))
        .unwrap();

    let s1 = store.get_session("sess-title-test").unwrap().unwrap();
    assert_eq!(s1.title.as_deref(), Some("Initial Title"));

    let updated = store
        .update_session_title("sess-title-test", Some("Updated Title"))
        .unwrap();
    assert!(updated);

    let s2 = store.get_session("sess-title-test").unwrap().unwrap();
    assert_eq!(s2.title.as_deref(), Some("Updated Title"));
    assert!(s2.updated_at >= s1.updated_at);

    let missing = store
        .update_session_title("sess-non-existent", Some("Nope"))
        .unwrap();
    assert!(!missing);
}

#[test]
fn test_gui_store_crud() {
    let store = SqliteSessionStore::in_memory().unwrap();
    assert_eq!(store.gui_length().unwrap(), 0);
    assert_eq!(store.gui_get_item("theme").unwrap(), None);

    store.gui_set_item("theme", "dark").unwrap();
    assert_eq!(
        store.gui_get_item("theme").unwrap().as_deref(),
        Some("dark")
    );
    assert_eq!(store.gui_length().unwrap(), 1);

    store.gui_set_item("sidebar", "expanded").unwrap();
    assert_eq!(store.gui_length().unwrap(), 2);

    // Update existing
    store.gui_set_item("theme", "light").unwrap();
    assert_eq!(
        store.gui_get_item("theme").unwrap().as_deref(),
        Some("light")
    );

    // Remove single item
    assert!(store.gui_remove_item("theme").unwrap());
    assert_eq!(store.gui_get_item("theme").unwrap(), None);
    assert_eq!(store.gui_length().unwrap(), 1);

    // Removing non-existent item returns false
    assert!(!store.gui_remove_item("theme").unwrap());

    // Clear all
    store.gui_clear().unwrap();
    assert_eq!(store.gui_length().unwrap(), 0);
}

#[test]
fn test_search_messages_and_titles() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session("sess-1", Some("Refactoring Core"))
        .unwrap();
    store.create_session("sess-2", Some("Bug Fixes")).unwrap();

    store
        .save_turn(
            "sess-1",
            "t1",
            1,
            &[
                LLMMessage::user("Please optimize the database queries."),
                LLMMessage::assistant("I have added an index to the table."),
            ],
            None,
            None,
        )
        .unwrap();
    store
        .save_turn(
            "sess-2",
            "t2",
            1,
            &[LLMMessage::user("Fix the null pointer crash.")],
            None,
            None,
        )
        .unwrap();

    // 1. Search by content keyword
    let hits = store
        .search_messages("database", None, None, 10, 0)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session_id, "sess-1");
    assert_eq!(hits[0].session_title, "Refactoring Core");
    assert_eq!(hits[0].role, "user");
    assert_eq!(hits[0].step_id, "t1");
    assert_eq!(hits[0].turn, 1);
    assert_eq!(hits[0].score, 1.0);
    assert!(hits[0].snippet.contains("database"));
    chrono::DateTime::parse_from_rfc3339(&hits[0].time).expect("valid ISO-8601 hit time");

    // 2. Search by title keyword
    let hits_title = store
        .search_messages("Refactoring", None, None, 10, 0)
        .unwrap();
    assert_eq!(
        hits_title.len(),
        2,
        "both messages in sess-1 match session title"
    );
    assert_eq!(hits_title[0].session_title, "Refactoring Core");
    assert_eq!(hits_title[1].session_title, "Refactoring Core");

    // 3. Filter by role
    let hits_assistant = store
        .search_messages("index", None, Some("assistant"), 10, 0)
        .unwrap();
    assert_eq!(hits_assistant.len(), 1);
    assert_eq!(hits_assistant[0].role, "assistant");

    let hits_user = store
        .search_messages("index", None, Some("user"), 10, 0)
        .unwrap();
    assert_eq!(hits_user.len(), 0);

    // 4. Filter by session_id
    let hits_sess2 = store
        .search_messages("Fix", Some("sess-2"), None, 10, 0)
        .unwrap();
    assert_eq!(hits_sess2.len(), 1);
    assert_eq!(hits_sess2[0].session_id, "sess-2");

    // 5. Query edge cases
    assert!(
        store
            .search_messages("", None, None, 10, 0)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .search_messages("   \t ", None, None, 10, 0)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .search_messages("nonexistent query", None, None, 10, 0)
            .unwrap()
            .is_empty()
    );

    // 6. Search with limit
    let limited = store.search_messages("the", None, None, 1, 0).unwrap();
    assert_eq!(limited.len(), 1);

    // 6b. Search with offset skips the first page (LIMIT/OFFSET).
    // Note turn t1 holds two messages, so identity must include the
    // snippet, not just (session_id, step_id).
    let page1 = store.search_messages("the", None, None, 1, 0).unwrap();
    let page2 = store.search_messages("the", None, None, 1, 1).unwrap();
    let all = store.search_messages("the", None, None, 10, 0).unwrap();
    assert!(all.len() >= 2, "fixture needs 2+ hits, got {}", all.len());
    assert_eq!(page1.len(), 1);
    assert_eq!(page2.len(), 1);
    assert_ne!(
        page1[0].snippet, page2[0].snippet,
        "offset must advance past the first hit"
    );

    // 7. Snippet extraction with query in middle of long string
    let long_text = "start_prefix ".repeat(15) + "CRITICAL_KEYWORD" + &" end_suffix".repeat(15);
    store
        .save_turn(
            "sess-1",
            "t3",
            2,
            &[LLMMessage::user(&long_text)],
            None,
            None,
        )
        .unwrap();
    let snippet_hits = store
        .search_messages("CRITICAL_KEYWORD", None, None, 1, 0)
        .unwrap();
    assert_eq!(snippet_hits.len(), 1);
    assert!(snippet_hits[0].snippet.starts_with("..."));
    assert!(snippet_hits[0].snippet.ends_with("..."));
    assert!(snippet_hits[0].snippet.contains("CRITICAL_KEYWORD"));

    // 8. Total count
    assert_eq!(store.count_messages().unwrap(), 4);
}

#[test]
fn test_file_history_crud_and_diff() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session("sess-fh", Some("File History Test"))
        .unwrap();

    // Direct test of compute_line_diff logic
    assert_eq!(
        compute_line_diff(None, Some("line1\nline2\n")),
        ("added".to_string(), 2, 0)
    );
    assert_eq!(
        compute_line_diff(Some("line1\nline2\n"), None),
        ("deleted".to_string(), 0, 2)
    );
    assert_eq!(
        compute_line_diff(Some("line1\nline2\n"), Some("line1\nline2\n")),
        ("modified".to_string(), 0, 0)
    );
    assert_eq!(
        compute_line_diff(Some("line1\nline2\n"), Some("line1\nline3\n")),
        ("modified".to_string(), 1, 1)
    );
    assert_eq!(
        compute_line_diff(None, None),
        ("modified".to_string(), 0, 0)
    );

    // Turn 1: Add a new file
    store
        .record_file_change(
            "sess-fh",
            1,
            "src/main.rs",
            None,
            Some("fn main() {\n    println!(\"hello\");\n}\n"),
        )
        .unwrap();

    // Turn 2: Modify the file
    store
        .record_file_change(
            "sess-fh",
            2,
            "src/main.rs",
            Some("fn main() {\n    println!(\"hello\");\n}\n"),
            Some("fn main() {\n    println!(\"world\");\n    let x = 10;\n}\n"),
        )
        .unwrap();

    // Turn 3: Delete the file
    store
        .record_file_change(
            "sess-fh",
            3,
            "src/main.rs",
            Some("fn main() {\n    println!(\"world\");\n    let x = 10;\n}\n"),
            None,
        )
        .unwrap();

    // Check Turn 1 changes
    let (ch1, rec1) = store.get_file_history_changes("sess-fh", Some(1)).unwrap();
    assert!(rec1);
    assert_eq!(ch1.len(), 1);
    assert_eq!(ch1[0].path, "src/main.rs");
    assert_eq!(ch1[0].status, "added");
    assert_eq!(ch1[0].additions, 3);
    assert_eq!(ch1[0].deletions, 0);

    // Check Turn 2 changes
    let (ch2, rec2) = store.get_file_history_changes("sess-fh", Some(2)).unwrap();
    assert!(rec2);
    assert_eq!(ch2.len(), 1);
    assert_eq!(ch2[0].path, "src/main.rs");
    assert_eq!(ch2[0].status, "modified");

    // Check Turn 3 (deletion)
    let (ch3, rec3) = store.get_file_history_changes("sess-fh", Some(3)).unwrap();
    assert!(rec3);
    assert_eq!(ch3.len(), 1);
    assert_eq!(ch3[0].path, "src/main.rs");
    assert_eq!(ch3[0].status, "deleted");
    assert_eq!(ch3[0].additions, 0);
    assert_eq!(ch3[0].deletions, 4);

    // Check turn_id = None returns changes across all turns in chronological order
    let (all_changes, all_rec) = store.get_file_history_changes("sess-fh", None).unwrap();
    assert!(all_rec);
    assert_eq!(all_changes.len(), 3);
    assert_eq!(all_changes[0].status, "added");
    assert_eq!(all_changes[1].status, "modified");
    assert_eq!(all_changes[2].status, "deleted");

    // Verify retention prune
    let pruned = store.prune_file_history("sess-fh", 2).unwrap();
    assert_eq!(pruned, 1);
    let (retained_changes, _) = store.get_file_history_changes("sess-fh", None).unwrap();
    assert_eq!(retained_changes.len(), 2);
    assert_eq!(retained_changes[0].status, "modified");
    assert_eq!(retained_changes[1].status, "deleted");

    // Non-existent turn returns recorded = false
    let (ch_none, rec_none) = store.get_file_history_changes("sess-fh", Some(99)).unwrap();
    assert!(!rec_none);
    assert!(ch_none.is_empty());

    // Check content at Turn 2 phase start & end
    let start_content = store
        .get_file_history_content("sess-fh", 2, "src/main.rs", "start")
        .unwrap()
        .unwrap();
    assert!(
        start_content
            .content
            .unwrap()
            .contains("println!(\"hello\")")
    );

    let end_content = store
        .get_file_history_content("sess-fh", 2, "src/main.rs", "end")
        .unwrap()
        .unwrap();
    assert!(end_content.content.unwrap().contains("println!(\"world\")"));

    // Non-existent path content
    assert!(
        store
            .get_file_history_content("sess-fh", 2, "src/missing.rs", "end")
            .unwrap()
            .is_none()
    );

    // Test revert_turn_file_changes restores to content_before
    let temp_ws = tempfile::tempdir().unwrap();
    let file_path = temp_ws.path().join("src/main.rs");
    std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    std::fs::write(&file_path, "modified code").unwrap();

    store.create_session("sess-revert", None).unwrap();
    store
        .record_file_change(
            "sess-revert",
            1,
            "src/main.rs",
            Some("original code"),
            Some("modified code"),
        )
        .unwrap();

    let reverted = store
        .revert_turn_file_changes("sess-revert", 1, temp_ws.path())
        .unwrap();
    assert_eq!(reverted, vec!["src/main.rs"]);
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "original code"
    );
}

/// `plan_undo_turns` reports the newest turn numbers first and refuses a
/// selection that would cross the compaction boundary — before anything
/// is deleted, so callers can bail out ahead of reverting files.
#[test]
fn test_plan_undo_turns_newest_first_and_boundary_refusal() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-plan", None).unwrap();
    let msgs = vec![crate::turn_loop::types::LLMMessage::user("hi")];
    store
        .save_turn("sess-plan", "t1", 1, &msgs, None, None)
        .unwrap();
    store
        .save_turn("sess-plan", "t2", 2, &msgs, None, None)
        .unwrap();

    assert_eq!(store.plan_undo_turns("sess-plan", 1).unwrap(), vec![2]);
    assert_eq!(store.plan_undo_turns("sess-plan", 2).unwrap(), vec![2, 1]);
    let err = store.plan_undo_turns("sess-plan", 5).unwrap_err();
    assert!(err.contains("only 2 of 5"), "unexpected error: {err}");

    store
        .save_turn("sess-plan", COMPACT_TURN_ID, 3, &msgs, None, None)
        .unwrap();
    let err = store.plan_undo_turns("sess-plan", 1).unwrap_err();
    assert!(err.contains("undo refused"), "unexpected error: {err}");
    // The refusal left the history intact.
    assert_eq!(store.load_session_history("sess-plan").unwrap().len(), 3);
}

#[test]
fn test_next_turn_number() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-seq", None).unwrap();

    // Fresh session starts at turn 1
    assert_eq!(store.next_turn_number("sess-seq").unwrap(), 1);

    // After turn 1, next is 2
    store
        .save_turn(
            "sess-seq",
            "t1",
            1,
            &[LLMMessage::user("first")],
            None,
            None,
        )
        .unwrap();
    assert_eq!(store.next_turn_number("sess-seq").unwrap(), 2);

    // After jump to turn 5, next is 6
    store
        .save_turn(
            "sess-seq",
            "t5",
            5,
            &[LLMMessage::user("fifth")],
            None,
            None,
        )
        .unwrap();
    assert_eq!(store.next_turn_number("sess-seq").unwrap(), 6);
}

/// v2 #3906: the symptom upstream fixed was "a steered follow-up mints a
/// second context id, so the host prompt becomes un-undoable". The fork's
/// undo is turn-scoped (`DELETE FROM messages WHERE turn_id = ?`), and a
/// steer rides the active turn's id, so the host prompt and its steered
/// follow-ups are already one undo unit — the divergence never existed
/// here. The `prompt_id` this branch now carries is what lets the
/// projection *show* that grouping instead of rendering the steer as its
/// own turn. This pins the invariant so a future move to message-scoped
/// undo does not silently break it.
#[test]
fn a_steer_and_its_host_prompt_are_one_undo_unit() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-steer-undo", None).unwrap();

    // Host turn: the prompt, then an assistant reply.
    store
        .save_turn(
            "sess-steer-undo",
            "t1",
            1,
            &[
                LLMMessage::user("original"),
                LLMMessage::assistant("working on it"),
            ],
            None,
            None,
        )
        .unwrap();
    // The steer joins the same turn (as `admit_locked` does), carrying its
    // prompt id.
    let mut steer = LLMMessage::user("actually use python");
    steer.prompt_id = Some("prompt-1".to_string());
    store
        .save_turn("sess-steer-undo", "t1", 1, &[steer], None, None)
        .unwrap();

    let history = store.load_session_history("sess-steer-undo").unwrap();
    assert_eq!(history.len(), 3, "prompt + reply + steer in one turn");
    // The id survives the round-trip — that is what the projection reads.
    assert_eq!(history[2].prompt_id.as_deref(), Some("prompt-1"));

    // One undo removes the whole turn, steer included.
    let undone = store.undo_turns("sess-steer-undo", 1).unwrap();
    assert_eq!(undone, 1);
    assert!(
        store
            .load_session_history("sess-steer-undo")
            .unwrap()
            .is_empty(),
        "the steer must not outlive its host prompt"
    );
}

#[test]
fn test_undo_turns() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-undo", None).unwrap();

    store
        .save_turn("sess-undo", "t1", 1, &[LLMMessage::user("1")], None, None)
        .unwrap();
    store
        .save_turn("sess-undo", "t2", 2, &[LLMMessage::user("2")], None, None)
        .unwrap();
    store
        .save_turn("sess-undo", "t3", 3, &[LLMMessage::user("3")], None, None)
        .unwrap();

    assert_eq!(store.load_session_history("sess-undo").unwrap().len(), 3);

    // Undo 1 turn -> turn 3 deleted
    let undone = store.undo_turns("sess-undo", 1).unwrap();
    assert_eq!(undone, 1);
    let history = store.load_session_history("sess-undo").unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].content, "1");
    assert_eq!(history[1].content, "2");

    // Undo 10 turns when only 2 remain -> an error, not a short undo
    let err = store.undo_turns("sess-undo", 10).unwrap_err();
    assert!(err.contains("only 2 of 10"), "unexpected error: {err}");
    let undone_all = store.undo_turns("sess-undo", 2).unwrap();
    assert_eq!(undone_all, 2);
    assert!(store.load_session_history("sess-undo").unwrap().is_empty());

    // Undo on an empty session is unavailable
    let err = store.undo_turns("sess-undo", 1).unwrap_err();
    assert!(err.contains("nothing to undo"), "unexpected error: {err}");
}

#[test]
fn test_compact_session() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-compact", None).unwrap();

    // 1. Session with <= 2 messages returns 0
    store
        .save_turn(
            "sess-compact",
            "t1",
            1,
            &[LLMMessage::user("hi")],
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        store.compact_session("sess-compact", None).unwrap(),
        CompactionReport::skipped()
    );

    // 2. Add many turns
    for i in 2..=15 {
        store
            .save_turn(
                "sess-compact",
                &format!("t{i}"),
                i,
                &[
                    LLMMessage::user(format!("User message {i}")),
                    LLMMessage::assistant(format!("Assistant message {i}")),
                ],
                None,
                None,
            )
            .unwrap();
    }
    // A trailing user message is never a safe split point
    // (`can_split_after` refuses user messages), so the fold stops one
    // short of the end and the kept tail is observable.
    store
        .save_turn(
            "sess-compact",
            "t-tail",
            16,
            &[LLMMessage::user("Trailing question".to_string())],
            None,
            None,
        )
        .unwrap();
    let pre_history = store.load_session_history("sess-compact").unwrap();
    let pre_len = pre_history.len();
    assert!(pre_len >= 30);

    let report = store
        .compact_session_with_summary(
            "sess-compact",
            Some("keep decisions"),
            Some("The user requested numbered messages.".into()),
        )
        .unwrap();
    let removed = report.removed;
    assert!(removed > 0);
    assert_eq!(report.messages_before, pre_len);
    assert_eq!(
        report.compacted_count as usize,
        pre_len - 1,
        "the trailing user message stays outside the fold"
    );
    // The v2 shape keeps the compacted range's user input verbatim, so
    // tokens_after can hold steady when everything fits the head/tail
    // budgets; it must never grow past `before` plus the summary block.
    assert!(report.tokens_after <= report.tokens_before + 512);

    let post_history = store.load_session_history("sess-compact").unwrap();
    assert_eq!(post_history.len(), pre_len - removed);
    assert!(post_history.len() < pre_len);
    // `compacted_count` is the first kept index: everything beyond the
    // summary block is `pre_history[count..]`, untouched.
    assert!(
        post_history.ends_with(&pre_history[report.compacted_count as usize..]),
        "the kept tail starts exactly at compacted_count"
    );
    assert_eq!(
        report.kept_user_message_count,
        post_history
            .iter()
            .filter(|message| message.role == "user")
            .count(),
        "the report counts the user messages that survived"
    );

    // A normal turn recorded after the compaction is a user anchor and
    // undoes cleanly; the trailing-pre-compaction row above has no
    // messages of its own (the fold re-saved them under the compact turn),
    // so it is not an anchor.
    store
        .save_turn(
            "sess-compact",
            "t-post",
            17,
            &[
                LLMMessage::user("Post-compaction question"),
                LLMMessage::assistant("ok"),
            ],
            None,
            None,
        )
        .unwrap();
    let undone = store.undo_turns("sess-compact", 1).unwrap();
    assert_eq!(undone, 1);

    // …but an undo that would swallow the compaction summary itself is
    // refused — the pre-compaction rows no longer exist, so deleting the
    // boundary turn would corrupt the projection (event_store
    // `UndoCompactionBoundary`, event_store/mod.rs:43-47).
    let err = store.undo_turns("sess-compact", 100).unwrap_err();
    assert!(
        err.contains("undo refused"),
        "expected boundary refusal, got: {err}"
    );
}

#[test]
fn test_compact_session_with_summary_writes_the_summary() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-summary", None).unwrap();
    for i in 1..=15 {
        store
            .save_turn(
                "sess-summary",
                &format!("t{i}"),
                i,
                &[
                    LLMMessage::user(format!("User message {i}")),
                    LLMMessage::assistant(format!("Assistant message {i}")),
                ],
                None,
                None,
            )
            .unwrap();
    }

    let original = store.load_session_history("sess-summary").unwrap();
    for summary in [None, Some(String::new()), Some(" \n\t".to_string())] {
        assert!(
            store
                .compact_session_with_summary("sess-summary", None, summary)
                .is_err()
        );
        assert_eq!(
            serde_json::to_value(store.load_session_history("sess-summary").unwrap()).unwrap(),
            serde_json::to_value(&original).unwrap(),
        );
    }

    let report = store
        .compact_session_with_summary(
            "sess-summary",
            None,
            Some("The user asked for fifteen numbered messages.".to_string()),
        )
        .unwrap();
    assert!(report.removed > 0);

    let history = store.load_session_history("sess-summary").unwrap();
    let summary_message = history
        .iter()
        .find(|m| {
            m.content
                .contains(crate::compaction::COMPACTION_SUMMARY_PREFIX)
        })
        .expect("the caller's summary must be stored under the compaction prefix");
    assert!(
        summary_message
            .content
            .ends_with("The user asked for fifteen numbered messages."),
        "the caller's summary replaces the omitted prefix"
    );
    assert!(
        !summary_message
            .content
            .contains("Earlier conversation compacted:"),
        "the placeholder must not survive a supplied summary"
    );

    // The split point is recomputed from the stored history, so a summary
    // handed in for a history that cannot be compacted changes nothing.
    let short = SqliteSessionStore::in_memory().unwrap();
    short.create_session("sess-short", None).unwrap();
    short
        .save_turn("sess-short", "t1", 1, &[LLMMessage::user("hi")], None, None)
        .unwrap();
    assert_eq!(
        short
            .compact_session_with_summary("sess-short", None, Some("unused".to_string()))
            .unwrap(),
        CompactionReport::skipped()
    );
}

#[test]
fn test_delete_session_cascades() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-del", Some("To Delete")).unwrap();

    store
        .save_turn(
            "sess-del",
            "t1",
            1,
            &[LLMMessage::user("hello")],
            None,
            None,
        )
        .unwrap();

    store
        .save_checkpoint(
            "sess-del",
            "chk-del",
            "Checkpoint",
            &serde_json::json!({ "ok": true }),
        )
        .unwrap();

    store
        .record_file_change("sess-del", 1, "file.rs", None, Some("code"))
        .unwrap();

    // Delete session
    let deleted = store.delete_session("sess-del").unwrap();
    assert!(deleted);

    // Session is gone
    assert!(store.get_session("sess-del").unwrap().is_none());
    assert!(store.load_session_history("sess-del").unwrap().is_empty());

    // Cascading deletion verified directly in tables
    {
        let conn = store.conn.lock();
        let turn_count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM turns WHERE session_id = 'sess-del'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(turn_count, 0, "turns must cascade delete");

        let msg_count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = 'sess-del'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(msg_count, 0, "messages must cascade delete");

        let chk_count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM checkpoints WHERE session_id = 'sess-del'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(chk_count, 0, "checkpoints must cascade delete");

        let fh_count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM session_file_history WHERE session_id = 'sess-del'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fh_count, 0, "file history must cascade delete");
    }

    // Deleting non-existent session returns false
    assert!(!store.delete_session("sess-del").unwrap());
}

#[test]
fn test_undo_turns_transactional_integrity() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session("sess-undo", Some("Undo Test"))
        .unwrap();

    // Save Turn 1
    let msgs_t1 = vec![
        LLMMessage::user("prompt 1"),
        LLMMessage::assistant("reply 1"),
    ];
    store
        .save_turn("sess-undo", "t1", 1, &msgs_t1, None, None)
        .unwrap();

    // Save Turn 2
    let msgs_t2 = vec![
        LLMMessage::user("prompt 2"),
        LLMMessage::assistant("reply 2"),
    ];
    store
        .save_turn("sess-undo", "t2", 2, &msgs_t2, None, None)
        .unwrap();

    // Save Turn 3
    let msgs_t3 = vec![
        LLMMessage::user("prompt 3"),
        LLMMessage::assistant("reply 3"),
    ];
    store
        .save_turn("sess-undo", "t3", 3, &msgs_t3, None, None)
        .unwrap();

    let history_before = store.load_session_history("sess-undo").unwrap();
    assert_eq!(history_before.len(), 6);

    // Undo 1 turn (should remove t3)
    let undone = store.undo_turns("sess-undo", 1).unwrap();
    assert_eq!(undone, 1);
    let history_after_1 = store.load_session_history("sess-undo").unwrap();
    assert_eq!(history_after_1.len(), 4);
    assert_eq!(history_after_1[0].content, "prompt 1");
    assert_eq!(history_after_1[1].content, "reply 1");
    assert_eq!(history_after_1[2].content, "prompt 2");
    assert_eq!(history_after_1[3].content, "reply 2");

    // Next turn number must be 3
    assert_eq!(store.next_turn_number("sess-undo").unwrap(), 3);

    // Asking for more anchors than exist is an error, not a silent short
    // undo: v2 reports SESSION_UNDO_UNAVAILABLE.
    let err = store.undo_turns("sess-undo", 10).unwrap_err();
    assert!(err.contains("only 2 of 10"), "unexpected error: {err}");

    // Undo the remaining 2 anchors.
    let undone_all = store.undo_turns("sess-undo", 2).unwrap();
    assert_eq!(undone_all, 2);
    let history_empty = store.load_session_history("sess-undo").unwrap();
    assert_eq!(history_empty.len(), 0);
    assert_eq!(store.next_turn_number("sess-undo").unwrap(), 1);
}

#[test]
fn test_sqlite_patch_and_diff_state() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .put_state(
            "metadata",
            "sess-patch-1",
            &json!({ "theme": "dark", "fontSize": 14 }),
        )
        .unwrap();

    let diff = store
        .diff_state(
            "metadata",
            "sess-patch-1",
            &json!({ "theme": "light", "fontSize": 16 }),
        )
        .unwrap();
    assert!(!diff.is_empty());

    let (patched, inverse) = store
        .patch_state("metadata", "sess-patch-1", &diff)
        .unwrap();
    assert_eq!(patched["theme"], "light");
    assert_eq!(patched["fontSize"], 16);

    // Revert using inverse patch
    let (reverted, _) = store
        .patch_state("metadata", "sess-patch-1", &inverse)
        .unwrap();
    assert_eq!(reverted["theme"], "dark");
    assert_eq!(reverted["fontSize"], 14);
}

/// The v3 history fold's source for interaction entities: the six
/// interaction lifecycle types, oldest first, and nothing else.
#[test]
fn test_interaction_wire_events_selects_the_lifecycle_types() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session("sess-interaction", Some("Interaction"))
        .unwrap();
    let append = |event_type: &str| {
        store
            .append_wire_event(&RawWireEvent {
                id: format!("evt-{event_type}"),
                session_id: "sess-interaction".into(),
                event_type: event_type.into(),
                payload: json!({ "approval_id": "a1", "question_id": "q1" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 1_000,
            })
            .unwrap();
    };
    append("event.approval.requested");
    append("event.message.created");
    append("event.approval.resolved");
    append("event.question.answered");

    let events = store.interaction_wire_events("sess-interaction").unwrap();
    let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(
        types,
        [
            "event.approval.requested",
            "event.approval.resolved",
            "event.question.answered",
        ],
        "only the interaction lifecycle, in journal order"
    );
}

/// The cold baseline's source for prompt entities (`transcript::
/// cold_prompts`): the four prompt lifecycle types, oldest first — and
/// `turn.steer` stays out, its user frame rides the rebuilt history.
#[test]
fn test_prompt_wire_events_selects_the_lifecycle_types() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session("sess-prompt-lifecycle", Some("Prompts"))
        .unwrap();
    let append = |event_type: &str| {
        store
            .append_wire_event(&RawWireEvent {
                id: format!("evt-{event_type}"),
                session_id: "sess-prompt-lifecycle".into(),
                event_type: event_type.into(),
                payload: json!({ "type": event_type, "promptId": "prompt-1" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 1_000,
            })
            .unwrap();
    };
    append("prompt.submitted");
    append("event.message.created");
    append("turn.steer");
    append("prompt.steered");
    append("prompt.completed");

    let events = store.prompt_wire_events("sess-prompt-lifecycle").unwrap();
    let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(
        types,
        ["prompt.submitted", "prompt.steered", "prompt.completed",],
        "only the prompt lifecycle, in journal order"
    );
}

/// ROADMAP §6.1 item 4's P2 decision: session-scoped state carries its
/// owner in a `session_id` column, so a session delete cascades to it.
/// Rows written before the column existed (key-encoded
/// `agent_config` / `metadata`) are cascaded by their key pattern, and
/// the column itself arrives by an idempotent migration on an existing
/// database.
#[test]
fn test_session_scoped_state_survives_migration_and_cascades_on_delete() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state-migration.db");
    {
        let store = SqliteSessionStore::open(&path).unwrap();
        store.create_session("sess-a", Some("a")).unwrap();
        store.create_session("sess-b", Some("b")).unwrap();
        // Legacy shape: no session_id column value.
        store
            .put_state("agent_config", "sess-a", &json!({ "model": "k2" }))
            .unwrap();
        store
            .put_state("metadata", "sess-a", &json!({ "permission_mode": "auto" }))
            .unwrap();
        // A workspace-scoped row (no session of its own) must survive.
        store
            .put_state("plan", "workspace", &json!({ "active": true }))
            .unwrap();
    }
    // Reopen: the migration adds the column without touching the rows.
    {
        let store = SqliteSessionStore::open(&path).unwrap();
        assert_eq!(
            store.get_state("agent_config", "sess-a").unwrap(),
            Some(json!({ "model": "k2" })),
            "legacy rows read back unchanged after the migration"
        );
        assert!(
            store.session_state_entries("sess-a").unwrap().is_empty(),
            "legacy rows carry no column owner yet"
        );

        // New-style writes name their owner.
        store
            .put_session_state(
                "agent_config",
                "sess-b",
                "sess-b",
                &json!({ "model": "k3" }),
            )
            .unwrap();
        let owned = store.session_state_entries("sess-b").unwrap();
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].0, "agent_config");
        assert_eq!(owned[0].2, json!({ "model": "k3" }));

        // The delete cascades both shapes and leaves the workspace row.
        assert!(store.delete_session("sess-a").unwrap());
        assert!(store.get_state("agent_config", "sess-a").unwrap().is_none());
        assert!(store.get_state("metadata", "sess-a").unwrap().is_none());
        assert!(store.delete_session("sess-b").unwrap());
        assert!(store.session_state_entries("sess-b").unwrap().is_empty());
        assert_eq!(
            store.get_state("plan", "workspace").unwrap(),
            Some(json!({ "active": true })),
            "workspace-scoped state is not a session's to delete"
        );
    }
}

#[test]
fn test_sqlite_event_store_append_query_fold_undo() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session("sess-evt-test", Some("Event Test"))
        .unwrap();

    // 1. Append user message event
    let seq1 = store
        .append_wire_event(&RawWireEvent {
            id: "evt-1".into(),
            session_id: "sess-evt-test".into(),
            event_type: "message.user".into(),
            payload: json!({ "content": "Calculate 2+2" }),
            is_checkpoint: false,
            is_compaction: false,
            created_at: 1000,
        })
        .unwrap();
    assert_eq!(seq1, 1);

    // 2. Append assistant tool call event
    let seq2 = store
        .append_wire_event(&RawWireEvent {
            id: "evt-2".into(),
            session_id: "sess-evt-test".into(),
            event_type: "message.assistant".into(),
            payload: json!({
                "content": "Let me calculate that.",
                "tool_calls": [{
                    "id": "call_calc_1",
                    "type": "function",
                    "function": { "name": "calc", "arguments": "{\"expr\": \"2+2\"}" }
                }]
            }),
            is_checkpoint: true,
            is_compaction: false,
            created_at: 2000,
        })
        .unwrap();
    assert_eq!(seq2, 2);

    // 3. Append tool result event
    let seq3 = store
        .append_wire_event(&RawWireEvent {
            id: "evt-3".into(),
            session_id: "sess-evt-test".into(),
            event_type: "tool.result".into(),
            payload: json!({
                "tool_call_id": "call_calc_1",
                "output": "4"
            }),
            is_checkpoint: false,
            is_compaction: false,
            created_at: 3000,
        })
        .unwrap();
    assert_eq!(seq3, 3);

    // Verify count and latest sequence
    assert_eq!(store.count_wire_events("sess-evt-test").unwrap(), 3);
    assert_eq!(store.latest_wire_event_seq("sess-evt-test").unwrap(), 3);

    // Verify pagination
    let page1 = store.get_wire_events("sess-evt-test", 0, 2).unwrap();
    assert_eq!(page1.len(), 2);
    assert_eq!(page1[0].seq, 1);
    assert_eq!(page1[1].seq, 2);

    let page2 = store.get_wire_events("sess-evt-test", 2, 10).unwrap();
    assert_eq!(page2.len(), 1);
    assert_eq!(page2[0].seq, 3);
    assert_eq!(page2[0].event_type, "tool.result");

    // Verify projection folding
    let msgs = store.fold_projection("sess-evt-test").unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].role, crate::native::event_store::MessageRole::User);
    assert_eq!(msgs[0].content, "Calculate 2+2");
    assert_eq!(
        msgs[1].role,
        crate::native::event_store::MessageRole::Assistant
    );
    assert_eq!(msgs[1].content, "Let me calculate that.");
    assert!(msgs[1].tool_calls.is_some());
    assert_eq!(msgs[2].role, crate::native::event_store::MessageRole::Tool);
    assert_eq!(msgs[2].content, "4");
    assert_eq!(msgs[2].tool_call_id.as_deref(), Some("call_calc_1"));

    // Verify undo to last checkpoint (evt-2 was is_checkpoint = true)
    let undone_count = store.undo_to_last_checkpoint("sess-evt-test").unwrap();
    assert_eq!(undone_count, 2); // Deleted seq 2 and 3
    assert_eq!(store.count_wire_events("sess-evt-test").unwrap(), 1);

    let msgs_after_undo = store.fold_projection("sess-evt-test").unwrap();
    assert_eq!(msgs_after_undo.len(), 1);
    assert_eq!(
        msgs_after_undo[0].role,
        crate::native::event_store::MessageRole::User
    );

    // Verify compaction boundary
    store
        .checkpoint_compress("sess-evt-test", "Historical math summary")
        .unwrap();
    let msgs_compacted = store.fold_projection("sess-evt-test").unwrap();
    assert_eq!(msgs_compacted.len(), 1);
    assert_eq!(
        msgs_compacted[0].role,
        crate::native::event_store::MessageRole::System
    );
    assert_eq!(msgs_compacted[0].content, "Historical math summary");

    // Undoing across compaction boundary must return UndoCompactionBoundary error
    let undo_err = store.undo_to_last_checkpoint("sess-evt-test").unwrap_err();
    match undo_err {
        EventStoreError::UndoCompactionBoundary => {}
        other => panic!("Expected UndoCompactionBoundary, got {:?}", other),
    }
}

/// The outcome mirror persists, and an explicit `None` clears it — a turn
/// that ended must not leave the previous outcome stuck on the session.
#[test]
fn last_turn_reason_round_trips_and_clears() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-outcome", None).unwrap();

    assert_eq!(store.last_turn_reason("sess-outcome"), None);
    store
        .set_last_turn_reason("sess-outcome", Some("completed"))
        .unwrap();
    assert_eq!(
        store.last_turn_reason("sess-outcome").as_deref(),
        Some("completed")
    );
    store
        .set_last_turn_reason("sess-outcome", Some("failed"))
        .unwrap();
    assert_eq!(
        store.last_turn_reason("sess-outcome").as_deref(),
        Some("failed")
    );
    store.set_last_turn_reason("sess-outcome", None).unwrap();
    assert_eq!(store.last_turn_reason("sess-outcome"), None);

    // An unknown session is not an error: the wire asks unconditionally.
    store
        .set_last_turn_reason("sess-absent", Some("completed"))
        .unwrap();
    assert_eq!(store.last_turn_reason("sess-absent"), None);
}

/// v2 passes `touchUpdatedAt: false` for this write, and the reason matters:
/// the outcome mirrors a turn that already ended elsewhere, so treating it
/// as activity would reorder the session list on every turn end.
#[test]
fn last_turn_reason_does_not_touch_updated_at() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-stamp", None).unwrap();
    let pinned: i64 = 1_700_000_000_000;
    {
        let conn = store.conn.lock();
        conn.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE session_id = ?1",
            params!["sess-stamp", pinned],
        )
        .unwrap();
    }

    store
        .set_last_turn_reason("sess-stamp", Some("cancelled"))
        .unwrap();

    let after: i64 = {
        let conn = store.conn.lock();
        conn.query_row(
            "SELECT updated_at FROM sessions WHERE session_id = 'sess-stamp'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(after, pinned, "the mirror must not read as activity");
    assert_eq!(
        store.last_turn_reason("sess-stamp").as_deref(),
        Some("cancelled")
    );
}

#[test]
fn one_directory_is_one_workspace_however_it_is_spelled() {
    let store = SqliteSessionStore::in_memory().unwrap();
    let dir = std::env::temp_dir().join(format!("kimi-ws-alias-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let real = dir.to_string_lossy().replace('\\', "/");

    let first = store.create_workspace(&real, None).unwrap();
    let with_slash = store.create_workspace(&format!("{real}/"), None).unwrap();
    assert_eq!(
        first.id, with_slash.id,
        "a trailing slash is not a new workspace"
    );

    if cfg!(windows) {
        let upper = real.to_uppercase();
        if upper != real {
            let cased = store.create_workspace(&upper, None).unwrap();
            assert_eq!(first.id, cased.id, "path case is not a new workspace");
        }
    }

    let count: i64 = {
        let conn = store.conn.lock();
        conn.query_row("SELECT COUNT(*) FROM workspaces", [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(count, 1, "one directory, one row");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_missing_root_still_gets_a_stable_key() {
    let missing = std::env::temp_dir().join("kimi-ws-absent/deeper/still-gone");
    let as_posix = missing.to_string_lossy().replace('\\', "/");
    let with_slash = format!("{as_posix}/");
    assert_eq!(
        workspace_root_key(&as_posix),
        workspace_root_key(&with_slash)
    );
    assert!(!workspace_root_key(&as_posix).is_empty());
    assert_ne!(
        workspace_root_key(&as_posix),
        workspace_root_key(&as_posix.replace("still-gone", "other"))
    );
}

/// v2's own examples, pinned verbatim: the key is lexical, and the case fold
/// is decided by the path's *shape*, not by the host this runs on. An earlier
/// version of `workspace_root_key` canonicalized instead, which reached the
/// filesystem and answered differently for symlinked roots.
#[test]
fn workspace_root_key_is_lexical_and_folds_by_path_shape() {
    // v2 `workspaceService.test.ts:752-754`.
    assert_eq!(
        workspace_root_key("C:\\Users\\Foo\\Proj"),
        "c:/users/foo/proj"
    );
    assert_eq!(
        workspace_root_key("c:/Users/Foo/Proj/"),
        "c:/users/foo/proj"
    );
    assert_eq!(
        workspace_root_key("C:\\Users\\Foo\\Proj"),
        workspace_root_key("c:/users/foo/proj")
    );

    // A POSIX root keeps its case *wherever this runs*: on a
    // case-sensitive filesystem two spellings are two directories, so
    // folding them would merge workspaces the user keeps apart.
    assert_eq!(workspace_root_key("/Home/Foo/Proj"), "/Home/Foo/Proj");
    // A UNC root is Windows-shaped and folds.
    assert_eq!(
        workspace_root_key("\\\\Server\\Share\\Proj"),
        "//server/share/proj"
    );

    // Shape decides, not the OS: on a case-insensitive host a POSIX-shaped
    // root still keeps its case, and on a case-sensitive host a
    // Windows-shaped one still folds.
    assert_ne!(
        workspace_root_key("/tmp/Case"),
        workspace_root_key("/tmp/case")
    );
    assert_eq!(
        workspace_root_key("D:/Case/"),
        workspace_root_key("d:/case")
    );
}

/// Derived turns (goal/cron/task) and in-turn steers are not undo anchors;
/// cutting at the Nth real user turn removes every turn after it too.
#[test]
fn test_undo_selects_user_anchors_not_derived_turns() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-anchor", None).unwrap();
    let msgs = vec![crate::turn_loop::types::LLMMessage::user("hi")];
    let user_origin = serde_json::json!({ "kind": "user" });
    let goal_origin = serde_json::json!({ "kind": "goal" });
    let cron_origin = serde_json::json!({ "kind": "cron" });
    store
        .save_turn("sess-anchor", "t1", 1, &msgs, None, Some(&user_origin))
        .unwrap();
    store
        .save_turn("sess-anchor", "t2", 2, &msgs, None, Some(&goal_origin))
        .unwrap();
    store
        .save_turn("sess-anchor", "t3", 3, &msgs, None, Some(&user_origin))
        .unwrap();
    store
        .save_turn("sess-anchor", "t4", 4, &msgs, None, Some(&cron_origin))
        .unwrap();

    // The newest anchor is t3; the cut also removes the newer derived t4.
    assert_eq!(store.plan_undo_turns("sess-anchor", 1).unwrap(), vec![4, 3]);
    assert_eq!(store.undo_turns("sess-anchor", 1).unwrap(), 2);
    assert_eq!(store.load_session_history("sess-anchor").unwrap().len(), 2);

    // Only t1 remains as an anchor; t2 is derived and does not count.
    let err = store.undo_turns("sess-anchor", 2).unwrap_err();
    assert!(
        err.contains("only 1 of 2"),
        "a derived turn must not count as an anchor: {err}"
    );
}

/// A file over the history byte cap has no stored pre-image. Treating that
/// as "the file was added" deleted the user's file on undo; the revert must
/// leave it untouched and not report it as reverted.
#[test]
fn test_revert_skips_oversize_files_instead_of_deleting_them() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-revert-big", None).unwrap();
    let temp_ws = tempfile::tempdir().unwrap();
    let file_path = temp_ws.path().join("big.bin");
    std::fs::write(&file_path, "current contents").unwrap();
    let before = "x".repeat(FILE_HISTORY_MAX_CONTENT_BYTES + 1);
    store
        .record_file_change(
            "sess-revert-big",
            1,
            "big.bin",
            Some(&before),
            Some("current contents"),
        )
        .unwrap();

    let reverted = store
        .revert_turn_file_changes("sess-revert-big", 1, temp_ws.path())
        .unwrap();
    assert!(reverted.is_empty(), "oversize changes are not revertible");
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "current contents",
        "the file must not be deleted when its pre-image was not stored"
    );
}

/// Every appended wire row carries the protocol version that wrote it, so a
/// later engine can tell its own rows from a newer one's.
#[test]
fn append_wire_event_stamps_the_current_protocol_version() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-stamp", None).unwrap();
    store
        .append_wire_event(&RawWireEvent {
            id: "evt-stamp".into(),
            session_id: "sess-stamp".into(),
            event_type: "test".into(),
            payload: json!({}),
            is_checkpoint: false,
            is_compaction: false,
            created_at: 1,
        })
        .unwrap();

    let conn = store.conn.lock();
    let stamped: String = conn
        .query_row(
            "SELECT protocol_version FROM wire_events WHERE id = 'evt-stamp'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stamped, WIRE_PROTOCOL_VERSION);
}

/// A journal row written by a newer engine closes every read that would
/// interpret it: the fold and each slice helper refuse rather than half-read.
#[test]
fn the_wire_readers_refuse_a_newer_protocol_version() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-newer", None).unwrap();
    store
        .append_wire_event(&RawWireEvent {
            id: "evt-newer".into(),
            session_id: "sess-newer".into(),
            event_type: "prompt.submitted".into(),
            payload: json!({}),
            is_checkpoint: false,
            is_compaction: false,
            created_at: 1,
        })
        .unwrap();
    {
        let conn = store.conn.lock();
        conn.execute(
            "UPDATE wire_events SET protocol_version = '9.9' WHERE session_id = 'sess-newer'",
            [],
        )
        .unwrap();
    }

    let expect_too_new =
        |result: Result<Vec<WireEventRecord>, EventStoreError>, reader: &str| match result {
            Err(EventStoreError::WireProtocolTooNew { found, supported }) => {
                assert_eq!(found, "9.9");
                assert_eq!(supported, WIRE_PROTOCOL_VERSION);
            }
            other => panic!("{reader} must refuse a newer journal, got {other:?}"),
        };
    expect_too_new(
        store.get_wire_events("sess-newer", 0, 10),
        "get_wire_events",
    );
    expect_too_new(
        store.interaction_wire_events("sess-newer"),
        "interaction_wire_events",
    );
    expect_too_new(store.prompt_wire_events("sess-newer"), "prompt_wire_events");
    expect_too_new(store.task_wire_events("sess-newer"), "task_wire_events");
    match store.fold_projection("sess-newer") {
        Err(EventStoreError::WireProtocolTooNew { .. }) => {}
        other => panic!("fold_projection must refuse a newer journal, got {other:?}"),
    }
}

/// Rows written before the column existed are NULL and read as this version:
/// an old database opens without a stamp backfill.
#[test]
fn null_protocol_versions_read_as_the_current_version() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-legacy", None).unwrap();
    store
        .append_wire_event(&RawWireEvent {
            id: "evt-legacy".into(),
            session_id: "sess-legacy".into(),
            event_type: "prompt.submitted".into(),
            payload: json!({}),
            is_checkpoint: false,
            is_compaction: false,
            created_at: 1,
        })
        .unwrap();
    {
        let conn = store.conn.lock();
        conn.execute(
            "UPDATE wire_events SET protocol_version = NULL WHERE session_id = 'sess-legacy'",
            [],
        )
        .unwrap();
    }

    assert_eq!(
        store.get_wire_events("sess-legacy", 0, 10).unwrap().len(),
        1
    );
    assert_eq!(store.prompt_wire_events("sess-legacy").unwrap().len(), 1);
    assert!(store.task_wire_events("sess-legacy").unwrap().is_empty());
    assert!(
        store
            .interaction_wire_events("sess-legacy")
            .unwrap()
            .is_empty()
    );
    assert!(store.fold_projection("sess-legacy").unwrap().is_empty());
}

/// The wire write guard (v2 `bindWriteGuard`, upstream #4246): a live foreign
/// holder closes the journal to this process; a freed one opens it again.
#[test]
fn the_wire_write_guard_refuses_a_foreign_held_session() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-wire-guard", None).unwrap();
    let event = |id: &str| RawWireEvent {
        id: id.into(),
        session_id: "sess-wire-guard".into(),
        event_type: "test".into(),
        payload: json!({}),
        is_checkpoint: false,
        is_compaction: false,
        created_at: 1,
    };

    // Baseline: an unheld session accepts its rows.
    store.append_wire_event(&event("evt-before")).unwrap();

    // Plant a holder the way another live process writes it. The pid must be
    // alive and not this process's: init on unix, System on Windows — the
    // guard exempts its own pid, so a same-pid plant cannot exercise it.
    let foreign_pid: u32 = if cfg!(windows) { 4 } else { 1 };
    let lock_path = crate::session::lock::SESSION_LOCKS
        .lock_dir()
        .join("sess-wire-guard.lock");
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    let holder = crate::session::lock::SessionLockHolder {
        version: 1,
        token: format!("{foreign_pid}:foreign"),
        pid: foreign_pid,
        hostname: whoami::fallible::hostname().unwrap_or_else(|_| "unknown-host".into()),
        started_at: chrono::Utc::now().timestamp_millis(),
        client_identity: "kimi_code".into(),
        client_display_name: None,
    };
    std::fs::write(&lock_path, serde_json::to_string(&holder).unwrap()).unwrap();

    match store.append_wire_event(&event("evt-blocked")) {
        Err(EventStoreError::SessionOccupied(message)) => {
            assert!(
                message.contains("in use by another Kimi Code process"),
                "{message}"
            );
        }
        other => panic!("expected SessionOccupied, got {other:?}"),
    }

    // A takeover (or the foreign process exiting) frees the journal.
    crate::session::lock::SESSION_LOCKS.takeover("sess-wire-guard");
    assert!(!lock_path.exists());
    store.append_wire_event(&event("evt-after")).unwrap();
}
