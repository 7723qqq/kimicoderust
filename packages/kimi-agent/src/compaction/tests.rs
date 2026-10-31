use super::*;
use crate::rpc::types::BoxFuture;
use crate::rpc::types::TokenUsage;
use crate::turn_loop::types::{ContentBlock, LLM, LLMChatParams, LLMChatResponse, ToolCall};

fn msg(role: &str, content: &str) -> LLMMessage {
    LLMMessage {
        role: role.into(),
        content: content.into(),
        ..Default::default()
    }
}

fn tool_call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        extras: None,
    }
}

fn small_config(max_context_tokens: u32) -> CompactionConfig {
    CompactionConfig {
        max_context_tokens,
        ..Default::default()
    }
}

/// The pre-shrink is what keeps a small-window model from burning its
/// whole retry budget: without it the summarizer's first request carries the
/// entire omitted history, overflows, and only then starts dropping one
/// message per attempt (v2 #3911).
#[test]
fn pre_shrink_keeps_the_newest_messages_that_fit_the_window() {
    // 4000 chars ≈ 1000 tokens per message under the module's 4:1 estimate,
    // so the ten-message history below is ≈ 10 000 tokens.
    let history: Vec<LLMMessage> = (0..10)
        .map(|index| msg("user", &format!("{index}").repeat(4000)))
        .collect();

    // A generous window: everything fits, so nothing is shrunk.
    assert!(
        pre_shrink_to_window_budget(&history, None, Some(1_000_000)).is_none(),
        "a history that fits must be sent whole"
    );
    // No window known: no shrink either.
    assert!(pre_shrink_to_window_budget(&history, None, None).is_none());
    // 20 000 leaves a ≈ 13 875 budget, which still takes all ten.
    assert!(pre_shrink_to_window_budget(&history, None, Some(20_000)).is_none());

    // 12 000 leaves a ≈ 7 925 budget — about seven of the ten messages.
    let shrunken =
        pre_shrink_to_window_budget(&history, None, Some(12_000)).expect("a tight window shrinks");
    assert!(
        !shrunken.is_empty() && shrunken.len() < history.len(),
        "expected a proper subset, got {} of {}",
        shrunken.len(),
        history.len()
    );
    // The tail is what survives — the shrink must not drop the newest turn.
    assert_eq!(
        shrunken.last().map(|message| message.content.clone()),
        history.last().map(|message| message.content.clone())
    );
}

/// A shrunk history must not start with a tool result: a result without its
/// call is not a sequence a provider accepts (v2 `dropLeadingToolResults`).
#[test]
fn pre_shrink_drops_leading_tool_results() {
    // Four ≈ 1000-token messages against a ≈ 7 925 budget: everything fits,
    // so force the shrink by growing the history instead.
    let mut history = vec![
        msg("assistant", &"a".repeat(4000)),
        msg("tool", &"b".repeat(4000)),
        msg("user", &"c".repeat(4000)),
        msg("user", &"d".repeat(4000)),
    ];
    // Push the total past the budget so the shrink runs and the surviving
    // slice is forced to begin at the `tool` entry.
    for index in 0..8 {
        history.insert(0, msg("user", &format!("filler{index}").repeat(2000)));
    }
    let shrunken = pre_shrink_to_window_budget(&history, None, Some(12_000)).expect("shrinks");
    assert_ne!(
        shrunken.first().map(|message| message.role.as_str()),
        Some("tool"),
        "the shrink must not leave a dangling tool result at the front"
    );
}

/// v2's helper returns an empty slice when nothing fits, and the caller
/// reads that as "send what you had" — never an empty prompt, which would
/// ask the model to summarize nothing.
#[test]
fn pre_shrink_falls_back_to_the_full_history_when_nothing_fits() {
    let history = vec![msg("user", &"x".repeat(400_000))];
    assert!(
        pre_shrink_to_window_budget(&history, None, Some(4_000)).is_none(),
        "a single message larger than the budget must fall back to the whole history"
    );
}

fn assert_messages_eq(actual: &[LLMMessage], expected: &[LLMMessage]) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "message count mismatch: actual {} vs expected {}",
        actual.len(),
        expected.len()
    );
    for (i, (act, exp)) in actual.iter().zip(expected.iter()).enumerate() {
        assert_eq!(act.role, exp.role, "role mismatch at index {i}");
        assert_eq!(act.content, exp.content, "content mismatch at index {i}");
        assert_eq!(act.blocks, exp.blocks, "blocks mismatch at index {i}");
        assert_eq!(
            act.tool_calls.len(),
            exp.tool_calls.len(),
            "tool_calls count mismatch at index {i}"
        );
        for (tc_idx, (tc_act, tc_exp)) in
            act.tool_calls.iter().zip(exp.tool_calls.iter()).enumerate()
        {
            assert_eq!(
                tc_act.id, tc_exp.id,
                "tool_call id mismatch at msg {i} tc {tc_idx}"
            );
            assert_eq!(
                tc_act.name, tc_exp.name,
                "tool_call name mismatch at msg {i} tc {tc_idx}"
            );
            assert_eq!(
                tc_act.arguments, tc_exp.arguments,
                "tool_call args mismatch at msg {i} tc {tc_idx}"
            );
        }
        assert_eq!(
            act.tool_call_id, exp.tool_call_id,
            "tool_call_id mismatch at index {i}"
        );
    }
}

#[test]
fn config_for_window_uses_the_host_window_and_keeps_other_knobs() {
    let host_window = 262_144;
    let config = config_for_window(Some(host_window));
    let default = CompactionConfig::default();
    assert_eq!(
        config,
        CompactionConfig {
            max_context_tokens: host_window,
            ..default
        },
        "P63: host window must be used and all other knobs preserved"
    );
    assert_eq!(config.max_context_tokens, 262_144);
    assert_eq!(config.trigger_ratio, 0.85);
    assert_eq!(config.reserved_context_size, 50_000);
    assert_eq!(config.max_recent_messages, 4);
    assert_eq!(config.max_recent_user_messages, u32::MAX);
    assert_eq!(config.max_recent_size_ratio, 0.2);

    // Boundary windows
    assert_eq!(config_for_window(Some(1)).max_context_tokens, 1);
    assert_eq!(
        config_for_window(Some(u32::MAX)).max_context_tokens,
        u32::MAX
    );
}

#[test]
fn config_for_window_falls_back_without_a_usable_window() {
    let default = CompactionConfig::default();
    assert_eq!(
        config_for_window(None),
        default,
        "P63: None window must fall back to CompactionConfig::default()"
    );
    assert_eq!(
        config_for_window(Some(0)),
        default,
        "P63: zero window must fall back to CompactionConfig::default()"
    );
}

#[test]
fn test_estimate_tokens_ascii_boundaries_and_unicode() {
    assert_eq!(estimate_tokens(""), 0);
    // ASCII stepping: div_ceil(len, 4)
    assert_eq!(estimate_tokens("a"), 1);
    assert_eq!(estimate_tokens("ab"), 1);
    assert_eq!(estimate_tokens("abc"), 1);
    assert_eq!(estimate_tokens("abcd"), 1);
    assert_eq!(estimate_tokens("abcde"), 2);
    assert_eq!(estimate_tokens("abcdefg"), 2);
    assert_eq!(estimate_tokens("abcdefgh"), 2);
    assert_eq!(estimate_tokens("abcdefghi"), 3);
    assert_eq!(estimate_tokens("123456789012"), 3);

    // Unicode and CJK: 1 token per code point > 127
    assert_eq!(estimate_tokens("你好"), 2);
    assert_eq!(estimate_tokens("你好世界"), 4);
    assert_eq!(estimate_tokens("ab你"), 2); // 2 ASCII -> 1 token + 1 CJK -> 2 tokens
    assert_eq!(estimate_tokens("Hello, 世界!"), 4); // 8 ASCII -> 2 tokens + 2 CJK -> 4 tokens
    assert_eq!(estimate_tokens("café"), 2); // 3 ASCII -> 1 token + 1 accented char -> 2 tokens
    assert_eq!(estimate_tokens("🦀🚀"), 2); // 2 emojis -> 2 tokens
    assert_eq!(estimate_tokens("AI 助手 🤖: 您好！"), 8); // 5 ASCII (2) + 6 non-ASCII (6) = 8 tokens
}

