//! Integration test: a recorded engine event stream becomes terminal rows.
//!
//! This is the vertical slice design `reports/design/RUST-UI-DECOUPLING.md` §4
//! describes, minus the terminal: recorded events go in, the projection turns
//! them into [`EntryOp`]s, the headless backend applies them, and the result is
//! laid out into rows a real backend would print. Everything the slice's
//! acceptance criteria name as *testable* lives here:
//!
//! - the op sequence a turn produces (prompt → streamed assistant text → a tool
//!   call → the answer);
//! - the final rendered text, wrapped to a terminal width;
//! - that the projection refuses nothing and records no contract violation on
//!   a well-formed stream.
//!
//! It lives in `tests/` rather than beside the projector because it is the
//! only place both halves of the seam are exercised together: the projector is
//! engine-side, the backend is contract-side, and a unit test in either crate
//! can only see its own half.
//!
//! The events are JSON, on purpose. That is the shape they arrive in from the
//! engine — over napi, over WS, or replayed from a recorded transcript — so a
//! test that constructs the enum directly would skip the deserialization the
//! product depends on.

#![cfg(feature = "cli")]

use kimi_agent::events::{EngineEvent, Projector};
use kimi_ui_core::CoalescingPolicy;
use kimi_ui_core::headless::HeadlessBackend;

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

/// Drive the whole chain and return the painted rows.
fn paint(width: usize) -> Vec<String> {
    let mut projector = Projector::new(CoalescingPolicy::default());
    let mut backend = HeadlessBackend::new();
    for raw in recorded_turn() {
        let event = EngineEvent::from_json(serde_json::from_str(raw).expect("fixture is JSON"));
        // A recorded stream must never fall through to the opaque `Custom`
        // variant: that would mean the wire shape and this crate disagree.
        assert!(
            !matches!(event, EngineEvent::Custom(_)),
            "event did not deserialize into a typed variant: {raw}"
        );
        backend.apply_all(projector.project(event));
    }
    backend.paint(width)
}

