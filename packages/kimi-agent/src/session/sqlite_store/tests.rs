use super::*;

#[test]
fn a_panic_under_the_connection_guard_does_not_poison_the_store() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-alive", None).unwrap();

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = store.conn.lock();
        panic!("a panic inside the connection guard");
    }));
    std::panic::set_hook(previous);

    assert!(caught.is_err(), "the panic must actually have happened");

    let sessions = store
        .list_sessions()
        .expect("a read after a guarded panic must still work");
    assert!(
        sessions.iter().any(|s| s.session_id == "sess-alive"),
        "the session written before the panic is still readable: {sessions:?}"
    );
}

#[test]
fn wire_event_payloads_filter_by_type_and_keep_order() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-payloads", None).unwrap();
    for (id, payload, created_at) in [
        (
            "evt-1",
            json!({"type": "permission.set_mode", "mode": "auto"}),
            1,
        ),
        ("evt-2", json!({"type": "other"}), 2),
        (
            "evt-3",
            json!({"type": "permission.set_mode", "mode": "yolo"}),
            3,
        ),
    ] {
        store
            .append_wire_event_json(
                id,
                "sess-payloads",
                payload["type"].as_str().unwrap(),
                &payload.to_string(),
                false,
                false,
                created_at,
            )
            .unwrap();
    }
    let payloads = store
        .wire_event_payloads("sess-payloads", "permission.set_mode")
        .unwrap();
    assert_eq!(
        payloads,
        vec![
            json!({"type": "permission.set_mode", "mode": "auto"}),
            json!({"type": "permission.set_mode", "mode": "yolo"}),
        ]
    );
}

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

    assert_eq!(contents, vec!["first\n\nthird"]);

    let conn = store.conn.lock();
    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM wire_events WHERE session_id = 'sess-fold'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 2, "the corrupt row was dropped");
}

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

    let (changes, _) = store.get_file_history_changes("sess-big", Some(1)).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].status, "modified");
    assert_eq!(changes[0].oversize, Some(true));

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

    let (small, _) = store.get_file_history_changes("sess-big", Some(2)).unwrap();
    assert_eq!(small[0].oversize, Some(false));
    let small_end = store
        .get_file_history_content("sess-big", 2, "small.txt", "end")
        .unwrap()
        .unwrap();
    assert_eq!(small_end.oversize, None, "no marker key for stored content");
    assert_eq!(small_end.content.as_deref(), Some("b"));
}

#[test]
fn test_file_history_keeps_only_the_last_twenty_turns() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-window", None).unwrap();

    store
        .record_file_change("sess-window", 1, "a.txt", None, Some("a"))
        .unwrap();
    store
        .record_file_change("sess-window", 1, "b.txt", None, Some("b"))
        .unwrap();
    for turn in 2..=21usize {
        store
            .record_file_change("sess-window", turn, "f.txt", None, Some("x"))
            .unwrap();
    }

    let turn_ids: Vec<i64> = {
        let conn = store.conn.lock();
        let mut stmt = conn
            .prepare("SELECT turn_id FROM session_file_history WHERE session_id = 'sess-window'")
            .unwrap();
        let rows = stmt.query_map([], |row| row.get(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    };

    assert_eq!(turn_ids.len(), 20);
    assert!(!turn_ids.contains(&1), "turn 1 fell outside the window");
    assert!(turn_ids.contains(&2) && turn_ids.contains(&21));
}

#[test]
fn test_file_history_window_is_per_session() {
    let store = SqliteSessionStore::in_memory().unwrap();
    for session in ["sess-old", "sess-new"] {
        store.create_session(session, None).unwrap();
        store
            .record_file_change(session, 1, "keep.txt", None, Some("keep"))
            .unwrap();
    }
    for turn in 2..=21usize {
        store
            .record_file_change("sess-new", turn, "f.txt", None, Some("x"))
            .unwrap();
    }

    let old_turns: Vec<i64> = {
        let conn = store.conn.lock();
        let mut stmt = conn
            .prepare("SELECT turn_id FROM session_file_history WHERE session_id = 'sess-old'")
            .unwrap();
        let rows = stmt.query_map([], |row| row.get(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    };
    assert_eq!(
        old_turns,
        vec![1],
        "another session's snapshots were pruned"
    );
}

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

    store.create_session("sess-1", None).unwrap();
    let s_preserved = store.get_session("sess-1").unwrap().unwrap();
    assert_eq!(s_preserved.title.as_deref(), Some("Test Session"));
    assert!(s_preserved.updated_at >= s_init.updated_at);

    let msgs = vec![
        LLMMessage::user("Hello"),
        LLMMessage::assistant("Hi there!"),
    ];
    let usage = TokenUsage {
        input_other: 150,
        output: 42,
        input_cache_creation: 10,
        input_cache_read: 5,
        raw: None,
    };
    store
        .save_turn("sess-1", "turn-1", 1, &msgs, Some(&usage), None)
        .unwrap();

    let loaded = store.load_session_history("sess-1").unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].role, "user");
    assert_eq!(loaded[0].content, "Hello");
    assert_eq!(loaded[1].role, "assistant");
    assert_eq!(loaded[1].content, "Hi there!");

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
        assert_eq!(parsed_usage.input_other, 150);
        assert_eq!(parsed_usage.output, 42);
        assert_eq!(parsed_usage.input_cache_creation, 10);
        assert_eq!(parsed_usage.input_cache_read, 5);
    }

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

    let updated_val = serde_json::json!({ "name": "commit", "count": 43 });
    store.put_state("skill", "commit", &updated_val).unwrap();
    assert_eq!(
        store.get_state("skill", "commit").unwrap().unwrap()["count"],
        43
    );

    assert!(store.get_state("skill", "unknown").unwrap().is_none());
    assert!(store.get_state("unknown_domain", "key").unwrap().is_none());

    let chk_data = serde_json::json!({ "step": 1, "status": "in_progress" });
    store
        .save_checkpoint("sess-1", "chk-1", "Step 1", &chk_data)
        .unwrap();

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
    use crate::turn_loop::types::{ContentPart, ToolCall};

    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-structured", None).unwrap();

    let msgs = vec![
        LLMMessage {
            role: kimi_core::types::Role::User,
            content: "show me image".into(),
            blocks: vec![
                ContentPart::Text {
                    text: "show me image".into(),
                    meta: None,
                },
                ContentPart::ImageUrl {
                    image_url: kimi_core::types::ImageUrl {
                        url: "https://example.com/cat.png".into(),
                        id: None,
                        name: None,
                        dimensions: None,
                        detail: None,
                    },
                },
            ],
            tool_calls: vec![],
            tool_call_id: None,
            tools: Vec::new(),

            prompt_id: None,
            origin: None,
        },
        LLMMessage {
            role: kimi_core::types::Role::Assistant,
            content: "calling tools".into(),
            blocks: vec![],
            tool_calls: vec![
                ToolCall {
                    id: "call_read_1".into(),
                    name: "Read".into(),
                    arguments: Some(serde_json::json!({ "path": "file.txt" }).to_string()),
                    extras: None,
                    raw_id: None,
                    _stream_index: None,
                },
                ToolCall {
                    id: "call_read_2".into(),
                    name: "Grep".into(),
                    arguments: Some(serde_json::json!({ "pattern": "TODO" }).to_string()),
                    extras: None,
                    raw_id: None,
                    _stream_index: None,
                },
            ],
            tool_call_id: None,
            tools: Vec::new(),

            prompt_id: None,
            origin: None,
        },
        LLMMessage {
            role: kimi_core::types::Role::Tool,
            content: "file contents".into(),
            blocks: vec![],
            tool_calls: vec![],
            tool_call_id: Some("call_read_1".into()),
            tools: Vec::new(),
            prompt_id: None,
            origin: None,
        },
        LLMMessage {
            role: kimi_core::types::Role::Tool,
            content: "matches found".into(),
            blocks: vec![],
            tool_calls: vec![],
            tool_call_id: Some("call_read_2".into()),
            tools: Vec::new(),
            prompt_id: None,
            origin: None,
        },
    ];

    store
        .save_turn("sess-structured", "turn-1", 1, &msgs, None, None)
        .unwrap();

    let loaded = store.load_session_history("sess-structured").unwrap();
    assert_eq!(loaded.len(), 4);

    assert_eq!(loaded[0].role, "user");
    assert_eq!(loaded[0].blocks.len(), 2);
    assert_eq!(
        loaded[0].blocks[0],
        ContentPart::Text {
            text: "show me image".into(),
            meta: None,
        }
    );
    assert_eq!(
        loaded[0].blocks[1],
        crate::llm::media_resolver::media_url_part(
            kimi_core::types::MediaKind::Image,
            "https://example.com/cat.png".into(),
            None,
            None,
        )
    );

    assert_eq!(loaded[1].role, "assistant");
    assert_eq!(loaded[1].tool_calls.len(), 2);
    assert_eq!(loaded[1].tool_calls[0].id, "call_read_1");
    assert_eq!(loaded[1].tool_calls[0].name, "Read");
    assert_eq!(
        loaded[1].tool_calls[0].arguments.as_deref(),
        Some("{\"path\":\"file.txt\"}")
    );
    assert_eq!(loaded[1].tool_calls[1].id, "call_read_2");
    assert_eq!(loaded[1].tool_calls[1].name, "Grep");
    assert_eq!(
        loaded[1].tool_calls[1].arguments.as_deref(),
        Some("{\"pattern\":\"TODO\"}")
    );

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

    let ok_inherit = store
        .fork_session("sess-orig", "sess-inherit", None, None)
        .unwrap();
    assert!(ok_inherit);
    let inherited = store.get_session("sess-inherit").unwrap().unwrap();
    assert_eq!(inherited.title.as_deref(), Some("Original"));
    assert_eq!(inherited.workspace_id.as_deref(), Some("wd_test_123"));

    let missing = store
        .fork_session("sess-missing", "sess-none", None, None)
        .unwrap();
    assert!(!missing);
}

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

    assert_eq!(
        store.list_turns("sess-c").unwrap().len(),
        2,
        "the source keeps both of its rows"
    );

    assert!(
        store.plan_undo_turns("sess-c-fork", 2).is_err(),
        "the fork must refuse an undo that crosses the compaction boundary"
    );

    assert_eq!(
        store.plan_undo_turns("sess-c-fork", 1).unwrap(),
        vec![2],
        "the compaction guard does not block a same-side undo"
    );
}

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