#[test]
fn test_estimate_tokens_for_json() {
    assert_eq!(estimate_tokens_for_json(""), 0);
    // 1 token base -> ceil(1 * 1.3) = 2
    assert_eq!(estimate_tokens_for_json("abcd"), 2);
    // 2 tokens base -> ceil(2 * 1.3) = 3
    assert_eq!(estimate_tokens_for_json("abcdefgh"), 3);
    // 3 tokens base -> ceil(3 * 1.3) = 4
    assert_eq!(estimate_tokens_for_json("abcdefghi"), 4);
    // 4 tokens base (15-16 ASCII chars) -> ceil(4 * 1.3) = 6
    assert_eq!(estimate_tokens_for_json("{\"key\":\"value\"}"), 6);
    // 5 tokens base (17 ASCII chars) -> ceil(5 * 1.3) = 7
    assert_eq!(estimate_tokens_for_json("{\"path\":\"/a.txt\"}"), 7);
    // JSON with CJK: 10 ASCII (3 tokens) + 2 CJK (2 tokens) = 5 tokens -> ceil(5 * 1.3) = 7
    assert_eq!(estimate_tokens_for_json("{\"name\":\"张三\"}"), 7);
}

#[test]
fn test_estimate_message_tokens_multimodal_content_blocks() {
    let text_msg = msg("user", "12345678"); // 8 chars -> 2 tokens
    assert_eq!(estimate_message_tokens(&text_msg), 2);

    // ContentBlock::Text
    let mut m_text = msg("user", "");
    m_text.blocks.push(ContentBlock::Text {
        text: "123456789012".into(), // 12 chars -> 3 tokens
    });
    assert_eq!(estimate_message_tokens(&m_text), 3);

    // ContentBlock::Think
    let mut m_think = msg("assistant", "");
    m_think.blocks.push(ContentBlock::Think {
        think: "reasoning step".into(), // 14 chars -> 4 tokens
        encrypted: None,
        details_index: None,
        reasoning_key: None,
        details_summary: None,
    });
    assert_eq!(estimate_message_tokens(&m_think), 4);

    // Multimodal media blocks: all count MEDIA_TOKEN_ESTIMATE (2000)
    let mut m_img = msg("user", "");
    m_img.blocks.push(ContentBlock::Image {
        media_type: "image/png".into(),
        data: "base64data".into(),
        name: None,
    });
    assert_eq!(estimate_message_tokens(&m_img), MEDIA_TOKEN_ESTIMATE);

    let mut m_img_url = msg("user", "");
    m_img_url.blocks.push(ContentBlock::ImageUrl {
        url: "http://example.com/pic.png".into(),
        id: None,
        name: None,
    });
    assert_eq!(estimate_message_tokens(&m_img_url), MEDIA_TOKEN_ESTIMATE);

    let mut m_audio = msg("user", "");
    m_audio.blocks.push(ContentBlock::AudioUrl {
        url: "http://example.com/audio.mp3".into(),
        id: Some("a1".into()),
        name: None,
    });
    assert_eq!(estimate_message_tokens(&m_audio), MEDIA_TOKEN_ESTIMATE);

    let mut m_video = msg("user", "");
    m_video.blocks.push(ContentBlock::VideoUrl {
        url: "http://example.com/video.mp4".into(),
        id: None,
        name: None,
    });
    assert_eq!(estimate_message_tokens(&m_video), MEDIA_TOKEN_ESTIMATE);

    // Additive combination: content + text block + think block + image block
    let mut m_combo = msg("user", "abcd"); // 1 token
    m_combo.blocks.push(ContentBlock::Text {
        text: "efgh".into(),
    }); // 1 token
    m_combo.blocks.push(ContentBlock::Think {
        think: "ijkl".into(), // 1 token
        encrypted: None,
        details_index: None,
        reasoning_key: None,
        details_summary: None,
    });
    m_combo.blocks.push(ContentBlock::ImageUrl {
        url: "http://example.com/img.jpg".into(), // 2000 tokens
        id: None,
        name: None,
    });
    assert_eq!(estimate_message_tokens(&m_combo), 1 + 1 + 1 + 2000);
}

#[test]
fn test_estimate_message_tokens_tool_calls_and_results_exact() {
    let mut m = msg("assistant", "hello world!"); // 12 chars -> 3 tokens
    // tool_call 1: name "read" (4 chars -> 1 token), args {"path":"/a.txt"} (17 chars -> 5 tokens -> ceil(5*1.3) = 7 tokens)
    // Subtotal = 1 + 7 = 8 tokens.
    m.tool_calls.push(tool_call(
        "tc1",
        "read",
        serde_json::json!({ "path": "/a.txt" }),
    ));
    assert_eq!(estimate_message_tokens(&m), 3 + 8);

    // tool_call 2: name "bash" (4 chars -> 1 token), args {"cmd":"ls"} (10 chars -> 3 tokens -> ceil(3*1.3) = 4 tokens)
    // Subtotal = 1 + 4 = 5 tokens.
    m.tool_calls
        .push(tool_call("tc2", "bash", serde_json::json!({ "cmd": "ls" })));
    assert_eq!(estimate_message_tokens(&m), 3 + 8 + 5);

    // Tool result message with tool_call_id
    let mut tool_msg = msg("tool", "file content"); // 12 chars -> 3 tokens
    tool_msg.tool_call_id = Some("tc1_unique_id".into()); // 13 chars -> 4 tokens
    assert_eq!(estimate_message_tokens(&tool_msg), 3 + 4);
}

#[test]
fn test_estimate_messages_tokens_empty_and_accumulated() {
    assert_eq!(estimate_messages_tokens(&[]), 0);

    let m1 = msg("user", "1234"); // 1 token
    let m2 = msg("assistant", "12345678"); // 2 tokens
    let m3 = msg("user", "123456789012"); // 3 tokens
    assert_eq!(estimate_messages_tokens(&[m1, m2, m3]), 6);
}

#[test]
fn test_empty_and_single_system_messages_invariance() {
    let config = CompactionConfig::default();
    // Empty message slice
    assert_eq!(estimate_messages_tokens(&[]), 0);
    assert_eq!(compute_compact_count(&[], &config), 0);
    assert!(compact_messages(&[], &config).is_empty());
    assert!(force_compact_messages(&[], &config).is_empty());
    assert!(!should_compact(0, &config));
    assert!(!can_split_after(&[], 0));

    // System message alone: never compacted under normal or forced compaction
    let system_only = vec![msg("system", "You are an assistant.")];
    assert_eq!(compute_compact_count(&system_only, &config), 0);
    assert_messages_eq(&compact_messages(&system_only, &config), &system_only);
    assert_messages_eq(&force_compact_messages(&system_only, &config), &system_only);

    // System message + single user message (2 messages total): cannot split without removing system
    let two_msgs = vec![msg("system", "sys"), msg("user", "hi")];
    assert_eq!(compute_compact_count(&two_msgs, &config), 0);
    assert_messages_eq(&compact_messages(&two_msgs, &config), &two_msgs);
    assert_messages_eq(&force_compact_messages(&two_msgs, &config), &two_msgs);
}

#[test]
fn test_should_compact_all_threshold_and_boundary_conditions() {
    // Zero context window: never compacts
    let zero_cfg = CompactionConfig {
        max_context_tokens: 0,
        ..Default::default()
    };
    assert!(!should_compact(0, &zero_cfg));
    assert!(!should_compact(100_000, &zero_cfg));

    // trigger_ratio boundary (isolated by setting reserved_context_size = 0)
    let ratio_cfg = CompactionConfig {
        max_context_tokens: 10_000,
        trigger_ratio: 0.85,
        reserved_context_size: 0,
        ..Default::default()
    };
    assert!(!should_compact(8499, &ratio_cfg), "8499 < 8500 threshold");
    assert!(should_compact(8500, &ratio_cfg), "8500 >= 8500 threshold");
    assert!(should_compact(8501, &ratio_cfg), "8501 >= 8500 threshold");

    // reserved_context_size boundary (triggers before trigger_ratio)
    let reserved_cfg = CompactionConfig {
        max_context_tokens: 100_000,
        trigger_ratio: 0.90,           // 90,000 threshold
        reserved_context_size: 20_000, // triggers at 80,000 (100k - 20k)
        ..Default::default()
    };
    assert!(!should_compact(79_999, &reserved_cfg));
    assert!(should_compact(80_000, &reserved_cfg));
    assert!(should_compact(80_001, &reserved_cfg));
    assert!(should_compact(90_000, &reserved_cfg));

    // reserved_context_size >= max_context_tokens disabled guard
    let disabled_reserved_cfg = CompactionConfig {
        max_context_tokens: 1_000,
        trigger_ratio: 0.85,
        reserved_context_size: 50_000, // 50k >= 1k, disabled
        ..Default::default()
    };
    assert!(!should_compact(849, &disabled_reserved_cfg));
    assert!(should_compact(850, &disabled_reserved_cfg));

    // Saturating addition does not overflow u32
    let sat_cfg = CompactionConfig {
        max_context_tokens: 100_000,
        trigger_ratio: 0.85,
        reserved_context_size: 50_000,
        ..Default::default()
    };
    assert!(should_compact(u32::MAX, &sat_cfg));
}

