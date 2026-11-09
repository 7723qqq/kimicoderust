use super::super::{is_unknown_tool_refusal, provider_finish_reason, step_event_id};

/// v2 `ProviderFinishReason` (`human/llm/finish-reason.ts:1-6`) is the
/// normalized enum the protocol's step event carries next to the provider's own
/// spelling. Every provider vocabulary in the reference must land on the same
/// value: Anthropic says `end_turn`, OpenAI says `stop`, Google says `STOP`,
/// and a gateway may invent another.
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

/// v2 stamps one `stepId` on both `turn.step.started` and
/// `turn.step.completed` (loopService.ts:1393-1424). The derived id must be
/// stable across the boundaries of the same step and distinct across steps.
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

/// v2 `isCannedUnknownToolResult` (`loopService.ts:1818-1834`) is narrow on
/// purpose: only an error whose whole content is exactly `unknown tool: <name>`
/// is deferred and retried. A tool that reported *about* an unknown tool is a
/// normal result and must not be re-run.
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
