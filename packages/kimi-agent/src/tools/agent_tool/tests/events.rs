use super::super::*;
use super::common::*;
use super::*;

#[test]
fn tool_policy_filter_allowlist_and_disallowed() {
    use crate::subagent::manager::ToolPolicyFilter;
    use crate::subagent::types::SubagentDefinition;
    let allow = ToolPolicyFilter::from_definition(&SubagentDefinition {
        name: "a".into(),
        description: String::new(),
        system_prompt: String::new(),
        tools: vec!["read".into(), "grep".into()],
        disallowed_tools: vec![],
        when_to_use: None,
        prompt_prefix: None,
        summary_policy: None,
        model: None,
        subagents: None,
    });
    assert!(allow.allows("Read"));
    assert!(allow.allows("grep"));
    assert!(!allow.allows("bash"));

    let deny = ToolPolicyFilter::from_definition(&SubagentDefinition {
        name: "b".into(),
        description: String::new(),
        system_prompt: String::new(),
        tools: vec![],
        disallowed_tools: vec!["Bash".into()],
        when_to_use: None,
        prompt_prefix: None,
        summary_policy: None,
        model: None,
        subagents: None,
    });
    assert!(deny.allows("read"));
    assert!(!deny.allows("bash"));
}

#[tokio::test]
async fn prompt_prefix_rides_ahead_of_the_prompt() {
    let llm = Arc::new(RecordingPromptLlm::new(vec!["done".into()]));
    let manager = manager_with(llm.clone()).await;
    manager
        .register_definition(crate::subagent::types::SubagentDefinition {
            name: "explore".into(),
            description: "d".into(),
            when_to_use: None,
            system_prompt: "You explore.".into(),
            tools: vec![],
            disallowed_tools: vec![],
            prompt_prefix: Some("<git-context>".into()),
            summary_policy: None,
            model: None,
            subagents: None,
        })
        .await;
    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({ "subagent_type": "explore", "prompt": "scan it" }),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("prefix profile runs natively");
    assert!(!result.is_error);
    let prompt = llm.first_prompt();
    assert!(
        prompt.contains("<git-context>\n\nscan it"),
        "prefix rides ahead of the prompt as {{prefix}}\\n\\n{{prompt}}: {prompt}"
    );
    assert_eq!(llm.call_count(), 1, "no policy: no continuation turns");
}

#[tokio::test]
async fn summary_policy_distills_through_continuation_turns() {
    let llm = Arc::new(RecordingPromptLlm::new(vec![
        "too short".into(),
        "a much longer final summary that clearly clears the floor".into(),
    ]));
    let manager = manager_with(llm.clone()).await;
    manager
        .register_definition(crate::subagent::types::SubagentDefinition {
            name: "coder".into(),
            description: "d".into(),
            system_prompt: "You code.".into(),
            tools: vec![],
            disallowed_tools: vec![],
            when_to_use: None,
            prompt_prefix: None,
            summary_policy: Some(SummaryPolicy {
                min_chars: 20,
                continuation_prompt: "Summarize fully.".into(),
                retries: 2,
            }),
            model: None,
            subagents: None,
        })
        .await;
    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({ "subagent_type": "coder", "prompt": "do it" }),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("distill runs natively");
    assert!(!result.is_error);
    assert!(
        result
            .content
            .contains("[summary]\na much longer final summary that clearly clears the floor"),
        "the adequate continuation text becomes the summary: {}",
        result.content
    );
    assert_eq!(llm.call_count(), 2, "one continuation turn");
    let second_prompt = llm.prompts.lock().unwrap()[1].clone();
    assert_eq!(second_prompt, "Summarize fully.");
}