#[test]
fn test_should_compact_auto_stays_silent_when_the_split_search_finds_nowhere_to_cut() {
    let config = CompactionConfig {
        max_context_tokens: 1_000,
        trigger_ratio: 0.01,
        reserved_context_size: 0,
        max_recent_messages: 4,
        max_recent_user_messages: u32::MAX,
        max_recent_size_ratio: 0.5,
        max_attempts: None,
        max_overflow_compaction_attempts: DEFAULT_MAX_OVERFLOW_COMPACTION_ATTEMPTS,
    };

    // The shape a previous compaction leaves behind: the kept user input,
    // the elision note, the summary and the continuation are all user
    // messages, and `can_split_after` refuses to cut after a user message.
    // The only split left is after the system prompt, which floors to 0 —
    // so crossing the threshold alone must not get the turn loop to
    // announce a compaction that folds nothing.
    let compacted_head = vec![
        msg("system", "system-prompt"),
        msg("user", "kept user input"),
        msg("user", "[messages omitted during compaction]"),
        msg("user", "[summary of the compacted conversation]"),
        msg("user", "[context compaction is complete]"),
    ];
    assert_eq!(compute_compact_count(&compacted_head, &config), 0);
    let used = estimate_messages_tokens(&compacted_head);
    assert!(
        should_compact(used, &config),
        "the threshold itself must be crossed, so only the split search can veto"
    );
    assert!(!should_compact_auto(&compacted_head, used, &config));

    // The same threshold with a real split point still runs.
    let splittable = vec![
        msg("system", "system-prompt"),
        msg("user", "user-1"),
        msg("assistant", "assistant-1"),
        msg("user", "user-2"),
        msg("assistant", "assistant-2"),
        msg("user", "user-3"),
        msg("assistant", "assistant-3"),
        msg("user", "user-4"),
    ];
    assert!(compute_compact_count(&splittable, &config) > 0);
    assert!(should_compact_auto(&splittable, used, &config));

    // Under the threshold it stays out of the way either way.
    assert!(!should_compact_auto(&splittable, 0, &config));
    assert!(!should_compact_auto(&compacted_head, 0, &config));
}

#[test]
fn test_summary_placeholder_format_exact() {
    assert_eq!(
        summary_placeholder(0),
        "[Earlier conversation compacted: 0 messages were summarized away to fit the context window. Continue from the most recent context.]"
    );
    assert_eq!(
        summary_placeholder(1),
        "[Earlier conversation compacted: 1 messages were summarized away to fit the context window. Continue from the most recent context.]"
    );
    assert_eq!(
        summary_placeholder(42),
        "[Earlier conversation compacted: 42 messages were summarized away to fit the context window. Continue from the most recent context.]"
    );
}

#[test]
fn test_compact_preserves_system_replaces_middle_with_exact_placeholder_and_tail() {
    let messages = vec![
        msg("system", "system-prompt"),
        msg("user", "user-1"),
        msg("assistant", "assistant-1"),
        msg("user", "user-2"),
        msg("assistant", "assistant-2"),
        msg("user", "user-3"),
        msg("assistant", "assistant-3"),
        msg("user", "user-4"),
    ];

    let config = CompactionConfig {
        max_context_tokens: 1000,
        trigger_ratio: 0.01, // Force trigger
        reserved_context_size: 0,
        max_recent_messages: 4,
        max_recent_user_messages: u32::MAX,
        max_recent_size_ratio: 0.5,
        max_attempts: None,
        max_overflow_compaction_attempts: DEFAULT_MAX_OVERFLOW_COMPACTION_ATTEMPTS,
    };

    let count = compute_compact_count(&messages, &config);
    assert_eq!(
        count, 5,
        "compacts messages 0..5 (system + 4 conversation messages)"
    );

    let compacted = compact_messages(&messages, &config);
    let expected = vec![
        msg("system", "system-prompt"),
        msg("user", &summary_placeholder(4)),
        msg("user", "user-3"),
        msg("assistant", "assistant-3"),
        msg("user", "user-4"),
    ];
    assert_messages_eq(&compacted, &expected);
}

#[test]
fn test_budget_boundary_exact() {
    let config = small_config(1_000);
    // Each message is 16 chars = 4 tokens.
    // 211 messages * 4 tokens = 844 tokens < 850 threshold -> no compaction.
    let mut messages = vec![msg("system", "sys-16chars-pad!")];
    for i in 0..210 {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        messages.push(msg(role, &format!("msg-{:012}", i)));
    }
    assert_eq!(estimate_messages_tokens(&messages), 844);
    assert!(!should_compact(
        estimate_messages_tokens(&messages),
        &config
    ));
    assert_messages_eq(&compact_messages(&messages, &config), &messages);

    // Add 2 more messages: 213 messages * 4 tokens = 852 tokens >= 850 threshold -> triggers compaction.
    messages.push(msg("user", "msg-000000000210"));
    messages.push(msg("assistant", "msg-000000000211"));
    assert_eq!(estimate_messages_tokens(&messages), 852);
    assert!(should_compact(estimate_messages_tokens(&messages), &config));

    let count = compute_compact_count(&messages, &config);
    assert!(count >= 2, "must compact at least system + 1 message");
    let compacted = compact_messages(&messages, &config);
    assert_eq!(compacted.len(), messages.len() - count as usize + 2);
    assert_messages_eq(&compacted[0..1], &messages[0..1]);
    assert_eq!(compacted[1].role, "user");
    assert_eq!(
        compacted[1].content,
        summary_placeholder(count as usize - 1)
    );
    assert_messages_eq(&compacted[2..], &messages[count as usize..]);
}

#[test]
fn test_does_not_split_inside_tool_exchange_keeps_or_drops_intact() {
    let messages = vec![
        msg("system", "sys"),
        msg("user", "u1"),
        msg("assistant", "a1"),
        msg("user", "u2"),
        {
            let mut a = msg("assistant", "a2");
            a.tool_calls = vec![
                tool_call("t1", "read", serde_json::json!({})),
                tool_call("t2", "write", serde_json::json!({})),
            ];
            a
        },
        msg("tool", "r1"),
        msg("tool", "r2"),
        msg("assistant", "a3"),
        msg("user", "u3"),
    ];

    // Case A: Recent tail cutoff (max_recent_messages = 3) lands at a3 / r2.
    // It cannot split after r1 (open exchange) or a2 (pending tool calls),
    // so it compacts the ENTIRE tool exchange into the prefix.
    let config_a = CompactionConfig {
        max_context_tokens: 1000,
        trigger_ratio: 0.01,
        reserved_context_size: 0,
        max_recent_messages: 3,
        max_recent_user_messages: u32::MAX,
        max_recent_size_ratio: 0.5,
        max_attempts: None,
        max_overflow_compaction_attempts: DEFAULT_MAX_OVERFLOW_COMPACTION_ATTEMPTS,
    };
    let count_a = compute_compact_count(&messages, &config_a);
    assert_eq!(
        count_a, 7,
        "compacts up to index 7 (sys + u1 + a1 + u2 + a2 + r1 + r2)"
    );
    let compacted_a = compact_messages(&messages, &config_a);
    let expected_a = vec![
        msg("system", "sys"),
        msg("user", &summary_placeholder(6)),
        msg("assistant", "a3"),
        msg("user", "u3"),
    ];
    assert_messages_eq(&compacted_a, &expected_a);

    // Case B: Longer recent tail requirement (max_recent_messages = 6) forces
    // the split BEFORE the tool exchange (after a1 at index 2).
    // The ENTIRE tool exchange is kept in the tail intact.
    let config_b = CompactionConfig {
        max_context_tokens: 1000,
        trigger_ratio: 0.01,
        reserved_context_size: 0,
        max_recent_messages: 6,
        max_recent_user_messages: u32::MAX,
        max_recent_size_ratio: 0.5,
        max_attempts: None,
        max_overflow_compaction_attempts: DEFAULT_MAX_OVERFLOW_COMPACTION_ATTEMPTS,
    };
    let count_b = compute_compact_count(&messages, &config_b);
    assert_eq!(count_b, 3, "compacts up to index 3 (sys + u1 + a1)");
    let compacted_b = compact_messages(&messages, &config_b);
    assert_eq!(compacted_b.len(), 8);
    assert_messages_eq(&compacted_b[0..1], &messages[0..1]);
    assert_eq!(compacted_b[1].content, summary_placeholder(2));
    assert_messages_eq(&compacted_b[2..], &messages[3..]);
}