#[test]
fn an_invisible_turn_does_not_shift_the_turn_index() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session_with_workspace("sess-src", Some("Src"), Some("wd_vis"))
        .unwrap();
    let user_origin = json!({ "kind": "user" });

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

#[test]
fn a_non_user_opening_message_is_not_a_visible_turn() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session_with_workspace("sess-role", Some("Src"), Some("wd_role"))
        .unwrap();
    let user_origin = json!({ "kind": "user" });

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

    let key1 = encode_workdir_key("G:\\kimi\\kimi-code\\");
    assert!(key1.starts_with("wd_kimi-code_"));
    assert_eq!(key1.len(), "wd_kimi-code_".len() + 12);

    let key_empty = encode_workdir_key("");
    assert!(key_empty.starts_with("wd_workspace_"));
    assert_eq!(key_empty.len(), "wd_workspace_".len() + 12);

    let key_dot = encode_workdir_key(".");
    assert!(key_dot.starts_with("wd_workspace_"));

    let ws1 = store
        .create_workspace("/path/to/my-project", Some("My Project"))
        .unwrap();
    assert_eq!(ws1.id, encode_workdir_key("/path/to/my-project"));
    assert_eq!(ws1.root, "/path/to/my-project");
    assert_eq!(ws1.name, "My Project");
    assert_eq!(ws1.session_count, 0);

    let created_dt =
        chrono::DateTime::parse_from_rfc3339(&ws1.created_at).expect("valid ISO-8601 created_at");
    let last_opened_dt = chrono::DateTime::parse_from_rfc3339(&ws1.last_opened_at)
        .expect("valid ISO-8601 last_opened_at");
    assert!(last_opened_dt >= created_dt);

    assert!(!store.is_workspace_trusted(&ws1.id).unwrap());
    assert!(!store.is_workspace_trusted("wd_missing_workspace").unwrap());

    assert!(store.set_workspace_trusted(&ws1.id, true).unwrap());
    assert!(store.is_workspace_trusted(&ws1.id).unwrap());
    assert!(store.set_workspace_trusted(&ws1.id, false).unwrap());
    assert!(!store.is_workspace_trusted(&ws1.id).unwrap());

    store
        .create_session_with_workspace("s1", Some("Session 1"), Some(&ws1.id))
        .unwrap();
    store
        .create_session_with_workspace("s2", Some("Session 2"), Some(&ws1.id))
        .unwrap();

    let ws1_updated = store.get_workspace(&ws1.id).unwrap().unwrap();
    assert_eq!(ws1_updated.session_count, 2);

    let ws1_renamed = store
        .create_workspace("/path/to/my-project", Some("Renamed Project"))
        .unwrap();
    assert_eq!(ws1_renamed.name, "Renamed Project");
    assert_eq!(ws1_renamed.session_count, 2);

    let list = store.list_workspaces().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, ws1.id);
    assert_eq!(list[0].name, "Renamed Project");

    let deleted = store.delete_workspace(&ws1.id).unwrap();
    assert!(deleted);
    assert!(store.get_workspace(&ws1.id).unwrap().is_none());
    assert_eq!(store.list_workspaces().unwrap().len(), 0);

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

    let export = store.export_session("sess-exp").unwrap().unwrap();
    assert_eq!(export.session.session_id, "sess-exp");
    assert_eq!(export.session.title.as_deref(), Some("Export Session"));
    assert_eq!(export.messages.len(), 2);
    assert_eq!(export.messages[0].content, "Please export this");
    assert_eq!(export.messages[1].content, "Exporting now");
    assert_eq!(export.turns_count, 1);

    let parsed_dt = chrono::DateTime::parse_from_rfc3339(&export.exported_at)
        .expect("exported_at must be valid RFC 3339 timestamp");
    assert!(parsed_dt.timestamp_millis() <= chrono::Utc::now().timestamp_millis());

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

    assert!(store.export_session("sess-missing").unwrap().is_none());
}

#[test]
fn test_export_session_bundles_the_raw_wire_journal() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-wire-exp", None).unwrap();
    let data_uri = format!("data:image/png;base64,{}", "QUJD".repeat(2048));
    store
        .append_wire_event(&RawWireEvent {
            id: "evt_1".into(),
            session_id: "sess-wire-exp".into(),
            event_type: "context.append_message".into(),
            payload: json!({
                "message": {
                    "role": "user",
                    "content": "",
                    "blocks": [{ "type": "image_url", "image_url": { "url": data_uri } }]
                },
                "time": 1,
            }),
            is_checkpoint: false,
            is_compaction: false,
            created_at: 1,
        })
        .unwrap();

    let export = store.export_session("sess-wire-exp").unwrap().unwrap();
    assert_eq!(export.wire_jsonl.len(), 1);
    let line: Value = serde_json::from_str(&export.wire_jsonl[0]).unwrap();
    assert_eq!(line["type"], "context.append_message");
    assert_eq!(line["time"], 1);
    assert!(
        line["message"]["blocks"][0]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"),
        "the bundle restores media instead of leaving blobref handles dangling"
    );
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
fn test_legacy_session_metadata_is_imported_and_written_back() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-legacy-meta", None).unwrap();
    store
        .put_state(
            "metadata",
            "sess-legacy-meta",
            &json!({
                "workDir": "/tmp/legacy-workspace",
                "customTitle": "Renamed on the old build",
                "isCustomTitle": true,
                "titleSource": "user",
                "permission_mode": "auto",
            }),
        )
        .unwrap();

    assert!(
        store
            .migrate_legacy_session_metadata("sess-legacy-meta")
            .unwrap()
    );
    let session = store.get_session("sess-legacy-meta").unwrap().unwrap();
    assert_eq!(session.title.as_deref(), Some("Renamed on the old build"));
    assert_eq!(session.title_kind.as_deref(), Some("custom"));
    assert_eq!(session.cwd.as_deref(), Some("/tmp/legacy-workspace"));
    let metadata = store
        .get_state("metadata", "sess-legacy-meta")
        .unwrap()
        .unwrap();
    assert_eq!(metadata["isCustomTitle"], json!(true));
    assert_eq!(metadata["titleKind"], json!("custom"));
    assert_eq!(metadata["permission_mode"], json!("auto"));
    for legacy in ["workDir", "customTitle", "titleSource"] {
        assert!(
            metadata.get(legacy).is_none(),
            "legacy key {legacy} is folded away"
        );
    }

    assert!(
        !store
            .migrate_legacy_session_metadata("sess-legacy-meta")
            .unwrap(),
        "a normalized document needs no second write"
    );
}

