use crate::mcp::manager::{McpServerOptions, McpServerRecipe};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex};

use super::*;

fn js_native_llm() -> JsNativeLlmConfig {
    JsNativeLlmConfig {
        protocol: "openai".into(),
        provider_type: None,
        base_url: "https://api.example.com/v1".into(),
        api_key: "test-key".into(),
        api_key_env: None,
        model: "test-model".into(),
        max_tokens: None,
        custom_headers: None,
        reasoning_effort: None,
        thinking_budget: None,
        auth_provider: None,
        thinking_keep: None,
        beta_api: None,
        capabilities: None,
        system_prompt: None,
        max_input_size: None,
        adaptive_thinking: None,
        reasoning_key: None,
        off_effort: None,
    }
}

/// Two sessions built from the same configuration share one manager, so
/// `/new` reuses the live connections instead of re-spawning every server
/// (v2's workspace-scoped manager, workspaceMcpService.ts:61-83).
#[tokio::test]
async fn test_shared_mcp_manager_reuses_one_manager_per_configuration() {
    let demo = || {
        vec![(
            "demo".to_string(),
            McpServerRecipe::Mock,
            McpServerOptions::default(),
        )]
    };

    let first = shared_mcp_manager(demo(), None, None);
    let second = shared_mcp_manager(demo(), None, None);
    assert!(
        Arc::ptr_eq(&first, &second),
        "one configuration, one manager"
    );

    // The key covers the whole resolved server, not just its name: the
    // same name with different options is a different manager.
    let other = shared_mcp_manager(
        vec![(
            "demo".to_string(),
            McpServerRecipe::Mock,
            McpServerOptions {
                enabled: false,
                ..Default::default()
            },
        )],
        None,
        None,
    );
    assert!(
        !Arc::ptr_eq(&first, &other),
        "the same name with different options must not share a manager"
    );

    // `[mcp]` defaults are part of the identity: the disclosure and
    // timeouts a reused manager resolved for its first session would
    // otherwise leak into a session that configured different ones.
    let with_section = shared_mcp_manager(
        demo(),
        None,
        Some(crate::config::McpConfig {
            default_deferred: Some(true),
            ..Default::default()
        }),
    );
    assert!(
        !Arc::ptr_eq(&first, &with_section),
        "a different [mcp] section must not reuse the resolved manager"
    );

    // The per-server tri-state is part of the identity too: `deferred`
    // controls disclosure but was absent from the key before it became
    // an Option.
    let inline = shared_mcp_manager(
        vec![(
            "demo".to_string(),
            McpServerRecipe::Mock,
            McpServerOptions {
                deferred: Some(false),
                ..Default::default()
            },
        )],
        None,
        None,
    );
    assert!(
        !Arc::ptr_eq(&first, &inline),
        "an explicit per-server deferred verdict must not reuse the manager"
    );
}

/// A model that declares an input cap below its window compacts against the
/// cap; without one the host's total window stands.
#[test]
fn a_declared_input_cap_narrows_the_compaction_window() {
    let mut native_llm = js_native_llm();
    native_llm.max_input_size = Some(272_000);
    assert_eq!(
        compaction_window(Some(&native_llm), Some(400_000)),
        Some(272_000)
    );

    native_llm.max_input_size = None;
    assert_eq!(
        compaction_window(Some(&native_llm), Some(400_000)),
        Some(400_000)
    );
    assert_eq!(compaction_window(None, Some(400_000)), Some(400_000));
    assert_eq!(compaction_window(None, None), None);
}

/// Pruning must drop the OLDEST payloads. Iterating a HashMap yields ids
/// in arbitrary order, which used to let the prune discard payloads JS had
/// not collected yet while keeping long-dead ones.
#[test]
fn payload_registry_prunes_oldest_first() {
    let base = NEXT_CALLBACK_ID.fetch_add(2_000, Ordering::SeqCst);
    let total = PAYLOAD_REGISTRY_MAX_ENTRIES as u32 + 50;
    for offset in 0..total {
        store_payload(base + offset, format!("p{offset}"));
    }

    let registry = PAYLOAD_REGISTRY.lock().unwrap();
    assert!(
        registry.len() <= PAYLOAD_REGISTRY_MAX_ENTRIES,
        "registry must stay bounded"
    );
    assert!(
        !registry.contains_key(&base),
        "the oldest payload must go first"
    );
    assert!(
        registry.contains_key(&(base + total - 1)),
        "the newest payload must survive"
    );
}

static TEST_GUARD_MAP: LazyLock<Mutex<HashMap<String, u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn early_return_with_guard() -> Result<(), ()> {
    let _guard = MapEntryGuard::insert(&TEST_GUARD_MAP, "early".to_string(), 1);
    Err(())
}

/// The explicit `remove` after the awaited work is skipped by an early `?`
/// return and never runs at all when the napi future is dropped, so the
/// registration has to be released by scope exit.
#[test]
fn the_map_entry_guard_removes_on_every_exit_path() {
    {
        let _guard = MapEntryGuard::insert(&TEST_GUARD_MAP, "scope".to_string(), 7);
        assert_eq!(TEST_GUARD_MAP.lock().unwrap().get("scope"), Some(&7));
    }
    assert!(!TEST_GUARD_MAP.lock().unwrap().contains_key("scope"));

    assert!(early_return_with_guard().is_err());
    assert!(!TEST_GUARD_MAP.lock().unwrap().contains_key("early"));
}

/// A malformed tool schema used to become `Value::Null` and reach the
/// provider, which rejected the whole request with a 400 naming no tool.
#[test]
fn a_malformed_tool_schema_fails_naming_the_tool() {
    let tools = vec![JsToolDef {
        name: "Read".into(),
        description: "read a file".into(),
        input_schema: "{not json".into(),
    }];
    let error = tool_defs_from_wire(&tools).expect_err("a bad schema must not pass");
    assert!(
        error.reason.contains("Read"),
        "the error must name the tool: {}",
        error.reason
    );
}

#[test]
fn a_well_formed_tool_schema_is_passed_through() {
    let tools = vec![JsToolDef {
        name: "Read".into(),
        description: "read a file".into(),
        input_schema: r#"{"type":"object"}"#.into(),
    }];
    let defs = tool_defs_from_wire(&tools).expect("a valid schema must pass");
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].name, "Read");
    assert_eq!(defs[0].input_schema["type"], "object");
}

/// A malformed `blocks_json` used to erase the message's content with no
/// trace; the fallback stays empty, but the drop is now reported.
#[test]
fn unparseable_host_json_falls_back_to_empty() {
    let blocks: Vec<serde_json::Value> =
        parse_host_json(Some("{not json"), "message blocks", "assistant");
    assert!(blocks.is_empty());

    let blocks: Vec<serde_json::Value> =
        parse_host_json(Some(r#"[{"type":"text"}]"#), "message blocks", "assistant");
    assert_eq!(blocks.len(), 1);

    let blocks: Vec<serde_json::Value> = parse_host_json(None, "message blocks", "assistant");
    assert!(blocks.is_empty());
}