#[test]
fn test_can_split_after_all_safety_rules() {
    // 1. Out of bounds
    assert!(!can_split_after(&[], 0));
    let msgs = vec![msg("assistant", "a")];
    assert!(!can_split_after(&msgs, 1));
    assert!(!can_split_after(&msgs, 5));

    // 2. Never split after a user message
    let msgs = vec![msg("user", "u"), msg("assistant", "a")];
    assert!(!can_split_after(&msgs, 0));

    // 3. Never split after assistant with pending tool calls
    let mut a = msg("assistant", "a");
    a.tool_calls
        .push(tool_call("t1", "read", serde_json::json!({})));
    let msgs = vec![a, msg("user", "u")];
    assert!(!can_split_after(&msgs, 0));

    // 4. Never split immediately before a tool message (would orphan the tool)
    let msgs = vec![msg("assistant", "a"), msg("tool", "r"), msg("user", "u")];
    assert!(!can_split_after(&msgs, 0));

    // 5. Open tool exchange in prefix: assistant issued 2 calls, only 1 result in prefix
    let mut a = msg("assistant", "a");
    a.tool_calls = vec![
        tool_call("t1", "read", serde_json::json!({})),
        tool_call("t2", "write", serde_json::json!({})),
    ];
    let msgs = vec![a, msg("tool", "r1"), msg("user", "u")];
    assert!(
        !can_split_after(&msgs, 1),
        "cannot split after partial tool exchange"
    );

    // 6. Resolved tool exchange in prefix: 2 calls and 2 results
    let mut a = msg("assistant", "a");
    a.tool_calls = vec![
        tool_call("t1", "read", serde_json::json!({})),
        tool_call("t2", "write", serde_json::json!({})),
    ];
    let msgs = vec![a, msg("tool", "r1"), msg("tool", "r2"), msg("user", "u")];
    assert!(
        can_split_after(&msgs, 2),
        "safe to split after fully satisfied tool exchange"
    );

    // 7. Clean assistant -> user boundary
    let msgs = vec![msg("assistant", "a"), msg("user", "u")];
    assert!(can_split_after(&msgs, 0));

    // 8. Clean assistant at end of messages
    let msgs = vec![msg("user", "u"), msg("assistant", "a")];
    assert!(can_split_after(&msgs, 1));

    // 9. System message followed by user
    let msgs = vec![msg("system", "sys"), msg("user", "u")];
    assert!(can_split_after(&msgs, 0));

    // 10. System message followed by tool (malformed, but must be rejected)
    let msgs = vec![msg("system", "sys"), msg("tool", "r")];
    assert!(!can_split_after(&msgs, 0));
}

#[test]
fn test_prefix_ends_with_open_tool_exchange_direct() {
    // Non-tool message returns false immediately
    assert!(!prefix_ends_with_open_tool_exchange(
        &[msg("assistant", "a")],
        0
    ));
    assert!(!prefix_ends_with_open_tool_exchange(&[], 0));

    // 2 calls, 1 result -> open (true)
    let mut a2 = msg("assistant", "a");
    a2.tool_calls = vec![
        tool_call("1", "read", serde_json::json!({})),
        tool_call("2", "write", serde_json::json!({})),
    ];
    let msgs = vec![a2.clone(), msg("tool", "r1")];
    assert!(prefix_ends_with_open_tool_exchange(&msgs, 1));

    // 2 calls, 2 results -> closed (false)
    let msgs = vec![a2.clone(), msg("tool", "r1"), msg("tool", "r2")];
    assert!(!prefix_ends_with_open_tool_exchange(&msgs, 2));

    // 1 call, 2 results -> closed (false)
    let mut a1 = msg("assistant", "a");
    a1.tool_calls = vec![tool_call("1", "read", serde_json::json!({}))];
    let msgs = vec![a1, msg("tool", "r1"), msg("tool", "r2")];
    assert!(!prefix_ends_with_open_tool_exchange(&msgs, 2));

    // Multiple assistants in history: backwards walk checks the immediately preceding assistant
    let mut a_first = msg("assistant", "first");
    a_first.tool_calls = vec![tool_call("1", "read", serde_json::json!({}))];
    let mut a_second = msg("assistant", "second");
    a_second.tool_calls = vec![
        tool_call("2", "read", serde_json::json!({})),
        tool_call("3", "read", serde_json::json!({})),
    ];
    let msgs = vec![a_first, msg("tool", "r1"), a_second, msg("tool", "r2")];
    assert!(prefix_ends_with_open_tool_exchange(&msgs, 3));

    // Tool preceded by user (no assistant) -> returns false
    let msgs = vec![msg("user", "u"), msg("tool", "r")];
    assert!(!prefix_ends_with_open_tool_exchange(&msgs, 1));
}

#[test]
fn test_fit_compact_count_to_window_shrinks_safely() {
    let config = CompactionConfig {
        max_context_tokens: 5,
        ..Default::default()
    };
    // Zero window or zero count returns unchanged
    assert_eq!(fit_compact_count_to_window(&[], 0, &config), 0);
    let mut zero_cfg = config;
    zero_cfg.max_context_tokens = 0;
    assert_eq!(fit_compact_count_to_window(&[], 5, &zero_cfg), 5);

    // 8 messages, each 4 chars = 1 token
    let messages = vec![
        msg("system", "s000"),    // 1 token
        msg("user", "u001"),      // 1 token
        msg("assistant", "a002"), // 1 token (can split after index 2)
        msg("user", "u003"),      // 1 token
        msg("assistant", "a004"), // 1 token (can split after index 4)
        msg("user", "u005"),      // 1 token
        msg("assistant", "a006"), // 1 token (can split after index 6)
        msg("user", "u007"),      // 1 token
    ];

    // Candidate count 7 has 7 tokens > max_context_tokens 5.
    // It must shrink backwards:
    // n = 6: tokens = 6 > 5.
    // n = 5: tokens = 5 <= 5. Can split after index 4 (assistant a004)? Yes!
    // Returns 5.
    assert_eq!(fit_compact_count_to_window(&messages, 7, &config), 5);
}

#[test]
fn test_compaction_config_knobs_control_tail_boundary() {
    let messages = vec![
        msg("system", "system-prompt"),
        msg("user", "user-1"),
        msg("assistant", "assistant-1"),
        msg("user", "user-2"),
        msg("assistant", "assistant-2"),
        msg("user", "user-3"),
        msg("assistant", "assistant-3"),
        msg("user", "user-4"),
    ];

    // Knob: max_recent_messages = 2
    let cfg_recent = CompactionConfig {
        max_context_tokens: 10_000,
        trigger_ratio: 0.01,
        reserved_context_size: 0,
        max_recent_messages: 2,
        max_recent_user_messages: u32::MAX,
        max_recent_size_ratio: 0.5,
        max_attempts: None,
        max_overflow_compaction_attempts: DEFAULT_MAX_OVERFLOW_COMPACTION_ATTEMPTS,
    };
    assert_eq!(compute_compact_count(&messages, &cfg_recent), 7);

    // Knob: max_recent_user_messages = 1
    // As soon as 1 user message is included in recent tail (user-4),
    // and a safe split point exists (assistant-3), search halts.
    let cfg_user = CompactionConfig {
        max_context_tokens: 10_000,
        trigger_ratio: 0.01,
        reserved_context_size: 0,
        max_recent_messages: 10,
        max_recent_user_messages: 1,
        max_recent_size_ratio: 0.5,
        max_attempts: None,
        max_overflow_compaction_attempts: DEFAULT_MAX_OVERFLOW_COMPACTION_ATTEMPTS,
    };
    assert_eq!(compute_compact_count(&messages, &cfg_user), 7);

    // Knob: max_recent_size_ratio halts tail growth when size budget reached
    let cfg_ratio = CompactionConfig {
        max_context_tokens: 100,
        trigger_ratio: 0.01,
        reserved_context_size: 0,
        max_recent_messages: 10,
        max_recent_user_messages: u32::MAX,
        max_recent_size_ratio: 0.02, // 2 tokens max for recent tail
        max_attempts: None,
        max_overflow_compaction_attempts: DEFAULT_MAX_OVERFLOW_COMPACTION_ATTEMPTS,
    };
    // Each message is >= 3 tokens, so first message already hits 2-token budget
    assert_eq!(compute_compact_count(&messages, &cfg_ratio), 7);
}

