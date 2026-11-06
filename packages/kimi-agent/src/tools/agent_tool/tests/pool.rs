use super::super::*;
use super::common::*;
use super::*;

// ── [secondary_model] pool ─────────────────────────────────────────────

#[tokio::test]
async fn agent_with_a_pool_alias_runs_on_the_pool_llm() {
    let recorder = Arc::new(EventRecorder::new());
    let session_llm = Arc::new(RecordingPromptLlm::new(vec!["session answer".into()]));
    let alias_llm = Arc::new(RecordingPromptLlm::new(vec!["pool answer".into()]));
    let manager = manager_with_callbacks(session_llm.clone(), recorder.clone()).await;
    let pool = pool_runtime(alias_llm.clone());

    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({
            "subagent_type": "coder",
            "prompt": "hard task",
            "model": "strong",
        }),
        None,
        None,
        None,
        Some(&pool),
    )
    .await
    .expect("pool aliases run natively");

    assert!(!result.is_error, "{}", result.content);
    assert_eq!(alias_llm.call_count(), 1, "the pool alias served the turn");
    assert_eq!(session_llm.call_count(), 0, "the session model stayed idle");
}

/// v2 #4418: `primary` is no longer an implicit choice — on the Agent
/// path it is an invalid alias and the available list names pool entries
/// only.
#[tokio::test]
async fn agent_primary_is_an_invalid_alias_now() {
    let recorder = Arc::new(EventRecorder::new());
    let session_llm = Arc::new(RecordingPromptLlm::new(vec!["session answer".into()]));
    let alias_llm = Arc::new(RecordingPromptLlm::new(vec!["pool answer".into()]));
    let manager = manager_with_callbacks(session_llm.clone(), recorder.clone()).await;
    let pool = pool_runtime(alias_llm.clone());

    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({
            "subagent_type": "coder",
            "prompt": "quality task",
            "model": PRIMARY_MODEL_CHOICE,
        }),
        None,
        None,
        None,
        Some(&pool),
    )
    .await
    .expect("primary resolves to an error result, not a host fallback");

    assert!(result.is_error);
    assert!(
        result.content.contains("Invalid model \"primary\""),
        "{}",
        result.content
    );
    assert!(
        result.content.contains("Available models: strong."),
        "{}",
        result.content
    );
    assert_eq!(session_llm.call_count(), 0, "no turn ran");
}

#[tokio::test]
async fn agent_invalid_pool_alias_lists_the_choices() {
    let recorder = Arc::new(EventRecorder::new());
    let session_llm = Arc::new(RecordingPromptLlm::new(vec!["session answer".into()]));
    let manager = manager_with_callbacks(session_llm.clone(), recorder.clone()).await;
    let pool = pool_runtime(Arc::new(RecordingPromptLlm::new(vec![])));

    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({
            "subagent_type": "coder",
            "prompt": "task",
            "model": "nope",
        }),
        None,
        None,
        None,
        Some(&pool),
    )
    .await
    .expect("invalid aliases resolve to an error result, not a host fallback");

    assert!(result.is_error);
    assert!(
        result.content.contains("Invalid model \"nope\""),
        "{}",
        result.content
    );
    // v2 #4418: the available list names pool entries only — no implicit
    // `primary` choice.
    assert!(
        result.content.contains("Available models: strong."),
        "{}",
        result.content
    );
    assert_eq!(session_llm.call_count(), 0, "no turn ran");
}

/// v2 #4418: a single-entry pool pins every spawn — an omitted `model`
/// binds the default alias (the tool schema hides the parameter, but the
/// engine still resolves an explicit one).
#[tokio::test]
async fn agent_single_entry_pool_binds_the_default_when_model_is_omitted() {
    let recorder = Arc::new(EventRecorder::new());
    let session_llm = Arc::new(RecordingPromptLlm::new(vec!["session answer".into()]));
    let alias_llm = Arc::new(RecordingPromptLlm::new(vec!["pool answer".into()]));
    let manager = manager_with_callbacks(session_llm.clone(), recorder.clone()).await;
    let pool = pool_runtime(alias_llm.clone());

    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({
            "subagent_type": "coder",
            "prompt": "task",
        }),
        None,
        None,
        None,
        Some(&pool),
    )
    .await
    .expect("a pinned pool binds natively");

    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        alias_llm.call_count(),
        1,
        "the pool default served the turn"
    );
    assert_eq!(session_llm.call_count(), 0, "the session model stayed idle");
}