/// The op sequence is the slice's contract: a prompt entry, one streamed
/// assistant entry, one tool entry, and no contract violations.
#[test]
fn a_recorded_turn_projects_to_the_expected_op_sequence() {
    let mut projector = Projector::new(CoalescingPolicy::default());
    let mut backend = HeadlessBackend::new();
    for raw in recorded_turn() {
        let event = EngineEvent::from_json(serde_json::from_str(raw).expect("fixture is JSON"));
        backend.apply_all(projector.project(event));
    }

    let store = backend.store();
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

/// The painted rows are what a user would see: a gutter per entry kind, the
/// assistant text reassembled from its segments, and the tool result under its
/// call.
#[test]
fn the_painted_rows_are_what_a_terminal_would_show() {
    let rows = paint(60);
    let joined = rows.join("\n");

    assert!(
        rows.iter().any(|r| r.starts_with(">| ")),
        "user prompt gutter: {rows:?}"
    );
    assert!(
        joined.contains("explain the build"),
        "prompt text is present: {joined}"
    );
    // Three streamed segments became one line of prose, not three fragments.
    assert!(
        joined.contains("The build is two steps."),
        "streamed segments concatenate: {joined}"
    );
    assert!(
        rows.iter().any(|r| r.starts_with("$| ")),
        "tool gutter: {rows:?}"
    );
    assert!(joined.contains("Read"), "tool label is present: {joined}");
    assert!(joined.contains("make"), "tool output is present: {joined}");
}

/// A narrow terminal wraps the same content instead of truncating it, and no
/// row exceeds the width.
#[test]
fn narrowing_the_terminal_wraps_the_same_content() {
    let wide = paint(120);
    let narrow = paint(24);

    assert!(narrow.len() > wide.len(), "narrow layout needs more rows");
    for row in &narrow {
        assert!(
            kimi_ui_core::width(row) <= 24,
            "row exceeds the terminal width: {:?} ({} cells)",
            row,
            kimi_ui_core::width(row)
        );
    }
    // Nothing was lost: the content survives the re-wrap.
    let wide_text: String = wide.join("");
    let narrow_text: String = narrow.join("").replace(' ', "");
    for needle in ["Thebuildistwosteps.", "explainthebuild"] {
        assert!(
            wide_text.replace(' ', "").contains(needle),
            "wide render lost {needle}"
        );
        assert!(
            narrow_text.contains(needle),
            "narrow render lost {needle}: {narrow:?}"
        );
    }
}

/// A failed native tool call is distinguishable from a successful one in the
/// entry stream: `EntryOp` has no error arm, so the transcript projection's
/// structural `frame.state = Error` has to be said in text here.
#[test]
fn a_failed_native_tool_call_says_so() {
    let mut projector = Projector::new(CoalescingPolicy::default());
    let mut backend = HeadlessBackend::new();
    let events = [
        r#"{"type":"turn.started","agent_id":"main","turn_id":"t1","prompt":"run it"}"#,
        r#"{"type":"tool.call.started","agent_id":"main","turn_id":"t1","tool_call_id":"c1","name":"Bash","args":{}}"#,
        r#"{"type":"tool.native","agent_id":"main","turn_id":"t1","tool_call_id":"c1","tool_name":"Bash","arguments":{},"content":"exit 1","is_error":true}"#,
    ];
    for raw in events {
        let event = EngineEvent::from_json(serde_json::from_str(raw).expect("fixture is JSON"));
        backend.apply_all(projector.project(event));
    }
    let joined = backend.paint(60).join(
        "
",
    );
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
    let mut projector = Projector::new(CoalescingPolicy::default());
    let mut backend = HeadlessBackend::new();
    let events = [
        r#"{"type":"turn.started","agent_id":"main","turn_id":"t1","prompt":"dig here"}"#,
        r#"{"type":"subagent.spawned","subagent_id":"s1","subagent_name":"explorer","run_in_background":true}"#,
        r#"{"type":"subagent.completed","subagent_id":"s1","result_summary":"found the answer"}"#,
        r#"{"type":"turn.ended","agent_id":"main","turn_id":"t1","reason":"end_turn"}"#,
    ];
    for raw in events {
        let event = EngineEvent::from_json(serde_json::from_str(raw).expect("fixture is JSON"));
        assert!(!matches!(event, EngineEvent::Custom(_)), "untyped: {raw}");
        backend.apply_all(projector.project(event));
    }

    // The member is one entry (plus the prompt): spawn and completion share it.
    let rows = backend.paint(60);
    let joined = rows.join(
        "
",
    );
    assert!(
        joined.contains("explorer"),
        "the member's name is shown: {joined}"
    );
    assert!(
        joined.contains("found the answer"),
        "its result replaces the spawn line rather than stacking: {joined}"
    );
    assert_eq!(
        rows.iter().filter(|r| r.contains("[subagent]")).count(),
        1,
        "one card, not one per lifecycle event: {rows:?}"
    );
}

/// A terminal event for a member that never announced itself is dropped: a
/// result with no card would appear with no context for what produced it.
#[test]
fn a_subagent_result_without_a_spawn_is_dropped() {
    let mut projector = Projector::new(CoalescingPolicy::default());
    let mut backend = HeadlessBackend::new();
    let events = [
        r#"{"type":"turn.started","agent_id":"main","turn_id":"t1","prompt":"x"}"#,
        r#"{"type":"subagent.completed","subagent_id":"ghost","result_summary":"orphan"}"#,
    ];
    for raw in events {
        let event = EngineEvent::from_json(serde_json::from_str(raw).expect("fixture is JSON"));
        backend.apply_all(projector.project(event));
    }
    let joined = backend.paint(60).join(
        "
",
    );
    assert!(!joined.contains("orphan"), "no card, no result: {joined}");
    assert!(
        backend.store().violations().is_empty(),
        "{:?}",
        backend.store().violations()
    );
}

/// A cancelled turn leaves what was already streamed on screen. The slice's
/// acceptance criterion is that `turn.ended` does not erase output, and the
/// mechanism is that a partial entry is never removed — only sealed.
#[test]
fn a_cancelled_turn_keeps_the_partial_output() {
    let mut projector = Projector::new(CoalescingPolicy::default());
    let mut backend = HeadlessBackend::new();
    let events = [
        r#"{"type":"turn.started","agent_id":"main","turn_id":"t1","prompt":"do it"}"#,
        r#"{"type":"stream.segment","agent_id":"main","turn_id":"t1","stream":{"stream":"assistant"},"text":"partial ans","span":{"first":0,"last":0},"is_final":false}"#,
        r#"{"type":"turn.ended","agent_id":"main","turn_id":"t1","reason":"cancelled"}"#,
    ];
    for raw in events {
        let event = EngineEvent::from_json(serde_json::from_str(raw).expect("fixture is JSON"));
        backend.apply_all(projector.project(event));
    }

    let rows = backend.paint(40);
    let joined = rows.join("\n");
    assert!(
        joined.contains("partial ans"),
        "partial output survives: {joined}"
    );
    assert_eq!(backend.store().len(), 2, "prompt and partial answer");
}