#[test]
fn test_unsplittable_history_remains_untouched() {
    let config = small_config(100);

    // All user messages: cannot split after any user message
    let all_users = vec![
        msg("system", "sys"),
        msg("user", "u1"),
        msg("user", "u2"),
        msg("user", "u3"),
    ];
    assert_eq!(compute_compact_count(&all_users, &config), 0);
    assert_messages_eq(&compact_messages(&all_users, &config), &all_users);
    assert_messages_eq(&force_compact_messages(&all_users, &config), &all_users);

    // Open tool exchange with no safe prior split point
    let mut a = msg("assistant", "a");
    a.tool_calls = vec![
        tool_call("t1", "f", serde_json::json!({})),
        tool_call("t2", "f", serde_json::json!({})),
    ];
    let open_tool = vec![
        msg("system", "sys"),
        msg("user", "u1"),
        a,
        msg("tool", "r1"),
    ];
    assert_eq!(compute_compact_count(&open_tool, &config), 0);
    assert_messages_eq(&compact_messages(&open_tool, &config), &open_tool);
    assert_messages_eq(&force_compact_messages(&open_tool, &config), &open_tool);
}

/// v2 `observeContextOverflow` / `getEffectiveMaxContextTokens`: an
/// observed overflow lowers the model's effective window to
/// `floor(estimated × 0.85)`, never raises it, and the configured value
/// wins when nothing was observed. The cache is process-global, so each
/// test uses its own model name.
#[test]
fn test_observe_context_overflow_lowers_and_never_raises() {
    let model = "test-observe-lower";
    assert_eq!(effective_max_tokens(model, Some(10_000)), Some(10_000));

    observe_context_overflow(model, 1_000, Some(10_000));
    assert_eq!(effective_max_tokens(model, Some(10_000)), Some(850));

    // A later, larger observation never raises the floor.
    observe_context_overflow(model, 5_000, Some(10_000));
    assert_eq!(effective_max_tokens(model, Some(10_000)), Some(850));

    // A smaller one lowers further.
    observe_context_overflow(model, 500, Some(10_000));
    assert_eq!(effective_max_tokens(model, Some(10_000)), Some(425));
}

#[test]
fn test_effective_max_tokens_without_a_configured_window() {
    let model = "test-observe-no-config";
    assert_eq!(effective_max_tokens(model, None), None);

    // A lone observation stands when the host resolved no window.
    observe_context_overflow(model, 2_000, None);
    assert_eq!(effective_max_tokens(model, None), Some(1_700));

    // The floor is at least one token.
    let tiny = "test-observe-tiny";
    observe_context_overflow(tiny, 1, None);
    assert_eq!(effective_max_tokens(tiny, None), Some(1));
}

#[test]
fn test_observe_context_overflow_ignores_empty_marks() {
    let model = "test-observe-empty";
    observe_context_overflow("", 1_000, Some(10_000));
    observe_context_overflow(model, 0, Some(10_000));
    assert_eq!(effective_max_tokens(model, Some(10_000)), Some(10_000));
}

#[test]
fn test_is_context_overflow_error_all_patterns_and_negatives() {
    // All 11 recognized patterns from CONTEXT_OVERFLOW_MESSAGE_PATTERNS
    let positive_cases = [
        "llm http status 400: context_length_exceeded",
        "model error: context_length reached",
        "request error: context length exceeded",
        "exceeded context window limit of 128k",
        "reached maximum context for this model",
        "max_tokens limit was exceeded",
        "request exceeds model token limit",
        "too many tokens in prompt",
        "prompt is too long for selected model",
        "input token count exceeds allowed maximum",
        "request payload exceeds the maximum size allowed",
    ];
    for err in positive_cases {
        assert!(
            is_context_overflow_error(err),
            "pattern in '{}' must be detected as context overflow",
            err
        );
    }

    // Case insensitivity
    assert!(is_context_overflow_error("CONTEXT_LENGTH_EXCEEDED"));
    assert!(is_context_overflow_error("Prompt Is Too Long"));
    assert!(is_context_overflow_error("INPUT TOKEN COUNT EXCEEDED"));
    assert!(is_context_overflow_error("MAX_TOKENS"));
    assert!(is_context_overflow_error("EXCEEDS THE MAXIMUM SIZE"));

    // Negative cases that must NOT trigger
    let negative_cases = [
        "",
        "llm http status 401: unauthorized",
        "llm http status 429: rate limit exceeded",
        "llm http status 500: internal server error",
        "connection reset by peer",
        "context is important for good answers",
        "tokens remaining: 50",
        "maximum speed achieved",
        "prompt submitted successfully",
    ];
    for err in negative_cases {
        assert!(
            !is_context_overflow_error(err),
            "non-overflow error '{}' must not trigger overflow detection",
            err
        );
    }
}

/// v2 gates the wording table on the status code
/// (`isContextOverflowStatusError`): only 400/413/422 may be read as an
/// overflow, and an error carrying no status is classified on its own
/// terms — the in-stream provider-error path.
#[test]
fn test_context_overflow_wording_is_status_gated() {
    for status in [400, 413, 422] {
        assert!(
            is_context_overflow_error(&format!(
                "llm http status {status} Too Many Requests: context_length_exceeded"
            )),
            "a {status} that names the overflow must be detected"
        );
    }
    // The wording is there, but the status says this is a server fault.
    assert!(!is_context_overflow_error(
        "llm http status 500 Internal Server Error: max_tokens rejected by backend"
    ));
    assert!(!is_context_overflow_error(
        "llm http status 401 Unauthorized: prompt is too long for this plan"
    ));
    // No status at all: the in-stream `context_length_exceeded` code path.
    assert!(is_context_overflow_error(
        "llm provider stream error: context_length_exceeded"
    ));
}

/// A 413 with an opaque body is still an overflow when the request had
/// already filled half the window — v2's size-based branch, which is what
/// covers gateways that never echo the wording.
#[test]
fn test_bare_413_recovers_only_near_the_window() {
    let opaque = "llm http status 413 Payload Too Large: <html>nginx</html>";
    assert!(should_recover_from_context_overflow(
        opaque,
        100_000,
        Some(200_000)
    ));
    // Half the window exactly: v2's `>=` on the ratio.
    assert!(should_recover_from_context_overflow(
        opaque,
        100_000,
        Some(200_000)
    ));
    // One token under the ratio: a proxy rejecting a small request.
    assert!(!should_recover_from_context_overflow(
        opaque,
        99_999,
        Some(200_000)
    ));
    // Nothing to compare against, or a nonsensical window.
    assert!(!should_recover_from_context_overflow(opaque, 100_000, None));
    assert!(!should_recover_from_context_overflow(
        opaque,
        100_000,
        Some(0)
    ));
}

/// The three shapes that used to slip through: a gateway 413, a proxy 413
/// and a bare status line, none of which name the overflow.
#[test]
fn test_opaque_413_shapes_recover_near_the_window() {
    for err in [
        "llm http status 413 Payload Too Large: <html><body>413</body></html>",
        "llm http status 413 Request Entity Too Large: proxy refused the request",
        "llm http status 413: ",
    ] {
        assert!(
            !is_context_overflow_error(err),
            "none of these name the overflow: {err}"
        );
        assert!(
            should_recover_from_context_overflow(err, 190_000, Some(200_000)),
            "a full window behind an opaque 413 must still recover: {err}"
        );
        assert!(
            !should_recover_from_context_overflow(err, 1_000, Some(200_000)),
            "a small request behind a 413 is a proxy limit, not an overflow: {err}"
        );
    }
}