#[tokio::test]
async fn agent_without_a_pool_rejects_an_explicit_model() {
    let recorder = Arc::new(EventRecorder::new());
    let session_llm = Arc::new(RecordingPromptLlm::new(vec!["session answer".into()]));
    let manager = manager_with_callbacks(session_llm.clone(), recorder.clone()).await;

    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({
            "subagent_type": "coder",
            "prompt": "task",
            "model": "strong",
        }),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("no-pool model overrides are error results, not host fallbacks");

    assert!(result.is_error);
    assert!(
        result
            .content
            .contains("no [secondary_model] pool is configured"),
        "{}",
        result.content
    );
    assert_eq!(session_llm.call_count(), 0, "no turn ran");
}

/// v2 #4418's no-pool arm: `primary` is rejected like any other value —
/// omitting `model` is the only way to inherit.
#[tokio::test]
async fn agent_without_a_pool_rejects_primary_too() {
    let recorder = Arc::new(EventRecorder::new());
    let session_llm = Arc::new(RecordingPromptLlm::new(vec!["session answer".into()]));
    let manager = manager_with_callbacks(session_llm.clone(), recorder.clone()).await;

    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({
            "subagent_type": "coder",
            "prompt": "task",
            "model": PRIMARY_MODEL_CHOICE,
        }),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("primary without a pool is an error result");

    assert!(result.is_error);
    assert!(
        result
            .content
            .contains("no [secondary_model] pool is configured"),
        "{}",
        result.content
    );
    assert_eq!(session_llm.call_count(), 0, "no turn ran");
}

/// v2 `planSpawn`: a profile outside the caller's allowlist is refused
/// with the guidance text; a discovered profile joins the extras and
/// spawns; the caller profile's own declaration narrows the set.
#[tokio::test]
async fn the_spawn_gate_enforces_the_callers_allowlist() {
    let recorder = Arc::new(EventRecorder::new());
    let llm = Arc::new(RecordingPromptLlm::new(vec!["gate ok".into()]));
    let manager = manager_with_callbacks(llm.clone(), recorder.clone()).await;

    // The engine-builtin `research` is not in the default delegation set.
    let refused = execute_agent(
        &manager,
        None,
        &serde_json::json!({ "subagent_type": "research", "prompt": "go" }),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("a denied type is a native error result");
    assert!(refused.is_error);
    assert_eq!(
        refused.content,
        "Subagent type \"research\" is not allowed for this agent. Allowed subagent types: coder, explore, plan."
    );

    // Recursive delegation is refused by the same list.
    let refused = execute_agent(
        &manager,
        None,
        &serde_json::json!({ "subagent_type": "agent", "prompt": "go" }),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        refused.is_error,
        "agent must not spawn agent: {}",
        refused.content
    );

    // A discovered profile joins the extras and spawns.
    manager
        .register_discovered_definition(crate::subagent::types::SubagentDefinition {
            name: "reviewer".into(),
            description: "Reviewer subagent".into(),
            system_prompt: "You review.".into(),
            tools: vec![],
            disallowed_tools: vec![],
            when_to_use: None,
            prompt_prefix: None,
            summary_policy: None,
            model: None,
            subagents: None,
        })
        .await;
    let spawned = execute_agent(
        &manager,
        None,
        &serde_json::json!({ "subagent_type": "reviewer", "prompt": "go" }),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("a discovered profile spawns natively");
    assert!(!spawned.is_error, "{}", spawned.content);

    // The caller profile's own declaration narrows the set: a main
    // profile declaring only `coder` loses `explore` (discovered extras
    // still merge, the way v2's rootDelegationExtras works).
    manager
        .register_profile_snapshot(&[crate::rpc::types::SubagentProfileWire {
            name: "agent".into(),
            description: "custom main".into(),
            system_prompt: String::new(),
            tools: vec![],
            disallowed_tools: vec![],
            when_to_use: None,
            prompt_prefix: None,
            summary_policy: None,
            subagents: Some(vec!["coder".into()]),
        }])
        .await;
    let refused = execute_agent(
        &manager,
        None,
        &serde_json::json!({ "subagent_type": "explore", "prompt": "go" }),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(refused.is_error);
    assert!(
        refused
            .content
            .contains("Subagent type \"explore\" is not allowed"),
        "{}",
        refused.content
    );
}
