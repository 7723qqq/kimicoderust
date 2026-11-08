//! Integration test: a recorded engine event stream becomes the UI's entries.
//!
//! This is the engine-side half of the vertical slice in
//! `reports/design/RUST-UI-DECOUPLING.md` §4: recorded wire events go in, the
//! projection turns them into [`EntryOp`]s, and the store applies them. What
//! this half owns:
//!
//! - the op sequence a turn produces (prompt → streamed assistant text → a tool
//!   call → the answer);
//! - the text each entry carries, which is what a terminal will later paint;
//! - that the projection refuses nothing and records no contract violation on
//!   a well-formed stream.
//!
//! Painting is the other half and is pinned where the renderer lives
//! (`kimi-tui-app/tests/render_slice.rs`, on textual-rs's test terminal): gutters
//! and wrapping are decided by the row policy, not by the projector, and a test
//! in this crate can only see its own side of that seam.
//!
//! The events are JSON, on purpose. That is the shape they arrive in from the
//! engine — over napi, over WS, or replayed from a recorded transcript — so a
//! test that constructs the enum directly would skip the deserialization the
//! product depends on.

#![cfg(feature = "cli")]

use kimi_agent::events::{EngineEvent, Projector};
use kimi_ui_core::{CoalescingPolicy, EntryStore};

/// One recorded turn, in the wire shape the engine emits.
fn recorded_turn() -> Vec<&'static str> {
    vec![
        // The turn opens with the user's prompt.
        r#"{"type":"turn.started","agent_id":"main","turn_id":"t1","prompt":"explain the build"}"#,
        // Assistant text arrives as merged segments (design §8.4): one event
        // carries a coalesced span, and `is_final` marks the tail. A stream
        // that ends without a final segment is the tail-loss case the seal
        // check exists for, so a well-formed fixture carries one.
        r#"{"type":"stream.segment","agent_id":"main","turn_id":"t1","stream":{"stream":"assistant"},"text":"The build ","span":{"first":0,"last":0},"is_final":false}"#,
        r#"{"type":"stream.segment","agent_id":"main","turn_id":"t1","stream":{"stream":"assistant"},"text":"is two ","span":{"first":1,"last":1},"is_final":false}"#,
        r#"{"type":"stream.segment","agent_id":"main","turn_id":"t1","stream":{"stream":"assistant"},"text":"steps.","span":{"first":2,"last":2},"is_final":true}"#,
        // A tool call opens and completes; `result` is the wire field.
        r#"{"type":"tool.call.started","agent_id":"main","turn_id":"t1","tool_call_id":"c1","name":"Read","args":{"path":"build.sh"}}"#,
        "{\"type\":\"tool.call.completed\",\"agent_id\":\"main\",\"turn_id\":\"t1\",\"tool_call_id\":\"c1\",\"result\":{\"content\":\"make ok\"}}",
        // The turn ends.
        r#"{"type":"turn.ended","agent_id":"main","turn_id":"t1","reason":"end_turn"}"#,
    ]
}

/// Drive the projector over `events` and apply every op it emits.
fn project(events: &[&str]) -> EntryStore {
    let mut projector = Projector::new(CoalescingPolicy::default());
    let mut store = EntryStore::new();
    for raw in events {
        let event = EngineEvent::from_json(serde_json::from_str(raw).expect("fixture is JSON"));
        // A recorded stream must never fall through to the opaque `Custom`
        // variant: that would mean the wire shape and this crate disagree.
        assert!(
            !matches!(event, EngineEvent::Custom(_)),
            "event did not deserialize into a typed variant: {raw}"
        );
        for op in projector.project(event) {
            store.apply(op);
        }
    }
    store
}