/// The size gate is specific to 413: other statuses recover on wording
/// alone, and never on size.
#[test]
fn test_overflow_recovery_ignores_size_for_non_413_statuses() {
    assert!(should_recover_from_context_overflow(
        "llm http status 400 Bad Request: context length exceeded",
        1,
        Some(200_000)
    ));
    assert!(!should_recover_from_context_overflow(
        "llm http status 500 Internal Server Error: backend exploded",
        199_999,
        Some(200_000)
    ));
    // A 429 near the window is a rate limit, not an overflow.
    assert!(!should_recover_from_context_overflow(
        "llm http status 429 Too Many Requests: slow down",
        199_999,
        Some(200_000)
    ));
}

/// The recovery decision is the wording test plus the size branch — it must
/// not lose a case the wording test already accepts.
#[test]
fn test_overflow_recovery_supersedes_the_wording_test() {
    for err in [
        "context_length_exceeded",
        "model error: context length reached",
        "request payload exceeds the maximum size allowed",
        "llm http status 400: context_length_exceeded",
    ] {
        assert!(is_context_overflow_error(err));
        assert!(should_recover_from_context_overflow(err, 0, None));
    }
}

#[test]
fn test_force_compact_bypasses_should_compact_threshold() {
    let config = small_config(100_000);
    let messages = vec![
        msg("system", "sys"),
        msg("user", "u1"),
        msg("assistant", "a1"),
        msg("user", "u2"),
        msg("assistant", "a2"),
        msg("user", "u3"),
    ];

    // Normal compact_messages does not trigger because 100k window is far away
    assert_messages_eq(&compact_messages(&messages, &config), &messages);

    // force_compact_messages triggers emergency compaction
    let count = compute_compact_count(&messages, &config);
    assert_eq!(count, 3, "splits after assistant a1 at index 2");

    let forced = force_compact_messages(&messages, &config);
    let expected = vec![
        msg("system", "sys"),
        msg("user", &summary_placeholder(2)),
        msg("user", "u2"),
        msg("assistant", "a2"),
        msg("user", "u3"),
    ];
    assert_messages_eq(&forced, &expected);
}

/// Manual compaction walks the history from the tail and takes the
/// deepest safe split, so it compacts strictly more than the auto window
/// (v2 `computeCompactCount(..., 'manual')`, strategy.ts:182-189).
#[test]
fn test_manual_compaction_scans_deeper_than_auto() {
    let config = small_config(100_000);
    let mut messages = vec![msg("system", "sys")];
    for i in 0..20 {
        messages.push(msg("user", &format!("u{i}")));
        messages.push(msg("assistant", &format!("a{i}")));
    }

    let auto = compute_compact_count(&messages, &config);
    let manual = compute_compact_count_manual(&messages, &config);
    assert!(
        manual > auto,
        "manual {manual} must compact deeper than auto {auto}"
    );
    // The deepest safe split sits after the final assistant message, so a
    // manual compaction keeps only the system prompt plus the summary —
    // the manual branch probes `messages.length - 1`, unlike the auto
    // window (v2 strategy.ts:182-189 does the same).
    assert_eq!(manual as usize, messages.len());

    let manual_compacted = force_compact_messages_manual(&messages, &config);
    assert_eq!(manual_compacted.len(), 2);
    assert_eq!(manual_compacted[0].role, "system");
    assert_eq!(
        manual_compacted[1].content,
        summary_placeholder(messages.len() - 1)
    );
    assert!(force_compact_messages(&messages, &config).len() > manual_compacted.len());
}

// ── LLM summarizer tests ──────────────────────────────────────────────

/// Mock summarizer. The first `fail_times` calls fail; `retryable`
/// decides whether the engine may retry them. `calls` counts every call.
struct SummarizerMockLlm {
    content: String,
    fail_times: u32,
    retryable: bool,
    fail_message: String,
    calls: std::sync::atomic::AtomicU32,
    last_user_content: std::sync::Mutex<Option<String>>,
}

impl SummarizerMockLlm {
    /// Every call succeeds with `content`.
    fn ok(content: &str) -> Self {
        Self {
            content: content.into(),
            fail_times: 0,
            retryable: false,
            fail_message: "summarizer unavailable".into(),
            calls: std::sync::atomic::AtomicU32::new(0),
            last_user_content: std::sync::Mutex::new(None),
        }
    }

    /// Every call fails with a non-retryable error.
    fn error() -> Self {
        Self {
            content: String::new(),
            fail_times: u32::MAX,
            retryable: false,
            fail_message: "summarizer unavailable".into(),
            calls: std::sync::atomic::AtomicU32::new(0),
            last_user_content: std::sync::Mutex::new(None),
        }
    }

    /// The first `fail_times` calls fail with a retryable error, then
    /// calls succeed with `content`.
    fn transient(content: &str, fail_times: u32) -> Self {
        Self {
            content: content.into(),
            fail_times,
            retryable: true,
            fail_message: "summarizer unavailable".into(),
            calls: std::sync::atomic::AtomicU32::new(0),
            last_user_content: std::sync::Mutex::new(None),
        }
    }

    /// The first `fail_times` calls fail with a context-overflow error
    /// (the shape the compaction-overflow shrink recovers from), then
    /// calls succeed with `content`.
    fn overflow_then_ok(content: &str, fail_times: u32) -> Self {
        Self {
            content: content.into(),
            fail_times,
            retryable: false,
            fail_message: "llm http status 400 Bad Request: context_length_exceeded".into(),
            calls: std::sync::atomic::AtomicU32::new(0),
            last_user_content: std::sync::Mutex::new(None),
        }
    }

    /// Every call fails with a context-overflow error.
    fn overflow_always() -> Self {
        Self::overflow_then_ok("", u32::MAX)
    }