#[test]
fn test_legacy_metadata_does_not_override_a_normalized_title() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-legacy-stale", None).unwrap();
    store
        .update_session_title("sess-legacy-stale", Some("Renamed here"))
        .unwrap();
    store
        .put_state(
            "metadata",
            "sess-legacy-stale",
            &json!({ "isCustomTitle": false, "workDir": "/tmp/old" }),
        )
        .unwrap();

    assert!(
        store
            .migrate_legacy_session_metadata("sess-legacy-stale")
            .unwrap()
    );
    let session = store.get_session("sess-legacy-stale").unwrap().unwrap();
    assert_eq!(session.title.as_deref(), Some("Renamed here"));
    assert_eq!(session.title_kind.as_deref(), Some("custom"));
    assert_eq!(session.cwd.as_deref(), Some("/tmp/old"));
    let metadata = store
        .get_state("metadata", "sess-legacy-stale")
        .unwrap()
        .unwrap();
    assert_eq!(metadata["isCustomTitle"], json!(true));
}

#[test]
fn test_persisted_session_metadata_carries_the_compat_bit() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-meta-bit", None).unwrap();

    store
        .put_session_metadata("sess-meta-bit", &json!({ "permission_mode": "auto" }))
        .unwrap();
    let metadata = store
        .get_state("metadata", "sess-meta-bit")
        .unwrap()
        .unwrap();
    assert_eq!(metadata["isCustomTitle"], json!(false));

    store
        .update_session_title("sess-meta-bit", Some("Mine"))
        .unwrap();
    store
        .put_session_metadata("sess-meta-bit", &json!({ "permission_mode": "auto" }))
        .unwrap();
    let metadata = store
        .get_state("metadata", "sess-meta-bit")
        .unwrap()
        .unwrap();
    assert_eq!(metadata["isCustomTitle"], json!(true));

    store
        .set_generated_title_if_uncustomized("sess-meta-bit", "Generated", true)
        .unwrap();
    store
        .put_session_metadata("sess-meta-bit", &json!({ "permission_mode": "auto" }))
        .unwrap();
    let metadata = store
        .get_state("metadata", "sess-meta-bit")
        .unwrap()
        .unwrap();
    assert_eq!(
        metadata["isCustomTitle"],
        json!(false),
        "a generated title is not a custom one"
    );
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

    store.gui_set_item("theme", "light").unwrap();
    assert_eq!(
        store.gui_get_item("theme").unwrap().as_deref(),
        Some("light")
    );

    assert!(store.gui_remove_item("theme").unwrap());
    assert_eq!(store.gui_get_item("theme").unwrap(), None);
    assert_eq!(store.gui_length().unwrap(), 1);

    assert!(!store.gui_remove_item("theme").unwrap());

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

    let hits_assistant = store
        .search_messages("index", None, Some("assistant"), 10, 0)
        .unwrap();
    assert_eq!(hits_assistant.len(), 1);
    assert_eq!(hits_assistant[0].role, "assistant");

    let hits_user = store
        .search_messages("index", None, Some("user"), 10, 0)
        .unwrap();
    assert_eq!(hits_user.len(), 0);

    let hits_sess2 = store
        .search_messages("Fix", Some("sess-2"), None, 10, 0)
        .unwrap();
    assert_eq!(hits_sess2.len(), 1);
    assert_eq!(hits_sess2[0].session_id, "sess-2");

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

    let limited = store.search_messages("the", None, None, 1, 0).unwrap();
    assert_eq!(limited.len(), 1);

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

    assert_eq!(store.count_messages().unwrap(), 4);
}

#[test]
fn test_file_history_crud_and_diff() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session("sess-fh", Some("File History Test"))
        .unwrap();

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

    store
        .record_file_change(
            "sess-fh",
            1,
            "src/main.rs",
            None,
            Some("fn main() {\n    println!(\"hello\");\n}\n"),
        )
        .unwrap();

    store
        .record_file_change(
            "sess-fh",
            2,
            "src/main.rs",
            Some("fn main() {\n    println!(\"hello\");\n}\n"),
            Some("fn main() {\n    println!(\"world\");\n    let x = 10;\n}\n"),
        )
        .unwrap();

    store
        .record_file_change(
            "sess-fh",
            3,
            "src/main.rs",
            Some("fn main() {\n    println!(\"world\");\n    let x = 10;\n}\n"),
            None,
        )
        .unwrap();

    let (ch1, rec1) = store.get_file_history_changes("sess-fh", Some(1)).unwrap();
    assert!(rec1);
    assert_eq!(ch1.len(), 1);
    assert_eq!(ch1[0].path, "src/main.rs");
    assert_eq!(ch1[0].status, "added");
    assert_eq!(ch1[0].additions, 3);
    assert_eq!(ch1[0].deletions, 0);

    let (ch2, rec2) = store.get_file_history_changes("sess-fh", Some(2)).unwrap();
    assert!(rec2);
    assert_eq!(ch2.len(), 1);
    assert_eq!(ch2[0].path, "src/main.rs");
    assert_eq!(ch2[0].status, "modified");

    let (ch3, rec3) = store.get_file_history_changes("sess-fh", Some(3)).unwrap();
    assert!(rec3);
    assert_eq!(ch3.len(), 1);
    assert_eq!(ch3[0].path, "src/main.rs");
    assert_eq!(ch3[0].status, "deleted");
    assert_eq!(ch3[0].additions, 0);
    assert_eq!(ch3[0].deletions, 4);

    let (all_changes, all_rec) = store.get_file_history_changes("sess-fh", None).unwrap();
    assert!(all_rec);
    assert_eq!(all_changes.len(), 3);
    assert_eq!(all_changes[0].status, "added");
    assert_eq!(all_changes[1].status, "modified");
    assert_eq!(all_changes[2].status, "deleted");

    let pruned = store.prune_file_history("sess-fh", 2).unwrap();
    assert_eq!(pruned, 1);
    let (retained_changes, _) = store.get_file_history_changes("sess-fh", None).unwrap();
    assert_eq!(retained_changes.len(), 2);
    assert_eq!(retained_changes[0].status, "modified");
    assert_eq!(retained_changes[1].status, "deleted");

    let (ch_none, rec_none) = store.get_file_history_changes("sess-fh", Some(99)).unwrap();
    assert!(!rec_none);
    assert!(ch_none.is_empty());

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

    assert!(
        store
            .get_file_history_content("sess-fh", 2, "src/missing.rs", "end")
            .unwrap()
            .is_none()
    );

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

    std::fs::write(&file_path, "hand-edited after the turn").unwrap();
    let conflict = store
        .revert_turn_file_changes("sess-revert", 1, temp_ws.path())
        .expect_err("a manual edit is a conflict, not a silent overwrite");
    assert!(
        matches!(
            &conflict,
            crate::session::sqlite_store::FileRevertError::Conflict { path, .. } if path == "src/main.rs"
        ),
        "{conflict:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "hand-edited after the turn",
        "the manual edit survives the refused undo"
    );

    std::fs::remove_file(&file_path).unwrap();
    let deleted = store
        .revert_turn_file_changes("sess-revert", 1, temp_ws.path())
        .expect_err("a deleted file is a conflict");
    assert!(
        matches!(
            &deleted,
            crate::session::sqlite_store::FileRevertError::Conflict { path, .. } if path == "src/main.rs"
        ),
        "{deleted:?}"
    );
}

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

    assert_eq!(store.load_session_history("sess-plan").unwrap().len(), 3);
}

#[test]
fn test_next_turn_number() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-seq", None).unwrap();

    assert_eq!(store.next_turn_number("sess-seq").unwrap(), 1);

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