/// Every entry's text, in display order — the content a terminal paints.
fn text(store: &EntryStore) -> String {
    store
        .entries()
        .map(|(_, content)| content.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The op sequence is the slice's contract: a prompt entry, one streamed
/// assistant entry, one tool entry, and no contract violations.
#[test]
fn a_recorded_turn_projects_to_the_expected_op_sequence() {
    let store = project(&recorded_turn());

    assert!(
        store.violations().is_empty(),
        "a well-formed stream must not violate the projection contract: {:?}",
        store.violations()
    );
    assert_eq!(store.len(), 3, "prompt, assistant text, and the tool call");

    let kinds: Vec<&str> = store
        .entries()
        .map(|(id, _)| match id.kind {
            kimi_ui_core::EntryKind::UserMsg => "user",
            kimi_ui_core::EntryKind::AssistantMsg => "assistant",
            kimi_ui_core::EntryKind::ToolCall => "tool",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, vec!["user", "assistant", "tool"]);
}

/// The entries carry what a user reads: the prompt, the assistant text
/// reassembled from its segments, and the tool result under its call.
#[test]
fn the_projected_entries_carry_what_a_terminal_would_show() {
    let store = project(&recorded_turn());
    let joined = text(&store);

    assert!(
        joined.contains("explain the build"),
        "prompt text is present: {joined}"
    );
    // Three streamed segments became one line of prose, not three fragments.
    assert!(
        joined.contains("The build is two steps."),
        "streamed segments concatenate: {joined}"
    );
    assert!(joined.contains("Read"), "tool label is present: {joined}");
    assert!(
        joined.contains("make ok"),
        "tool output is present: {joined}"
    );
}

/// A failed native tool call is distinguishable from a successful one in the
/// entry stream: `EntryOp` has no error arm, so the transcript projection's
/// structural `frame.state = Error` has to be said in text here.
#[test]
fn a_failed_native_tool_call_says_so() {
    let store = project(&[
        r#"{"type":"turn.started","agent_id":"main","turn_id":"t1","prompt":"run it"}"#,
        r#"{"type":"tool.call.started","agent_id":"main","turn_id":"t1","tool_call_id":"c1","name":"Bash","args":{}}"#,
        r#"{"type":"tool.native","agent_id":"main","turn_id":"t1","tool_call_id":"c1","tool_name":"Bash","arguments":{},"content":"exit 1","is_error":true}"#,
    ]);
    let joined = text(&store);
    assert!(
        joined.contains("error: exit 1"),
        "the failure is stated, not silent: {joined}"
    );
}

/// A subagent is an entry of its own, not text folded into the parent's
/// paragraph: its spawn creates a card, and its completion settles that same
/// card rather than appending a second one.
#[test]
fn a_subagent_gets_its_own_settled_card() {
    let store = project(&[
        r#"{"type":"turn.started","agent_id":"main","turn_id":"t1","prompt":"dig here"}"#,
        r#"{"type":"subagent.spawned","subagent_id":"s1","subagent_name":"explorer","run_in_background":true}"#,
        r#"{"type":"subagent.completed","subagent_id":"s1","result_summary":"found the answer"}"#,
        r#"{"type":"turn.ended","agent_id":"main","turn_id":"t1","reason":"end_turn"}"#,
    ]);

    // The member is one entry (plus the prompt): spawn and completion share it.
    let joined = text(&store);
    assert!(
        joined.contains("explorer"),
        "the member's name is shown: {joined}"
    );
    assert!(
        joined.contains("found the answer"),
        "its result replaces the spawn line rather than stacking: {joined}"
    );
    assert_eq!(
        store
            .entries()
            .filter(|(_, content)| content.text.contains("[subagent]"))
            .count(),
        1,
        "one card, not one per lifecycle event: {joined}"
    );
}

/// A terminal event for a member that never announced itself is dropped: a
/// result with no card would appear with no context for what produced it.
#[test]
fn a_subagent_result_without_a_spawn_is_dropped() {
    let store = project(&[
        r#"{"type":"turn.started","agent_id":"main","turn_id":"t1","prompt":"x"}"#,
        r#"{"type":"subagent.completed","subagent_id":"ghost","result_summary":"orphan"}"#,
    ]);
    assert!(
        !text(&store).contains("orphan"),
        "no card, no result: {}",
        text(&store)
    );
    assert!(store.violations().is_empty(), "{:?}", store.violations());
}

/// A cancelled turn leaves what was already streamed in the entry table. The
/// slice's acceptance criterion is that `turn.ended` does not erase output, and
/// the mechanism is that a partial entry is never removed — only sealed.
#[test]
fn a_cancelled_turn_keeps_the_partial_output() {
    let store = project(&[
        r#"{"type":"turn.started","agent_id":"main","turn_id":"t1","prompt":"do it"}"#,
        r#"{"type":"stream.segment","agent_id":"main","turn_id":"t1","stream":{"stream":"assistant"},"text":"partial ans","span":{"first":0,"last":0},"is_final":false}"#,
        r#"{"type":"turn.ended","agent_id":"main","turn_id":"t1","reason":"cancelled"}"#,
    ]);

    assert!(
        text(&store).contains("partial ans"),
        "partial output survives: {}",
        text(&store)
    );
    assert_eq!(store.len(), 2, "prompt and partial answer");
}

/// v2 #4663: `turn.step.stream_reset` seals the partial stream. A later
/// delta starts a fresh entry instead of appending to text the provider
/// discarded.
#[test]
fn a_stream_reset_seals_the_dropped_partial_before_new_text() {
    let store = project(&[
        r#"{"type":"turn.started","agent_id":"main","turn_id":"t1","prompt":"go"}"#,
        r#"{"type":"assistant.delta","agent_id":"main","turn_id":"t1","delta":"discarded","block":0}"#,
        r#"{"type":"turn.step.stream_reset","agent_id":"main","turn_id":"t1","step":1,"step_id":"s1"}"#,
        r#"{"type":"assistant.delta","agent_id":"main","turn_id":"t1","delta":"fresh","block":0}"#,
        r#"{"type":"turn.ended","agent_id":"main","turn_id":"t1","reason":"end_turn"}"#,
    ]);
    let rendered = text(&store);
    assert!(
        rendered.contains("discarded"),
        "the dropped partial was shown: {rendered}"
    );
    assert!(
        rendered.contains("fresh"),
        "the fresh delta was lost: {rendered}"
    );
    assert!(
        !rendered.contains("discardedfresh"),
        "the reset did not seal the old entry: {rendered}"
    );
    assert_eq!(store.len(), 3, "prompt, dropped partial, fresh answer");
}

/// A subagent's stream is its own entry, and the owner rides the entry.
///
/// Both deltas are `assistant` text on the same turn; without the agent key they
/// would coalesce into one paragraph and the reader could not tell which member
/// said what (v2 announces the agent on every event, and `EntryContent.agent_id`
/// exists for exactly this).
#[test]
fn a_subagent_stream_does_not_fold_into_the_parent_answer() {
    let store = project(&[
        r#"{"type":"turn.started","agent_id":"main","turn_id":"t2","prompt":"run two members"}"#,
        r#"{"type":"llm.delta","agent_id":"main","turn_id":"t2","step":1,"part":{"type":"text","text":"the parent says "}}"#,
        r#"{"type":"llm.delta","agent_id":"sub-1","turn_id":"t2","step":1,"part":{"type":"text","text":"the member says"}}"#,
        r#"{"type":"llm.delta","agent_id":"main","turn_id":"t2","step":1,"part":{"type":"text","text":"done"}}"#,
        r#"{"type":"turn.ended","agent_id":"main","turn_id":"t2","reason":"end_turn"}"#,
    ]);

    let answers: Vec<(&str, Option<&str>)> = store
        .entries()
        .filter(|(_, content)| content.kind == kimi_ui_core::EntryKind::AssistantMsg)
        .map(|(_, content)| (content.text.as_str(), content.agent_id.as_deref()))
        .collect();
    assert_eq!(
        answers,
        vec![
            ("the parent says done", None),
            ("the member says", Some("sub-1")),
        ],
        "the parent's fragments continue its own entry; the member's answer is a separate, attributed entry"
    );
    assert_eq!(
        store.violations(),
        &[],
        "both streams keep their own seq continuity"
    );
}

/// v2 #4627: a steer row the host injects goes through the projector's
/// allocator, so it lands between the text streamed before it and the answer
/// that follows. The post-steer answer is a new entry, not a continuation: the
/// model received the steer as input, so its next message begins after it.
#[test]
fn a_steer_row_sits_between_the_text_before_it_and_the_answer_after_it() {
    fn feed(projector: &mut Projector, store: &mut EntryStore, raw: &str) {
        let event = EngineEvent::from_json(serde_json::from_str(raw).expect("fixture is JSON"));
        for op in projector.project(event) {
            store.apply(op);
        }
    }

    let mut projector = Projector::new(CoalescingPolicy::default());
    let mut store = EntryStore::new();
    feed(
        &mut projector,
        &mut store,
        r#"{"type":"turn.started","agent_id":"main","turn_id":"t1","prompt":"start"}"#,
    );
    feed(
        &mut projector,
        &mut store,
        r#"{"type":"assistant.delta","agent_id":"main","turn_id":"t1","delta":"before"}"#,
    );

    let (_id, ops) = projector
        .note_user_entry("t1", "steer me")
        .expect("the turn is open");
    for op in ops {
        store.apply(op);
    }

    feed(
        &mut projector,
        &mut store,
        r#"{"type":"assistant.delta","agent_id":"main","turn_id":"t1","delta":"after"}"#,
    );
    feed(
        &mut projector,
        &mut store,
        r#"{"type":"turn.ended","agent_id":"main","turn_id":"t1","reason":"end_turn"}"#,
    );

    assert_eq!(text(&store), "start\nbefore\nsteer me\nafter");
    assert_eq!(store.violations(), &[]);
}

/// A real transport flushes a step's text with `is_final = false` at every
/// `llm.step.end`; the turn-end seal must close that stream with a final
/// marker anyway. Without it, every real turn recorded `SealWithoutFinal` —
/// found by the real-transport steer test in `kimi-tui-host`.
#[test]
fn a_step_flush_before_turn_end_does_not_record_a_tail_loss() {
    let mut projector = Projector::new(CoalescingPolicy::default());
    let mut store = EntryStore::new();
    for raw in [
        r#"{"type":"turn.started","agent_id":"main","turn_id":"t1","prompt":"q"}"#,
        r#"{"type":"assistant.delta","agent_id":"main","turn_id":"t1","delta":"the answer"}"#,
        r#"{"type":"llm.step.end","agent_id":"main","turn_id":"t1","step":0,"usage":null}"#,
        r#"{"type":"turn.ended","agent_id":"main","turn_id":"t1","reason":"end_turn"}"#,
    ] {
        let event = EngineEvent::from_json(serde_json::from_str(raw).expect("fixture is JSON"));
        assert!(
            !matches!(event, EngineEvent::Custom(_)),
            "event did not deserialize: {raw}"
        );
        for op in projector.project(event) {
            store.apply(op);
        }
    }
    assert_eq!(
        store.violations(),
        &[],
        "the step-end flush must not read as a lost tail"
    );
    assert_eq!(
        text(&store),
        "q
the answer"
    );
}