    fn call_count(&self) -> u32 {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The user-role prompt of the most recent call (instruction +
    /// transcript), for asserting what the summarizer was asked.
    fn last_user_content(&self) -> String {
        self.last_user_content
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default()
    }
}

impl LLM for SummarizerMockLlm {
    fn system_prompt(&self) -> &str {
        "test"
    }
    fn model_name(&self) -> &str {
        "test-summarizer"
    }
    fn is_retryable_error(&self, _: &str) -> bool {
        self.retryable
    }
    fn chat(
        &self,
        params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        let content = self.content.clone();
        let fail_message = self.fail_message.clone();
        let attempt = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let fails = attempt <= self.fail_times;
        let user_content = params
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.clone());
        *self
            .last_user_content
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = user_content;
        Box::pin(async move {
            if fails {
                Err(fail_message.into())
            } else {
                Ok(LLMChatResponse {
                    content,
                    thinking: vec![],
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: TokenUsage::default(),
                    timing: None,
                })
            }
        })
    }
}

fn compactable_messages() -> Vec<LLMMessage> {
    vec![
        msg("system", "system-prompt"),
        msg("user", "user-1"),
        msg("assistant", "assistant-1"),
        msg("user", "user-2"),
        msg("assistant", "assistant-2"),
        msg("user", "user-3"),
        msg("assistant", "assistant-3"),
        msg("user", "user-4"),
    ]
}

fn compacting_config() -> CompactionConfig {
    CompactionConfig {
        max_context_tokens: 1000,
        trigger_ratio: 0.01,
        reserved_context_size: 0,
        max_recent_messages: 4,
        max_recent_user_messages: u32::MAX,
        max_recent_size_ratio: 0.5,
        max_attempts: None,
        max_overflow_compaction_attempts: DEFAULT_MAX_OVERFLOW_COMPACTION_ATTEMPTS,
    }
}

#[tokio::test]
async fn test_summarize_with_llm_returns_summary_on_success() {
    let omitted = vec![
        msg("user", "user-1"),
        msg("assistant", "assistant-1"),
        msg("user", "user-2"),
    ];
    let llm = SummarizerMockLlm::ok("  Summary of earlier conversation.  ");
    let result = summarize_with_llm(&omitted, &llm, None, None, None).await;
    assert_eq!(result.unwrap(), "Summary of earlier conversation.");
}

#[tokio::test]
async fn test_summarize_with_llm_returns_error_on_empty_content() {
    let omitted = vec![msg("user", "user-1"), msg("assistant", "assistant-1")];
    let llm = SummarizerMockLlm::ok("");
    let result = summarize_with_llm(&omitted, &llm, None, None, None).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_summarize_with_llm_returns_error_on_error() {
    let omitted = vec![msg("user", "user-1"), msg("assistant", "assistant-1")];
    let llm = SummarizerMockLlm::error();
    let result = summarize_with_llm(&omitted, &llm, None, None, None).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_summarization_prompt_includes_instruction_and_transcript() {
    let omitted = vec![msg("user", "hello"), msg("assistant", "hi there")];
    let prompt = summarization_prompt(&omitted, Some("Custom instruction."));
    assert_eq!(prompt.len(), 2);
    assert_eq!(prompt[0].role, "system");
    assert_eq!(prompt[1].role, "user");
    assert!(
        prompt[1].content.contains("Custom instruction."),
        "user message must contain the custom instruction"
    );
    assert!(
        prompt[1].content.contains("user: hello"),
        "user message must contain the transcript"
    );
    assert!(
        prompt[1].content.contains("assistant: hi there"),
        "user message must contain the transcript"
    );
}

#[tokio::test]
async fn test_summarization_prompt_uses_the_v2_handoff_template_when_none() {
    let omitted = vec![msg("user", "hello")];
    let prompt = summarization_prompt(&omitted, None);
    assert_eq!(prompt.len(), 2);
    assert_eq!(
        prompt[1].content,
        format!("user: hello\n\n{}", render_compaction_instruction(None)),
        "the transcript is followed by the rendered v2 template"
    );
    assert!(
        !prompt[1].content.contains("${custom_instruction_block}"),
        "the template placeholder must be filled, never sent verbatim"
    );
    // The two demands the retired one-liner omitted and the observed
    // summaries then omitted too: exact history, and a forward plan.
    assert!(
        prompt[1].content.contains("the exact file paths touched")
            && prompt[1].content.contains("The forward plan"),
        "the v2 handoff template must ship in full"
    );
}

#[tokio::test]
async fn test_a_caller_instruction_lands_inside_the_template() {
    let omitted = vec![msg("user", "hello")];
    let prompt = summarization_prompt(&omitted, Some("Custom instruction."));
    let rendered = render_compaction_instruction(Some("Custom instruction."));
    assert_eq!(prompt[1].content, format!("user: hello\n\n{rendered}"));
    assert!(
        rendered.contains("Optional user instruction:\nCustom instruction."),
        "v2 appends the caller's instruction inside the template"
    );
    assert!(
        rendered.contains("The forward plan"),
        "a caller instruction must not replace the template"
    );
    // A blank instruction is v2's `custom.length > 0` guard: no empty block.
    assert_eq!(
        render_compaction_instruction(Some("   ")),
        render_compaction_instruction(None)
    );
}

#[tokio::test]
async fn test_summarization_prompt_serializes_tool_calls() {
    let mut m = msg("assistant", "running tools");
    m.tool_calls
        .push(tool_call("t1", "read", serde_json::json!({ "path": "/a" })));
    let omitted = vec![m];
    let prompt = summarization_prompt(&omitted, None);
    assert!(
        prompt[1].content.contains("[tool_call: read("),
        "tool calls must be serialized in the transcript"
    );
}

/// v2 `postProcessSummary`: no todos means the summary passes through
/// untouched (v2 returns it untrimmed); with todos it is trimmed and the
/// rendered list rides after exactly one blank line.
#[test]
fn test_post_process_summary_appends_todo_list() {
    let raw = "  The earlier work, padded.  ".to_string();
    assert_eq!(
        post_process_summary(raw.clone(), None),
        "  The earlier work, padded.  "
    );
    assert_eq!(
        post_process_summary(raw, Some("## TODO List\n  [in_progress] t1: ship".into())),
        "The earlier work, padded.\n\n## TODO List\n  [in_progress] t1: ship"
    );
}

#[tokio::test]
async fn test_force_compact_with_summary_uses_llm_summary() {
    let messages = compactable_messages();
    let config = compacting_config();
    let llm = SummarizerMockLlm::ok("LLM summary of the conversation.");
    let compacted = force_compact_messages_with_summary(&messages, &config, &llm, None, None)
        .await
        .unwrap();
    assert_eq!(compacted[0].role, "system");
    assert_eq!(compacted[0].content, "system-prompt");
    let summary_message = compacted
        .iter()
        .find(|m| m.content.contains(COMPACTION_SUMMARY_PREFIX))
        .expect("the prefixed summary must be present");
    assert!(
        summary_message
            .content
            .ends_with("LLM summary of the conversation.")
    );
    assert!(
        compacted
            .iter()
            .any(|m| m.content.contains(COMPACTION_CONTINUATION_TEXT)),
        "the continuation note must be present after the summary"
    );
}

#[tokio::test]
async fn test_force_compact_with_summary_returns_error_on_empty_content() {
    let messages = compactable_messages();
    let config = compacting_config();
    let llm = SummarizerMockLlm::ok("");
    let compacted = force_compact_messages_with_summary(&messages, &config, &llm, None, None).await;
    assert!(compacted.is_err());
}

#[tokio::test]
async fn test_force_compact_with_summary_returns_error_on_error() {
    let messages = compactable_messages();
    let config = compacting_config();
    let llm = SummarizerMockLlm::error();
    let compacted = force_compact_messages_with_summary(&messages, &config, &llm, None, None).await;
    assert!(compacted.is_err());
}

#[tokio::test]
async fn test_force_compact_with_summary_no_compaction_returns_unchanged() {
    let messages = vec![msg("system", "sys"), msg("user", "hi")];
    let config = compacting_config();
    let llm = SummarizerMockLlm::ok("unused");
    let compacted = force_compact_messages_with_summary(&messages, &config, &llm, None, None)
        .await
        .unwrap();
    assert_messages_eq(&compacted, &messages);
}

#[tokio::test]
async fn test_compact_with_summary_skips_when_below_threshold() {
    let messages = compactable_messages();
    let config = small_config(100_000);
    let llm = SummarizerMockLlm::ok("unused");
    let compacted = compact_messages_with_summary(&messages, &config, &llm, None, None)
        .await
        .unwrap();
    assert!(compacted.is_none(), "below threshold must be a no-op");
}

#[tokio::test]
async fn test_compact_with_summary_triggers_when_above_threshold() {
    let messages = compactable_messages();
    let config = compacting_config();
    let llm = SummarizerMockLlm::ok("Real summary.");
    let compacted = compact_messages_with_summary(&messages, &config, &llm, None, None)
        .await
        .unwrap()
        .expect("above threshold must compact");
    let summary_message = compacted
        .iter()
        .find(|m| m.content.contains(COMPACTION_SUMMARY_PREFIX))
        .expect("the prefixed summary must be present");
    assert!(summary_message.content.ends_with("Real summary."));
}

#[tokio::test]
async fn test_compact_with_summary_passes_instruction_to_summarizer() {
    let messages = compactable_messages();
    let config = compacting_config();
    let llm = SummarizerMockLlm::ok("Instruction-aware summary.");
    let compacted =
        compact_messages_with_summary(&messages, &config, &llm, Some("Focus on user goals."), None)
            .await
            .unwrap()
            .expect("above threshold must compact");
    let summary_message = compacted
        .iter()
        .find(|m| m.content.contains(COMPACTION_SUMMARY_PREFIX))
        .expect("the prefixed summary must be present");
    assert!(
        summary_message
            .content
            .ends_with("Instruction-aware summary.")
    );
    assert!(
        llm.last_user_content().contains("Focus on user goals."),
        "the custom instruction must ride into the summarizer prompt, got: {}",
        llm.last_user_content()
    );
}

#[tokio::test]
async fn test_manual_compaction_with_summary_uses_deepest_split() {
    let messages = compactable_messages();
    let config = compacting_config();
    let llm = SummarizerMockLlm::ok("Manual summary.");
    let (compacted, raw_summary) = force_compact_messages_manual_with_summary(
        &messages, &config, &llm, None, None, None, None,
    )
    .await
    .unwrap();
    assert_eq!(raw_summary, "Manual summary.");
    let count = compute_compact_count_manual(&messages, &config);
    assert!(count >= 2, "manual compaction must remove messages");
    assert_eq!(compacted[0].role, "system");
    assert_eq!(compacted[0].content, "system-prompt");
    let summary_message = compacted
        .iter()
        .find(|m| m.content.contains(COMPACTION_SUMMARY_PREFIX))
        .expect("the prefixed summary must be present");
    assert!(summary_message.content.ends_with("Manual summary."));
    assert!(
        compacted.len()
            < force_compact_messages_with_summary(&messages, &config, &llm, None, None)
                .await
                .unwrap()
                .len(),
        "manual compaction must keep a smaller tail than auto compaction"
    );
}

#[tokio::test]
async fn test_manual_compaction_with_summary_guards_against_header_only_count() {
    // A window smaller than the header lets the fit loop shrink the split
    // to index 1 — compacting "only the header" must be a no-op, never an
    // inserted summary with nothing removed.
    let messages = vec![
        msg("system", &"s".repeat(100)),
        msg("user", "user-1"),
        msg("assistant", "assistant-1"),
    ];
    let config = CompactionConfig {
        max_context_tokens: 10,
        ..CompactionConfig::default()
    };
    assert_eq!(compute_compact_count_manual(&messages, &config), 0);
    let llm = SummarizerMockLlm::ok("unused");
    let (compacted, raw_summary) = force_compact_messages_manual_with_summary(
        &messages, &config, &llm, None, None, None, None,
    )
    .await
    .unwrap();
    assert_eq!(raw_summary, "");
    assert_messages_eq(&compacted, &messages);
    assert_eq!(
        llm.call_count(),
        0,
        "no summary call for a no-op compaction"
    );
}

#[tokio::test]
async fn test_summarizer_retries_retryable_failure_then_succeeds() {
    let omitted = vec![msg("user", "user-1"), msg("assistant", "assistant-1")];
    let llm = SummarizerMockLlm::transient("Recovered summary.", 1);
    let result = summarize_with_llm(&omitted, &llm, None, None, None).await;
    assert_eq!(result.unwrap(), "Recovered summary.");
    assert_eq!(llm.call_count(), 2, "one retry after the transient failure");
}

/// v2's overflow recovery inside the compaction request
/// (fullCompactionService.ts:690-710): when the summarization request
/// itself overflows, the history is shrunk by the attempt's ratio and
/// retried instead of failing the compaction.
#[tokio::test]
async fn test_summarizer_shrinks_history_after_overflow_and_retries() {
    let omitted = vec![
        msg("user", "user-1"),
        msg("assistant", "assistant-1"),
        msg("user", "user-2"),
        msg("assistant", "assistant-2"),
        msg("user", "user-3"),
    ];
    let llm = SummarizerMockLlm::overflow_then_ok("Recovered summary.", 1);
    let result = summarize_with_llm_budgeted(&omitted, &llm, None, None, None, None).await;
    assert_eq!(result.unwrap(), "Recovered summary.");
    assert_eq!(llm.call_count(), 2, "one shrink-retry after the overflow");
    let second_prompt = llm.last_user_content();
    assert!(
        !second_prompt.contains("user-1"),
        "the shrink must drop the oldest message: {second_prompt}"
    );
    assert!(
        second_prompt.contains("user-3"),
        "the newest messages must survive the shrink: {second_prompt}"
    );
}

/// The shrink is bounded: after `MAX_COMPACTION_OVERFLOW_SHRINK_ATTEMPTS`
/// shrinks the original error surfaces instead of looping forever.
#[tokio::test]
async fn test_summarizer_gives_up_after_max_overflow_shrinks() {
    // Enough messages that the `len <= 1` guard does not preempt the
    // shrink bound: each shrink keeps a majority of the tail.
    let mut omitted = Vec::new();
    for i in 0..11 {
        omitted.push(msg(
            if i % 2 == 0 { "user" } else { "assistant" },
            &format!("m{i}"),
        ));
    }
    let llm = SummarizerMockLlm::overflow_always();
    // A generous attempt cap isolates the shrink bound as the stop reason.
    let result = summarize_with_llm_budgeted(&omitted, &llm, None, None, Some(10), None).await;
    assert!(
        matches!(result, Err(CompactionError::Provider(_))),
        "the original overflow error must surface"
    );
    assert_eq!(
        llm.call_count(),
        1 + MAX_COMPACTION_OVERFLOW_SHRINK_ATTEMPTS,
        "one initial request plus one per shrink attempt"
    );
}

/// v2 #3750: the host's `loopControl.compactionMaxAttempts` is a true cap
/// on total requests, so a summarizer that never succeeds stops there
/// instead of at the engine default.
#[tokio::test]
async fn test_summarizer_honors_the_configured_attempt_cap() {
    let omitted = vec![msg("user", "user-1"), msg("assistant", "assistant-1")];
    let llm = SummarizerMockLlm::transient("never reached", u32::MAX);
    let result = summarize_with_llm(&omitted, &llm, None, None, Some(2)).await;
    assert!(result.is_err());
    assert_eq!(
        llm.call_count(),
        2,
        "the configured cap bounds the requests"
    );
}

/// The cap rides `CompactionConfig`, so the wrappers the turn loop calls
/// pass it through to the summarizer.
#[tokio::test]
async fn test_compaction_config_attempt_cap_reaches_the_summarizer() {
    let messages = compactable_messages();
    let config = CompactionConfig {
        max_attempts: Some(1),
        ..compacting_config()
    };
    let llm = SummarizerMockLlm::transient("never reached", u32::MAX);
    let compacted = force_compact_messages_with_summary(&messages, &config, &llm, None, None).await;
    assert_eq!(llm.call_count(), 1, "one request, then an error");
    assert!(compacted.is_err());
}

#[tokio::test]
async fn test_summarizer_cancel_during_backoff_returns_error() {
    let omitted = vec![msg("user", "user-1"), msg("assistant", "assistant-1")];
    let llm = SummarizerMockLlm::transient("never reached", u32::MAX);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let started = std::time::Instant::now();
    let result = summarize_with_llm(&omitted, &llm, None, Some(&cancel), None).await;
    assert!(result.is_err());
    assert_eq!(llm.call_count(), 0, "cancel must not start a request");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "cancel during backoff must return immediately, took {:?}",
        started.elapsed()
    );
}

#[test]
fn test_compaction_continuation_message_structure() {
    let msg = compaction_continuation_message();
    assert_eq!(msg.role, "user");
    assert!(msg.content.contains("Context compaction is complete"));
    assert!(msg.content.starts_with("<system-reminder>\n"));
    assert!(msg.content.ends_with("\n</system-reminder>"));
}

#[test]
fn test_select_kept_user_messages_head_elision_tail_budget() {
    // v2 `selectCompactionUserMessages` (compactionHandoff.ts:234): above
    // the 20k budget the selection keeps at most the 2k head (ended by the
    // first truncating message) plus the 18k tail, with the elision note
    // between. 10 user messages x 3000 tokens = 30000 > 20000.
    let user_messages: Vec<LLMMessage> = (0..10)
        .map(|i| LLMMessage {
            role: "user".into(),
            content: format!("m{i:02}") + &"x".repeat(11_997), // 12000 chars = 3000 tokens
            ..Default::default()
        })
        .collect();
    assert_eq!(estimate_messages_tokens(&user_messages), 30_000);

    let kept = select_kept_user_messages(&user_messages);
    // [1 truncated head, elision, 6 full tail messages]
    assert_eq!(kept.len(), 8);
    let head_message = &kept[0];
    assert_eq!(
        estimate_message_tokens(head_message),
        COMPACT_USER_MESSAGE_HEAD_TOKENS,
        "one truncating message ends the head at the head budget"
    );
    let elision = &kept[1];
    assert_eq!(elision.role, "user");
    assert!(
        elision.content.contains("omitted here during compaction"),
        "kept[1] must be the elision note, got: {}",
        &elision.content[..elision.content.len().min(80)]
    );
    assert!(
        elision.content.contains("roughly 10000 tokens"),
        "elision records 30000 - 20000 = 10000 omitted tokens"
    );
    let tail = &kept[2..];
    assert_eq!(tail.len(), 6, "the tail survives verbatim below the note");
    for message in tail {
        assert_eq!(estimate_message_tokens(message), 3_000);
    }
    let verbatim: u32 = std::iter::once(head_message)
        .chain(tail.iter())
        .map(estimate_message_tokens)
        .sum();
    assert_eq!(verbatim, COMPACT_USER_MESSAGE_MAX_TOKENS);
    // The tail loop stops at tail_remaining == 0 — no empty suffix message.
    assert!(
        kept.iter().all(|m| !m.content.is_empty()),
        "no empty-content message may be kept"
    );
}