#[test]
fn a_steer_and_its_host_prompt_are_one_undo_unit() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-steer-undo", None).unwrap();

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

    let mut steer = LLMMessage::user("actually use python");
    steer.prompt_id = Some("prompt-1".to_string());
    store
        .save_turn("sess-steer-undo", "t1", 1, &[steer], None, None)
        .unwrap();

    let history = store.load_session_history("sess-steer-undo").unwrap();
    assert_eq!(history.len(), 3, "prompt + reply + steer in one turn");

    assert_eq!(history[2].prompt_id.as_deref(), Some("prompt-1"));

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

    let undone = store.undo_turns("sess-undo", 1).unwrap();
    assert_eq!(undone, 1);
    let history = store.load_session_history("sess-undo").unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].content, "1");
    assert_eq!(history[1].content, "2");

    let err = store.undo_turns("sess-undo", 10).unwrap_err();
    assert!(err.contains("only 2 of 10"), "unexpected error: {err}");
    let undone_all = store.undo_turns("sess-undo", 2).unwrap();
    assert_eq!(undone_all, 2);
    assert!(store.load_session_history("sess-undo").unwrap().is_empty());

    let err = store.undo_turns("sess-undo", 1).unwrap_err();
    assert!(err.contains("nothing to undo"), "unexpected error: {err}");
}

#[test]
fn test_refresh_last_prompt_after_undo_walks_back_to_the_previous_anchor() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-last-prompt", None).unwrap();
    store
        .save_turn(
            "sess-last-prompt",
            "t1",
            1,
            &[LLMMessage::user("first")],
            None,
            None,
        )
        .unwrap();
    store
        .save_turn(
            "sess-last-prompt",
            "t2",
            2,
            &[LLMMessage::user("second")],
            None,
            None,
        )
        .unwrap();
    store
        .apply_prompt_metadata("sess-last-prompt", "second", "second")
        .unwrap();

    assert_eq!(
        store
            .refresh_last_prompt_after_undo("sess-last-prompt")
            .unwrap()
            .as_deref(),
        Some("second")
    );

    store.undo_turns("sess-last-prompt", 1).unwrap();
    assert_eq!(
        store
            .refresh_last_prompt_after_undo("sess-last-prompt")
            .unwrap()
            .as_deref(),
        Some("first")
    );

    store.undo_turns("sess-last-prompt", 1).unwrap();
    assert_eq!(
        store
            .refresh_last_prompt_after_undo("sess-last-prompt")
            .unwrap(),
        None
    );
    let session = store.get_session("sess-last-prompt").unwrap().unwrap();
    assert_eq!(session.last_prompt, None);
}

#[test]
fn test_refresh_last_prompt_after_undo_skips_non_anchor_turns() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-last-prompt-skip", None).unwrap();
    store
        .save_turn(
            "sess-last-prompt-skip",
            "t1",
            1,
            &[LLMMessage::user("prompt")],
            None,
            None,
        )
        .unwrap();
    store
        .save_turn(
            "sess-last-prompt-skip",
            "t2",
            2,
            &[LLMMessage::user("subagent follow-up")],
            None,
            Some(&serde_json::json!({"kind": "subagent", "inTurn": true})),
        )
        .unwrap();
    store
        .apply_prompt_metadata("sess-last-prompt-skip", "subagent follow-up", "title")
        .unwrap();

    assert_eq!(
        store
            .refresh_last_prompt_after_undo("sess-last-prompt-skip")
            .unwrap()
            .as_deref(),
        Some("prompt")
    );
}

#[test]
fn test_compact_session() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-compact", None).unwrap();

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

    let (report, _) = store
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

    assert!(report.tokens_after <= report.tokens_before + 512);

    let post_history = store.load_session_history("sess-compact").unwrap();
    assert_eq!(post_history.len(), pre_len - removed);
    assert!(post_history.len() < pre_len);

    assert!(
        post_history
            .iter()
            .all(|message| message.role != "assistant"),
        "completed-turn assistant messages are summarized away"
    );
    assert!(
        post_history
            .iter()
            .any(|message| message.content == "Trailing question"),
        "the trailing real user input survives verbatim"
    );
    assert_eq!(
        report.kept_user_message_count,
        post_history
            .iter()
            .filter(|message| message.role == "user")
            .count(),
        "the report counts the user messages that survived"
    );

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

    let err = store.undo_turns("sess-compact", 100).unwrap_err();
    assert!(
        err.contains("undo refused"),
        "expected boundary refusal, got: {err}"
    );
}

#[test]
fn journal_backed_compaction_keeps_the_projection_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().join("sessions.db")).unwrap();
    let workspace = store
        .create_workspace("/work/compact-journal", None)
        .unwrap();
    store
        .create_session_with_workspace("sess-compact-journal", None, Some(&workspace.id))
        .unwrap();
    for number in 1..=20u32 {
        store
            .save_turn(
                "sess-compact-journal",
                &format!("turn-{number}"),
                number,
                &[
                    LLMMessage::user(format!("q{number}")),
                    LLMMessage::assistant(format!("a{number}")),
                ],
                None,
                None,
            )
            .unwrap();
    }
    let path = store
        .wire_journal_path("sess-compact-journal", "main")
        .unwrap();
    let history = store.load_session_history("sess-compact-journal").unwrap();
    let records: Vec<serde_json::Value> = history
        .iter()
        .filter_map(crate::session::agent_records::history_entry)
        .map(|mut entry| {
            entry["type"] = serde_json::json!("context.append_message");
            entry
        })
        .collect();
    crate::session::wire_journal::write_journal_file(&path, &records).unwrap();
    let before = store
        .load_session_messages("sess-compact-journal")
        .unwrap()
        .len();
    let (report, _) = store
        .compact_session_with_summary("sess-compact-journal", None, Some("sum".into()))
        .unwrap();
    assert!(report.removed > 0);
    assert_eq!(
        store
            .load_session_messages("sess-compact-journal")
            .unwrap()
            .len(),
        before,
        "journal-backed compaction must not delete projection rows"
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

    let (report, _) = store
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
            .contains("The user asked for fifteen numbered messages."),
        "the caller's summary replaces the omitted prefix"
    );
    assert!(
        summary_message.content.ends_with("</system-reminder>"),
        "v2 wraps the summary in a system-reminder"
    );
    assert!(
        !summary_message
            .content
            .contains("Earlier conversation compacted:"),
        "the placeholder must not survive a supplied summary"
    );

    let short = SqliteSessionStore::in_memory().unwrap();
    short.create_session("sess-short", None).unwrap();
    short
        .save_turn("sess-short", "t1", 1, &[LLMMessage::user("hi")], None, None)
        .unwrap();
    assert_eq!(
        short
            .compact_session_with_summary("sess-short", None, Some("unused".to_string()))
            .unwrap()
            .0,
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

    let deleted = store.delete_session("sess-del").unwrap();
    assert!(deleted);

    assert!(store.get_session("sess-del").unwrap().is_none());
    assert!(store.load_session_history("sess-del").unwrap().is_empty());

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

    assert!(!store.delete_session("sess-del").unwrap());
}

#[test]
fn test_undo_turns_transactional_integrity() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session("sess-undo", Some("Undo Test"))
        .unwrap();

    let msgs_t1 = vec![
        LLMMessage::user("prompt 1"),
        LLMMessage::assistant("reply 1"),
    ];
    store
        .save_turn("sess-undo", "t1", 1, &msgs_t1, None, None)
        .unwrap();

    let msgs_t2 = vec![
        LLMMessage::user("prompt 2"),
        LLMMessage::assistant("reply 2"),
    ];
    store
        .save_turn("sess-undo", "t2", 2, &msgs_t2, None, None)
        .unwrap();

    let msgs_t3 = vec![
        LLMMessage::user("prompt 3"),
        LLMMessage::assistant("reply 3"),
    ];
    store
        .save_turn("sess-undo", "t3", 3, &msgs_t3, None, None)
        .unwrap();

    let history_before = store.load_session_history("sess-undo").unwrap();
    assert_eq!(history_before.len(), 6);

    let undone = store.undo_turns("sess-undo", 1).unwrap();
    assert_eq!(undone, 1);
    let history_after_1 = store.load_session_history("sess-undo").unwrap();
    assert_eq!(history_after_1.len(), 4);
    assert_eq!(history_after_1[0].content, "prompt 1");
    assert_eq!(history_after_1[1].content, "reply 1");
    assert_eq!(history_after_1[2].content, "prompt 2");
    assert_eq!(history_after_1[3].content, "reply 2");

    assert_eq!(store.next_turn_number("sess-undo").unwrap(), 3);

    let err = store.undo_turns("sess-undo", 10).unwrap_err();
    assert!(err.contains("only 2 of 10"), "unexpected error: {err}");

    let undone_all = store.undo_turns("sess-undo", 2).unwrap();
    assert_eq!(undone_all, 2);
    let history_empty = store.load_session_history("sess-undo").unwrap();
    assert_eq!(history_empty.len(), 0);
    assert_eq!(store.next_turn_number("sess-undo").unwrap(), 1);
}