#[tokio::test]
async fn adequate_summary_skips_distillation() {
    let long = "x".repeat(200);
    let llm = Arc::new(RecordingPromptLlm::new(vec![long.clone()]));
    let manager = manager_with(llm.clone()).await;
    manager
        .register_definition(crate::subagent::types::SubagentDefinition {
            name: "coder".into(),
            description: "d".into(),
            system_prompt: "You code.".into(),
            tools: vec![],
            disallowed_tools: vec![],
            when_to_use: None,
            prompt_prefix: None,
            summary_policy: Some(SummaryPolicy {
                min_chars: 100,
                continuation_prompt: "continue".into(),
                retries: 1,
            }),
            model: None,
            subagents: None,
        })
        .await;
    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({ "subagent_type": "coder", "prompt": "do it" }),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("runs natively");
    assert!(!result.is_error);
    assert_eq!(llm.call_count(), 1, "adequate summary: no re-prompt");
    assert!(result.content.contains(&long));
}

#[tokio::test]
async fn lifecycle_events_mirror_the_v2_surface() {
    let llm = Arc::new(RecordingPromptLlm::new(vec!["findings: all done".into()]));
    let recorder = Arc::new(EventRecorder::new());
    let manager = manager_with_callbacks(llm, recorder.clone()).await;
    manager
        .register_discovered_definition(crate::subagent::types::SubagentDefinition {
            name: "research".into(),
            description: "d".into(),
            system_prompt: "You research.".into(),
            tools: vec![],
            disallowed_tools: vec![],
            when_to_use: None,
            prompt_prefix: None,
            summary_policy: None,
            model: None,
            subagents: None,
        })
        .await;
    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({
            "subagent_type": "research",
            "prompt": "go",
            "description": "Scout"
        }),
        None,
        None,
        Some("tool-call-7"),
        None,
    )
    .await
    .expect("runs natively");
    assert!(!result.is_error);
    assert_eq!(
        recorder.types(),
        vec![
            "subagent.spawned".to_string(),
            "subagent.started".to_string(),
            "llm.step.begin".to_string(),
            "llm.step.end".to_string(),
            "subagent.completed".to_string(),
        ]
    );
    let events = recorder.events.lock().unwrap();
    let spawned = &events[0];
    assert_eq!(spawned["subagent_name"], "research");
    assert_eq!(spawned["parent_tool_call_id"], "tool-call-7");
    assert_eq!(spawned["description"], "Scout");
    assert_eq!(spawned["run_in_background"], false);
    let agent_id = spawned["subagent_id"].as_str().unwrap();
    assert!(agent_id.starts_with("agent-"));
    assert_eq!(events[1]["subagent_id"], spawned["subagent_id"]);

    assert_eq!(events[2]["type"], "llm.step.begin");
    assert_eq!(events[3]["type"], "llm.step.end");
    assert!(events[3]["step"].is_number());
    let completed = events.last().unwrap();
    assert_eq!(completed["result_summary"], "findings: all done");
    assert!(completed["usage"]["total_tokens"].is_number());
}

#[tokio::test]
async fn subagent_hooks_fire_from_the_emit_helpers() {
    use crate::tools::external_hooks::capture;
    let Some(dir) = capture::dir() else {
        return;
    };
    let started = dir.path().join("subagent-start.json");
    let stopped = dir.path().join("subagent-stop.json");
    let recorder = EventRecorder::with_hook_guard(Arc::new(
        crate::tools::external_hooks::HookGuard::new(vec![
            capture::hook("SubagentStart", "explore", &started),
            capture::hook("SubagentStop", "explore", &stopped),
        ]),
    ));

    emit_spawned_started(
        &recorder,
        "child-1",
        "explore",
        Some("c1"),
        Some("scan"),
        false,
        None,
        "scan the tree",
    );
    emit_completed(
        &recorder,
        "child-1",
        "explore",
        "all done",
        &TokenUsage::default(),
    );

    let start_payload = capture::wait(&started).await;
    assert_eq!(start_payload["hook_event_name"], "SubagentStart");
    assert_eq!(start_payload["agent_name"], "explore");
    assert_eq!(start_payload["prompt"], "scan the tree");

    let stop_payload = capture::wait(&stopped).await;
    assert_eq!(stop_payload["hook_event_name"], "SubagentStop");
    assert_eq!(stop_payload["response"], "all done");
}
