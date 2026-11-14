use super::super::{is_unknown_tool_refusal, provider_finish_reason, step_event_id};

#[test]
fn provider_finish_reason_normalizes_known_vocabularies() {
    assert_eq!(provider_finish_reason(Some("stop")), Some("completed"));
    assert_eq!(provider_finish_reason(Some("end_turn")), Some("completed"));
    assert_eq!(
        provider_finish_reason(Some("stop_sequence")),
        Some("completed")
    );
    assert_eq!(provider_finish_reason(Some("completed")), Some("completed"));
    assert_eq!(provider_finish_reason(Some("STOP")), Some("completed"));

    assert_eq!(
        provider_finish_reason(Some("tool_calls")),
        Some("tool_calls")
    );
    assert_eq!(
        provider_finish_reason(Some("function_call")),
        Some("tool_calls")
    );
    assert_eq!(provider_finish_reason(Some("tool_use")), Some("tool_calls"));

    assert_eq!(provider_finish_reason(Some("length")), Some("truncated"));
    assert_eq!(
        provider_finish_reason(Some("max_tokens")),
        Some("truncated")
    );
    assert_eq!(
        provider_finish_reason(Some("MAX_TOKENS")),
        Some("truncated")
    );

    assert_eq!(
        provider_finish_reason(Some("content_filter")),
        Some("filtered")
    );
    assert_eq!(provider_finish_reason(Some("SAFETY")), Some("filtered"));

    assert_eq!(provider_finish_reason(Some("pause_turn")), Some("paused"));
    assert_eq!(provider_finish_reason(Some("paused")), Some("paused"));

    assert_eq!(provider_finish_reason(Some("something_new")), Some("other"));

    assert_eq!(provider_finish_reason(None), None);
}

#[test]
fn step_event_id_is_stable_per_step_and_distinct_across_steps() {
    assert_eq!(
        step_event_id("turn-a", 1),
        step_event_id("turn-a", 1),
        "begin / interrupted / end must agree"
    );
    assert_ne!(step_event_id("turn-a", 1), step_event_id("turn-a", 2));
    assert_ne!(step_event_id("turn-a", 1), step_event_id("turn-b", 1));
}

#[test]
fn unknown_tool_refusal_matches_only_the_canned_shape() {
    assert!(is_unknown_tool_refusal(
        "unknown tool: mcp__late__search",
        true,
        "mcp__late__search"
    ));

    assert!(
        !is_unknown_tool_refusal("unknown tool: other", true, "mcp__late__search"),
        "a different name is a different answer"
    );

    assert!(
        !is_unknown_tool_refusal(
            "unknown tool: mcp__late__search",
            false,
            "mcp__late__search"
        ),
        "a successful result is never the canned error"
    );

    assert!(
        !is_unknown_tool_refusal(
            "Error: unknown tool: mcp__late__search",
            true,
            "mcp__late__search"
        ),
        "a tool's own prose must not be mistaken for the canned answer"
    );
}