#[test]
fn test_undo_turns_rolls_back_when_a_delete_fails() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-undo-atomic", None).unwrap();
    store
        .save_turn(
            "sess-undo-atomic",
            "t1",
            1,
            &[LLMMessage::user("1"), LLMMessage::assistant("a1")],
            None,
            None,
        )
        .unwrap();

    store
        .conn
        .lock()
        .execute_batch(
            "CREATE TRIGGER undo_fail BEFORE DELETE ON turns
             WHEN OLD.turn_id = 't1'
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        )
        .unwrap();

    let error = store.undo_turns("sess-undo-atomic", 1).unwrap_err();
    assert!(
        error.contains("injected failure"),
        "unexpected error: {error}"
    );
    assert_eq!(
        store
            .load_session_history("sess-undo-atomic")
            .unwrap()
            .len(),
        2,
        "a failed undo must not leave the messages deleted"
    );
    assert_eq!(
        store.next_turn_number("sess-undo-atomic").unwrap(),
        2,
        "the turn row must survive a failed undo"
    );
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

    let (reverted, _) = store
        .patch_state("metadata", "sess-patch-1", &inverse)
        .unwrap();
    assert_eq!(reverted["theme"], "dark");
    assert_eq!(reverted["fontSize"], 14);
}

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

#[test]
fn test_session_scoped_state_survives_migration_and_cascades_on_delete() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state-migration.db");
    {
        let store = SqliteSessionStore::open(&path).unwrap();
        store.create_session("sess-a", Some("a")).unwrap();
        store.create_session("sess-b", Some("b")).unwrap();

        store
            .put_state("agent_config", "sess-a", &json!({ "model": "k2" }))
            .unwrap();
        store
            .put_state("metadata", "sess-a", &json!({ "permission_mode": "auto" }))
            .unwrap();

        store
            .put_state("plan", "workspace", &json!({ "active": true }))
            .unwrap();
    }

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

    assert_eq!(store.count_wire_events("sess-evt-test").unwrap(), 3);
    assert_eq!(store.latest_wire_event_seq("sess-evt-test").unwrap(), 3);

    let page1 = store.get_wire_events("sess-evt-test", 0, 2).unwrap();
    assert_eq!(page1.len(), 2);
    assert_eq!(page1[0].seq, 1);
    assert_eq!(page1[1].seq, 2);

    let page2 = store.get_wire_events("sess-evt-test", 2, 10).unwrap();
    assert_eq!(page2.len(), 1);
    assert_eq!(page2[0].seq, 3);
    assert_eq!(page2[0].event_type, "tool.result");

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

    let undone_count = store.undo_to_last_checkpoint("sess-evt-test").unwrap();
    assert_eq!(undone_count, 2);
    assert_eq!(store.count_wire_events("sess-evt-test").unwrap(), 1);

    let msgs_after_undo = store.fold_projection("sess-evt-test").unwrap();
    assert_eq!(msgs_after_undo.len(), 1);
    assert_eq!(
        msgs_after_undo[0].role,
        crate::native::event_store::MessageRole::User
    );

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

    let undo_err = store.undo_to_last_checkpoint("sess-evt-test").unwrap_err();
    match undo_err {
        EventStoreError::UndoCompactionBoundary => {}
        other => panic!("Expected UndoCompactionBoundary, got {:?}", other),
    }
}

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

    store
        .set_last_turn_reason("sess-absent", Some("completed"))
        .unwrap();
    assert_eq!(store.last_turn_reason("sess-absent"), None);
}

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

#[test]
fn workspace_root_key_is_lexical_and_folds_by_path_shape() {
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

    assert_eq!(workspace_root_key("/Home/Foo/Proj"), "/Home/Foo/Proj");

    assert_eq!(
        workspace_root_key("\\\\Server\\Share\\Proj"),
        "//server/share/proj"
    );

    assert_ne!(
        workspace_root_key("/tmp/Case"),
        workspace_root_key("/tmp/case")
    );
    assert_eq!(
        workspace_root_key("D:/Case/"),
        workspace_root_key("d:/case")
    );
}

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

    assert_eq!(store.plan_undo_turns("sess-anchor", 1).unwrap(), vec![4, 3]);
    assert_eq!(store.undo_turns("sess-anchor", 1).unwrap(), 2);
    assert_eq!(store.load_session_history("sess-anchor").unwrap().len(), 2);

    let err = store.undo_turns("sess-anchor", 2).unwrap_err();
    assert!(
        err.contains("only 1 of 2"),
        "a derived turn must not count as an anchor: {err}"
    );
}

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

#[test]
fn the_wire_readers_refuse_a_minimum_reader_version_above_this_engine() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-min-reader", None).unwrap();
    store
        .append_wire_event(&RawWireEvent {
            id: "evt-min".into(),
            session_id: "sess-min-reader".into(),
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
            "UPDATE wire_events SET min_protocol_version = '9.9' WHERE session_id = 'sess-min-reader'",
            [],
        )
        .unwrap();
    }

    let expect_too_low =
        |result: Result<Vec<WireEventRecord>, EventStoreError>, reader: &str| match result {
            Err(EventStoreError::WireReaderTooLow {
                required,
                supported,
            }) => {
                assert_eq!(required, "9.9");
                assert_eq!(supported, WIRE_PROTOCOL_VERSION);
            }
            other => panic!("{reader} must refuse a newer min reader, got {other:?}"),
        };
    expect_too_low(
        store.get_wire_events("sess-min-reader", 0, 10),
        "get_wire_events",
    );
    expect_too_low(
        store.interaction_wire_events("sess-min-reader"),
        "interaction_wire_events",
    );
    match store.fold_projection("sess-min-reader") {
        Err(EventStoreError::WireReaderTooLow { .. }) => {}
        other => panic!("fold_projection must refuse a newer min reader, got {other:?}"),
    }
}

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

    store.append_wire_event(&event("evt-before")).unwrap();

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

    crate::session::lock::SESSION_LOCKS.takeover("sess-wire-guard");
    assert!(!lock_path.exists());
    store.append_wire_event(&event("evt-after")).unwrap();
}

#[test]
fn a_corrupt_state_entry_is_reported_instead_of_reading_as_null() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-state", None).unwrap();
    store
        .put_state("todo", "list", &json!({ "items": [] }))
        .unwrap();
    {
        let conn = store.conn.lock();
        conn.execute(
            "UPDATE state_entries SET value = ?1 WHERE domain = 'todo' AND key = 'list'",
            params!["{\"items\": ["],
        )
        .unwrap();
    }

    let error = store
        .get_state("todo", "list")
        .expect_err("a corrupt row must not read as a value");
    assert!(
        error.to_string().contains("todo/list"),
        "the error names the entry: {error}"
    );

    let error = store
        .diff_state("todo", "list", &json!([]))
        .expect_err("diffing on top of a corrupt row must fail");
    assert!(
        error.contains("todo/list"),
        "the diff error names the entry: {error}"
    );
}

#[test]
fn a_corrupt_blocks_column_is_reported_instead_of_reading_as_empty() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-blocks", None).unwrap();
    {
        let conn = store.conn.lock();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, blocks, created_at, turn_id)
             VALUES ('sess-blocks', 'assistant', 'hello', ?1, 1, 'turn-1')",
            params!["{\"kind\":"],
        )
        .unwrap();
    }

    let error = store
        .load_session_messages("sess-blocks")
        .expect_err("a corrupt blocks column must not read as an empty list");
    assert!(
        error.to_string().contains("blocks"),
        "the error names the column: {error}"
    );
}

#[test]
fn a_corrupt_wire_payload_is_skipped_and_the_rest_is_returned() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-wire-payload", None).unwrap();
    for (id, content) in [
        ("evt-1", "first"),
        ("evt-corrupt", "bad"),
        ("evt-3", "third"),
    ] {
        store
            .append_wire_event(&RawWireEvent {
                id: id.into(),
                session_id: "sess-wire-payload".into(),
                event_type: "message.user".into(),
                payload: json!({ "content": content }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 1,
            })
            .unwrap();
    }
    {
        let conn = store.conn.lock();
        conn.execute(
            "UPDATE wire_events SET payload = ?1 WHERE id = 'evt-corrupt'",
            params!["{\"content\": \"bad"],
        )
        .unwrap();
    }

    let records = store
        .get_wire_events("sess-wire-payload", 0, 10)
        .expect("one corrupt row must not fail the read");
    assert_eq!(
        records.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec!["evt-1", "evt-3"],
        "the corrupt row is dropped and the rest is returned in order"
    );
}

#[test]
fn a_legacy_database_gets_the_wire_version_columns_on_open() {
    let dir = std::env::temp_dir().join(format!(
        "kimi-legacy-wire-{}-{}",
        std::process::id(),
        fastrand::u64(..)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("sessions.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                id TEXT PRIMARY KEY,
                session_id TEXT,
                role TEXT,
                content TEXT,
                created_at INTEGER,
                tool_calls TEXT
             );
             CREATE TABLE wire_events (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                id TEXT NOT NULL UNIQUE,
                session_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                payload TEXT NOT NULL,
                is_checkpoint INTEGER NOT NULL DEFAULT 0,
                is_compaction INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT 0
             );",
        )
        .unwrap();
    }

    let store = SqliteSessionStore::open(&db).unwrap();
    let columns = {
        let conn = store.conn.lock();
        table_columns(&conn, "wire_events").unwrap()
    };
    assert!(
        columns.contains("protocol_version"),
        "the wire version column must be added; got {columns:?}"
    );
    assert!(
        columns.contains("min_protocol_version"),
        "the reader-minimum column must be added; got {columns:?}"
    );

    store.create_session("sess-legacy", None).unwrap();
    store
        .append_wire_event(&RawWireEvent {
            id: "evt-legacy".into(),
            session_id: "sess-legacy".into(),
            event_type: "message.user".into(),
            payload: json!({ "content": "hi" }),
            is_checkpoint: false,
            is_compaction: false,
            created_at: 1,
        })
        .expect("a migrated database must accept appends");

    drop(store);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn concurrent_patches_to_one_entry_do_not_lose_a_key() {
    use crate::session::patch::{JsonPatchSet, PatchOp};

    let store = std::sync::Arc::new(SqliteSessionStore::in_memory().unwrap());
    store.put_state("metadata", "shared", &json!({})).unwrap();

    let mut handles = Vec::new();
    for key in ["alpha", "beta"] {
        let store = store.clone();
        handles.push(std::thread::spawn(move || {
            let patch = JsonPatchSet::new(vec![PatchOp::Add {
                path: format!("/{key}"),
                value: json!(1),
            }]);
            for _ in 0..50 {
                store.patch_state("metadata", "shared", &patch).unwrap();
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }

    let value = store.get_state("metadata", "shared").unwrap().unwrap();
    assert_eq!(value["alpha"], json!(1), "the first writer's key survives");
    assert_eq!(value["beta"], json!(1), "the second writer's key survives");
}

#[test]
fn concurrent_appends_to_one_file_keep_sequences_strictly_increasing() {
    let dir = std::env::temp_dir().join(format!(
        "kimi-seq-race-{}-{}",
        std::process::id(),
        fastrand::u64(..)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("sessions.db");
    let first = std::sync::Arc::new(SqliteSessionStore::open(&db).unwrap());
    let second = std::sync::Arc::new(SqliteSessionStore::open(&db).unwrap());
    first.create_session("sess-seq", None).unwrap();

    let mut handles = Vec::new();
    for (writer, store) in ["a", "b"].into_iter().zip([first.clone(), second.clone()]) {
        handles.push(std::thread::spawn(move || {
            for index in 0..25 {
                store
                    .append_wire_event(&RawWireEvent {
                        id: format!("{writer}-{index}"),
                        session_id: "sess-seq".into(),
                        event_type: "message.user".into(),
                        payload: json!({ "content": format!("{writer}{index}") }),
                        is_checkpoint: false,
                        is_compaction: false,
                        created_at: 1,
                    })
                    .unwrap();
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }

    let records = first.get_wire_events("sess-seq", 0, 1000).unwrap();
    assert_eq!(records.len(), 50, "no append is lost or duplicated");
    let seqs: Vec<u64> = records.iter().map(|record| record.seq).collect();
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "the sequence order is strictly increasing: {seqs:?}"
    );

    drop((first, second));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_empty_database_file_opens_as_an_empty_store() {
    let dir = std::env::temp_dir().join(format!(
        "kimi-empty-db-{}-{}",
        std::process::id(),
        fastrand::u64(..)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("sessions.db");
    std::fs::write(&db, b"").unwrap();

    let store = SqliteSessionStore::open(&db).expect("a zero-byte database is a new store");
    assert!(store.list_sessions().unwrap().is_empty());
    assert!(
        store
            .fold_projection("no-such-session")
            .expect("an unknown session folds to nothing, not an error")
            .is_empty()
    );

    drop(store);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn generated_titles_never_replace_a_custom_title() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-title", None).unwrap();

    assert!(
        store
            .apply_prompt_metadata("sess-title", "hello", "hello")
            .unwrap()
    );
    let seeded = store.get_session("sess-title").unwrap().unwrap();
    assert_eq!(seeded.title.as_deref(), Some("hello"));
    assert_eq!(seeded.normalized_title_kind(), Some("replaceable"));
    assert_eq!(seeded.last_prompt.as_deref(), Some("hello"));

    assert!(
        store
            .update_session_title("sess-title", Some("Mine"))
            .unwrap()
    );
    assert!(
        !store
            .set_generated_title_if_uncustomized("sess-title", "Model title", false)
            .unwrap(),
        "an automatic title must not overwrite a custom one"
    );
    let renamed = store.get_session("sess-title").unwrap().unwrap();
    assert_eq!(renamed.title.as_deref(), Some("Mine"));
    assert_eq!(renamed.normalized_title_kind(), Some("custom"));
    assert_eq!(
        renamed.last_prompt.as_deref(),
        Some("hello"),
        "a title change must not disturb the recorded prompt"
    );

    assert!(
        store
            .set_generated_title_if_uncustomized("sess-title", "Forced", true)
            .unwrap()
    );
    assert_eq!(
        store
            .get_session("sess-title")
            .unwrap()
            .unwrap()
            .title
            .as_deref(),
        Some("Forced")
    );
}

#[test]
fn archiving_stamps_archived_at_without_reordering_the_session() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session_with_workspace("sess-archive", Some("T"), None)
        .unwrap();
    let before = store
        .get_session("sess-archive")
        .unwrap()
        .unwrap()
        .updated_at;
    std::thread::sleep(std::time::Duration::from_millis(5));

    assert!(store.archive_session("sess-archive").unwrap());
    let archived = store.get_session("sess-archive").unwrap().unwrap();
    assert!(archived.archived);
    assert!(archived.archived_at.is_some(), "v2 stamps archivedAt");
    assert_eq!(
        archived.updated_at, before,
        "archiving is not activity; updated_at must not move"
    );

    assert!(store.restore_session("sess-archive").unwrap());
    let restored = store.get_session("sess-archive").unwrap().unwrap();
    assert!(!restored.archived);
    assert!(restored.archived_at.is_none());
    assert_eq!(restored.updated_at, before);
}

#[test]
fn fork_copies_metadata_and_records_forked_from() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session("sess-meta-src", Some("Source title"))
        .unwrap();
    store
        .apply_prompt_metadata("sess-meta-src", "first ask", "first ask")
        .unwrap();
    store
        .update_session_title("sess-meta-src", Some("Custom source"))
        .unwrap();
    store
        .set_session_custom("sess-meta-src", &json!({ "source": "vscode" }))
        .unwrap();
    store
        .register_session_agent("sess-meta-src", "main", &json!({ "model": "m" }))
        .unwrap();
    store
        .save_turn(
            "sess-meta-src",
            "t1",
            1,
            &[LLMMessage::user("branch here"), LLMMessage::assistant("ok")],
            None,
            None,
        )
        .unwrap();

    assert!(
        store
            .fork_session_with_metadata(
                "sess-meta-src",
                "sess-meta-fork",
                None,
                None,
                Some(&json!({ "branch": "historical" })),
            )
            .unwrap()
    );
    let forked = store.get_session("sess-meta-fork").unwrap().unwrap();
    assert_eq!(forked.title.as_deref(), Some("Custom source"));
    assert_eq!(forked.normalized_title_kind(), Some("custom"));
    assert_eq!(forked.forked_from.as_deref(), Some("sess-meta-src"));
    assert_eq!(
        forked.last_prompt.as_deref(),
        Some("branch here"),
        "lastPrompt is derived from the retained turn"
    );
    let custom = forked.custom.as_ref().expect("custom is copied");
    assert_eq!(custom["source"], "vscode");
    assert_eq!(custom["branch"], "historical");
    assert_eq!(
        forked.agents.as_ref().expect("agents are copied")["main"]["model"],
        "m"
    );
}

#[test]
fn a_frozen_environment_round_trips_and_an_unbound_session_reports_none() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session_with_workspace("sess-env", Some("Env"), None)
        .unwrap();

    assert!(
        store
            .get_session_environment_snapshot("sess-env")
            .unwrap()
            .is_none(),
        "a row that never froze an environment is unbound, not an error"
    );

    let env =
        std::collections::BTreeMap::from([("KIMI_ENV_TEST".to_string(), "frozen".to_string())]);
    assert!(
        store
            .set_session_environment_snapshot("sess-env", "local", Some("/work/repo"), &env)
            .unwrap()
    );

    let frozen = store
        .get_session_environment_snapshot("sess-env")
        .unwrap()
        .expect("the binding is readable");
    assert_eq!(frozen.binding.environment_id, "local");
    assert_eq!(
        frozen.binding.cwd.as_deref(),
        Some(std::path::Path::new("/work/repo"))
    );
    assert_eq!(frozen.env, env);

    let row = store.get_session("sess-env").unwrap().unwrap();
    assert_eq!(row.environment_id.as_deref(), Some("local"));
    assert_eq!(row.environment_cwd.as_deref(), Some("/work/repo"));
}

#[test]
fn a_full_fork_copies_file_history_and_a_slice_keeps_none() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session_with_workspace("sess-src", Some("Src"), Some("wd_fork_fh"))
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
    store
        .save_turn(
            "sess-src",
            "turn-b",
            2,
            &[LLMMessage::user("u2")],
            None,
            None,
        )
        .unwrap();
    store
        .record_file_change("sess-src", 1, "a.txt", None, Some("a"))
        .unwrap();
    store
        .record_file_change("sess-src", 2, "b.txt", None, Some("b"))
        .unwrap();

    store
        .fork_session("sess-src", "sess-full", None, None)
        .unwrap();
    let (full_one, _) = store
        .get_file_history_changes("sess-full", Some(1))
        .unwrap();
    let (full_two, _) = store
        .get_file_history_changes("sess-full", Some(2))
        .unwrap();
    assert_eq!(full_one.len(), 1, "turn 1's history must reach the fork");
    assert_eq!(full_one[0].path, "a.txt");
    assert_eq!(full_two.len(), 1, "turn 2's history must reach the fork");
    assert_eq!(full_two[0].path, "b.txt");

    store
        .fork_session("sess-src", "sess-cut", None, Some(0))
        .unwrap();
    let (cut, _) = store.get_file_history_changes("sess-cut", None).unwrap();
    assert!(
        cut.is_empty(),
        "a sliced fork must not carry history for turns it does not have"
    );

    let (source, _) = store.get_file_history_changes("sess-src", None).unwrap();
    assert_eq!(source.len(), 2, "the source keeps its own history");
}

#[test]
fn deleting_a_session_reclaims_its_file_history_content() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session_with_workspace("sess-gone", Some("Gone"), Some("wd_del_fh"))
        .unwrap();
    store
        .save_turn(
            "sess-gone",
            "turn-a",
            1,
            &[LLMMessage::user("u")],
            None,
            None,
        )
        .unwrap();
    let big = "x".repeat(64 * 1024);
    store
        .record_file_change("sess-gone", 1, "big.bin", None, Some(&big))
        .unwrap();
    assert!(
        store
            .get_file_history_content("sess-gone", 1, "big.bin", "end")
            .unwrap()
            .is_some()
    );

    assert!(store.delete_session("sess-gone").unwrap());
    let (changes, _) = store.get_file_history_changes("sess-gone", None).unwrap();
    assert!(changes.is_empty());
    assert!(
        store
            .get_file_history_content("sess-gone", 1, "big.bin", "end")
            .unwrap()
            .is_none(),
        "the content must be gone with the session"
    );
    assert!(
        !store.delete_session("sess-gone").unwrap(),
        "a second delete reports it found nothing"
    );
}

#[test]
fn a_full_fork_carries_the_wire_journal_and_folds_to_the_same_history() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session_with_workspace("sess-wire-src", Some("Src"), Some("wd_wire_fork"))
        .unwrap();
    store
        .save_turn(
            "sess-wire-src",
            "turn-a",
            1,
            &[LLMMessage::user("hello"), LLMMessage::assistant("hi")],
            None,
            None,
        )
        .unwrap();
    for (id, event_type, content) in [
        ("evt_1", "message.user", "hello"),
        ("evt_2", "message.assistant", "hi"),
        ("evt_3", "message.user", "again"),
    ] {
        store
            .append_wire_event(&RawWireEvent {
                id: id.into(),
                session_id: "sess-wire-src".into(),
                event_type: event_type.into(),
                payload: json!({ "content": content }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 1_000,
            })
            .unwrap();
    }

    store
        .fork_session("sess-wire-src", "sess-wire-fork", None, None)
        .unwrap();

    let source_rows = store.get_wire_events("sess-wire-src", 0, 100).unwrap();
    let fork_rows = store.get_wire_events("sess-wire-fork", 0, 100).unwrap();
    assert_eq!(
        fork_rows.len(),
        source_rows.len(),
        "the full fork must copy every journal row"
    );
    assert_eq!(
        fork_rows
            .iter()
            .map(|r| (r.event_type.as_str(), r.payload["content"].as_str()))
            .collect::<Vec<_>>(),
        source_rows
            .iter()
            .map(|r| (r.event_type.as_str(), r.payload["content"].as_str()))
            .collect::<Vec<_>>(),
        "the copy keeps the source order"
    );

    let fork_seqs: Vec<u64> = fork_rows.iter().map(|r| r.seq).collect();
    assert!(
        fork_seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "the fork's seq numbers must be strictly increasing: {fork_seqs:?}"
    );

    assert!(
        fork_rows
            .iter()
            .all(|r| r.id.starts_with("sess-wire-fork:"))
    );

    let source_fold = store.fold_projection("sess-wire-src").unwrap();
    let fork_fold = store.fold_projection("sess-wire-fork").unwrap();
    assert_eq!(
        fork_fold, source_fold,
        "the fork's projection must equal the source's"
    );
    assert!(!fork_fold.is_empty(), "the fork must not open empty");
}

#[test]
fn a_sliced_fork_copies_only_the_wire_rows_before_the_cut() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session_with_workspace("sess-slice-src", Some("Src"), Some("wd_slice_fork"))
        .unwrap();
    for (id, number) in [("turn-a", 1u32), ("turn-b", 2)] {
        store
            .save_turn(
                "sess-slice-src",
                id,
                number,
                &[LLMMessage::user(id), LLMMessage::assistant("reply")],
                None,
                None,
            )
            .unwrap();
    }

    {
        let conn = store.conn.lock();
        for (turn_id, start, end) in [("turn-a", 1_000i64, 1_500i64), ("turn-b", 3_000, 3_500)] {
            conn.execute(
                "UPDATE turns SET started_at = ?2, completed_at = ?3 WHERE turn_id = ?1",
                params![turn_id, start, end],
            )
            .unwrap();
        }
    }
    for (id, content, created_at) in [
        ("evt_in_a", "in turn a", 1_100i64),
        ("evt_in_b", "in turn b", 3_100),
    ] {
        store
            .append_wire_event(&RawWireEvent {
                id: id.into(),
                session_id: "sess-slice-src".into(),
                event_type: "message.user".into(),
                payload: json!({ "content": content }),
                is_checkpoint: false,
                is_compaction: false,
                created_at,
            })
            .unwrap();
    }

    store
        .fork_session("sess-slice-src", "sess-slice-fork", None, Some(0))
        .unwrap();

    let rows = store.get_wire_events("sess-slice-fork", 0, 100).unwrap();
    assert_eq!(
        rows.len(),
        1,
        "only the retained turn's journal row is kept"
    );
    assert_eq!(rows[0].payload["content"], "in turn a");
    assert_eq!(store.list_turns("sess-slice-fork").unwrap().len(), 1);
}

#[test]
fn a_sliced_fork_cuts_by_wire_order_not_time() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session_with_workspace("sess-cut-src", Some("Src"), Some("wd_cut_fork"))
        .unwrap();
    for (id, number) in [("turn-a", 1u32), ("turn-b", 2)] {
        store
            .save_turn(
                "sess-cut-src",
                id,
                number,
                &[LLMMessage::user(id), LLMMessage::assistant("reply")],
                None,
                None,
            )
            .unwrap();
    }

    for (id, content) in [("evt_in_a", "in turn a"), ("evt_in_b", "in turn b")] {
        store
            .append_wire_event(&RawWireEvent {
                id: id.into(),
                session_id: "sess-cut-src".into(),
                event_type: "message.user".into(),
                payload: json!({ "content": content }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 3_000,
            })
            .unwrap();
    }

    store
        .fork_session("sess-cut-src", "sess-cut-fork", None, Some(0))
        .unwrap();

    let rows = store.get_wire_events("sess-cut-fork", 0, 100).unwrap();
    assert_eq!(rows.len(), 1, "the cut is the second user prompt");
    assert_eq!(rows[0].payload["content"], "in turn a");
}

#[test]
fn a_sliced_fork_counts_turn_prompt_anchors() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session_with_workspace("sess-prompt-src", Some("Src"), Some("wd_prompt_fork"))
        .unwrap();
    for (id, number) in [("turn-a", 1u32), ("turn-b", 2)] {
        store
            .save_turn(
                "sess-prompt-src",
                id,
                number,
                &[LLMMessage::user(id)],
                None,
                None,
            )
            .unwrap();
    }
    for (id, event_type, payload) in [
        (
            "evt-prompt-a",
            "turn.prompt",
            json!({"origin": {"kind": "user"}}),
        ),
        ("evt-step-a", "step.begin", json!({})),
        (
            "evt-prompt-bg",
            "turn.prompt",
            json!({"origin": {"kind": "task"}}),
        ),
        (
            "evt-prompt-b",
            "turn.prompt",
            json!({"origin": {"kind": "user"}}),
        ),
    ] {
        store
            .append_wire_event(&RawWireEvent {
                id: id.into(),
                session_id: "sess-prompt-src".into(),
                event_type: event_type.into(),
                payload,
                is_checkpoint: false,
                is_compaction: false,
                created_at: 1_000,
            })
            .unwrap();
    }

    store
        .fork_session("sess-prompt-src", "sess-prompt-fork", None, Some(0))
        .unwrap();

    let rows = store.get_wire_events("sess-prompt-fork", 0, 100).unwrap();
    assert_eq!(
        rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        [
            "sess-prompt-fork:evt-prompt-a",
            "sess-prompt-fork:evt-step-a",
            "sess-prompt-fork:evt-prompt-bg",
        ],
        "all rows before the dropped turn's anchor are kept, background prompts excluded"
    );
}

#[test]
fn a_failed_wire_copy_leaves_no_fork_session() {
    let store = SqliteSessionStore::in_memory().unwrap();
    store
        .create_session_with_workspace("sess-atomic-src", Some("Src"), Some("wd_atomic"))
        .unwrap();
    for (id, number) in [("turn-a", 1u32), ("turn-b", 2)] {
        store
            .save_turn(
                "sess-atomic-src",
                id,
                number,
                &[LLMMessage::user(id)],
                None,
                None,
            )
            .unwrap();
    }
    store
        .append_wire_event(&RawWireEvent {
            id: "evt-atomic".into(),
            session_id: "sess-atomic-src".into(),
            event_type: "message.user".into(),
            payload: json!({ "content": "hello" }),
            is_checkpoint: false,
            is_compaction: false,
            created_at: 1_000,
        })
        .unwrap();
    {
        let conn = store.conn.lock();
        conn.execute_batch(
            "CREATE TRIGGER refuse_fork_wire BEFORE INSERT ON wire_events
                 WHEN NEW.session_id = 'sess-atomic-fork'
             BEGIN SELECT RAISE(ABORT, 'fork copy refused'); END;",
        )
        .unwrap();
    }

    let error = store
        .fork_session("sess-atomic-src", "sess-atomic-fork", None, None)
        .unwrap_err();
    assert!(matches!(error, ForkError::Store(_)), "{error:?}");
    assert!(
        store.get_session("sess-atomic-fork").unwrap().is_none(),
        "a failed copy must not leave the empty child behind"
    );
    assert!(
        store.list_turns("sess-atomic-fork").unwrap().is_empty(),
        "nor its turns"
    );
}

#[test]
fn blob_usage_reports_stored_bytes_and_unreferenced_blobs() {
    use base64::Engine as _;

    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-blob-audit", None).unwrap();
    let inline = base64::engine::general_purpose::STANDARD.encode(vec![3u8; 8_192]);
    store
        .append_wire_event(&RawWireEvent {
            id: "evt_media".into(),
            session_id: "sess-blob-audit".into(),
            event_type: "message.user".into(),
            payload: json!({
                "content": "look",
                "blocks": [{
                    "type": "image_url",
                    "image_url": { "url": format!("data:image/png;base64,{inline}") }
                }]
            }),
            is_checkpoint: false,
            is_compaction: false,
            created_at: 1,
        })
        .unwrap();

    {
        let conn = store.conn.lock();
        conn.execute(
            "INSERT INTO wire_blobs (hash, mime, bytes) VALUES ('orphan', 'image/png', x'00')",
            [],
        )
        .unwrap();
    }

    let usage = store.blob_usage().unwrap();
    assert_eq!(usage.stored, 2);
    assert_eq!(usage.unreferenced, 1, "the orphan is the unreferenced one");
    assert_eq!(usage.scanned, 2);
    assert!(!usage.truncated);
    assert_eq!(
        usage.bytes,
        8_192 + 1,
        "the audit sums the stored payload bytes"
    );
}

#[test]
fn deleting_or_archiving_a_session_refuses_a_live_foreign_holder() {
    let store = SqliteSessionStore::in_memory().unwrap();
    let session = "sess-held-by-other";
    store.create_session(session, None).unwrap();

    let lock_path = crate::session::lock::SESSION_LOCKS
        .lock_dir()
        .join(format!("{session}.lock"));
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    let holder = json!({
        "version": 1,
        "token": "999999999:other",
        "pid": 999_999_999,
        "hostname": "other-host",
        "startedAt": chrono::Utc::now().timestamp_millis(),
        "clientIdentity": "kimi_code",
        "clientDisplayName": null,
    });
    std::fs::write(&lock_path, holder.to_string()).unwrap();

    let delete_error = store.delete_session(session).unwrap_err();
    assert!(
        matches!(delete_error, SessionRemovalError::Occupied { .. }),
        "delete must refuse: {delete_error:?}"
    );
    assert!(
        store.get_session(session).unwrap().is_some(),
        "the refused delete must not have removed the row"
    );
    let archive_error = store.archive_session(session).unwrap_err();
    assert!(
        matches!(archive_error, SessionRemovalError::Occupied { .. }),
        "archive must refuse: {archive_error:?}"
    );

    std::fs::remove_file(&lock_path).unwrap();
    assert!(store.archive_session(session).unwrap());
    assert!(store.delete_session(session).unwrap());
}
